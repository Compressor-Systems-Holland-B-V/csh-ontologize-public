use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};

use crate::config::{self, Config};
use crate::llm::job::{Options as JobOptions, Outcome};

pub const STAGES: [&str; 11] = [
    "scan",
    "classify",
    "extract",
    "attributes",
    "embed",
    "decisions",
    "resolve",
    "curate",
    "resolve again",
    "graph",
    "eval",
];

pub struct Options {
    pub max_usd: Option<f64>,
    pub claude_program: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StageResult {
    pub stage: String,
    pub status: String,
    pub seconds: f64,
    pub calls: i64,
    pub usd: f64,
    pub billed_usd: f64,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Numbers {
    pub found: Option<f64>,
    pub duplicates: Option<f64>,
    pub wrong_merges: Option<f64>,
    pub class_accuracy: Option<f64>,
    pub mentions: Option<f64>,
    pub relations: Option<f64>,
    pub attributes: Option<f64>,
    pub catch_rate: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub started_at: String,
    pub finished: bool,
    pub stages: Vec<StageResult>,
    pub numbers: Numbers,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    Good,
    Fair,
    Poor,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Good => "good",
            Verdict::Fair => "fair",
            Verdict::Poor => "poor",
        }
    }
}

pub fn verdict(value: f64, target: f64, higher_is_better: bool) -> Verdict {
    let gap = if higher_is_better {
        target - value
    } else {
        value - target
    };
    if gap <= 0.0 {
        Verdict::Good
    } else if gap <= 0.1 {
        Verdict::Fair
    } else {
        Verdict::Poor
    }
}

pub fn remaining(max_usd: Option<f64>, billed: f64) -> Option<f64> {
    max_usd.map(|max| (max - billed).max(0.0))
}

async fn spend_since(
    pool: &PgPool,
    since: DateTime<Utc>,
    task: Option<&str>,
) -> Result<(i64, f64, f64)> {
    let row: (i64, f64, f64) = sqlx::query_as(
        "select count(*), coalesce(sum(cost_usd), 0)::float8, \
         coalesce(sum(cost_usd) filter (where not subscription), 0)::float8 \
         from llm_calls where created_at >= $1 and ($2::text is null or task = $2)",
    )
    .bind(since)
    .bind(task)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

fn task_of(stage: &str) -> Option<&'static str> {
    match stage {
        "classify" => Some("classify"),
        "extract" => Some("extract"),
        "attributes" => Some("attributes"),
        "curate" => Some("curate"),
        _ => None,
    }
}

fn job_options(options: &Options, billed: f64) -> JobOptions {
    let mut job = JobOptions::new(options.claude_program.clone());
    job.max_usd = remaining(options.max_usd, billed);
    job.yes = true;
    job
}

async fn stage(
    name: &str,
    config_dir: Option<&Path>,
    config: &Config,
    options: &Options,
    billed: f64,
    notes: &mut Vec<String>,
) -> Result<Option<Outcome>> {
    let url = crate::database_url();
    let url = url.as_deref();
    let job = job_options(options, billed);
    Ok(match name {
        "scan" => {
            let program = std::env::current_exe().context("cannot find the zontologize binary")?;
            crate::scan::run(config_dir, &[], url, &crate::scan::Options::new(program)).await?;
            let pool = crate::db::pool::connect(url, &config.database.schema, 2).await?;
            let reasons = crate::db::documents::skip_reasons(&pool).await?;
            let textless = crate::db::documents::without_text(&pool).await?;
            notes.push(skip_note(&reasons, textless));
            None
        }
        "classify" => Some(crate::classify::run(config_dir, url, &job).await?),
        "extract" => Some(crate::facts::run::run(config_dir, url, &job).await?),
        "attributes" => Some(crate::attributes::run::run(config_dir, url, &job).await?),
        "embed" => {
            let embed = crate::embed::run::Options {
                model: None,
                minutes: None,
                no_index: false,
                prune: false,
            };
            let stats = crate::embed::run::run(config_dir, url, &embed).await?;
            notes.push(format!(
                "{} chunks left out as unreadable, {} truncated",
                stats.junk, stats.chunks.truncated
            ));
            None
        }
        "decisions" => {
            if config.database.decisions.is_empty() {
                notes.push("no decisions table configured".to_string());
            } else {
                let report = crate::decisions::run(config_dir, url).await?;
                notes.push(serde_json::to_string(&report)?);
            }
            None
        }
        "resolve" | "resolve again" => {
            let stats = crate::resolve::run(config_dir, url).await?;
            notes.push(format!(
                "{} links added, {} removed, {} kept; {} clusters",
                stats.added.values().sum::<u64>(),
                stats.removed,
                stats.kept,
                stats.clusters
            ));
            None
        }
        "curate" => Some(crate::curate::run::run(config_dir, url, &job).await?),
        "graph" => {
            let stats = crate::graph::sync(config_dir, url).await?;
            notes.push(format!(
                "{} vertices, {} edges",
                stats.vertices, stats.edges
            ));
            None
        }
        _ => None,
    })
}

