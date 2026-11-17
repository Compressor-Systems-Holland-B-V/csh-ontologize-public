use std::collections::{BTreeMap, HashMap, HashSet};

use super::{Answer, Built, DOC};
use crate::ontology::Ontology;
use crate::resolve::names::Normaliser;
use crate::text::{fold, squash, words};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NotInShortlist,
    QuoteNotFound,
    NameNotFound,
    PropertyNotAllowed,
    UnknownRef,
    DomainRange,
    Language,
    EmptyLabel,
    ClassName,
    RefTaken,
    DisjointClass,
    QuoteWithoutName,
    QuoteWithoutEnds,
    BareTag,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotInShortlist => "class not in the shortlist",
            Reason::QuoteNotFound => "quote not in the chunk",
            Reason::NameNotFound => "name not in the chunk",
            Reason::PropertyNotAllowed => "property not allowed",
            Reason::UnknownRef => "unknown or dropped ref",
            Reason::DomainRange => "domain or range not met",
            Reason::Language => "language not configured",
            Reason::EmptyLabel => "empty label",
            Reason::ClassName => "named after its class",
            Reason::RefTaken => "ref already used for a known thing",
            Reason::DisjointClass => "class disjoint with a known thing of the same name",
            Reason::QuoteWithoutName => "quote does not hold the name",
            Reason::QuoteWithoutEnds => "quote does not name both ends",
            Reason::BareTag => "bare tag, kept as a mention of the document",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Endpoint {
    Doc,
    Known(usize),
    Entity(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub class: String,
    pub key: String,
    pub name: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub from: Endpoint,
    pub property: String,
    pub to: Endpoint,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Other {
    pub from_class: String,
    pub label: String,
    pub lang: String,
    pub to_class: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    pub others: Vec<Other>,
    pub doc_mentions: Vec<(usize, usize)>,
    pub drops: BTreeMap<String, i64>,
}

impl Checked {
    fn drop(&mut self, reason: Reason, about: &str) {
        let about: String = about.chars().take(MAX_DROP_TEXT).collect();
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

pub const MAX_DROP_TEXT: usize = 80;

pub const MARKUP: [char; 4] = ['*', '`', '|', '#'];

pub struct Haystack {
    text: String,
    origin: Vec<usize>,
}

impl Haystack {
    pub fn new(source: &str) -> Haystack {
        let mut text = String::new();
        let mut origin = Vec::new();
        let mut space = false;
        for (index, c) in source.chars().enumerate() {
            if MARKUP.contains(&c) {
                continue;
            }
            if c.is_whitespace() {
                space = !text.is_empty();
                continue;
            }
            if space {
                text.push(' ');
                origin.push(index);
                space = false;
            }
            for lower in c.to_lowercase() {
                text.push(lower);
                origin.push(index);
            }
        }
        Haystack { text, origin }
    }

    pub fn find(&self, quote: &str) -> Option<(usize, usize)> {
        let needle = Haystack::new(quote).text;
        if needle.is_empty() {
            return None;
        }
        let byte = self.text.find(&needle)?;
        let first = self.text[..byte].chars().count();
        let last = first + needle.chars().count() - 1;
        Some((self.origin[first], self.origin[last] + 1))
    }
}

fn named_after_class(ontology: &Ontology, class: &str, name: &str) -> bool {
    let Some(class) = ontology.classes.get(class) else {
        return false;
    };
    let name = fold(name);
    class.labels.values().any(|l| fold(l) == name)
        || class.alt_labels.values().flatten().any(|l| fold(l) == name)
}

fn exact<'a>(allowed: &'a [String], value: &str) -> Option<&'a String> {
    allowed.iter().find(|a| *a == value.trim())
}

fn language<'a>(allowed: &'a [String], value: &str) -> Option<&'a String> {
    allowed
        .iter()
        .find(|a| a.eq_ignore_ascii_case(value.trim()))
}

