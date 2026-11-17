use regex::Regex;
use serde::Deserialize;

use crate::ontology::Ontology;

const NUMBER_GROUP: &str = "n";
const NUMBER_SLOT: &str = "{n}";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberKeySpec {
    class: String,
    pattern: String,
    key: String,
    #[serde(default)]
    link: Option<LinkSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSpec {
    property: String,
    key: String,
}

#[derive(Debug, Clone)]
pub struct NumberKey {
    pub class: String,
    regex: Regex,
    key: String,
    link: Option<(String, String)>,
}

impl NumberKey {
    pub fn key_of(&self, name: &str) -> Option<String> {
        self.keys_of(name, &self.key)
    }

    pub fn link_of(&self, name: &str) -> Option<(&str, String)> {
        let (property, key) = self.link.as_ref()?;
        Some((property, self.keys_of(name, key)?))
    }

    pub fn link_of_property(&self) -> Option<&str> {
        self.link.as_ref().map(|(property, _)| property.as_str())
    }

    fn keys_of(&self, name: &str, template: &str) -> Option<String> {
        let captures = self.regex.captures(name.trim())?;
        let number = captures.name(NUMBER_GROUP)?.as_str().to_uppercase();
        Some(template.replace(NUMBER_SLOT, &number))
    }
}

pub fn compile(specs: &[NumberKeySpec], ontology: &Ontology) -> Result<Vec<NumberKey>, String> {
    let mut out = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let class = ontology
            .class(&spec.class)
            .map_err(|e| format!("number_keys[{index}].class: {e}"))?
            .iri
            .clone();
        let regex =
            Regex::new(&spec.pattern).map_err(|e| format!("number_keys[{index}].pattern: {e}"))?;
        if !regex.capture_names().flatten().any(|n| n == NUMBER_GROUP) {
            return Err(format!("number_keys[{index}].pattern has no named group n"));
        }
        if !spec.key.contains(NUMBER_SLOT) {
            return Err(format!("number_keys[{index}].key does not contain {{n}}"));
        }
        let link = match &spec.link {
            Some(link) if !link.key.contains(NUMBER_SLOT) => {
                return Err(format!(
                    "number_keys[{index}].link.key does not contain {{n}}"
                ));
            }
            Some(link) => Some((
                ontology
                    .property(&link.property)
                    .map_err(|e| format!("number_keys[{index}].link.property: {e}"))?
                    .iri
                    .clone(),
                link.key.clone(),
            )),
            None => None,
        };
        out.push(NumberKey {
            class,
            regex,
            key: spec.key.clone(),
            link,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, key: &str) -> NumberKey {
        NumberKey {
            class: "urn:test#Project".to_string(),
            regex: Regex::new(pattern).unwrap(),
            key: key.to_string(),
            link: None,
        }
    }

    const P: &str = r"(?i)^(?:P-?)?(?P<n>\d{5})(?:-[a-z0-9]+)*(?:\s|$)";
    const W: &str = r"(?i)^(?:W-?)?(?P<n>\d{8})(?:-[a-z0-9]+)*(?:\s|$)";
    const SP: &str = r"(?i)^SP-?(?P<n>\d{5})(?:-[a-z0-9]+)*(?:\s|$)";

    #[test]
    fn spellings_of_one_project_get_one_key() {
        let p = rule(P, "P-{n}");
        for name in [
            "P-12345",
            "12345",
            "12345 Northwind",
            "P-12345-B",
            "p-12345-b",
            "P-12345 Booster Skid",
        ] {
            assert_eq!(p.key_of(name).as_deref(), Some("P-12345"), "{name}");
        }
        assert_eq!(p.key_of("P-12346").as_deref(), Some("P-12346"));
        assert_eq!(p.key_of("123450"), None);
        assert_eq!(p.key_of("Northwind 12345"), None);
    }

    #[test]
    fn eight_digits_are_work_order_numbers() {
        let w = rule(W, "W-{n}");
        assert_eq!(w.key_of("20210042 Contoso").as_deref(), Some("W-20210042"));
        assert_eq!(w.key_of("w-20210042").as_deref(), Some("W-20210042"));
        assert_eq!(w.key_of("W-2021004"), None);
    }

    #[test]
    fn a_code_after_the_number_keys_the_project() {
        let w = rule(W, "W-{n}");
        for name in [
            "W-20210042-QP",
            "W-20210042-H-1001",
            "w-20210042-pump rev 2",
        ] {
            assert_eq!(w.key_of(name).as_deref(), Some("W-20210042"), "{name}");
        }
        let p = rule(P, "P-{n}");
        assert_eq!(p.key_of("P-12345-C").as_deref(), Some("P-12345"));
        assert_eq!(p.key_of("P-12345-B-FAT").as_deref(), Some("P-12345"));
    }

    #[test]
    fn spare_numbers_get_their_own_key_and_a_link_to_the_project() {
        let mut sp = rule(SP, "SP-{n}");
        sp.link = Some(("urn:test#sparesOf".to_string(), "P-{n}".to_string()));
        let p = rule(P, "P-{n}");
        for name in ["SP-12345", "sp12345", "SP-12345 spares"] {
            assert_eq!(sp.key_of(name).as_deref(), Some("SP-12345"), "{name}");
            assert_eq!(
                sp.link_of(name),
                Some(("urn:test#sparesOf", "P-12345".to_string())),
                "{name}"
            );
            assert_eq!(p.key_of(name), None, "{name}");
        }
        assert_eq!(p.key_of("P-12345").as_deref(), Some("P-12345"));
        assert_eq!(sp.key_of("P-12345"), None);
        assert_eq!(p.link_of("P-12345"), None);
    }
}