async fn judged(config: &Config, url: Option<&str>, numbers: &mut Numbers) -> Result<()> {
    let pool = crate::db::pool::connect(url, &config.database.schema, 1).await?;
    let rows = crate::db::review::judged(&pool).await;
    pool.close().await;
    let scores = crate::eval::judge::scores(&rows?);
    let of = |kind: &str| {
        scores
            .iter()
            .find(|s| s.kind == kind && s.total > 0)
            .map(|s| s.precision())
    };
    numbers.mentions = of("mention");
    numbers.relations = of("relation");
    numbers.attributes = of("attribute");
    numbers.catch_rate = crate::eval::judge::catch_rate(&scores);
    Ok(())
}

async fn numbers(
    config_dir: Option<&Path>,
    config: &Config,
    options: &Options,
    notes: &mut Vec<String>,
) -> Numbers {
    let mut numbers = Numbers::default();
    if config.run.set.is_empty() {
        notes.push("no gold set in [run] set".to_string());
        return numbers;
    }
    let url = crate::database_url();
    let judge = crate::eval::judge::JudgeOptions {
        method: String::new(),
        seed: None,
        job: job_options(options, 0.0),
    };
    let judged = match crate::eval::judge::run(config_dir, url.as_deref(), &judge).await {
        Ok(_) => judged(config, url.as_deref(), &mut numbers).await,
        Err(error) => Err(error),
    };
    if let Err(error) = judged {
        notes.push(format!("judge failed: {error:#}"));
    }
    match crate::eval::run(config_dir, url.as_deref(), Some(&config.run.set), None).await {
        Ok(scores) => {
            // The last row is the total; summing every row would count it twice.
            let total = scores.last().cloned().unwrap_or_default();
            let share = |a: usize, b: usize| (b > 0).then(|| a as f64 / b as f64);
            Numbers {
                found: share(total.found, total.world),
                duplicates: share(total.duplicates, total.found),
                wrong_merges: Some(total.wrong_merges as f64),
                class_accuracy: share(total.right_class, total.matched),
                ..numbers
            }
        }
        Err(error) => {
            notes.push(format!("eval failed: {error:#}"));
            numbers
        }
    }
}

pub fn skip_note(reasons: &[(String, i64)], textless: i64) -> String {
    let skipped: Vec<String> = reasons
        .iter()
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect();
    let skipped = if skipped.is_empty() {
        "none".to_string()
    } else {
        skipped.join(", ")
    };
    format!("skipped: {skipped}; read without text: {textless}")
}

fn percent(value: Option<f64>) -> String {
    value.map_or("-".to_string(), |v| format!("{:.1}%", v * 100.0))
}

fn change(now: Option<f64>, before: Option<f64>) -> String {
    match (now, before) {
        (Some(now), Some(before)) => format!("{:+.1}", (now - before) * 100.0),
        _ => "-".to_string(),
    }
}