fn names(normaliser: &Normaliser, quote: &str, name: &str) -> bool {
    let needle = Haystack::new(name).text;
    if !needle.is_empty() && Haystack::new(quote).text.contains(&needle) {
        return true;
    }
    let key = normaliser.key(name);
    !key.is_empty() && format!(" {} ", normaliser.key(quote)).contains(&format!(" {key} "))
}

fn bare_tag(name: &str) -> bool {
    let longest_letters = name
        .split(|c: char| !c.is_alphabetic())
        .map(|run| run.chars().count())
        .max()
        .unwrap_or(0);
    name.chars().any(|c| c.is_ascii_digit()) && longest_letters < 3
}

fn line_of(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let end = end.min(chars.len());
    let first = chars[..start.min(end)]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |i| i + 1);
    let last = chars[end..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |i| end + i);
    chars[first..last].iter().collect()
}

fn described(ontology: &Ontology, class: &str, line: &str, name: &str, min_words: usize) -> bool {
    let folded = fold(line);
    let labelled = ontology.classes.get(class).is_some_and(|c| {
        c.labels
            .values()
            .chain(c.alt_labels.values().flatten())
            .any(|l| !l.trim().is_empty() && folded.contains(&fold(l)))
    });
    labelled || words(line).saturating_sub(words(name)) >= min_words
}

pub fn entity_key(normaliser: &Normaliser, name: &str) -> String {
    match normaliser.key(name) {
        key if key.is_empty() => fold(name),
        key => key,
    }
}

