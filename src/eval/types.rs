use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use super::gold;
use crate::db;

pub const FILE: &str = "types.csv";
pub const OTHER: &str = "other";

#[derive(Debug, Deserialize)]
struct GoldRow {
    path: String,
    class: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct TypeRow {
    pub class: String,
    pub gold: usize,
    pub run: usize,
    pub right: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct TypeScore {
    pub total: usize,
    pub right: usize,
    pub missing: usize,
    pub rows: Vec<TypeRow>,
}

pub fn short(class: &str) -> &str {
    class.rsplit(['#', '/', ':']).next().unwrap_or(class)
}

pub fn score(gold: &[(String, String)], run: &HashMap<String, String>) -> TypeScore {
    let mut rows: BTreeMap<String, TypeRow> = BTreeMap::new();
    let mut out = TypeScore::default();
    for (path, class) in gold.iter().filter(|(_, class)| class != OTHER) {
        let Some(found) = run.get(path) else {
            out.missing += 1;
            continue;
        };
        out.total += 1;
        rows.entry(class.clone()).or_default().gold += 1;
        rows.entry(found.clone()).or_default().run += 1;
        if found == class {
            out.right += 1;
            rows.entry(class.clone()).or_default().right += 1;
        }
    }
    out.rows = rows
        .into_iter()
        .map(|(class, row)| TypeRow { class, ..row })
        .collect();
    out
}

fn share(part: usize, whole: usize) -> String {
    match whole {
        0 => "-".to_string(),
        _ => format!("{:.1}%", part as f64 * 100.0 / whole as f64),
    }
}

pub fn table(score: &TypeScore) -> String {
    let mut out = format!(
        "accuracy: {} of {} ({}); not in the run: {}\n\n\
         | class | gold | run | right | precision | recall |\n|---|---|---|---|---|---|\n",
        score.right,
        score.total,
        share(score.right, score.total),
        score.missing
    );
    for row in &score.rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            short(&row.class),
            row.gold,
            row.run,
            row.right,
            share(row.right, row.run),
            share(row.right, row.gold)
        ));
    }
    out
}

fn gold_path(config_dir: Option<&Path>, eval_dir: &str, set: &str) -> Result<PathBuf> {
    gold::check_set(set)?;
    let set_dir = crate::scan::resolve_root(config_dir, eval_dir).join(set);
    let Some(dir) = gold::latest(&set_dir)? else {
        bail!(
            "no frozen version of set {set}; run zontologize eval draft --set {set} --what types"
        );
    };
    let path = dir.join(FILE);
    if !path.exists() {
        bail!(
            "{} is missing; run zontologize eval draft --set {set} --what types",
            path.display()
        );
    }
    Ok(path)
}

fn read_gold(path: &Path) -> Result<Vec<(String, String)>> {
    let mut reader =
        csv::Reader::from_path(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for row in reader.deserialize::<GoldRow>() {
        let row = row.map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        out.push((row.path, row.class));
    }
    Ok(out)
}

pub struct TypeScores {
    pub by_folder: TypeScore,
    pub by_class: TypeScore,
}

pub fn report(scores: &TypeScores) -> String {
    format!(
        "By folder type (doc_type only):\n\n{}\nWith file name types (the document's class):\n\n{}",
        table(&scores.by_folder),
        table(&scores.by_class)
    )
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>, set: &str) -> Result<TypeScores> {
    let config = crate::config::load(config_dir)?;
    let path = gold_path(config_dir, &config.eval.dir, set)?;
    let gold_rows = read_gold(&path)?;
    let ontology = crate::ontology::load_checked(config_dir, &config.ontology)?.ontology;
    let rules = crate::rules::load(config_dir, &ontology, &config.ontology.document_class)?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let classes = db::eval::document_classes(&pool).await;
    pool.close().await;
    let mut by_class = HashMap::new();
    let mut by_folder = HashMap::new();
    for (doc_path, class, doc_type) in classes? {
        if let Some(folder) = rules.document_class_for(doc_type.as_deref()) {
            by_folder.insert(doc_path.clone(), folder.to_string());
        }
        by_class.insert(doc_path, class);
    }
    let scores = TypeScores {
        by_folder: score(&gold_rows, &by_folder),
        by_class: score(&gold_rows, &by_class),
    };
    if !config.eval.summary.is_empty() {
        let summary = crate::scan::resolve_root(config_dir, &config.eval.summary);
        let body = format!("Against `{}`.\n\n{}", path.display(), report(&scores));
        gold::write_section(&summary, "Document types", &body)?;
        println!("eval types: written to {}", summary.display());
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn score_counts_right_classes_per_class() {
        let gold = pairs(&[
            ("a.pdf", "ex:PAndID"),
            ("b.pdf", "ex:PAndID"),
            ("c.pdf", "ex:Drawing"),
            ("d.pdf", OTHER),
            ("e.pdf", "ex:Drawing"),
        ]);
        let run: HashMap<String, String> = pairs(&[
            ("a.pdf", "ex:PAndID"),
            ("b.pdf", "ex:Drawing"),
            ("c.pdf", "ex:Drawing"),
            ("d.pdf", "ex:Drawing"),
        ])
        .into_iter()
        .collect();
        let found = score(&gold, &run);
        assert_eq!((found.total, found.right, found.missing), (3, 2, 1));
        let drawing = &found.rows[0];
        assert_eq!(
            (
                drawing.class.as_str(),
                drawing.gold,
                drawing.run,
                drawing.right
            ),
            ("ex:Drawing", 1, 2, 1)
        );
        let text = table(&found);
        assert!(text.contains("accuracy: 2 of 3 (66.7%)"), "{text}");
        assert!(
            text.contains("| Drawing | 1 | 2 | 1 | 50.0% | 100.0% |"),
            "{text}"
        );
    }

    #[test]
    fn short_names_drop_the_namespace() {
        assert_eq!(short("urn:x:local#PAndID"), "PAndID");
        assert_eq!(short("http://e.org/a/Drawing"), "Drawing");
    }
}