pub fn render(
    summary: &Summary,
    previous: Option<&Summary>,
    targets: &config::RunTargets,
) -> String {
    let mut out = format!(
        "# Run summary\n\nStarted {}; {}.\n\n## Stages\n\n| Stage | Status | Took | Calls | USD at API prices | USD billed | Notes |\n|---|---|---|---|---|---|---|\n",
        summary.started_at,
        if summary.finished {
            "finished"
        } else {
            "stopped; run `zontologize run` again to go on"
        }
    );
    for s in &summary.stages {
        out.push_str(&format!(
            "| {} | {} | {:.0} min | {} | {:.2} | {:.2} | {} |\n",
            s.stage,
            s.status,
            s.seconds / 60.0,
            s.calls,
            s.usd,
            s.billed_usd,
            s.notes.join("; ").replace('|', "/")
        ));
    }
    let before = previous.map(|p| &p.numbers);
    let n = &summary.numbers;
    out.push_str("\n## Quality\n\n| Layer | Value | Target | Verdict | Change (points) |\n|---|---|---|---|---|\n");
    let rows = [
        (
            "Mentions right (judge)",
            n.mentions,
            before.and_then(|b| b.mentions),
            targets.mentions,
            true,
        ),
        (
            "Relations right (judge)",
            n.relations,
            before.and_then(|b| b.relations),
            targets.relations,
            true,
        ),
        (
            "Attributes right (judge)",
            n.attributes,
            before.and_then(|b| b.attributes),
            targets.attributes,
            true,
        ),
        (
            "Planted errors caught",
            n.catch_rate,
            before.and_then(|b| b.catch_rate),
            targets.catch_rate,
            true,
        ),
        (
            "Entities found",
            n.found,
            before.and_then(|b| b.found),
            targets.found,
            true,
        ),
        (
            "Duplicate rate",
            n.duplicates,
            before.and_then(|b| b.duplicates),
            targets.duplicates,
            false,
        ),
        (
            "Class accuracy",
            n.class_accuracy,
            before.and_then(|b| b.class_accuracy),
            targets.class_accuracy,
            true,
        ),
    ];
    for (label, value, old, target, higher) in rows {
        out.push_str(&format!(
            "| {label} | {} | {} | {} | {} |\n",
            percent(value),
            percent(Some(target)),
            value.map_or("-", |v| verdict(v, target, higher).as_str()),
            change(value, old)
        ));
    }
    if let Some(wrong) = n.wrong_merges {
        out.push_str(&format!(
            "| Wrong merges | {wrong:.0} | 0 | {} | - |\n",
            if wrong == 0.0 { "good" } else { "poor" }
        ));
    }
    out
}