pub fn check(
    answer: Answer,
    built: &Built,
    text: &str,
    ontology: &Ontology,
    languages: &[String],
    normaliser: &Normaliser,
    min_tag_words: usize,
) -> Checked {
    let haystack = Haystack::new(text);
    let mut checked = Checked::default();
    let mut refs: HashMap<String, Endpoint> = HashMap::new();
    if built.doc_class.is_some() {
        refs.insert(DOC.to_string(), Endpoint::Doc);
    }
    for i in 0..built.known.len() {
        refs.insert(format!("r{}", i + 1), Endpoint::Known(i));
    }
    let reserved: HashSet<String> = refs.keys().cloned().collect();
    let mut by_key: HashMap<(String, String), usize> = HashMap::new();
    for entity in answer.entities {
        let reference = entity.reference.trim().to_lowercase();
        if reserved.contains(&reference) {
            refs.remove(&reference);
            checked.drop(Reason::RefTaken, &reference);
            continue;
        }
        let Some(class) = exact(&built.shortlist, &entity.class) else {
            checked.drop(Reason::NotInShortlist, &entity.class);
            continue;
        };
        if named_after_class(ontology, class, &entity.name) {
            checked.drop(Reason::ClassName, class);
            continue;
        }
        let name = fold(&entity.name);
        if built
            .known
            .iter()
            .any(|k| fold(&k.name) == name && ontology.disjoint(class, &k.class))
        {
            checked.drop(Reason::DisjointClass, class);
            continue;
        }
        if haystack.find(&entity.name).is_none() {
            checked.drop(Reason::NameNotFound, class);
            continue;
        }
        let Some((start, end)) = haystack.find(&entity.evidence) else {
            checked.drop(Reason::QuoteNotFound, class);
            continue;
        };
        if !names(normaliser, &entity.evidence, &entity.name) {
            checked.drop(Reason::QuoteWithoutName, class);
            continue;
        }
        if bare_tag(&entity.name)
            && !described(
                ontology,
                class,
                &line_of(text, start, end),
                &entity.name,
                min_tag_words,
            )
        {
            checked.doc_mentions.push((start, end));
            checked.drop(Reason::BareTag, class);
            continue;
        }
        let key = entity_key(normaliser, &entity.name);
        let index = *by_key
            .entry((class.clone(), key.clone()))
            .or_insert_with(|| {
                checked.entities.push(Entity {
                    class: class.clone(),
                    key,
                    name: entity.name.trim().to_string(),
                    start,
                    end,
                });
                checked.entities.len() - 1
            });
        refs.insert(reference, Endpoint::Entity(index));
    }
    let class_of = |checked: &Checked, end: Endpoint| -> String {
        match end {
            Endpoint::Doc => built.doc_class.clone().unwrap_or_default(),
            Endpoint::Known(i) => built.known[i].class.clone(),
            Endpoint::Entity(i) => checked.entities[i].class.clone(),
        }
    };
    let name_of = |checked: &Checked, end: Endpoint| -> Option<String> {
        match end {
            Endpoint::Doc => None,
            Endpoint::Known(i) => Some(built.known[i].name.clone()),
            Endpoint::Entity(i) => Some(checked.entities[i].name.clone()),
        }
    };
    let endpoint = |name: &str| refs.get(&name.trim().to_lowercase()).copied();
    for relation in answer.relations {
        let Some(property) = exact(&built.properties, &relation.property) else {
            checked.drop(Reason::PropertyNotAllowed, &relation.property);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&relation.from), endpoint(&relation.to)) else {
            checked.drop(Reason::UnknownRef, property);
            continue;
        };
        let Some(definition) = ontology.properties.get(property) else {
            checked.drop(Reason::PropertyNotAllowed, property);
            continue;
        };
        if !ontology.fits(&class_of(&checked, from), &definition.domain)
            || !ontology.fits(&class_of(&checked, to), &definition.range)
        {
            checked.drop(Reason::DomainRange, property);
            continue;
        }
        let Some((start, end)) = haystack.find(&relation.evidence) else {
            checked.drop(Reason::QuoteNotFound, property);
            continue;
        };
        if [from, to].into_iter().any(|end| {
            name_of(&checked, end).is_some_and(|name| !names(normaliser, &relation.evidence, &name))
        }) {
            checked.drop(Reason::QuoteWithoutEnds, property);
            continue;
        }
        let relation = Relation {
            from,
            property: property.clone(),
            to,
            start,
            end,
        };
        if !checked.relations.iter().any(|r| {
            r.from == relation.from && r.property == relation.property && r.to == relation.to
        }) {
            checked.relations.push(relation);
        }
    }
    for other in answer.other_relations {
        let label = squash(&other.label);
        if label.is_empty() {
            checked.drop(Reason::EmptyLabel, "");
            continue;
        }
        let Some(lang) = language(languages, &other.lang) else {
            checked.drop(Reason::Language, &other.lang);
            continue;
        };
        let (Some(from), Some(to)) = (endpoint(&other.from), endpoint(&other.to)) else {
            checked.drop(Reason::UnknownRef, &label);
            continue;
        };
        let Some((start, end)) = haystack.find(&other.evidence) else {
            checked.drop(Reason::QuoteNotFound, &label);
            continue;
        };
        checked.others.push(Other {
            from_class: class_of(&checked, from),
            label,
            lang: lang.clone(),
            to_class: class_of(&checked, to),
            start,
            end,
        });
    }
    checked
}

#[cfg(test)]
mod tests {
    use super::super::Known;
    use super::super::tests::{TTL, built, input, iri, languages, ontology, ontology_from};
    use super::*;

    #[test]
    fn drop_reason_texts_are_stored_unchanged() {
        let texts: Vec<&str> = [
            Reason::NotInShortlist,
            Reason::QuoteNotFound,
            Reason::NameNotFound,
            Reason::PropertyNotAllowed,
            Reason::UnknownRef,
            Reason::DomainRange,
            Reason::Language,
            Reason::EmptyLabel,
            Reason::ClassName,
            Reason::RefTaken,
            Reason::DisjointClass,
            Reason::QuoteWithoutName,
            Reason::QuoteWithoutEnds,
            Reason::BareTag,
        ]
        .iter()
        .map(|r| r.as_str())
        .collect();
        assert_eq!(
            texts,
            [
                "class not in the shortlist",
                "quote not in the chunk",
                "name not in the chunk",
                "property not allowed",
                "unknown or dropped ref",
                "domain or range not met",
                "language not configured",
                "empty label",
                "named after its class",
                "ref already used for a known thing",
                "class disjoint with a known thing of the same name",
                "quote does not hold the name",
                "quote does not name both ends",
                "bare tag, kept as a mention of the document"
            ]
        );
        let mut checked = Checked::default();
        checked.drop(Reason::Language, "de");
        assert!(checked.drops.contains_key("language not configured|de"));
    }

