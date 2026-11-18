use anyhow::{Context, Result};
use sqlx::{Connection, PgConnection};

use super::ontology::CURRENT;

const TEXTS: &str = "select d.id, concat_ws(' ', d.path, d.file, \
        (select string_agg(p.value, ' ') from jsonb_each_text(d.props) p), \
        (select string_agg(distinct concat_ws(' ', e.key, e.name), ' ') \
            from relations r join entities e on e.id in (r.src_id, r.dst_id) \
            where r.document_id = d.id and r.method = 'path' and r.status <> 'rejected' \
            and e.document_id is null), \
        (select string_agg(l.label, ' ') from entities de \
            left join entity_classes o on o.entity_id = de.id \
                and o.status in ('auto', 'confirmed') \
            join classes c on c.iri = coalesce(o.class_iri, de.class_iri) \
                and c.ontology_version_id = ({current}) \
            cross join lateral (select value from jsonb_each_text(c.labels) \
                union all select jsonb_array_elements_text(value) from jsonb_each(c.alt_labels)) \
                l(label) \
            where de.document_id = d.id and de.key = d.id::text)) as text \
    from documents d where d.status <> 'deleted'";

const VECTOR: &str = "to_tsvector('simple', translate(t.text, '/-_.', '    ')) \
    || to_tsvector('simple', coalesce((select string_agg(regexp_replace(m[1], '[-_]', '', 'g'), ' ') \
        from regexp_matches(t.text, '(?<![[:alnum:]])({PART}(?:[-_]{PART})+)(?![[:alnum:]])', 'g') m \
        where m[1] ~ '[0-9]'), ''))";

const PART: &str = "(?:[[:alnum:]]*[0-9][[:alnum:]]*|[[:alpha:]]{1,3})";

pub async fn refresh(conn: &mut PgConnection) -> Result<u64> {
    let mut tx = conn.begin().await?;
    sqlx::query(
        "delete from document_names n where not exists (select 1 from documents d \
         where d.id = n.document_id and d.status <> 'deleted')",
    )
    .execute(&mut *tx)
    .await?;
    let sql = format!(
        "insert into document_names (document_id, tsv) select t.id, {} from ({}) t \
         on conflict (document_id) do update set tsv = excluded.tsv \
         where document_names.tsv is distinct from excluded.tsv",
        VECTOR.replace("{PART}", PART),
        TEXTS.replace("{current}", CURRENT)
    );
    let written = sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&mut *tx)
        .await
        .context("cannot refresh the document names")?
        .rows_affected();
    tx.commit().await?;
    Ok(written)
}