pub async fn run(config_dir: Option<&Path>, options: &Options) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let url = crate::database_url();
    let pool = crate::db::pool::connect(url.as_deref(), &config.database.schema, 2).await?;
    let started = Utc::now();
    let path = PathBuf::from(&config.run.summary);
    let json = path.with_extension("json");
    let previous: Option<Summary> = std::fs::read_to_string(&json)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let mut summary = Summary {
        started_at: started.format("%Y-%m-%d %H:%M UTC").to_string(),
        ..Summary::default()
    };
    let mut code = ExitCode::SUCCESS;
    for name in STAGES {
        println!("run: {name}");
        let clock = Instant::now();
        let since = Utc::now();
        let (_, _, billed) = spend_since(&pool, started, None).await?;
        let mut notes = Vec::new();
        let outcome = if name == "eval" {
            summary.numbers = numbers(config_dir, &config, options, &mut notes).await;
            Ok(None)
        } else {
            stage(name, config_dir, &config, options, billed, &mut notes).await
        };
        let (calls, usd, billed_usd) = spend_since(&pool, since, task_of(name)).await?;
        let mut result = StageResult {
            stage: name.to_string(),
            status: "finished".to_string(),
            seconds: clock.elapsed().as_secs_f64(),
            calls,
            usd,
            billed_usd,
            notes,
        };
        let stop = match outcome {
            Ok(Some(Outcome::Paused)) => {
                result.status = "paused (cap or usage window)".to_string();
                code = ExitCode::from(crate::PAUSED_EXIT_CODE);
                true
            }
            Ok(_) => false,
            Err(error) => {
                result.status = format!("failed: {error:#}");
                code = ExitCode::FAILURE;
                true
            }
        };
        summary.stages.push(result);
        if stop {
            break;
        }
    }
    summary.finished = summary.stages.len() == STAGES.len()
        && summary.stages.iter().all(|s| s.status == "finished");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(
        &path,
        render(&summary, previous.as_ref(), &config.run.targets),
    )
    .with_context(|| format!("cannot write {}", path.display()))?;
    if summary.finished {
        std::fs::write(&json, serde_json::to_string_pretty(&summary)?)
            .with_context(|| format!("cannot write {}", json.display()))?;
    }
    println!("run: summary in {}", path.display());
    pool.close().await;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_note_lists_reasons_and_textless_files() {
        let reasons = vec![
            ("extension .zip".to_string(), 12),
            ("binary content".to_string(), 1),
        ];
        assert_eq!(
            skip_note(&reasons, 3),
            "skipped: 12 extension .zip, 1 binary content; read without text: 3"
        );
        assert_eq!(skip_note(&[], 0), "skipped: none; read without text: 0");
    }

    #[test]
    fn verdicts_follow_the_target_and_direction() {
        assert_eq!(verdict(0.95, 0.9, true), Verdict::Good);
        assert_eq!(verdict(0.85, 0.9, true), Verdict::Fair);
        assert_eq!(verdict(0.7, 0.9, true), Verdict::Poor);
        assert_eq!(verdict(0.04, 0.05, false), Verdict::Good);
        assert_eq!(verdict(0.1, 0.05, false), Verdict::Fair);
        assert_eq!(verdict(0.3, 0.05, false), Verdict::Poor);
    }

    #[test]
    fn the_overall_cap_shrinks_with_what_was_billed() {
        assert_eq!(remaining(None, 5.0), None);
        assert_eq!(remaining(Some(10.0), 4.0), Some(6.0));
        assert_eq!(remaining(Some(10.0), 12.0), Some(0.0));
    }

    #[test]
    fn the_summary_names_stages_numbers_and_changes() {
        let targets = config::RunTargets::default();
        let mut summary = Summary {
            started_at: "2030-01-01 00:00 UTC".to_string(),
            finished: true,
            stages: vec![StageResult {
                stage: "extract".to_string(),
                status: "finished".to_string(),
                seconds: 120.0,
                calls: 3,
                usd: 1.5,
                notes: vec!["a | b".to_string()],
                ..StageResult::default()
            }],
            numbers: Numbers {
                found: Some(0.8),
                duplicates: Some(0.02),
                wrong_merges: Some(0.0),
                class_accuracy: None,
                mentions: Some(0.93),
                ..Numbers::default()
            },
        };
        let before = summary.clone();
        summary.numbers.found = Some(0.92);
        let text = render(&summary, Some(&before), &targets);
        assert!(
            text.contains("| extract | finished | 2 min | 3 | 1.50 | 0.00 | a / b |"),
            "{text}"
        );
        assert!(
            text.contains("| Entities found | 92.0% | 90.0% | good | +12.0 |"),
            "{text}"
        );
        assert!(
            text.contains("| Duplicate rate | 2.0% | 5.0% | good | +0.0 |"),
            "{text}"
        );
        assert!(
            text.contains("| Class accuracy | - | 90.0% | - | - |"),
            "{text}"
        );
        assert!(
            text.contains("| Wrong merges | 0 | 0 | good | - |"),
            "{text}"
        );
        assert!(
            text.contains("| Mentions right (judge) | 93.0% | 90.0% | good | +0.0 |"),
            "{text}"
        );
        assert!(
            text.contains("| Planted errors caught | - | 90.0% | - | - |"),
            "{text}"
        );
    }
}