    #[test]
    fn drop_keys_cut_long_model_text() {
        let mut checked = Checked::default();
        checked.drop(Reason::UnknownRef, &"x".repeat(500));
        let key = checked.drops.keys().next().unwrap();
        assert_eq!(
            key,
            &format!("{}|{}", Reason::UnknownRef, "x".repeat(MAX_DROP_TEXT))
        );
    }

    fn normaliser() -> Normaliser {
        Normaliser::new(&["B.V.".to_string(), "BV".to_string()])
    }

    fn answer(json: serde_json::Value) -> Answer {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn haystack_maps_quotes_back_to_char_offsets() {
        let text = "Één  booster\nPump at the site; booster pump again.";
        let haystack = Haystack::new(text);
        let (start, end) = haystack.find("BOOSTER pump").unwrap();
        let found: String = text.chars().skip(start).take(end - start).collect();
        assert_eq!(found, "booster\nPump");
        assert_eq!(start, 5);
        assert!(haystack.find("rotor").is_none());
        let markdown = "**Subject**: Pump  | **Site** |";
        let (start, end) = Haystack::new(markdown).find("Subject: Pump Site").unwrap();
        let found: String = markdown.chars().skip(start).take(end - start).collect();
        assert_eq!(found, "Subject**: Pump  | **Site");
        assert!(Haystack::new(markdown).find("**").is_none());
        assert!(haystack.find("  ").is_none());
    }

    #[test]
    fn entities_are_checked_against_shortlist_and_text() {
        let onto = ontology();
        let built = built(&onto, &input(&[("Pump", 1)]));
        let text = "The booster pump at the north site, booster pump B.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "Booster Pump", "evidence": "the booster pump"},
                    {"ref": "e7", "class": iri("pump"), "name": "booster pump", "evidence": "the booster pump"},
                    {"ref": "e2", "class": iri("Site"), "name": "north site", "evidence": "north site"},
                    {"ref": "e3", "class": iri("Pump"), "name": "rotor", "evidence": "booster"},
                    {"ref": "e4", "class": iri("Pump"), "name": "booster pump", "evidence": "a pump not here"},
                    {"ref": "e5", "class": iri("Pump"), "name": "booster  pump", "evidence": "booster pump B"},
                    {"ref": "e6", "class": iri("Pump"), "name": " PUMP ", "evidence": "booster pump B"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert_eq!(checked.entities.len(), 1);
        let entity = &checked.entities[0];
        assert_eq!(entity.class, iri("Pump"));
        assert_eq!(entity.key, "booster pump");
        assert_eq!((entity.start, entity.end), (0, 16));
        assert_eq!(checked.drops.values().sum::<i64>(), 5);
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::ClassName, iri("Pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NotInShortlist, iri("Site"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NotInShortlist, iri("pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::NameNotFound, iri("Pump"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteNotFound, iri("Pump"))],
            1
        );
    }

    #[test]
    fn relations_need_allowed_properties_refs_and_domain_and_range() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 1), ("Site", 1)]);
        chunk.doc_class = Some(iri("Report"));
        chunk.known = vec![Known {
            entity_id: 9,
            class: iri("Site"),
            name: "north".to_string(),
        }];
        let built = built(&onto, &chunk);
        let text = "The booster pump is installed at the north site.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"},
                    {"ref": "e2", "class": iri("Pump"), "name": "rotor", "evidence": "rotor"}
                ],
                "relations": [
                    {"from": "E1", "property": iri("installedAt"), "to": "r1", "evidence": "booster pump is installed at the north"},
                    {"from": "e1", "property": iri("installedat"), "to": "r1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "installed at the north"},
                    {"from": "r1", "property": iri("installedAt"), "to": "e1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "e2", "evidence": "installed at"},
                    {"from": "doc", "property": iri("mentions"), "to": "e1", "evidence": "booster pump"},
                    {"from": "e1", "property": iri("owns"), "to": "r1", "evidence": "installed at"},
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "nowhere"}
                ],
                "other_relations": [
                    {"from": "e1", "label": " placed   near ", "lang": "EN", "to": "r1", "evidence": "at the north"},
                    {"from": "e1", "label": "near", "lang": "de", "to": "r1", "evidence": "at the north"},
                    {"from": "e2", "label": "near", "lang": "en", "to": "r1", "evidence": "at the north"}
                ]
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert_eq!(
            checked.relations,
            [
                Relation {
                    from: Endpoint::Entity(0),
                    property: iri("installedAt"),
                    to: Endpoint::Known(0),
                    start: 4,
                    end: 42,
                },
                Relation {
                    from: Endpoint::Doc,
                    property: iri("mentions"),
                    to: Endpoint::Entity(0),
                    start: 4,
                    end: 16,
                }
            ]
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::DomainRange, iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::UnknownRef, iri("installedAt"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::PropertyNotAllowed, iri("owns"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::PropertyNotAllowed, iri("installedat"))],
            1
        );
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteNotFound, iri("installedAt"))],
            1
        );
        assert_eq!(checked.others.len(), 1);
        assert_eq!(checked.others[0].label, "placed near");
        assert_eq!(checked.others[0].lang, "en");
        assert_eq!(checked.others[0].to_class, iri("Site"));
        assert_eq!(checked.drops[&format!("{}|de", Reason::Language)], 1);
        assert_eq!(checked.drops[&format!("{}|near", Reason::UnknownRef)], 1);
    }

    #[test]
    fn an_entity_cannot_take_a_known_ref() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 1), ("Site", 1)]);
        chunk.doc_class = Some(iri("Report"));
        chunk.known = vec![Known {
            entity_id: 9,
            class: iri("Site"),
            name: "north".to_string(),
        }];
        let built = built(&onto, &chunk);
        let text = "The booster pump is installed at the north site.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"},
                    {"ref": "R1", "class": iri("Site"), "name": "north site", "evidence": "north site"},
                    {"ref": "doc", "class": iri("Pump"), "name": "booster pump", "evidence": "booster pump"}
                ],
                "relations": [
                    {"from": "e1", "property": iri("installedAt"), "to": "r1", "evidence": "installed at"},
                    {"from": "doc", "property": iri("mentions"), "to": "e1", "evidence": "booster pump"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert_eq!(checked.entities.len(), 1);
        assert!(checked.relations.is_empty(), "{:?}", checked.relations);
        assert_eq!(checked.drops[&format!("{}|r1", Reason::RefTaken)], 1);
        assert_eq!(checked.drops[&format!("{}|doc", Reason::RefTaken)], 1);
    }

    #[test]
    fn empty_answer_is_valid() {
        let onto = ontology();
        let built = built(&onto, &input(&[]));
        let checked = check(
            answer(serde_json::json!({"entities": [], "other_relations": []})),
            &built,
            "text",
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert!(checked.entities.is_empty() && checked.drops.is_empty());
    }

    #[test]
    fn legal_form_variants_land_on_one_normalised_key() {
        let onto = ontology();
        let built = built(&onto, &input(&[("Site", 1)]));
        let text = "Northwind Pumps B.V. and Northwind Pumps BV, also B.V. alone.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Site"), "name": "Northwind Pumps B.V.", "evidence": "Northwind Pumps B.V."},
                    {"ref": "e2", "class": iri("Site"), "name": "Northwind Pumps BV", "evidence": "Northwind Pumps BV"},
                    {"ref": "e3", "class": iri("Site"), "name": "B.V.", "evidence": "B.V. alone"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        let keys: Vec<&str> = checked.entities.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["northwind pumps", "b.v."]);
    }

    #[test]
    fn known_name_in_disjoint_class_is_dropped() {
        let onto = ontology_from(&format!("{TTL} ex:Company owl:disjointWith ex:Site ."));
        let mut chunk = input(&[("Company", 1), ("Asset", 1), ("Pump", 1)]);
        chunk.known = vec![
            Known {
                entity_id: 5,
                class: iri("Site"),
                name: "North Site".to_string(),
            },
            Known {
                entity_id: 6,
                class: iri("Pump"),
                name: "booster pump".to_string(),
            },
        ];
        let built = built(&onto, &chunk);
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Company"), "name": "north site", "evidence": "north site"},
                    {"ref": "e2", "class": iri("Asset"), "name": "Booster Pump", "evidence": "booster pump"}
                ],
                "other_relations": []
            })),
            &built,
            &chunk.text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        let names: Vec<&str> = checked.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Booster Pump"]);
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::DisjointClass, iri("Company"))],
            1
        );
    }

    #[test]
    fn a_mention_quote_must_hold_the_name() {
        let onto = ontology();
        let built = built(&onto, &input(&[("Pump", 1)]));
        let text = "The booster pump at the north site.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "at the north site"},
                    {"ref": "e2", "class": iri("Pump"), "name": "Booster  PUMP", "evidence": "the booster pump at"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert_eq!(checked.entities.len(), 1);
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteWithoutName, iri("Pump"))],
            1
        );
    }

    #[test]
    fn a_relation_quote_must_name_both_ends() {
        let onto = ontology();
        let mut chunk = input(&[("Pump", 1), ("Company", 1)]);
        chunk.known = vec![Known {
            entity_id: 9,
            class: iri("Company"),
            name: "Acme B.V.".to_string(),
        }];
        let built = built(&onto, &chunk);
        let text = "Acme owns the booster pump. The booster pump is new.";
        let checked = check(
            answer(serde_json::json!({
                "entities": [
                    {"ref": "e1", "class": iri("Pump"), "name": "booster pump", "evidence": "the booster pump"}
                ],
                "relations": [
                    {"from": "r1", "property": iri("owns"), "to": "e1", "evidence": "The booster pump is new"},
                    {"from": "r1", "property": iri("owns"), "to": "e1", "evidence": "acme owns the booster pump"}
                ],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        assert_eq!(checked.relations.len(), 1);
        assert_eq!(checked.relations[0].from, Endpoint::Known(0));
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::QuoteWithoutEnds, iri("owns"))],
            1
        );
    }

    #[test]
    fn a_bare_tag_needs_a_description_or_its_class_on_the_line() {
        let onto = ontology();
        let built = built(&onto, &input(&[("Pump", 1)]));
        let text = "| P-12 | 10K4 |\n| 3 | P-14 | lube oil transfer |\nP-15 pump\nboosters X-7";
        let tag = |name: &str| {
            serde_json::json!(
                {"ref": name, "class": iri("Pump"), "name": name, "evidence": name}
            )
        };
        let checked = check(
            answer(serde_json::json!({
                "entities": [tag("P-12"), tag("10K4"), tag("P-14"), tag("P-15"), tag("X-7")],
                "other_relations": []
            })),
            &built,
            text,
            &onto,
            &languages(),
            &normaliser(),
            2,
        );
        let names: Vec<&str> = checked.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["P-14", "P-15"]);
        assert_eq!(checked.doc_mentions, [(2, 6), (9, 13), (68, 71)]);
        assert_eq!(
            checked.drops[&format!("{}|{}", Reason::BareTag, iri("Pump"))],
            3
        );
        assert!(!bare_tag("Booster pump 2"));
        assert!(bare_tag("HI-39400"));
        assert!(!bare_tag("KSB"));
    }
}
