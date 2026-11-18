pub mod common;

use common::{Env, folder_source, repo};
use zontologize::decisions::{Outcome, Report};

const ACME: &str = "https://example.org/acme#";
const PROJECT_DOC: &str =
    "Projects/2023/P-2023-0005 Northwind Pumps B.V. - Eastport/03 Drawings/DWG-310-A2 rev A.txt";

async fn setup(name: &str) -> Env {
    let env = Env::new(name).await;
    env.copy_acme_rules();
    env.write_config(&folder_source(
        "projects",
        &repo("tests/fixtures/acme/projects"),
        "current",
    ));
    let path = env.config().join("zontologize.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let view = format!("decisions = \"{}_app.decisions\"\n", env.schema);
    std::fs::write(
        &path,
        text.replacen("[extract]\n", &format!("{view}[extract]\n"), 1),
    )
    .unwrap();
    env.scan().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "create schema {0}_app; create table {0}_app.decisions (id bigint, kind text, \
         class_iri text, key text, other_class_iri text, other_key text, property_iri text, \
         verdict text, value text, old_value text, source text, path text, page int)",
        env.schema
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    env
}

struct Row<'a> {
    kind: &'a str,
    class: &'a str,
    key: &'a str,
    other: Option<(&'a str, &'a str)>,
    property: Option<&'a str>,
    verdict: Option<&'a str>,
    value: Option<&'a str>,
    document: bool,
}

fn row<'a>(kind: &'a str, class: &'a str, key: &'a str) -> Row<'a> {
    Row {
        kind,
        class,
        key,
        other: None,
        property: None,
        verdict: None,
        value: None,
        document: false,
    }
}

async fn decide(env: &Env, id: i64, row: Row<'_>) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into {}_app.decisions values ($1, $2, $3, $4, $5, $6, $7, $8, $9, null, $10, \
         $11, 1)",
        env.schema
    )))
    .bind(id)
    .bind(row.kind)
    .bind(format!("{ACME}{}", row.class))
    .bind(row.key)
    .bind(row.other.map(|o| format!("{ACME}{}", o.0)))
    .bind(row.other.map(|o| o.1))
    .bind(row.property.map(|p| format!("{ACME}{p}")))
    .bind(row.verdict)
    .bind(row.value)
    .bind(row.document.then_some("projects"))
    .bind(row.document.then_some(PROJECT_DOC))
    .execute(&env.pool)
    .await
    .unwrap();
}

async fn apply(env: &Env) -> Report {
    zontologize::decisions::run(Some(&env.config()), Some(&common::test_url()))
        .await
        .unwrap()
}

async fn canonical(env: &Env, key: &str) -> i64 {
    sqlx::query_scalar("select coalesce(canonical_id, id) from entities where key = $1")
        .bind(key)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

async fn name_of(env: &Env, key: &str) -> String {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select name from {}_api.canonical_entities where key = $1",
        env.schema
    )))
    .bind(key)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

async fn visible(env: &Env, sql: &str) -> i64 {
    env.count(&sql.replace("{api}", &format!("{}_api", env.schema)))
        .await
}

const AT_HAVENSTAD: &str = "select count(*) from {api}.relations r join entities s on s.id = r.src_id \
     join entities d on d.id = r.dst_id where s.key = 'P-2022-0107' and d.key = 'havenstad'";
const HAND_FACT: &str = "select count(*) from {api}.relations where method = 'human' \
     and document_id is not null";
const SIX_BAR: &str = "select count(*) from {api}.attributes where method = 'human' \
     and si_low = 600000";

