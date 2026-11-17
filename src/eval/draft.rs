use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;

use super::gold::{self, Agreement, grounded, norm};
use super::judge::{self, Claim};
use crate::config::{self, Config};
use crate::db;
use crate::db::eval::Document;
use crate::facts::sample::{Candidate, Settings, first_folder, letter_share, select, text_hash};
use crate::llm::batch::{self, Batch, parse};
use crate::llm::job::{Options, Outcome};
use crate::llm::prompt::{self, Prompt};
use crate::llm::{ModelName, Request};
use crate::ontology::{self, Ontology};

const WORLD_SCHEMA: &str = include_str!("../../prompts/eval-world-schema.json");
const JOIN_SCHEMA: &str = include_str!("../../prompts/eval-join-schema.json");
const QUESTIONS_SCHEMA: &str = include_str!("../../prompts/eval-questions-schema.json");
const ANSWER_SCHEMA: &str = include_str!("../../prompts/eval-answer-schema.json");
const ATTRIBUTES_SCHEMA: &str = include_str!("../../prompts/eval-attributes-schema.json");
const MAX_EVIDENCE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    World,
    Questions,
    Attributes,
}

impl What {
    pub const ALL: [What; 3] = [What::World, What::Questions, What::Attributes];

    pub fn as_str(self) -> &'static str {
        match self {
            What::World => "world",
            What::Questions => "questions",
            What::Attributes => "attributes",
        }
    }

    pub fn parse(name: &str) -> Option<What> {
        What::ALL.into_iter().find(|w| w.as_str() == name)
    }

    pub fn file(self) -> &'static str {
        match self {
            What::World => "world.csv",
            What::Questions => "questions.toml",
            What::Attributes => "attributes.csv",
        }
    }
}

pub struct DraftOptions {
    pub set: String,
    pub what: What,
    pub folders: Vec<String>,
    pub job: Options,
}

pub(crate) struct Ctx<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    pub name: &'a ModelName,
    pub options: &'a Options,
    pub version_id: i64,
}

impl Ctx<'_> {
    pub(crate) async fn ask<A: DeserializeOwned>(
        &self,
        task: &'static str,
        requests: &[Request],
    ) -> Result<Option<HashMap<String, String>>> {
        if requests.is_empty() {
            return Ok(Some(HashMap::new()));
        }
        let batch = Batch {
            pool: self.pool,
            config: self.config,
            name: self.name,
            task,
            version_id: self.version_id,
        };
        batch::run::<A>(&batch, requests, self.options).await
    }

    pub(crate) fn key(&self, request: &Request) -> String {
        request.key(self.name)
    }
}

pub(crate) struct Prompts {
    system: Prompt,
    user: Prompt,
    schema: serde_json::Value,
}

impl Prompts {
    pub(crate) fn load(
        config_dir: Option<&Path>,
        name: &str,
        variables: &[&str],
        required: &str,
        schema: &str,
    ) -> Result<Prompts> {
        let system = prompt::load(config_dir, &format!("{name}-system"))?;
        system.check(&[])?;
        let user = prompt::load(config_dir, &format!("{name}-user"))?;
        user.check(variables)?;
        user.require(required)?;
        let schema = serde_json::from_str(schema)
            .with_context(|| format!("built-in prompts/{name}-schema.json is not valid JSON"))?;
        Ok(Prompts {
            system,
            user,
            schema,
        })
    }

