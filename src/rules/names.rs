use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::ontology::Ontology;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameTypeSpec {
    pattern: String,
    class: String,
    #[serde(default)]
    exclude: String,
    #[serde(default)]
    extensions: Vec<String>,
}

pub struct NameType {
    pattern: Regex,
    exclude: Option<Regex>,
    extensions: Vec<String>,
    class: String,
}

fn regex(text: &str) -> Result<Regex, String> {
    RegexBuilder::new(text)
        .case_insensitive(true)
        .build()
        .map_err(|e| e.to_string())
}

pub fn compile(
    specs: &[NameTypeSpec],
    ontology: &Ontology,
    is_document: impl Fn(&str) -> bool,
) -> Result<Vec<NameType>, String> {
    let mut types = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let at = |e: String| format!("name_types[{index}]: {e}");
        let class = ontology.class(&spec.class).map_err(at)?.iri.clone();
        if !is_document(&class) {
            return Err(at(format!("{} is not a document class", spec.class)));
        }
        let exclude = if spec.exclude.is_empty() {
            None
        } else {
            Some(regex(&spec.exclude).map_err(|e| at(format!("exclude: {e}")))?)
        };
        types.push(NameType {
            pattern: regex(&spec.pattern).map_err(|e| at(format!("pattern: {e}")))?,
            exclude,
            extensions: spec
                .extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect(),
            class,
        });
    }
    Ok(types)
}

pub fn class_of<'a>(types: &'a [NameType], path: &str) -> Option<&'a str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();
    types
        .iter()
        .rev()
        .find(|t| {
            t.pattern.is_match(name)
                && !t.exclude.as_ref().is_some_and(|x| x.is_match(name))
                && (t.extensions.is_empty() || t.extensions.contains(&extension))
        })
        .map(|t| t.class.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::tests::ontology;

    const RULES: &str = r#"
[[name_types]]
pattern = 'report|rpt'
class = "ex:Report"
extensions = ["pdf", ".DWG"]

[[name_types]]
pattern = 'final'
class = "ex:Document"
exclude = 'comment|review'
"#;

    fn types(toml_text: &str) -> Result<Vec<NameType>, String> {
        #[derive(Deserialize)]
        struct File {
            name_types: Vec<NameTypeSpec>,
        }
        let file: File = toml::from_str(toml_text).map_err(|e| e.to_string())?;
        let onto = ontology();
        let document = onto.class("ex:Document")?.iri.clone();
        compile(&file.name_types, &onto, |iri| onto.is_a(iri, &document))
    }

    fn iri(name: &str) -> String {
        ontology()
            .class(name)
            .map(|c| c.iri.clone())
            .unwrap_or_default()
    }

    #[test]
    fn the_last_matching_rule_wins() {
        let found = types(RULES).unwrap();
        assert_eq!(
            class_of(&found, "P/Reports/Final Report.pdf"),
            Some(iri("ex:Document").as_str())
        );
    }

    #[test]
    fn exclude_and_extensions_narrow_a_rule() {
        let found = types(RULES).unwrap();
        assert_eq!(
            class_of(&found, "P/final report COMMENTS.pdf"),
            Some(iri("ex:Report").as_str())
        );
        assert_eq!(
            class_of(&found, "P/old.rpt.dwg"),
            Some(iri("ex:Report").as_str())
        );
        assert_eq!(class_of(&found, "P/report list.xlsx"), None);
    }

    #[test]
    fn only_the_file_name_counts() {
        let found = types(RULES).unwrap();
        assert_eq!(class_of(&found, "P/Reports/pump.pdf"), None);
    }

    #[test]
    fn a_class_must_be_a_known_document_class() {
        assert!(types("[[name_types]]\npattern = 'x'\nclass = \"ex:Nope\"\n").is_err());
        let error = types("[[name_types]]\npattern = 'x'\nclass = \"ex:Pump\"\n")
            .err()
            .unwrap_or_default();
        assert!(error.contains("not a document class"), "{error}");
    }
}
