use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::text::nfkc_squash;

pub const HASH_FILE: &str = "documents.sha256";

pub fn norm(text: &str) -> String {
    nfkc_squash(text)
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            c => c,
        })
        .collect::<String>()
        .to_lowercase()
}

pub fn grounded(normed_text: &str, quote: &str) -> bool {
    let quote = norm(quote);
    !quote.is_empty() && normed_text.contains(&quote)
}

pub fn components(keys: &[Vec<String>]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..keys.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (i, list) in keys.iter().enumerate() {
        for key in list.iter().filter(|k| !k.is_empty()) {
            match owner.get(key.as_str()) {
                Some(&j) => {
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[a.max(b)] = a.min(b);
                }
                None => {
                    owner.insert(key, i);
                }
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..keys.len() {
        let r = root(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

#[derive(Debug, PartialEq)]
pub struct Agreement<T> {
    pub agreed: Vec<(Vec<T>, Vec<T>)>,
    pub only_a: Vec<Vec<T>>,
    pub only_b: Vec<Vec<T>>,
}

impl<T> Agreement<T> {
    pub fn rate(&self) -> f64 {
        let total = self.agreed.len() + self.only_a.len() + self.only_b.len();
        if total == 0 {
            return 0.0;
        }
        self.agreed.len() as f64 / total as f64
    }
}

pub fn agree<T>(a: Vec<T>, b: Vec<T>, keys: impl Fn(&T) -> Vec<String>) -> Agreement<T> {
    let side_a = a.len();
    let all: Vec<T> = a.into_iter().chain(b).collect();
    let key_lists: Vec<Vec<String>> = all.iter().map(&keys).collect();
    let groups = components(&key_lists);
    let mut slots: Vec<Option<T>> = all.into_iter().map(Some).collect();
    let mut out = Agreement {
        agreed: Vec::new(),
        only_a: Vec::new(),
        only_b: Vec::new(),
    };
    for group in groups {
        let (mut in_a, mut in_b) = (Vec::new(), Vec::new());
        for i in group {
            if let Some(item) = slots[i].take() {
                if i < side_a {
                    in_a.push(item);
                } else {
                    in_b.push(item);
                }
            }
        }
        match (in_a.is_empty(), in_b.is_empty()) {
            (false, false) => out.agreed.push((in_a, in_b)),
            (false, true) => out.only_a.push(in_a),
            (true, false) => out.only_b.push(in_b),
            (true, true) => {}
        }
    }
    out
}

fn rank(seed: &str, id: i64) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(id.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

pub fn split_sample(docs: &[(i64, &str)], seed: &str, per_kind: usize) -> (Vec<i64>, Vec<i64>) {
    let mut kinds: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for (id, kind) in docs {
        kinds.entry(kind).or_default().push(*id);
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for ids in kinds.values_mut() {
        ids.sort_by_key(|id| rank(seed, *id));
        if ids.len() == 1 {
            a.push(ids[0]);
            b.push(ids[0]);
            continue;
        }
        a.extend(ids.iter().step_by(2).take(per_kind));
        b.extend(ids.iter().skip(1).step_by(2).take(per_kind));
    }
    (a, b)
}

pub fn documents_hash(rows: &[(String, String, String)]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
        .map(|(source, path, sha)| format!("{source}\t{path}\t{sha}\n"))
        .collect();
    lines.sort();
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
    }
    crate::hash::hex(&hasher.finalize())
}

pub fn check_set(name: &str) -> Result<()> {
    let fine = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !fine {
        bail!("set name {name:?} may hold only letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

pub fn versions(set_dir: &Path) -> Result<Vec<u32>> {
    if !set_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(set_dir).with_context(|| format!("reading {}", set_dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", set_dir.display()))?;
        if let Some(n) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix('v'))
            .and_then(|n| n.parse::<u32>().ok())
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

pub fn latest(set_dir: &Path) -> Result<Option<PathBuf>> {
    Ok(versions(set_dir)?
        .last()
        .map(|n| set_dir.join(format!("v{n}"))))
}

pub fn version_for(set_dir: &Path, hash: &str) -> Result<(PathBuf, bool)> {
    let numbers = versions(set_dir)?;
    if let Some(n) = numbers.last() {
        let dir = set_dir.join(format!("v{n}"));
        let stored = std::fs::read_to_string(dir.join(HASH_FILE)).unwrap_or_default();
        if stored.trim() == hash {
            return Ok((dir, true));
        }
    }
    let next = numbers.last().map_or(1, |n| n + 1);
    Ok((set_dir.join(format!("v{next}")), false))
}

pub fn set_section(text: &str, title: &str, body: &str) -> String {
    let heading = format!("## {title}\n");
    let section = format!("{heading}\n{}\n", body.trim_end());
    let start = if text.starts_with(&heading) {
        Some(0)
    } else {
        text.find(&format!("\n{heading}")).map(|i| i + 1)
    };
    let Some(start) = start else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&section);
        return out;
    };
    let after = start + heading.len();
    let end = text[after..]
        .find("\n## ")
        .map_or(text.len(), |i| after + i + 1);
    let rest = &text[end..];
    let mut out = format!("{}{section}", &text[..start]);
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(rest);
    }
    out
}

pub fn write_section(path: &Path, title: &str, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    std::fs::write(path, set_section(&text, title, body))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grounding_ignores_case_spaces_and_typographic_quotes() {
        let text = norm("Delivery by  Brix Ltd’s\nyard on 3 May.");
        assert!(grounded(&text, "brix ltd's yard"));
        assert!(grounded(&text, "Delivery by Brix"));
        assert!(!grounded(&text, "Brix Limited"));
        assert!(!grounded(&text, "  "));
    }

    #[test]
    fn components_join_items_that_share_a_key() {
        let keys = vec![
            vec!["a".to_string()],
            vec!["b".to_string()],
            vec!["b".to_string(), "a".to_string()],
            vec!["c".to_string()],
            vec![String::new()],
            vec![String::new()],
        ];
        assert_eq!(
            components(&keys),
            vec![vec![0, 1, 2], vec![3], vec![4], vec![5]]
        );
    }

    #[test]
    fn agreement_splits_shared_and_one_sided_items() {
        let a = vec!["acme", "brix", "delta"];
        let b = vec!["ACME", "eastport"];
        let found = agree(a, b, |s| vec![s.to_lowercase()]);
        assert_eq!(found.agreed, vec![(vec!["acme"], vec!["ACME"])]);
        assert_eq!(found.only_a, vec![vec!["brix"], vec!["delta"]]);
        assert_eq!(found.only_b, vec![vec!["eastport"]]);
        assert!((found.rate() - 0.25).abs() < 1e-9);
        let empty: Agreement<&str> = agree(Vec::new(), Vec::new(), |s| vec![s.to_string()]);
        assert_eq!(empty.rate(), 0.0);
    }

    #[test]
    fn split_sample_alternates_per_kind_and_shares_a_lone_document() {
        let docs = [
            (1, "mail"),
            (2, "mail"),
            (3, "mail"),
            (4, "mail"),
            (5, "order"),
        ];
        let (a, b) = split_sample(&docs, "s", 1);
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert!(a.contains(&5) && b.contains(&5));
        let mail_a: Vec<_> = a.iter().filter(|id| **id != 5).collect();
        let mail_b: Vec<_> = b.iter().filter(|id| **id != 5).collect();
        assert_ne!(mail_a, mail_b);
        assert_eq!(split_sample(&docs, "s", 1), (a, b));
    }

    #[test]
    fn documents_hash_ignores_order() {
        let one = ("s".to_string(), "a".to_string(), "x".to_string());
        let two = ("s".to_string(), "b".to_string(), "y".to_string());
        assert_eq!(
            documents_hash(&[one.clone(), two.clone()]),
            documents_hash(&[two.clone(), one.clone()])
        );
        let changed = ("s".to_string(), "b".to_string(), "z".to_string());
        assert_ne!(
            documents_hash(&[one.clone(), two]),
            documents_hash(&[one, changed])
        );
    }

    #[test]
    fn set_names_are_plain() {
        assert!(check_set("five-projects_1.0").is_ok());
        assert!(check_set("../x").is_err());
        assert!(check_set("a/b").is_err());
        assert!(check_set("").is_err());
    }

    #[test]
    fn a_new_version_only_when_the_documents_change() {
        let dir = std::env::temp_dir().join(format!("zontologize-gold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (first, reused) = version_for(&dir, "h1").unwrap();
        assert_eq!((first.ends_with("v1"), reused), (true, false));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join(HASH_FILE), "h1\n").unwrap();
        assert_eq!(version_for(&dir, "h1").unwrap(), (first.clone(), true));
        let (second, reused) = version_for(&dir, "h2").unwrap();
        assert_eq!((second.ends_with("v2"), reused), (true, false));
        std::fs::create_dir_all(dir.join("v10")).unwrap();
        assert_eq!(latest(&dir).unwrap(), Some(dir.join("v10")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_section_replaces_or_appends() {
        let text = "# Report\n\n## world\n\nold\n\n## questions\n\nq\n";
        let out = set_section(text, "world", "new");
        assert_eq!(out, "# Report\n\n## world\n\nnew\n\n## questions\n\nq\n");
        let out = set_section(&out, "questions", "q2");
        assert_eq!(out, "# Report\n\n## world\n\nnew\n\n## questions\n\nq2\n");
        let out = set_section(&out, "attributes", "a");
        assert!(
            out.ends_with("## questions\n\nq2\n\n## attributes\n\na\n"),
            "{out}"
        );
        assert_eq!(set_section("", "x", "y"), "## x\n\ny\n");
    }
}
