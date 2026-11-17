use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::draft::{Ctx, Prompts};
use super::gold;
use crate::config;
use crate::db;
use crate::db::review::{Fact, Judged};
use crate::llm::ModelName;
use crate::llm::batch::parse;
use crate::llm::job::{Options, Outcome};
use crate::ns::local_name;
use crate::ontology::{self, Ontology};
use crate::text::fold;

pub(crate) const JUDGE_VARIABLES: [&str; 1] = ["claims"];
const JUDGE_SCHEMA: &str = include_str!("../../prompts/eval-judge-schema.json");
const POOL_FACTOR: i64 = 20;
const REPORT_FILE: &str = "judge-report.md";
const Z: f64 = 1.96;
pub const KINDS: [&str; 3] = ["mention", "relation", "attribute"];

#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub text: String,
    pub evidence: String,
}

#[derive(Deserialize)]
pub(crate) struct Answer {
    verdicts: Vec<Verdict>,
}

#[derive(Deserialize)]
struct Verdict {
    id: String,
    right: bool,
}

pub(crate) fn prompts(config_dir: Option<&Path>) -> Result<Prompts> {
    Prompts::load(
        config_dir,
        "eval-judge",
        &JUDGE_VARIABLES,
        "claims",
        JUDGE_SCHEMA,
    )
}

fn block(claims: &[&Claim]) -> String {
    let mut out = String::new();
    for (i, claim) in claims.iter().enumerate() {
        out.push_str(&format!("- j{}: {}\n", i + 1, claim.text));
        for line in claim.evidence.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

fn verdicts(answer: Answer, count: usize) -> Vec<Option<bool>> {
    let mut out = vec![None; count];
    for verdict in answer.verdicts {
        if let Some(i) = verdict
            .id
            .trim()
            .strip_prefix('j')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=count).contains(n))
        {
            out[i - 1].get_or_insert(verdict.right);
        }
    }
    out
}

pub(crate) async fn judge(
    ctx: &Ctx<'_>,
    task: &'static str,
    prompts: &Prompts,
    claims: &[Claim],
) -> Result<Option<Vec<Option<bool>>>> {
    let size = ctx.config.eval.judge_batch;
    let parts: Vec<Vec<&Claim>> = claims
        .chunks(size)
        .map(|part| part.iter().collect())
        .collect();
    let requests: Vec<_> = parts
        .iter()
        .map(|part| prompts.request(task, &[("claims", &block(part))]))
        .collect();
    let Some(answers) = ctx.ask::<Answer>(task, &requests).await? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(claims.len());
    for (part, request) in parts.iter().zip(&requests) {
        match parse::<Answer>(&answers, &ctx.key(request)) {
            Some(answer) => out.extend(verdicts(answer, part.len())),
            None => out.extend(std::iter::repeat_n(None, part.len())),
        }
    }
    Ok(Some(out))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plant {
    SwapEnds,
    OtherClass,
    OtherNumber,
    OtherUnit,
    OtherName,
}

impl Plant {
    pub fn as_str(self) -> &'static str {
        match self {
            Plant::SwapEnds => "swapped ends",
            Plant::OtherClass => "another class",
            Plant::OtherNumber => "a changed number",
            Plant::OtherUnit => "a changed unit",
            Plant::OtherName => "a name from another chunk",
        }
    }

    fn for_kind(kind: &str) -> &'static [Plant] {
        match kind {
            "mention" => &[Plant::OtherClass, Plant::OtherName],
            "relation" => &[Plant::SwapEnds, Plant::OtherName],
            _ => &[Plant::OtherNumber, Plant::OtherUnit, Plant::OtherName],
        }
    }
}

fn unit_of(object: &str) -> Option<&str> {
    let (_, last) = object.trim().rsplit_once(' ')?;
    (!last.chars().any(|c| c.is_ascii_digit())).then_some(last)
}