    pub(crate) fn request(&self, task: &str, values: &[(&str, &str)]) -> Request {
        Request {
            task: task.to_string(),
            system: self.system.text.clone(),
            user: self.user.fill(values),
            schema: self.schema.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Doc {
    path: String,
    kind: String,
    text: String,
    normed: String,
}

struct Folder {
    a: Vec<Doc>,
    b: Vec<Doc>,
}

struct Output {
    files: Vec<(&'static str, String)>,
    report: String,
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

fn documents_block(docs: &[&Doc], max_chars: usize) -> String {
    let mut out = String::new();
    for doc in docs {
        out.push_str(&format!(
            "=== file: {} ({})\n{}\n\n",
            doc.path,
            doc.kind,
            truncate(&doc.text, max_chars).trim_end()
        ));
    }
    out
}

fn batches<T>(items: &[T], size: impl Fn(&T) -> usize, limit: usize) -> Vec<Vec<&T>> {
    let mut out: Vec<Vec<&T>> = Vec::new();
    let mut used = 0;
    for item in items {
        let chars = size(item);
        if out.is_empty() || used + chars > limit {
            out.push(Vec::new());
            used = 0;
        }
        used += chars;
        if let Some(last) = out.last_mut() {
            last.push(item);
        }
    }
    out
}

fn doc_batches<'a>(docs: &'a [Doc], config: &Config) -> Vec<Vec<&'a Doc>> {
    let max = config.eval.max_doc_chars;
    batches(
        docs,
        |d| d.text.chars().count().min(max),
        config.eval.request_chars,
    )
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &DraftOptions,
) -> Result<Outcome> {
    gold::check_set(&options.set)?;
    let config = config::load(config_dir)?;
    if config.models.eval.is_empty() {
        bail!(
            "[models] eval is empty; set it to a model stronger than the extract model, such as claude-code:opus"
        );
    }
    let name = ModelName::configured(&config.models.eval, &config.providers)
        .map_err(|e| anyhow!("models.eval: {e}"))?;
    let ontology = ontology::load_checked(config_dir, &config.ontology)?.ontology;
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
    let result = draft(&pool, &config, &name, &ontology, config_dir, options).await;
    pool.close().await;
    db::runs::release(lock).await;
    result
}

async fn draft(
    pool: &PgPool,
    config: &Config,
    name: &ModelName,
    ontology: &Ontology,
    config_dir: Option<&Path>,
    options: &DraftOptions,
) -> Result<Outcome> {
    let docs: Vec<Document> = db::eval::documents(pool)
        .await?
        .into_iter()
        .filter(|d| {
            options.folders.is_empty() || options.folders.iter().any(|f| f == first_folder(&d.path))
        })
        .collect();
    if docs.is_empty() {
        bail!("eval draft: no present documents in scope; run zontologize scan first");
    }
    let rows: Vec<(String, String, String)> = docs
        .iter()
        .map(|d| (d.source.clone(), d.path.clone(), d.sha256.clone()))
        .collect();
    let hash = gold::documents_hash(&rows);
    let set_dir = crate::scan::resolve_root(config_dir, &config.eval.dir).join(&options.set);
    let (dir, reused) = gold::version_for(&set_dir, &hash)?;
    let what = options.what;
    let file = dir.join(what.file());
    if file.exists() {
        println!(
            "eval draft: {} is frozen and the documents did not change; nothing to do",
            file.display()
        );
        return Ok(Outcome::Finished);
    }
    let Some(version_id) = db::fit::ontology_version(pool, &ontology.version).await? else {
        bail!("the ontology changed since the last scan; run zontologize scan first");
    };
    let ctx = Ctx {
        pool,
        config,
        name,
        options: &options.job,
        version_id,
    };
    let output = match what {
        What::World => world(&ctx, ontology, config_dir, &docs).await?,
        What::Questions => questions(&ctx, config_dir, &docs).await?,
        What::Attributes => attributes(&ctx, config_dir, &docs).await?,
    };
    let Some(output) = output else {
        return Ok(judge::paused(&ctx));
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if !reused {
        let mut writer = csv::Writer::from_writer(Vec::new());
        writer.write_record(["source", "path", "sha256"])?;
        for row in &rows {
            writer.write_record([&row.0, &row.1, &row.2])?;
        }
        std::fs::write(dir.join("documents.csv"), writer.into_inner()?)
            .with_context(|| format!("writing {}", dir.display()))?;
        std::fs::write(dir.join(gold::HASH_FILE), format!("{hash}\n"))
            .with_context(|| format!("writing {}", dir.display()))?;
    }
    for (file_name, text) in &output.files {
        let path = dir.join(file_name);
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    let body = format!(
        "Made by {name} from {} documents in scope (documents hash `{hash}`).\n\n{}",
        docs.len(),
        output.report
    );
    gold::write_section(&dir.join("report.md"), what.as_str(), &body)?;
    println!("eval draft: {} written in {}", what.as_str(), dir.display());
    Ok(Outcome::Finished)
}

async fn folders(ctx: &Ctx<'_>, docs: &[Document]) -> Result<Vec<Folder>> {
    let eval = &ctx.config.eval;
    let mut by_folder: BTreeMap<&str, Vec<&Document>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for doc in docs {
        if doc.chars >= eval.min_doc_chars as i64 && seen.insert(doc.content_id) {
            by_folder
                .entry(first_folder(&doc.path))
                .or_default()
                .push(doc);
        }
    }
    let mut picks = Vec::new();
    for list in by_folder.values() {
        let keys: Vec<(i64, &str)> = list.iter().map(|d| (d.id, d.kind.as_str())).collect();
        let (a, b) = gold::split_sample(&keys, &eval.seed, eval.docs_per_kind);
        picks.push((list, a, b));
    }
    let contents: Vec<i64> = picks
        .iter()
        .flat_map(|(list, a, b)| {
            list.iter()
                .filter(|d| a.contains(&d.id) || b.contains(&d.id))
                .map(|d| d.content_id)
        })
        .collect();
    let texts = db::eval::texts(ctx.pool, &contents).await?;
    let load = |list: &[&Document], ids: &[i64]| -> Vec<Doc> {
        ids.iter()
            .filter_map(|id| list.iter().find(|d| d.id == *id))
            .filter_map(|d| {
                let text = texts.get(&d.content_id)?;
                Some(Doc {
                    path: d.path.clone(),
                    kind: d.kind.clone(),
                    normed: norm(text),
                    text: text.clone(),
                })
            })
            .collect()
    };
    Ok(picks
        .into_iter()
        .map(|(list, a, b)| Folder {
            a: load(list, &a),
            b: load(list, &b),
        })
        .collect())
}

#[derive(Deserialize)]
struct WorldAnswer {
    entities: Vec<WorldItem>,
}

#[derive(Deserialize)]
struct WorldItem {
    name: String,
    class: String,
    spellings: Vec<SpellingItem>,
}

#[derive(Deserialize)]
struct SpellingItem {
    text: String,
    file: String,
    quote: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Spelling {
    variant: String,
    file: String,
    quote: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Found {
    name: String,
    class: String,
    spellings: Vec<Spelling>,
}

#[derive(Debug, Default, PartialEq)]
struct Drops {
    kept: usize,
    file: usize,
    quote: usize,
    spelling: usize,
    class: usize,
}

impl Drops {
    fn line(&self) -> String {
        format!(
            "{} kept; dropped: {} not in a file shown, {} quote not in the file, {} spelling \
             not in the quote, {} class not allowed",
            self.kept, self.file, self.quote, self.spelling, self.class
        )
    }
}

fn ground_world(
    items: Vec<WorldItem>,
    shown: &HashMap<&str, &str>,
    classes: &HashSet<&str>,
    drops: &mut Drops,
) -> Vec<Found> {
    let mut out = Vec::new();
    for item in items {
        if !classes.contains(item.class.as_str()) {
            drops.class += item.spellings.len();
            continue;
        }
        let mut spellings = Vec::new();
        for s in item.spellings {
            let Some(text) = shown.get(s.file.as_str()) else {
                drops.file += 1;
                continue;
            };
            if !grounded(text, &s.quote) {
                drops.quote += 1;
                continue;
            }
            if !grounded(&norm(&s.quote), &s.text) {
                drops.spelling += 1;
                continue;
            }
            drops.kept += 1;
            spellings.push(Spelling {
                variant: s.text.trim().to_string(),
                file: s.file,
                quote: s.quote.trim().to_string(),
            });
        }
        if !spellings.is_empty() {
            out.push(Found {
                name: item.name.trim().to_string(),
                class: item.class,
                spellings,
            });
        }
    }
    out
}

fn keys(found: &Found) -> Vec<String> {
    std::iter::once(&found.name)
        .chain(found.spellings.iter().map(|s| &s.variant))
        .map(|n| norm(n))
        .collect()
}

fn merge(members: Vec<Found>, name: Option<&str>) -> Option<Found> {
    let mut members = members.into_iter();
    let mut out = members.next()?;
    for member in members {
        out.spellings.extend(member.spellings);
    }
    let mut seen = HashSet::new();
    out.spellings.retain(|s| seen.insert(norm(&s.variant)));
    if let Some(name) = name.filter(|n| keys(&out).contains(&norm(n))) {
        out.name = name.trim().to_string();
    }
    Some(out)
}

fn by_class(members: Vec<Found>) -> Vec<Found> {
    let mut classes: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Found>> = HashMap::new();
    for member in members {
        if !groups.contains_key(&member.class) {
            classes.push(member.class.clone());
        }
        groups.entry(member.class.clone()).or_default().push(member);
    }
    classes
        .into_iter()
        .filter_map(|c| groups.remove(&c).and_then(|m| merge(m, None)))
        .collect()
}

#[derive(Deserialize)]
struct JoinAnswer {
    groups: Vec<JoinGroup>,
}

#[derive(Deserialize)]
struct JoinGroup {
    refs: Vec<String>,
    name: String,
}

fn join_lines(found: &[Found]) -> String {
    found
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let variants: Vec<&str> = f.spellings.iter().map(|s| s.variant.as_str()).collect();
            format!(
                "- e{} | {} | {} | {}\n",
                i + 1,
                f.class,
                f.name,
                variants.join("; ")
            )
        })
        .collect()
}

fn apply_join(found: Vec<Found>, groups: &[JoinGroup]) -> Vec<Found> {
    let mut keys: Vec<Vec<String>> = vec![Vec::new(); found.len()];
    let mut names: HashMap<usize, &str> = HashMap::new();
    for (g, group) in groups.iter().enumerate() {
        for r in &group.refs {
            let Some(i) = r
                .trim()
                .strip_prefix('e')
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| (1..=found.len()).contains(n))
            else {
                continue;
            };
            keys[i - 1].push(format!("{g}\u{1f}{}", found[i - 1].class));
            names.entry(i - 1).or_insert(&group.name);
        }
    }
    let components = gold::components(&keys);
    let mut slots: Vec<Option<Found>> = found.into_iter().map(Some).collect();
    components
        .into_iter()
        .filter_map(|members| {
            let name = members.iter().find_map(|i| names.get(i).copied());
            let list: Vec<Found> = members.iter().filter_map(|i| slots[*i].take()).collect();
            merge(list, name)
        })
        .collect()
}

struct Disputed {
    component: usize,
    found: Found,
}

fn world_claim(found: &Found, ontology: &Ontology) -> Claim {
    let class = ontology.classes.get(&found.class).map_or_else(
        || found.class.clone(),
        |c| crate::llm::shortlist::label(c, &ontology.languages),
    );
    let variants: Vec<&str> = found.spellings.iter().map(|s| s.variant.as_str()).collect();
    let evidence: Vec<String> = found
        .spellings
        .iter()
        .take(MAX_EVIDENCE)
        .map(|s| format!("{}: \"{}\"", s.file, s.quote))
        .collect();
    Claim {
        text: format!(
            "\"{}\" is a {class} that these quotes name (spellings: {})",
            found.name,
            variants.join("; ")
        ),
        evidence: evidence.join("\n"),
    }
}

fn split_agreement(agreement: Agreement<Found>) -> (Vec<Found>, Vec<Disputed>, usize) {
    let mut gold = Vec::new();
    let mut disputed = Vec::new();
    let mut component = 0;
    let groups = agreement
        .agreed
        .into_iter()
        .map(|(a, b)| (true, a.into_iter().chain(b).collect::<Vec<_>>()))
        .chain(agreement.only_a.into_iter().map(|a| (false, a)))
        .chain(agreement.only_b.into_iter().map(|b| (false, b)));
    for (agreed, members) in groups {
        let mut candidates = by_class(members);
        if agreed && candidates.len() == 1 {
            gold.extend(candidates.pop());
            continue;
        }
        disputed.extend(
            candidates
                .into_iter()
                .map(|found| Disputed { component, found }),
        );
        component += 1;
    }
    (gold, disputed, component)
}

fn world_classes(ontology: &Ontology, config: &Config) -> Vec<String> {
    let document = ontology
        .resolve(&config.ontology.document_class)
        .unwrap_or_default();
    let roots: Vec<String> = ontology
        .roots
        .iter()
        .filter_map(|r| ontology.resolve(r).ok())
        .filter(|iri| ontology.classes.get(iri).is_some_and(|c| c.selected) && *iri != document)
        .collect();
    if !roots.is_empty() {
        return roots;
    }
    ontology
        .classes
        .values()
        .filter(|c| c.selected && c.iri != document)
        .map(|c| c.iri.clone())
        .collect()
}

fn world_csv(found: &[Found], ontology: &Ontology) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["name", "variant", "class", "file", "quote"])?;
    for f in found {
        let class = crate::facts::report::compact(ontology, &f.class);
        for s in &f.spellings {
            writer.write_record([&f.name, &s.variant, &class, &s.file, &s.quote])?;
        }
    }
    String::from_utf8(writer.into_inner()?).context("world.csv is not UTF-8")
}

async fn world(
    ctx: &Ctx<'_>,
    ontology: &Ontology,
    config_dir: Option<&Path>,
    docs: &[Document],
) -> Result<Option<Output>> {
    let config = ctx.config;
    let languages = config.ontology.languages.join(", ");
    let classes = world_classes(ontology, config);
    let class_set: HashSet<&str> = classes.iter().map(String::as_str).collect();
    let class_lines = crate::llm::shortlist::shortlist_lines(
        ontology,
        &classes,
        &config.ontology.languages,
        config.llm_extract.definition_chars,
    );
    let prompts = Prompts::load(
        config_dir,
        "eval-world",
        &["languages", "classes", "documents"],
        "documents",
        WORLD_SCHEMA,
    )?;
    let join_prompts = Prompts::load(
        config_dir,
        "eval-join",
        &["entities"],
        "entities",
        JOIN_SCHEMA,
    )?;
    let judge_prompts = judge::prompts(config_dir)?;
    let folders = folders(ctx, docs).await?;
    let mut sent: Vec<(usize, usize, Vec<&Doc>, Request)> = Vec::new();
    for (f, folder) in folders.iter().enumerate() {
        for (side, list) in [&folder.a, &folder.b].into_iter().enumerate() {
            for part in doc_batches(list, config) {
                let block = documents_block(&part, config.eval.max_doc_chars);
                let request = prompts.request(
                    "eval-world",
                    &[
                        ("languages", &languages),
                        ("classes", class_lines.trim_end()),
                        ("documents", &block),
                    ],
                );
                sent.push((f, side, part, request));
            }
        }
    }
    let requests: Vec<Request> = sent.iter().map(|s| s.3.clone()).collect();
    let Some(answers) = ctx.ask::<WorldAnswer>("eval-world", &requests).await? else {
        return Ok(None);
    };
    let mut drops = Drops::default();
    let mut unanswered = 0;
    let mut found: Vec<[Vec<Found>; 2]> =
        folders.iter().map(|_| [Vec::new(), Vec::new()]).collect();
    for (f, side, part, request) in &sent {
        let Some(answer) = parse::<WorldAnswer>(&answers, &ctx.key(request)) else {
            unanswered += 1;
            continue;
        };
        let shown: HashMap<&str, &str> = part
            .iter()
            .map(|d| (d.path.as_str(), d.normed.as_str()))
            .collect();
        found[*f][*side].extend(ground_world(
            answer.entities,
            &shown,
            &class_set,
            &mut drops,
        ));
    }
    let mut joins: Vec<(usize, usize, Request)> = Vec::new();
    for (f, sides) in found.iter().enumerate() {
        for (side, list) in sides.iter().enumerate() {
            if list.len() > 1 {
                let request = join_prompts.request("eval-join", &[("entities", &join_lines(list))]);
                joins.push((f, side, request));
            }
        }
    }
    let requests: Vec<Request> = joins.iter().map(|j| j.2.clone()).collect();
    let Some(answers) = ctx.ask::<JoinAnswer>("eval-join", &requests).await? else {
        return Ok(None);
    };
    for (f, side, request) in &joins {
        if let Some(answer) = parse::<JoinAnswer>(&answers, &ctx.key(request)) {
            let list = std::mem::take(&mut found[*f][*side]);
            found[*f][*side] = apply_join(list, &answer.groups);
        }
    }
    let mut gold_list = Vec::new();
    let mut disputed: Vec<Disputed> = Vec::new();
    let (mut agreed, mut only_a, mut only_b) = (0, 0, 0);
    let mut offset = 0;
    let mut sizes = [0usize; 2];
    for sides in found {
        let [a, b] = sides;
        sizes[0] += a.len();
        sizes[1] += b.len();
        let agreement = gold::agree(a, b, keys);
        agreed += agreement.agreed.len();
        only_a += agreement.only_a.len();
        only_b += agreement.only_b.len();
        let (kept, open, components) = split_agreement(agreement);
        gold_list.extend(kept);
        disputed.extend(open.into_iter().map(|mut c| {
            c.component += offset;
            c
        }));
        offset += components;
    }
    let claims: Vec<Claim> = disputed
        .iter()
        .map(|c| world_claim(&c.found, ontology))
        .collect();
    let Some(verdicts) = judge::judge(ctx, "eval-world-judge", &judge_prompts, &claims).await?
    else {
        return Ok(None);
    };
    let mut decided = HashSet::new();
    let mut confirmed = 0;
    for (candidate, verdict) in disputed.into_iter().zip(&verdicts) {
        if *verdict == Some(true) && decided.insert(candidate.component) {
            confirmed += 1;
            gold_list.push(candidate.found);
        }
    }
    let final_keys: Vec<Vec<String>> = gold_list.iter().map(keys).collect();
    let mut slots: Vec<Option<Found>> = gold_list.into_iter().map(Some).collect();
    let merged: Vec<Found> = gold::components(&final_keys)
        .into_iter()
        .filter_map(|members| {
            merge(
                members.iter().filter_map(|i| slots[*i].take()).collect(),
                None,
            )
        })
        .collect();
    let csv = world_csv(&merged, ontology)?;
    super::parse_world(super::WORLD_FILE, &csv, ontology)?;
    let total = agreed + only_a + only_b;
    let spellings: usize = merged.iter().map(|f| f.spellings.len()).sum();
    let report = format!(
        "- folders: {}; documents in sample A: {}, in sample B: {}\n\
         - requests without a usable answer: {unanswered}\n\
         - spellings: {}\n\
         - entities after joining: {} in A, {} in B\n\
         - agreement: {agreed} of {total} entities found in both samples ({})\n\
         - judged: {} claims on {offset} disputed entities, {confirmed} confirmed\n\
         - gold: {} entities, {spellings} spellings\n",
        folders.len(),
        folders.iter().map(|f| f.a.len()).sum::<usize>(),
        folders.iter().map(|f| f.b.len()).sum::<usize>(),
        drops.line(),
        sizes[0],
        sizes[1],
        gold::percent(agreed, total),
        claims.len(),
        merged.len(),
    );
    Ok(Some(Output {
        files: vec![(What::World.file(), csv)],
        report,
    }))
}

#[derive(Deserialize)]
struct QuestionsAnswer {
    questions: Vec<QuestionItem>,
}

#[derive(Deserialize)]
struct QuestionItem {
    text: String,
    language: String,
    files: Vec<String>,
    quote: String,
}

#[derive(Deserialize)]
struct AnswerCheck {
    answerable: bool,
    quote: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Question {
    folder: usize,
    text: String,
    language: String,
    files: Vec<String>,
}

#[derive(serde::Serialize)]
struct QuestionsFile {
    questions: Vec<QuestionRow>,
}

#[derive(serde::Serialize)]
struct QuestionRow {
    text: String,
    expect: Vec<String>,
}

fn ground_questions(
    folder: usize,
    items: Vec<QuestionItem>,
    shown: &HashMap<&str, &str>,
    dropped: &mut usize,
) -> Vec<Question> {
    let mut out = Vec::new();
    for item in items {
        let files: Vec<String> = item
            .files
            .into_iter()
            .filter(|f| shown.contains_key(f.as_str()))
            .collect();
        let holds = files.iter().any(|f| {
            shown
                .get(f.as_str())
                .is_some_and(|t| grounded(t, &item.quote))
        });
        if item.text.trim().is_empty() || !holds {
            *dropped += 1;
            continue;
        }
        out.push(Question {
            folder,
            text: item.text.trim().to_string(),
            language: item.language.trim().to_lowercase(),
            files,
        });
    }
    out
}

fn spread_questions(questions: Vec<Question>, folders: usize, limit: usize) -> Vec<Question> {
    let mut lists: Vec<std::vec::IntoIter<Question>> = (0..folders)
        .map(|f| {
            questions
                .iter()
                .filter(|q| q.folder == f)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect();
    let mut out = Vec::new();
    while out.len() < limit {
        let before = out.len();
        for list in &mut lists {
            if out.len() < limit
                && let Some(q) = list.next()
            {
                out.push(q);
            }
        }
        if out.len() == before {
            break;
        }
    }
    out
}

async fn questions(
    ctx: &Ctx<'_>,
    config_dir: Option<&Path>,
    docs: &[Document],
) -> Result<Option<Output>> {
    let config = ctx.config;
    let eval = &config.eval;
    let languages = config.ontology.languages.join(", ");
    let prompts = Prompts::load(
        config_dir,
        "eval-questions",
        &["count", "languages", "documents"],
        "documents",
        QUESTIONS_SCHEMA,
    )?;
    let check = Prompts::load(
        config_dir,
        "eval-answer",
        &["question", "documents"],
        "documents",
        ANSWER_SCHEMA,
    )?;
    let folders = folders(ctx, docs).await?;
    let count = eval.questions.div_ceil(folders.len().max(1)).to_string();
    let mut sent: Vec<(usize, Vec<&Doc>, Request)> = Vec::new();
    for (f, folder) in folders.iter().enumerate() {
        if let Some(part) = doc_batches(&folder.a, config).into_iter().next() {
            let block = documents_block(&part, eval.max_doc_chars);
            let request = prompts.request(
                "eval-questions",
                &[
                    ("count", &count),
                    ("languages", &languages),
                    ("documents", &block),
                ],
            );
            sent.push((f, part, request));
        }
    }
    let requests: Vec<Request> = sent.iter().map(|s| s.2.clone()).collect();
    let Some(answers) = ctx
        .ask::<QuestionsAnswer>("eval-questions", &requests)
        .await?
    else {
        return Ok(None);
    };
    let mut dropped = 0;
    let mut proposed = 0;
    let mut found = Vec::new();
    for (f, part, request) in &sent {
        let Some(answer) = parse::<QuestionsAnswer>(&answers, &ctx.key(request)) else {
            continue;
        };
        proposed += answer.questions.len();
        let shown: HashMap<&str, &str> = part
            .iter()
            .map(|d| (d.path.as_str(), d.normed.as_str()))
            .collect();
        found.extend(ground_questions(*f, answer.questions, &shown, &mut dropped));
    }
    let by_path: HashMap<&str, &Doc> = folders
        .iter()
        .flat_map(|f| f.a.iter())
        .map(|d| (d.path.as_str(), d))
        .collect();
    let checks: Vec<Request> = found
        .iter()
        .map(|q| {
            let pages: Vec<&Doc> = q
                .files
                .iter()
                .filter_map(|f| by_path.get(f.as_str()).copied())
                .collect();
            check.request(
                "eval-answer",
                &[
                    ("question", &q.text),
                    ("documents", &documents_block(&pages, eval.max_doc_chars)),
                ],
            )
        })
        .collect();
    let Some(answers) = ctx.ask::<AnswerCheck>("eval-answer", &checks).await? else {
        return Ok(None);
    };
    let mut failed = 0;
    let mut passed = Vec::new();
    for (question, request) in found.into_iter().zip(&checks) {
        let ok = parse::<AnswerCheck>(&answers, &ctx.key(request)).is_some_and(|a| {
            a.answerable
                && question.files.iter().any(|f| {
                    by_path
                        .get(f.as_str())
                        .is_some_and(|d| grounded(&d.normed, &a.quote))
                })
        });
        if ok {
            passed.push(question);
        } else {
            failed += 1;
        }
    }
    let kept = spread_questions(passed, folders.len(), eval.questions);
    let mut per_language: BTreeMap<&str, usize> = BTreeMap::new();
    for q in &kept {
        *per_language.entry(q.language.as_str()).or_default() += 1;
    }
    let languages_line: Vec<String> = per_language
        .iter()
        .map(|(l, n)| format!("{n} {l}"))
        .collect();
    let file = QuestionsFile {
        questions: kept
            .iter()
            .map(|q| QuestionRow {
                text: q.text.clone(),
                expect: q.files.iter().map(|f| globset::escape(f)).collect(),
            })
            .collect(),
    };
    let text = toml::to_string(&file).context("writing questions.toml")?;
    let report = format!(
        "- folders: {}\n- proposed: {proposed}; dropped by grounding: {dropped}; \
         not answered from the expected files alone: {failed}\n- kept: {} ({})\n",
        folders.len(),
        kept.len(),
        languages_line.join(", ")
    );
    Ok(Some(Output {
        files: vec![(What::Questions.file(), text)],
        report,
    }))
}

#[derive(Deserialize)]
struct ValuesAnswer {
    values: Vec<ValueItem>,
}

#[derive(Deserialize)]
struct ValueItem {
    chunk: String,
    equipment: String,
    property: String,
    value: String,
    unit: String,
    quote: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Value {
    chunk: i64,
    equipment: String,
    property: String,
    value: String,
    unit: String,
    quote: String,
}

struct Labelled {
    id: i64,
    path: String,
    page: Option<i32>,
    ord: i32,
    text: String,
    normed: String,
}

fn ground_values(
    items: Vec<ValueItem>,
    shown: &HashMap<i64, &str>,
    dropped: &mut usize,
) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        let id = item
            .chunk
            .trim()
            .strip_prefix('c')
            .and_then(|n| n.parse::<i64>().ok());
        let text = id.and_then(|id| shown.get(&id));
        let ok = text.is_some_and(|t| grounded(t, &item.quote))
            && grounded(&norm(&item.quote), &item.value);
        match (id, ok) {
            (Some(chunk), true) => out.push(Value {
                chunk,
                equipment: item.equipment.trim().to_string(),
                property: item.property.trim().to_string(),
                value: item.value.trim().to_string(),
                unit: item.unit.trim().to_string(),
                quote: item.quote.trim().to_string(),
            }),
            _ => *dropped += 1,
        }
    }
    out
}

fn value_key(value: &Value) -> Vec<String> {
    vec![format!(
        "{}\u{1f}{}\u{1f}{}",
        value.chunk,
        norm(&value.value),
        norm(&value.unit)
    )]
}

fn chunks_block(chunks: &[&Labelled], max_chars: usize) -> String {
    let mut out = String::new();
    for chunk in chunks {
        let page = chunk.page.map_or(String::new(), |p| format!(", page {p}"));
        out.push_str(&format!(
            "=== chunk c{} (file: {}{page})\n{}\n\n",
            chunk.id,
            chunk.path,
            truncate(&chunk.text, max_chars).trim_end()
        ));
    }
    out
}

async fn attributes(
    ctx: &Ctx<'_>,
    config_dir: Option<&Path>,
    docs: &[Document],
) -> Result<Option<Output>> {
    let config = ctx.config;
    let eval = &config.eval;
    let extract = &config.llm_extract;
    let prompts = Prompts::load(
        config_dir,
        "eval-attributes",
        &["pass", "chunks"],
        "chunks",
        ATTRIBUTES_SCHEMA,
    )?;
    let judge_prompts = judge::prompts(config_dir)?;
    let mut doc_of: HashMap<i64, &Document> = HashMap::new();
    for doc in docs {
        doc_of.entry(doc.content_id).or_insert(doc);
    }
    let contents: Vec<i64> = doc_of.keys().copied().collect();
    let chunks = db::eval::chunks(ctx.pool, &contents).await?;
    let candidates: Vec<Candidate> = chunks
        .iter()
        .filter_map(|c| {
            let doc = doc_of.get(&c.content_id)?;
            Some(Candidate {
                id: c.id,
                document_id: doc.id,
                stratum: format!("{}\u{1f}{}", doc.kind, first_folder(&doc.path)),
                text_hash: text_hash(&c.text),
                letters: letter_share(&c.text),
                ocr_confidence: c.ocr_confidence,
                matched: true,
            })
        })
        .collect();
    let settings = Settings {
        size: eval.attribute_chunks,
        seed: &eval.seed,
        max_per_document: extract.max_per_document,
        unmatched_share: 0.0,
        min_letter_share: extract.min_letter_share,
        min_ocr_confidence: extract.min_ocr_confidence,
    };
    let selected = select(&candidates, &settings);
    let by_id: HashMap<i64, &db::eval::Chunk> = chunks.iter().map(|c| (c.id, c)).collect();
    let labelled: Vec<Labelled> = selected
        .ids
        .iter()
        .filter_map(|id| {
            let chunk = by_id.get(id)?;
            let doc = doc_of.get(&chunk.content_id)?;
            Some(Labelled {
                id: chunk.id,
                path: doc.path.clone(),
                page: chunk.page_from,
                ord: chunk.ord,
                normed: norm(&chunk.text),
                text: chunk.text.clone(),
            })
        })
        .collect();
    let reversed: Vec<&Labelled> = labelled.iter().rev().collect();
    let max = eval.max_doc_chars;
    let size = |c: &&Labelled| c.text.chars().count().min(max);
    let forward: Vec<&Labelled> = labelled.iter().collect();
    let mut sent: Vec<(usize, Vec<&Labelled>, Request)> = Vec::new();
    for (pass, list) in [forward, reversed].iter().enumerate() {
        for part in batches(list, size, eval.request_chars) {
            let part: Vec<&Labelled> = part.into_iter().copied().collect();
            let request = prompts.request(
                "eval-attributes",
                &[
                    ("pass", &(pass + 1).to_string()),
                    ("chunks", &chunks_block(&part, max)),
                ],
            );
            sent.push((pass, part, request));
        }
    }
    let requests: Vec<Request> = sent.iter().map(|s| s.2.clone()).collect();
    let Some(answers) = ctx
        .ask::<ValuesAnswer>("eval-attributes", &requests)
        .await?
    else {
        return Ok(None);
    };
    let mut dropped = 0;
    let mut passes: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
    for (pass, part, request) in &sent {
        let Some(answer) = parse::<ValuesAnswer>(&answers, &ctx.key(request)) else {
            continue;
        };
        let shown: HashMap<i64, &str> = part.iter().map(|c| (c.id, c.normed.as_str())).collect();
        passes[*pass].extend(ground_values(answer.values, &shown, &mut dropped));
    }
    let [one, two] = passes;
    let sizes = (one.len(), two.len());
    let agreement = gold::agree(one, two, value_key);
    let rate = gold::percent(
        agreement.agreed.len(),
        agreement.agreed.len() + agreement.only_a.len() + agreement.only_b.len(),
    );
    let agreed_count = agreement.agreed.len();
    let mut values: Vec<Value> = agreement
        .agreed
        .into_iter()
        .filter_map(|(a, _)| a.into_iter().next())
        .collect();
    let disputed: Vec<Value> = agreement
        .only_a
        .into_iter()
        .chain(agreement.only_b)
        .filter_map(|g| g.into_iter().next())
        .collect();
    let text_of: HashMap<i64, &Labelled> = labelled.iter().map(|c| (c.id, c)).collect();
    let claims: Vec<Claim> = disputed
        .iter()
        .map(|v| Claim {
            text: format!(
                "In this text, \"{}\" has {} {} {}",
                v.equipment, v.property, v.value, v.unit
            )
            .trim_end()
            .to_string(),
            evidence: text_of
                .get(&v.chunk)
                .map_or(String::new(), |c| truncate(&c.text, max).to_string()),
        })
        .collect();
    let Some(verdicts) =
        judge::judge(ctx, "eval-attributes-judge", &judge_prompts, &claims).await?
    else {
        return Ok(None);
    };
    let (confirmed, rejected) = judge::keep_confirmed(disputed, &verdicts);
    let confirmed_count = confirmed.len();
    values.extend(confirmed);
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record([
        "file",
        "page",
        "chunk_ord",
        "equipment",
        "property",
        "value",
        "unit",
        "quote",
    ])?;
    for v in &values {
        let Some(chunk) = text_of.get(&v.chunk) else {
            continue;
        };
        writer.write_record([
            chunk.path.as_str(),
            &chunk.page.map_or(String::new(), |p| p.to_string()),
            &chunk.ord.to_string(),
            &v.equipment,
            &v.property,
            &v.value,
            &v.unit,
            &v.quote,
        ])?;
    }
    let values_csv = String::from_utf8(writer.into_inner()?).context("attributes.csv")?;
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["file", "page", "chunk_ord"])?;
    for chunk in &labelled {
        writer.write_record([
            chunk.path.as_str(),
            &chunk.page.map_or(String::new(), |p| p.to_string()),
            &chunk.ord.to_string(),
        ])?;
    }
    let chunks_csv = String::from_utf8(writer.into_inner()?).context("chunks.csv")?;
    let report = format!(
        "- chunks labelled: {} ({} repeated and {} unreadable left out)\n\
         - values kept by grounding: {} in pass 1, {} in pass 2; dropped: {dropped}\n\
         - agreement: {agreed_count} values in both passes ({rate})\n\
         - judged: {} values in one pass only, {confirmed_count} confirmed, {rejected} dropped\n\
         - gold: {} values\n",
        labelled.len(),
        selected.duplicates,
        selected.junk,
        sizes.0,
        sizes.1,
        claims.len(),
        values.len()
    );
    Ok(Some(Output {
        files: vec![
            (What::Attributes.file(), values_csv),
            ("chunks.csv", chunks_csv),
        ],
        report,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str, class: &str, variants: &[&str]) -> Found {
        Found {
            name: name.to_string(),
            class: class.to_string(),
            spellings: variants
                .iter()
                .map(|v| Spelling {
                    variant: v.to_string(),
                    file: "a.txt".to_string(),
                    quote: v.to_string(),
                })
                .collect(),
        }
    }

    fn spelling(text: &str, file: &str, quote: &str) -> SpellingItem {
        SpellingItem {
            text: text.to_string(),
            file: file.to_string(),
            quote: quote.to_string(),
        }
    }

    #[test]
    fn grounding_drops_spellings_the_file_does_not_hold() {
        let text = norm("Order to BRIX LTD for pump P-101.\nBrix Ltd confirms.");
        let shown: HashMap<&str, &str> = [("Alpha/order.txt", text.as_str())].into();
        let classes: HashSet<&str> = ["ex:Org"].into();
        let items = vec![
            WorldItem {
                name: "Brix Ltd".to_string(),
                class: "ex:Org".to_string(),
                spellings: vec![
                    spelling("BRIX LTD", "Alpha/order.txt", "Order to BRIX LTD"),
                    spelling("Brix Limited", "Alpha/order.txt", "Brix Limited confirms"),
                    spelling("Brix", "Alpha/other.txt", "Brix"),
                    spelling("Brix BV", "Alpha/order.txt", "Brix Ltd confirms"),
                ],
            },
            WorldItem {
                name: "Ghost".to_string(),
                class: "ex:Nope".to_string(),
                spellings: vec![spelling("Ghost", "Alpha/order.txt", "pump")],
            },
        ];
        let mut drops = Drops::default();
        let kept = ground_world(items, &shown, &classes, &mut drops);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].spellings.len(), 1);
        assert_eq!(kept[0].spellings[0].variant, "BRIX LTD");
        assert_eq!(
            drops,
            Drops {
                kept: 1,
                file: 1,
                quote: 1,
                spelling: 1,
                class: 1
            }
        );
    }

    #[test]
    fn join_merges_refs_of_one_class_and_keeps_a_known_name() {
        let list = vec![
            found("Brix", "ex:Org", &["Brix"]),
            found("Brix Ltd", "ex:Org", &["Brix Ltd", "BRIX LTD"]),
            found("Brix", "ex:Site", &["Brix yard"]),
            found("Delta", "ex:Org", &["Delta"]),
        ];
        let groups = vec![JoinGroup {
            refs: vec!["e1".into(), "e2".into(), "e3".into(), "e99".into()],
            name: "Brix Ltd".to_string(),
        }];
        let joined = apply_join(list, &groups);
        assert_eq!(joined.len(), 3);
        assert_eq!(joined[0].name, "Brix Ltd");
        assert_eq!(joined[0].spellings.len(), 2);
        assert_eq!(joined[1].class, "ex:Site");
        assert_eq!(joined[2].name, "Delta");
        let unknown = vec![JoinGroup {
            refs: vec!["e1".into(), "e2".into()],
            name: "Made Up".to_string(),
        }];
        let joined = apply_join(
            vec![
                found("Brix", "ex:Org", &["Brix"]),
                found("B", "ex:Org", &["B"]),
            ],
            &unknown,
        );
        assert_eq!(joined[0].name, "Brix");
    }

    #[test]
    fn agreed_entities_are_gold_and_the_rest_is_disputed() {
        let a = vec![
            found("Brix Ltd", "ex:Org", &["Brix Ltd"]),
            found("P-101", "ex:Pump", &["P-101"]),
            found("Eastport", "ex:Site", &["Eastport"]),
        ];
        let b = vec![
            found("BRIX LTD", "ex:Org", &["BRIX LTD"]),
            found("P-101", "ex:Org", &["P-101"]),
            found("Jan", "ex:Person", &["Jan"]),
        ];
        let agreement = gold::agree(a, b, keys);
        assert!((agreement.rate() - 0.5).abs() < 1e-9);
        let (kept, disputed, components) = split_agreement(agreement);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "Brix Ltd");
        assert_eq!(components, 3);
        let p101: Vec<&Disputed> = disputed.iter().filter(|c| c.component == 0).collect();
        assert_eq!(p101.len(), 2);
        assert_eq!(disputed.len(), 4);
    }

    #[test]
    fn values_need_the_quote_in_the_chunk_and_the_value_in_the_quote() {
        let text = norm("Motor M-1: rated power 55 kW at 1480 rpm.");
        let shown: HashMap<i64, &str> = [(7, text.as_str())].into();
        let item = |chunk: &str, value: &str, quote: &str| ValueItem {
            chunk: chunk.to_string(),
            equipment: "M-1".to_string(),
            property: "power".to_string(),
            value: value.to_string(),
            unit: "kW".to_string(),
            quote: quote.to_string(),
        };
        let mut dropped = 0;
        let kept = ground_values(
            vec![
                item("c7", "55", "rated power 55 kW"),
                item("c7", "75", "rated power 55 kW"),
                item("c8", "55", "rated power 55 kW"),
                item("c7", "55", "rated power 55 kilowatt"),
            ],
            &shown,
            &mut dropped,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 3);
        assert_eq!(value_key(&kept[0]), vec!["7\u{1f}55\u{1f}kw".to_string()]);
    }

    #[test]
    fn questions_keep_shown_files_and_need_a_grounded_quote() {
        let text = norm("The test pressure is 16 bar.");
        let shown: HashMap<&str, &str> = [("A/x.txt", text.as_str())].into();
        let item = |files: &[&str], quote: &str| QuestionItem {
            text: "What is the test pressure?".to_string(),
            language: "EN".to_string(),
            files: files.iter().map(|f| f.to_string()).collect(),
            quote: quote.to_string(),
        };
        let mut dropped = 0;
        let kept = ground_questions(
            0,
            vec![
                item(&["A/x.txt", "A/gone.txt"], "16 bar"),
                item(&["A/x.txt"], "20 bar"),
                item(&["A/gone.txt"], "16 bar"),
            ],
            &shown,
            &mut dropped,
        );
        assert_eq!(dropped, 2);
        assert_eq!(kept[0].files, vec!["A/x.txt".to_string()]);
        assert_eq!(kept[0].language, "en");
    }

    #[test]
    fn questions_are_spread_over_folders() {
        let q = |folder: usize, text: &str| Question {
            folder,
            text: text.to_string(),
            language: "en".to_string(),
            files: Vec::new(),
        };
        let all = vec![q(0, "a"), q(0, "b"), q(0, "c"), q(1, "d")];
        let texts: Vec<String> = spread_questions(all, 2, 3)
            .into_iter()
            .map(|q| q.text)
            .collect();
        assert_eq!(texts, vec!["a", "d", "b"]);
    }

    #[test]
    fn batches_fill_up_to_the_limit_and_keep_big_items_alone() {
        let sizes = [4, 4, 10, 1];
        let parts = batches(&sizes, |s| *s, 8);
        let got: Vec<Vec<usize>> = parts
            .into_iter()
            .map(|p| p.into_iter().copied().collect())
            .collect();
        assert_eq!(got, vec![vec![4, 4], vec![10], vec![1]]);
        assert_eq!(truncate("äbc", 2), "äb");
    }
}
