use anyhow::{Context, Result};
use sqlx::{FromRow, PgConnection};

use super::identifier;
use super::resolve::SAME_AS;

pub const METHOD: &str = "human";

#[derive(Debug, Clone, FromRow, PartialEq)]
pub struct Decision {
    pub id: i64,
    pub kind: String,
    pub class_iri: String,
    pub key: String,
    pub other_class_iri: Option<String>,
    pub other_key: Option<String>,
    pub property_iri: Option<String>,
    pub verdict: Option<String>,
    pub value: Option<String>,
    pub old_value: Option<String>,
    pub source: Option<String>,
    pub path: Option<String>,
    pub page: Option<i32>,
}

pub fn view_name(name: &str) -> Result<String> {
    let (schema, view) = name
        .split_once('.')
        .with_context(|| format!("{name:?} is not schema.view"))?;
    Ok(format!("{}.{}", identifier(schema)?, identifier(view)?))
}

pub async fn read(conn: &mut PgConnection, view: &str) -> Result<Vec<Decision>> {
    let sql = format!(
        "select id, kind, class_iri, key, other_class_iri, other_key, property_iri, verdict, \
         value, old_value, source, path, page from {} order by id",
        view_name(view)?
    );
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .with_context(|| format!("cannot read the decisions in {view}"))?;
    Ok(rows)
}

pub async fn reset(conn: &mut PgConnection) -> Result<()> {
    sqlx::raw_sql(
        "update relations r set status = d.before from decision_statuses d \
         where r.id = d.relation_id; \
         delete from decision_statuses; \
         delete from relations where method = 'human'; \
         delete from aliases where method = 'human'; \
         delete from entity_classes where method = 'human'; \
         insert into entity_classes (entity_id, class_iri, name, method, status, confidence, \
         ontology_version_id) select entity_id, class_iri, name, method, status, confidence, \
         ontology_version_id from decision_overrides on conflict (entity_id) do nothing; \
         delete from decision_overrides; \
         delete from attributes where method = 'human'; \
         delete from hidden_attributes;",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn entity(conn: &mut PgConnection, class_iri: &str, key: &str) -> Result<Option<i64>> {
    let id = sqlx::query_scalar(
        "select coalesce(e.canonical_id, e.id) from entities e \
         left join entity_classes o on o.entity_id = e.id and o.status in ('auto', 'confirmed') \
         where e.key = $2 and coalesce(o.class_iri, e.class_iri) = $1 \
         order by (e.canonical_id is null or e.canonical_id = e.id) desc, e.id limit 1",
    )
    .bind(class_iri)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

pub async fn current_class(conn: &mut PgConnection, entity_id: i64) -> Result<String> {
    let class = sqlx::query_scalar(
        "select coalesce(o.class_iri, e.class_iri) from entities e \
         left join entity_classes o on o.entity_id = e.id and o.status in ('auto', 'confirmed') \
         where e.id = $1",
    )
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(class)
}

pub async fn document(conn: &mut PgConnection, source: &str, path: &str) -> Result<Option<i64>> {
    let id = sqlx::query_scalar(
        "select d.id from documents d join sources s on s.id = d.source_id \
         where s.name = $1 and d.path = $2 and d.status = 'present'",
    )
    .bind(source)
    .bind(path)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

pub async fn chunk(
    conn: &mut PgConnection,
    document_id: i64,
    page: Option<i32>,
) -> Result<Option<i64>> {
    let id = sqlx::query_scalar(
        "select k.id from chunks k join documents d on d.content_id = k.content_id \
         where d.id = $1 order by ($2::int is not null and $2 between coalesce(k.page_from, 0) \
         and coalesce(k.page_to, k.page_from, 0)) desc, k.ord limit 1",
    )
    .bind(document_id)
    .bind(page)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id)
}

pub async fn set_status(conn: &mut PgConnection, ids: &[i64], status: &str) -> Result<u64> {
    sqlx::query(
        "insert into decision_statuses (relation_id, before) \
         select id, status from relations where id = any($1) on conflict do nothing",
    )
    .bind(ids)
    .execute(&mut *conn)
    .await?;
    let done = sqlx::query("update relations set status = $2 where id = any($1)")
        .bind(ids)
        .bind(status)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected())
}

pub async fn facts(
    conn: &mut PgConnection,
    src: i64,
    property: &str,
    dst: i64,
) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar(
        "select r.id from relations r join entities s on s.id = r.src_id \
         join entities d on d.id = r.dst_id where r.property_iri = $2 and r.method <> 'human' \
         and coalesce(s.canonical_id, s.id) = $1 and coalesce(d.canonical_id, d.id) = $3",
    )
    .bind(src)
    .bind(property)
    .bind(dst)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ids)
}

