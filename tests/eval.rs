pub mod common;

use common::{Env, folder_source, repo};
use zontologize::eval::draft::{DraftOptions, What};
use zontologize::llm::job::{Options, Outcome};

#[tokio::test]
#[ignore]
async fn eval_entities_counts_duplicates_merges_and_classes() {
    let env = Env::new("eval").await;
    env.write_config("");
    std::fs::write(
        env.config().join("world.csv"),
        "name,variant,class\n\
         Acme BV,Acme BV,acme:Customer\n\
         Acme BV,Acme B.V.,acme:Customer\n\
         Brix Ltd,Brix Ltd,acme:Customer\n\
         Eastport,Eastport,acme:Site\n\
         Westhaven,Westhaven,acme:Site\n",
    )
    .unwrap();
    sqlx::raw_sql(
        "insert into ontology_versions (sha256, files) values ('x', '[]');
         insert into entities (class_iri, key, name, method, confidence, ontology_version_id)
         values ('https://example.org/acme#Customer', 'acme bv', 'Acme BV', 'rules', 1, 1),
                ('https://example.org/acme#Customer', 'acme', 'Acme', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'brix', 'Brix', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'eastport', 'Eastport', 'rules', 1, 1),
                ('https://example.org/acme#Site', 'westhaven', 'Westhaven', 'rules', 1, 1);
         insert into aliases (entity_id, alias, method) values (2, 'Acme B.V.', 'gazetteer'),
                (3, 'Brix Ltd', 'gazetteer');
         update entities set canonical_id = 4 where id in (4, 5);",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let world = env.config().join("world.csv");
    let scores = zontologize::eval::run(
        Some(&env.config()),
        Some(&common::test_url()),
        None,
        Some(&world),
    )
    .await
    .unwrap();
    let table = zontologize::eval::table(&scores);
    let customer = &scores[0];
    assert_eq!(customer.class, "acme:Customer", "{table}");
    assert_eq!(
        (customer.world, customer.found, customer.duplicates),
        (2, 2, 1),
        "{table}"
    );
    assert_eq!((customer.matched, customer.right_class), (3, 2), "{table}");
    let site = &scores[1];
    assert_eq!(
        (site.world, site.found, site.wrong_merges),
        (2, 2, 1),
        "{table}"
    );
    let total = &scores[2];
    assert_eq!((total.world, total.wrong_merges), (4, 1), "{table}");
    assert!(table.contains("acme:Customer"), "{table}");
    env.finish().await;
}

fn options(program: &str) -> Options {
    let mut options = Options::new(repo(&format!("tests/fixtures/llm/claude-code/{program}")));
    options.backoff = Some(std::time::Duration::from_millis(1));
    options.no_wait = true;
    options
}

async fn gold_env(name: &str) -> Env {
    let env = Env::new(name).await;
    env.file(
        "Alpha/mail one.txt",
        "From the yard: Brix Ltd confirms delivery of pump P-101 to the Eastport site.",
    );
    env.file(
        "Alpha/order two.txt",
        "Purchase order 7 to BRIX LTD for pump P-101. Delivery to Eastport in May.",
    );
    env.file(
        "Beta/datasheet.txt",
        "Motor M-1: rated power 55 kW at 1480 rpm. Frame size and weight follow later.",
    );
    env.file(
        "Gamma/note.txt",
        "From the yard: Brix Ltd confirms delivery of pump P-101 to the Eastport site.",
    );
    env.write_config(&format!(
        "[models]\neval = \"claude-code:opus\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [eval]\ndir = \"{}\"\nsummary = \"{}\"\nmin_doc_chars = 10\ndocs_per_kind = 1\n\
         questions = 2\njudge_mentions = 3\njudge_relations = 3\njudge_attributes = 3\n\
         planted = 2\n{}",
        env.dir.join("gold").display(),
        env.dir.join("summary.md").display(),
        folder_source("docs", &env.data(), "current")
    ));
    env.scan().await;
    env
}

async fn draft(env: &Env, set: &str, what: What, folder: &str) -> Outcome {
    let options = DraftOptions {
        set: set.to_string(),
        what,
        folders: vec![folder.to_string()],
        job: options("fake-claude-eval.sh"),
    };
    zontologize::eval::draft::run(Some(&env.config()), Some(&common::test_url()), &options)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn eval_draft_world_is_grounded_agreed_judged_and_frozen() {
    let env = gold_env("eval_draft_world").await;
    assert_eq!(
        draft(&env, "s1", What::World, "Alpha").await,
        Outcome::Finished
    );
    let dir = env.dir.join("gold/s1/v1");
    let world = std::fs::read_to_string(dir.join("world.csv")).unwrap();
    let lower = world.to_lowercase();
    assert!(
        lower.contains("brix ltd,brix ltd,acme:organisation"),
        "{world}"
    );
    assert!(lower.contains("eastport,eastport,acme:site"), "{world}");
    assert!(lower.contains("p-101,p-101,acme:equipment"), "{world}");
    assert!(!world.contains("Ghost"), "{world}");
    assert!(!world.contains("yard"), "{world}");
    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(report.contains("## world"), "{report}");
    assert!(report.contains("agreement: 2 of 4 entities"), "{report}");
    assert!(report.contains("(50.0%)"), "{report}");
    assert!(report.contains("1 quote not in the file"), "{report}");
    assert!(
        report.contains("2 confirmed") || report.contains("1 confirmed"),
        "{report}"
    );
    assert!(dir.join("documents.sha256").exists());

    let calls = env.count("select count(*) from llm_calls").await;
    assert!(calls >= 5, "{calls}");
    assert_eq!(
        draft(&env, "s1", What::World, "Alpha").await,
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, calls);
    assert!(!env.dir.join("gold/s1/v2").exists());

    let scores = zontologize::eval::run(
        Some(&env.config()),
        Some(&common::test_url()),
        Some("s1"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(scores.last().unwrap().world, 3);
    let summary = std::fs::read_to_string(env.dir.join("summary.md")).unwrap();
    assert!(summary.contains("## Gold list"), "{summary}");
    assert!(summary.contains("found %"), "{summary}");
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn eval_draft_questions_keep_only_answered_ones() {
    let env = gold_env("eval_draft_questions").await;
    assert_eq!(
        draft(&env, "q1", What::Questions, "Gamma").await,
        Outcome::Finished
    );
    let path = env.dir.join("gold/q1/v1/questions.toml");
    let questions = zontologize::embed::eval::read_questions_file(&path).unwrap();
    assert_eq!(questions.len(), 1, "{questions:?}");
    assert_eq!(questions[0].expect, vec!["Gamma/note.txt".to_string()]);
    let report = std::fs::read_to_string(env.dir.join("gold/q1/v1/report.md")).unwrap();
    assert!(report.contains("dropped by grounding: 1"), "{report}");
    assert!(report.contains("expected files alone: 1"), "{report}");
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn eval_draft_attributes_label_chunks_twice_and_judge_the_rest() {
    let env = gold_env("eval_draft_attributes").await;
    assert_eq!(
        draft(&env, "a1", What::Attributes, "Beta").await,
        Outcome::Finished
    );
    let dir = env.dir.join("gold/a1/v1");
    let values = std::fs::read_to_string(dir.join("attributes.csv")).unwrap();
    assert!(
        values.contains("Beta/datasheet.txt,1,0,M-1,rated power,55,kW,rated power 55 kW"),
        "{values}"
    );
    assert!(values.contains(",speed,1480,rpm,"), "{values}");
    let chunks = std::fs::read_to_string(dir.join("chunks.csv")).unwrap();
    assert_eq!(chunks.lines().count(), 2, "{chunks}");
    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(
        report.contains("1 values in both passes (50.0%)"),
        "{report}"
    );
    env.finish().await;
}

async fn judge_env(name: &str) -> Env {
    let env = Env::new(name).await;
    env.copy_acme_rules();
    env.write_config(&format!(
        "[models]\neval = \"claude-code:opus\"\n\
         [providers.claude-code]\nconcurrency = 1\n\
         [eval]\ndir = \"{}\"\nsummary = \"{}\"\n\
         judge_mentions = 3\njudge_relations = 3\njudge_attributes = 3\nplanted = 2\n{}",
        env.dir.join("gold").display(),
        env.dir.join("summary.md").display(),
        folder_source("projects", &repo("tests/fixtures/acme/projects"), "current"),
    ));
    env.scan().await;
    env
}

async fn judge(env: &Env, program: &str) -> Outcome {
    let options = zontologize::eval::judge::JudgeOptions {
        method: String::new(),
        seed: None,
        job: options(program),
    };
    zontologize::eval::judge::run(Some(&env.config()), Some(&common::test_url()), &options)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn a_strict_judge_catches_every_planted_error() {
    let env = judge_env("eval_judge_strict").await;
    sqlx::query(
        "insert into fact_reviews (kind, fact_id, class_iri, method, verdict, snapshot) \
         select 'mention', id, 'x', 'rules', 'wrong', 'by hand' from mentions order by id limit 1",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        judge(&env, "fake-claude-eval-strict.sh").await,
        Outcome::Finished
    );
    let real = env
        .count("select count(*) from fact_reviews where reviewer = 'judge' and not planted")
        .await;
    let planted = env
        .count("select count(*) from fact_reviews where reviewer = 'judge' and planted")
        .await;
    assert!(real >= 3, "{real}");
    assert!(planted >= 2, "{planted}");
    assert_eq!(
        env.count(
            "select count(*) from fact_reviews where reviewer = 'judge' and verdict = 'right'"
        )
        .await,
        0
    );
    let report = std::fs::read_to_string(env.dir.join("gold/judge-report.md")).unwrap();
    assert!(report.contains("Planted errors caught: 100.0%"), "{report}");
    assert!(report.contains("Verdict: passed"), "{report}");
    assert!(report.contains("same verdict: 100.0%"), "{report}");
    let summary = std::fs::read_to_string(env.dir.join("summary.md")).unwrap();
    assert!(summary.contains("## Judged precision"), "{summary}");
    let precision = zontologize::db::review::precision(&env.pool).await.unwrap();
    let total: i64 = precision.iter().map(|p| p.total).sum();
    assert_eq!(total, real + 1);

    let calls = env.count("select count(*) from llm_calls").await;
    assert_eq!(
        judge(&env, "fake-claude-eval-strict.sh").await,
        Outcome::Finished
    );
    assert_eq!(env.count("select count(*) from llm_calls").await, calls);
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_lenient_judge_fails_its_report() {
    let env = judge_env("eval_judge_lenient").await;
    assert_eq!(judge(&env, "fake-claude-eval.sh").await, Outcome::Finished);
    let report = std::fs::read_to_string(env.dir.join("gold/judge-report.md")).unwrap();
    assert!(report.contains("Planted errors caught: 0.0%"), "{report}");
    assert!(report.contains("Verdict: failed"), "{report}");
    env.finish().await;
}