fn change_number(object: &str, text: &str) -> Option<String> {
    let start = object.find(|c: char| c.is_ascii_digit())?;
    let end = object[start..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(object.len(), |i| start + i);
    let number: u64 = object[start..end].parse().ok()?;
    let changed = (number + 1).checked_mul(7)?.to_string();
    if text.contains(&changed) {
        return None;
    }
    Some(format!("{}{changed}{}", &object[..start], &object[end..]))
}

pub fn plant(
    fact: &Fact,
    how: Plant,
    others: &[Fact],
    ontology: &Ontology,
    offset: usize,
) -> Option<Fact> {
    let mut out = fact.clone();
    let pick = |list: Vec<&str>| -> Option<String> {
        (!list.is_empty()).then(|| list[offset % list.len()].to_string())
    };
    match how {
        Plant::SwapEnds => {
            let object = fact.object.clone()?;
            if fold(&object) == fold(&fact.subject) {
                return None;
            }
            out.object = Some(fact.subject.clone());
            out.subject = object;
        }
        Plant::OtherClass => {
            let classes: BTreeSet<&str> = others
                .iter()
                .map(|f| f.class_iri.as_str())
                .filter(|c| {
                    *c != fact.class_iri
                        && !ontology.is_a(c, &fact.class_iri)
                        && !ontology.is_a(&fact.class_iri, c)
                })
                .collect();
            out.class_iri = pick(classes.into_iter().collect())?;
        }
        Plant::OtherNumber => {
            out.object = Some(change_number(fact.object.as_deref()?, &fact.text)?);
        }
        Plant::OtherUnit => {
            let object = fact.object.as_deref()?;
            let unit = unit_of(object)?;
            let units: BTreeSet<&str> = others
                .iter()
                .filter_map(|f| f.object.as_deref().and_then(unit_of))
                .filter(|u| fold(u) != fold(unit))
                .collect();
            let other = pick(units.into_iter().collect())?;
            out.object = Some(format!("{}{other}", &object[..object.len() - unit.len()]));
        }
        Plant::OtherName => {
            let text = fold(&fact.text);
            let names: BTreeSet<&str> = others
                .iter()
                .map(|f| f.subject.as_str())
                .filter(|n| !n.trim().is_empty() && !text.contains(&fold(n)))
                .collect();
            out.subject = pick(names.into_iter().collect())?;
        }
    }
    Some(out)
}

pub fn claim(fact: &Fact, ontology: &Ontology, around: usize) -> Claim {
    let languages = &ontology.languages;
    let class = ontology.classes.get(&fact.class_iri).map_or_else(
        || local_name(&fact.class_iri).to_string(),
        |c| crate::llm::shortlist::label(c, languages),
    );
    let property = fact.property_iri.as_deref().map(local_name).unwrap_or("");
    let object = fact.object.as_deref().unwrap_or("");
    let text = match fact.kind.as_str() {
        "mention" => format!("\"{}\" is a {class}", fact.subject),
        "relation" => format!("\"{}\" {property} \"{object}\"", fact.subject),
        _ => format!("\"{}\" has {property} {object}", fact.subject),
    };
    let page = fact
        .page_from
        .map_or(String::new(), |p| format!(", page {p}"));
    let context = crate::review::context(&fact.text, fact.span_start, fact.span_end, around);
    Claim {
        text,
        evidence: format!("source: {}{page}\ntext: {context}", fact.path),
    }
}

fn order_key(seed: &str, text: &str) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

pub fn stratify(facts: Vec<Fact>, n: usize) -> Vec<Fact> {
    let mut groups: BTreeMap<(String, String), Vec<Fact>> = BTreeMap::new();
    let mut order = Vec::new();
    for fact in facts {
        let key = (
            fact.method.clone(),
            fact.property_iri
                .clone()
                .unwrap_or_else(|| fact.class_iri.clone()),
        );
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(fact);
    }
    let mut lists: Vec<std::vec::IntoIter<Fact>> = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .map(Vec::into_iter)
        .collect();
    let mut out = Vec::new();
    while out.len() < n {
        let before = out.len();
        for list in &mut lists {
            if out.len() == n {
                break;
            }
            if let Some(fact) = list.next() {
                out.push(fact);
            }
        }
        if out.len() == before {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct KindScore {
    pub kind: String,
    pub right: i64,
    pub total: i64,
    pub caught: i64,
    pub planted: i64,
}

impl KindScore {
    pub fn precision(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.right as f64 / self.total as f64
    }

    pub fn margin(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let p = self.precision();
        Z * (p * (1.0 - p) / self.total as f64).sqrt()
    }
}

pub fn scores(rows: &[Judged]) -> Vec<KindScore> {
    KINDS
        .iter()
        .map(|kind| {
            let mut score = KindScore {
                kind: kind.to_string(),
                right: 0,
                total: 0,
                caught: 0,
                planted: 0,
            };
            for row in rows.iter().filter(|r| r.kind == *kind) {
                if row.planted {
                    score.planted += row.total;
                    score.caught += row.total - row.right;
                } else {
                    score.right += row.right;
                    score.total += row.total;
                }
            }
            score
        })
        .collect()
}

pub fn catch_rate(scores: &[KindScore]) -> Option<f64> {
    let planted: i64 = scores.iter().map(|s| s.planted).sum();
    let caught: i64 = scores.iter().map(|s| s.caught).sum();
    (planted > 0).then(|| caught as f64 / planted as f64)
}

pub fn report(scores: &[KindScore], human: (i64, i64), min_catch_rate: f64) -> (String, bool) {
    let mut out = String::from(
        "| Kind | Judged | Right | Precision | Margin (95%) | Wilson lower | Planted | Caught |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for s in scores {
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | ±{:.1} | {:.1}% | {} | {} |\n",
            s.kind,
            s.total,
            s.right,
            s.precision() * 100.0,
            s.margin() * 100.0,
            crate::review::wilson_lower(s.right, s.total) * 100.0,
            s.planted,
            s.caught
        ));
    }
    let rate = catch_rate(scores);
    let passed = rate.is_some_and(|r| r >= min_catch_rate);
    match rate {
        Some(rate) => out.push_str(&format!(
            "\nPlanted errors caught: {:.1}% (at least {:.0}% needed).\n",
            rate * 100.0,
            min_catch_rate * 100.0
        )),
        None => out.push_str("\nNo planted errors judged yet.\n"),
    }
    let (both, agree) = human;
    out.push_str(&format!(
        "Facts with a human verdict too: {both}, same verdict: {}.\n",
        gold::percent(agree as usize, both as usize)
    ));
    out.push_str(&format!(
        "\nVerdict: {}\n",
        if passed {
            "passed"
        } else {
            "failed, the judge catches too few planted errors; its precision numbers are not trusted"
        }
    ));
    (out, passed)
}

pub struct JudgeOptions {
    pub method: String,
    pub seed: Option<String>,
    pub job: Options,
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &JudgeOptions,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.eval.is_empty() {
        bail!("[models] eval is empty; set it to a model stronger than extract");
    }
    let name = ModelName::configured(&config.models.eval, &config.providers)
        .map_err(|e| anyhow!("models.eval: {e}"))?;
    let ontology = ontology::load_checked(config_dir, &config.ontology)?.ontology;
    let prompts = prompts(config_dir)?;
    let lock = match options.job.dry_run {
        true => None,
        false => Some(db::runs::lock(url, &config.database.schema).await?),
    };
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let result = async {
        let Some(version_id) = db::fit::ontology_version(&pool, &ontology.version).await? else {
            bail!("the ontology changed since the last scan; run zontologize scan first");
        };
        let ctx = Ctx {
            pool: &pool,
            config: &config,
            name: &name,
            options: &options.job,
            version_id,
        };
        judge_facts(&ctx, &ontology, &prompts, options, config_dir).await
    }
    .await;
    pool.close().await;
    db::runs::release(lock).await;
    result
}

async fn judge_facts(
    ctx: &Ctx<'_>,
    ontology: &Ontology,
    prompts: &Prompts,
    options: &JudgeOptions,
    config_dir: Option<&Path>,
) -> Result<Outcome> {
    let pool = ctx.pool;
    let eval = &ctx.config.eval;
    let seed = options.seed.clone().unwrap_or_else(|| eval.seed.clone());
    let done = db::review::judged(pool).await?;
    let count = |kind: &str, planted: bool| -> usize {
        done.iter()
            .filter(|r| r.kind == kind && r.planted == planted)
            .map(|r| r.total as usize)
            .sum()
    };
    let mut picked: Vec<(Fact, bool, String)> = Vec::new();
    for (kind, target) in KINDS.into_iter().zip([
        eval.judge_mentions,
        eval.judge_relations,
        eval.judge_attributes,
    ]) {
        let need = target.saturating_sub(count(kind, false));
        let plant_need = eval.planted.saturating_sub(count(kind, true));
        if need == 0 && plant_need == 0 {
            continue;
        }
        let limit = (need.max(plant_need) as i64).saturating_mul(POOL_FACTOR);
        let pool_facts =
            db::review::judge_sample(pool, &options.method, kind, &seed, limit).await?;
        for fact in stratify(pool_facts.clone(), need) {
            picked.push((fact, false, String::new()));
        }
        let ways = Plant::for_kind(kind);
        let mut planted = 0;
        for (i, fact) in pool_facts.iter().enumerate() {
            if planted == plant_need {
                break;
            }
            let found = (0..ways.len()).find_map(|t| {
                let how = ways[(i + t) % ways.len()];
                plant(fact, how, &pool_facts, ontology, i).map(|f| (f, how))
            });
            if let Some((fact, how)) = found {
                picked.push((fact, true, how.as_str().to_string()));
                planted += 1;
            }
        }
    }
    if !picked.is_empty() {
        picked.sort_by_key(|(fact, planted, how)| {
            order_key(&seed, &format!("{}{}{planted}{how}", fact.kind, fact.id))
        });
        let claims: Vec<Claim> = picked
            .iter()
            .map(|(fact, _, _)| claim(fact, ontology, eval.judge_context))
            .collect();
        println!(
            "review judge: {} facts, {} of them planted errors",
            picked.len(),
            picked.iter().filter(|p| p.1).count()
        );
        let Some(found) = judge(ctx, "review-judge", prompts, &claims).await? else {
            return Ok(paused(ctx));
        };
        let mut rows = Vec::new();
        for ((fact, planted, how), (claim, verdict)) in picked.iter().zip(claims.iter().zip(found))
        {
            let Some(right) = verdict else {
                continue;
            };
            let mut snapshot = format!("{}\n{}", claim.text, claim.evidence);
            if *planted {
                snapshot.push_str(&format!("\nplanted: {how}"));
            }
            rows.push((fact, right, snapshot, *planted));
        }
        db::review::record_judged(pool, &rows).await?;
    }
    let scores = scores(&db::review::judged(pool).await?);
    let human = db::review::human_agreement(pool).await?;
    let (body, passed) = report(&scores, human, eval.min_catch_rate);
    print!("{body}");
    let dir = crate::scan::resolve_root(config_dir, &eval.dir);
    gold::write_section(&dir.join(REPORT_FILE), "Judged precision", &body)?;
    if !eval.summary.is_empty() {
        let summary = crate::scan::resolve_root(config_dir, &eval.summary);
        gold::write_section(&summary, "Judged precision", &body)?;
    }
    if !passed {
        println!("review judge: report marked failed; model facts keep their confidence");
        return Ok(Outcome::Finished);
    }
    let kinds = db::review::kind_precision(pool).await?;
    let iris = db::review::precision(pool).await?;
    for scores in crate::review::rescores(&kinds, &iris, ctx.config.review.min_verdicts) {
        let changed = db::review::rescore(pool, &scores).await?;
        println!(
            "review judge: {} {}s set to their measured precision",
            changed, scores.kind
        );
    }
    Ok(Outcome::Finished)
}

pub(crate) fn paused(ctx: &Ctx<'_>) -> Outcome {
    if ctx.options.dry_run {
        Outcome::DryRun
    } else {
        Outcome::Paused
    }
}

pub(crate) fn keep_confirmed<T>(items: Vec<T>, verdicts: &[Option<bool>]) -> (Vec<T>, usize) {
    let mut kept = Vec::new();
    let mut dropped = 0;
    for (item, verdict) in items.into_iter().zip(verdicts) {
        if *verdict == Some(true) {
            kept.push(item);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OntologyConfig;

    fn ontology() -> Ontology {
        let ttl = "@prefix ex: <https://example.org/x#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:Org a owl:Class ; rdfs:label \"Organisation\"@en .
ex:Customer a owl:Class ; rdfs:subClassOf ex:Org .
ex:Pump a owl:Class ; rdfs:label \"Pump\"@en .
";
        let config = OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string()],
            include: Vec::new(),
            exclude: Vec::new(),
            document_class: String::new(),
        };
        crate::ontology::load_sources(&[("x.ttl".to_string(), ttl.as_bytes().to_vec())], &config)
            .ontology
    }

    fn fact(kind: &str, id: i64, subject: &str, class: &str, object: Option<&str>) -> Fact {
        Fact {
            kind: kind.to_string(),
            id,
            class_iri: format!("https://example.org/x#{class}"),
            property_iri: (kind != "mention").then(|| "https://example.org/x#ratedPower".into()),
            method: "llm:m".to_string(),
            confidence: 0.7,
            subject: subject.to_string(),
            object: object.map(str::to_string),
            text: format!("The {subject} runs at 50 kW for Brix."),
            span_start: None,
            span_end: None,
            page_from: Some(2),
            path: "Alpha/sheet.txt".to_string(),
        }
    }

    #[test]
    fn swapped_ends_exchange_subject_and_object() {
        let f = fact("relation", 1, "P-101", "Pump", Some("Brix"));
        let planted = plant(&f, Plant::SwapEnds, &[], &ontology(), 0).unwrap();
        assert_eq!(
            (planted.subject.as_str(), planted.object.as_deref()),
            ("Brix", Some("P-101"))
        );
        let same = fact("relation", 1, "Brix", "Pump", Some("BRIX"));
        assert!(plant(&same, Plant::SwapEnds, &[], &ontology(), 0).is_none());
    }

    #[test]
    fn another_class_is_not_a_sub_or_superclass() {
        let others = [
            fact("mention", 2, "Acme", "Org", None),
            fact("mention", 3, "P-1", "Pump", None),
        ];
        let f = fact("mention", 1, "Brix", "Customer", None);
        let planted = plant(&f, Plant::OtherClass, &others, &ontology(), 0).unwrap();
        assert_eq!(planted.class_iri, "https://example.org/x#Pump");
        let lone = fact("mention", 1, "Brix", "Org", None);
        assert!(plant(&lone, Plant::OtherClass, &others[..1], &ontology(), 0).is_none());
    }

    #[test]
    fn a_changed_number_or_unit_differs_from_the_text() {
        let f = fact("attribute", 1, "P-101", "Pump", Some("50 kW"));
        let number = plant(&f, Plant::OtherNumber, &[], &ontology(), 0).unwrap();
        assert_eq!(number.object.as_deref(), Some("357 kW"));
        let others = [fact("attribute", 2, "P-2", "Pump", Some("8 bar"))];
        let unit = plant(&f, Plant::OtherUnit, &others, &ontology(), 0).unwrap();
        assert_eq!(unit.object.as_deref(), Some("50 bar"));
        let bare = fact("attribute", 1, "P-101", "Pump", Some("steel"));
        assert!(plant(&bare, Plant::OtherNumber, &[], &ontology(), 0).is_none());
        assert!(plant(&bare, Plant::OtherUnit, &others, &ontology(), 0).is_none());
    }

    #[test]
    fn a_name_from_another_chunk_is_not_in_the_text() {
        let f = fact("mention", 1, "P-101", "Pump", None);
        let others = [
            fact("mention", 2, "Brix", "Org", None),
            fact("mention", 3, "Delta Works", "Org", None),
        ];
        let planted = plant(&f, Plant::OtherName, &others, &ontology(), 0).unwrap();
        assert_eq!(planted.subject, "Delta Works");
    }

    #[test]
    fn claims_read_like_the_fact() {
        let mention = claim(&fact("mention", 1, "Brix", "Org", None), &ontology(), 20);
        assert_eq!(mention.text, "\"Brix\" is a Organisation");
        assert!(
            mention
                .evidence
                .starts_with("source: Alpha/sheet.txt, page 2\n"),
            "{}",
            mention.evidence
        );
        let attribute = claim(
            &fact("attribute", 1, "P-1", "Pump", Some("50 kW")),
            &ontology(),
            20,
        );
        assert_eq!(attribute.text, "\"P-1\" has ratedPower 50 kW");
    }

    #[test]
    fn verdicts_map_ids_and_ignore_strangers() {
        let answer: Answer = serde_json::from_str(
            r#"{"verdicts":[{"id":"j2","right":false},{"id":"j1","right":true},
            {"id":"j9","right":true},{"id":"j1","right":false}]}"#,
        )
        .unwrap();
        assert_eq!(verdicts(answer, 3), vec![Some(true), Some(false), None]);
    }

    #[test]
    fn stratify_takes_turns_over_methods_and_classes() {
        let facts = vec![
            fact("mention", 1, "a", "Org", None),
            fact("mention", 2, "b", "Org", None),
            fact("mention", 3, "c", "Org", None),
            fact("mention", 4, "d", "Pump", None),
        ];
        let ids: Vec<i64> = stratify(facts, 3).iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![1, 4, 2]);
    }

    #[test]
    fn catch_rate_and_precision_with_a_margin() {
        let rows = vec![
            Judged {
                kind: "mention".to_string(),
                planted: false,
                right: 90,
                total: 100,
            },
            Judged {
                kind: "mention".to_string(),
                planted: true,
                right: 1,
                total: 10,
            },
            Judged {
                kind: "relation".to_string(),
                planted: true,
                right: 2,
                total: 10,
            },
        ];
        let found = scores(&rows);
        assert_eq!((found[0].right, found[0].total), (90, 100));
        assert_eq!((found[0].caught, found[0].planted), (9, 10));
        assert!((found[0].margin() - 0.0588).abs() < 0.001);
        assert_eq!(catch_rate(&found), Some(0.85));
        let (text, passed) = report(&found, (4, 3), 0.9);
        assert!(!passed);
        assert!(text.contains("Verdict: failed"), "{text}");
        assert!(
            text.contains("| mention | 100 | 90 | 90.0% | ±5.9 |"),
            "{text}"
        );
        assert!(text.contains("same verdict: 75.0%"), "{text}");
        let (_, passed) = report(&found, (0, 0), 0.8);
        assert!(passed);
        assert_eq!(catch_rate(&scores(&[])), None);
    }

    #[test]
    fn only_confirmed_items_are_kept() {
        let (kept, dropped) = keep_confirmed(vec!["a", "b", "c"], &[Some(true), Some(false), None]);
        assert_eq!((kept, dropped), (vec!["a"], 2));
    }
}