pub async fn links_between(conn: &mut PgConnection, a: i64, b: i64) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar(
        "select r.id from relations r join entities s on s.id = r.src_id \
         join entities d on d.id = r.dst_id where r.property_iri = $3 and r.method <> 'human' \
         and r.status in ('auto', 'proposed') and \
         ((coalesce(s.canonical_id, s.id) = $1 and coalesce(d.canonical_id, d.id) = $2) or \
         (coalesce(s.canonical_id, s.id) = $2 and coalesce(d.canonical_id, d.id) = $1))",
    )
    .bind(a)
    .bind(b)
    .bind(SAME_AS)
    .fetch_all(&mut *conn)
    .await?;
    Ok(ids)
}

pub struct HumanRelation<'a> {
    pub src: i64,
    pub property: &'a str,
    pub dst: i64,
    pub status: &'a str,
    pub document_id: Option<i64>,
    pub page: Option<i32>,
}

pub async fn insert_relation(conn: &mut PgConnection, relation: &HumanRelation<'_>) -> Result<()> {
    let props = match relation.page {
        Some(page) => serde_json::json!({ "page": page }),
        None => serde_json::json!({}),
    };
    sqlx::query(
        "insert into relations (src_id, property_iri, dst_id, props, method, status, confidence, \
         document_id) values ($1, $2, $3, $4, 'human', $5, 1.0, $6)",
    )
    .bind(relation.src)
    .bind(relation.property)
    .bind(relation.dst)
    .bind(sqlx::types::Json(props))
    .bind(relation.status)
    .bind(relation.document_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn override_entity(
    conn: &mut PgConnection,
    entity_id: i64,
    class_iri: &str,
    name: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "insert into decision_overrides select entity_id, class_iri, name, method, status, \
         confidence, ontology_version_id from entity_classes where entity_id = $1 \
         on conflict do nothing",
    )
    .bind(entity_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into entity_classes (entity_id, class_iri, name, method, status, confidence, \
         ontology_version_id) values ($1, $2, $3, 'human', 'confirmed', 1.0, ({})) \
         on conflict (entity_id) do update set class_iri = excluded.class_iri, \
         name = excluded.name, method = excluded.method, status = excluded.status, \
         confidence = excluded.confidence, ontology_version_id = excluded.ontology_version_id, \
         created_at = now()",
        super::ontology::CURRENT
    )))
    .bind(entity_id)
    .bind(class_iri)
    .bind(name)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn add_alias(conn: &mut PgConnection, entity_id: i64, alias: &str) -> Result<()> {
    sqlx::query(
        "insert into aliases (entity_id, alias, method) values ($1, $2, 'human') \
         on conflict do nothing",
    )
    .bind(entity_id)
    .bind(alias)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn hide_values(
    conn: &mut PgConnection,
    entity_id: i64,
    property: &str,
    value_text: &str,
) -> Result<u64> {
    let done = sqlx::query(
        "insert into hidden_attributes (attribute_id) select a.id from attributes a \
         join entities e on e.id = a.entity_id where coalesce(e.canonical_id, e.id) = $1 \
         and a.property_iri = $2 and a.value_text = $3 and a.method <> 'human' \
         on conflict do nothing",
    )
    .bind(entity_id)
    .bind(property)
    .bind(value_text)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

pub struct HumanValue<'a> {
    pub entity_id: i64,
    pub property: &'a str,
    pub value_text: &'a str,
    pub unit_text: Option<&'a str>,
    pub unit_iri: Option<&'a str>,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub si_low: Option<f64>,
    pub si_high: Option<f64>,
    pub dimension: Option<&'a str>,
    pub chunk_id: i64,
}

pub async fn insert_value(conn: &mut PgConnection, value: &HumanValue<'_>) -> Result<()> {
    sqlx::query(
        "insert into attributes (entity_id, property_iri, value_text, unit_text, unit_iri, low, \
         high, si_low, si_high, dimension, chunk_id, span_start, span_end, method, confidence) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 0, 0, 'human', 1.0)",
    )
    .bind(value.entity_id)
    .bind(value.property)
    .bind(value.value_text)
    .bind(value.unit_text)
    .bind(value.unit_iri)
    .bind(value.low)
    .bind(value.high)
    .bind(value.si_low)
    .bind(value.si_high)
    .bind(value.dimension)
    .bind(value.chunk_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_names_are_checked_and_quoted() {
        assert_eq!(
            view_name("app_api.decisions").unwrap(),
            "\"app_api\".\"decisions\""
        );
        assert!(view_name("decisions").is_err());
        assert!(view_name("a.b; drop table x").is_err());
    }
}