#[tokio::test]
#[ignore]
async fn decisions_apply_undo_and_keep_command_line_reviews() {
    let env = setup("decisions").await;
    sqlx::raw_sql(
        "insert into relations (src_id, property_iri, dst_id, method, status, confidence) \
         select a.id, 'http://www.w3.org/2002/07/owl#sameAs', b.id, 'resolve:fuzzy', \
         'confirmed', 0.9 from entities a, entities b \
         where a.key = 'havenstad' and b.key = 'havenstadt'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    decide(
        &env,
        1,
        Row {
            other: Some(("Customer", "zuidwal technick")),
            verdict: Some("right"),
            ..row("same", "Customer", "zuidwal techniek")
        },
    )
    .await;
    decide(
        &env,
        2,
        Row {
            value: Some("Northwind Pumps Group"),
            ..row("name", "Customer", "northwind pumps")
        },
    )
    .await;
    decide(
        &env,
        3,
        Row {
            value: Some("Oosthaven"),
            ..row("alias", "Site", "eastport")
        },
    )
    .await;
    let site = format!("{ACME}Site");
    decide(
        &env,
        4,
        Row {
            value: Some(&site),
            ..row("class", "Project", "P-2024-0001")
        },
    )
    .await;
    decide(
        &env,
        5,
        Row {
            other: Some(("Site", "havenstad")),
            property: Some("atSite"),
            verdict: Some("wrong"),
            ..row("fact", "Project", "P-2022-0107")
        },
    )
    .await;
    decide(
        &env,
        6,
        Row {
            other: Some(("Site", "havenstad")),
            property: Some("atSite"),
            document: true,
            ..row("add_fact", "Project", "P-2023-0005")
        },
    )
    .await;
    decide(
        &env,
        7,
        Row {
            property: Some("dischargePressure"),
            value: Some("6 bar"),
            document: true,
            ..row("value", "Project", "P-2023-0005")
        },
    )
    .await;
    decide(
        &env,
        8,
        Row {
            property: Some("dischargePressure"),
            value: Some("6 bananen"),
            document: true,
            ..row("value", "Project", "P-2023-0005")
        },
    )
    .await;
    decide(
        &env,
        9,
        Row {
            value: Some("Nobody"),
            ..row("name", "Customer", "does not exist")
        },
    )
    .await;

    let report = apply(&env).await;
    let outcome = |id: i64| {
        report
            .decisions
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.outcome)
    };
    assert_eq!(report.applied, 7, "{report:?}");
    assert_eq!(outcome(8), Some(Outcome::Unit));
    assert_eq!(outcome(9), Some(Outcome::NotFound));
    assert_eq!(
        canonical(&env, "zuidwal techniek").await,
        canonical(&env, "zuidwal technick").await
    );
    assert_eq!(
        name_of(&env, "northwind pumps").await,
        "Northwind Pumps Group"
    );
    assert_eq!(visible(&env, AT_HAVENSTAD).await, 0);
    assert_eq!(visible(&env, HAND_FACT).await, 1);
    assert_eq!(visible(&env, SIX_BAR).await, 1);
    let moved = visible(
        &env,
        "select count(*) from {api}.canonical_entities where key = 'P-2024-0001' \
         and class_iri = 'https://example.org/acme#Site'",
    )
    .await;
    assert_eq!(moved, 1);
    let aliases = env
        .count("select count(*) from aliases where method = 'human' and alias = 'Oosthaven'")
        .await;
    assert_eq!(aliases, 1);

    let again = apply(&env).await;
    assert_eq!(again, report);
    assert_eq!(visible(&env, HAND_FACT).await, 1);
    assert_eq!(visible(&env, SIX_BAR).await, 1);

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "delete from {}_app.decisions",
        env.schema
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let empty = apply(&env).await;
    assert_eq!(empty.applied, 0);
    assert_ne!(
        canonical(&env, "zuidwal techniek").await,
        canonical(&env, "zuidwal technick").await
    );
    assert_eq!(name_of(&env, "northwind pumps").await, "Northwind Pumps");
    assert_eq!(visible(&env, AT_HAVENSTAD).await, 1);
    assert_eq!(visible(&env, HAND_FACT).await, 0);
    let human = env
        .count(
            "select (select count(*) from relations where method = 'human') + \
             (select count(*) from aliases where method = 'human') + \
             (select count(*) from entity_classes where method = 'human') + \
             (select count(*) from attributes where method = 'human') + \
             (select count(*) from decision_statuses) + (select count(*) from hidden_attributes)",
        )
        .await;
    assert_eq!(human, 0);
    let kept = env
        .count(
            "select count(*) from relations where method = 'resolve:fuzzy' \
             and status = 'confirmed'",
        )
        .await;
    assert_eq!(kept, 1);
    assert_eq!(
        canonical(&env, "havenstad").await,
        canonical(&env, "havenstadt").await
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn undoing_a_class_decision_restores_the_class_under_a_later_decision() {
    let env = setup("decisions_undo_class").await;
    let site = format!("{ACME}Site");
    decide(
        &env,
        1,
        Row {
            value: Some(&site),
            ..row("class", "Project", "P-2024-0001")
        },
    )
    .await;
    decide(
        &env,
        2,
        Row {
            value: Some("Harbour site"),
            ..row("name", "Site", "P-2024-0001")
        },
    )
    .await;
    assert_eq!(apply(&env).await.applied, 2);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "delete from {}_app.decisions where id = 1",
        env.schema
    )))
    .execute(&env.pool)
    .await
    .unwrap();
    let report = apply(&env).await;
    let outcome = report
        .decisions
        .iter()
        .find(|d| d.id == 2)
        .map(|d| d.outcome);
    assert_eq!(outcome, Some(Outcome::NotFound), "{report:?}");
    let as_project = visible(
        &env,
        "select count(*) from {api}.canonical_entities where key = 'P-2024-0001' \
         and class_iri = 'https://example.org/acme#Project'",
    )
    .await;
    assert_eq!(as_project, 1);
    assert_eq!(
        env.count("select count(*) from decision_overrides").await,
        0
    );
    env.finish().await;
}
