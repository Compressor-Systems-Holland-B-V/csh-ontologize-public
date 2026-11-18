pub mod common;

use common::Env;

#[tokio::test]
#[ignore]
async fn stemmed_text_and_trigram_names_are_searchable() {
    let env = Env::new("search").await;
    let stemmed: bool = sqlx::query_scalar(
        "select (to_tsvector('dutch'::regconfig, 'twee lagers vervangen') \
         || to_tsvector('english'::regconfig, 'twee lagers vervangen')) \
         @@ (plainto_tsquery('dutch'::regconfig, 'lager') || plainto_tsquery('english'::regconfig, 'lager'))",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(stemmed);
    let column: String = sqlx::query_scalar(
        "select pg_get_expr(d.adbin, d.adrelid) from pg_attrdef d \
         join pg_attribute a on a.attrelid = d.adrelid and a.attnum = d.adnum \
         where d.adrelid = 'chunks'::regclass and a.attname = 'tsv_stem'",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(
        column.contains("dutch") && column.contains("english"),
        "{column}"
    );
    let close: f32 = sqlx::query_scalar("select similarity(lower('Nortwind'), lower('Northwind'))")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert!(close > 0.3, "{close}");
    let indexes: i64 = sqlx::query_scalar(
        "select count(*) from pg_indexes where schemaname = current_schema() \
         and indexname in ('entities_name_trgm', 'aliases_alias_trgm', 'chunks_tsv_stem')",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(indexes, 3);
    let sql = format!("select {}_api.contract_version()", env.schema);
    let contract: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(contract, 9);
}

const NAME_RULES: &str = r#"[doc_types]
drawing = "acme:Drawing"

[[path_rules]]
pattern = '^(?P<project>N-\d{5}) (?P<customer>[^;/]+?); (?P<title>[^/]+?)/'
props = { title = "{title}" }
[[path_rules.entities]]
id = "project"
class = "acme:Project"
key = "{project}"
name = "{project} {title}"
[[path_rules.entities]]
id = "customer"
class = "acme:Customer"
key = "{customer}"
name = "{customer}"
key_case = "lower"
[[path_rules.relations]]
from = "document"
property = "acme:describes"
to = "project"
[[path_rules.relations]]
from = "project"
property = "acme:forCustomer"
to = "customer"

[[path_rules]]
pattern = '/Drawings/'
doc_type = "drawing"
"#;

async fn name_matches(env: &Env, query: &str) -> i64 {
    env.count(&format!(
        "select count(*) from \"{}_api\".documents where tsv_name @@ to_tsquery('simple', '{query}')",
        env.schema
    ))
    .await
}

#[tokio::test]
#[ignore]
async fn a_document_is_found_by_its_name_project_customer_and_class() {
    let env = Env::new("names").await;
    env.write_rules(NAME_RULES);
    env.write_config(&common::folder_source("projects", &env.data(), "current"));
    env.file(
        "N-12345 Brightwater Pumps; Cooling skid/Drawings/C01-00001_pump-layout.txt",
        "A made-up layout of a cooling skid.",
    );
    env.scan().await;
    for query in [
        "c0100001",
        "c01 & 00001",
        "pump & layout",
        "n12345",
        "n & 12345",
        "cooling & skid",
        "brightwater & pumps",
        "drawing",
        "tekening",
        "drawings",
        "txt",
    ] {
        assert_eq!(name_matches(&env, query).await, 1, "{query}");
    }
    assert_eq!(name_matches(&env, "invoice").await, 0);
    sqlx::query(
        "insert into entity_classes (entity_id, class_iri, method, status, confidence, \
         ontology_version_id) select e.id, 'https://example.org/acme#Invoice', 'human', 'confirmed', 1, \
         e.ontology_version_id from entities e join documents d on e.document_id = d.id \
         and e.key = d.id::text",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    env.scan().await;
    assert_eq!(name_matches(&env, "factuur").await, 1);
    assert_eq!(name_matches(&env, "tekening").await, 0);
    let indexed = env
        .count(
            "select count(*) from pg_indexes where schemaname = current_schema() \
             and indexname = 'document_names_tsv'",
        )
        .await;
    assert_eq!(indexed, 1);
    env.finish().await;
}
