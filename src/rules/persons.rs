use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use super::gazetteer::Entry;
use super::{EntityHit, TextEnd, TextRelation};
use crate::ontology::Ontology;

pub const HEADER_METHOD: &str = "mail";
pub const ADDRESS_METHOD: &str = "address";
pub const SIGNATURE_METHOD: &str = "signature";
const MAX_NAME_WORDS: usize = 5;
const MAX_NAME_CHARS: usize = 60;
const SIGNATURE_LOOKAHEAD: usize = 3;
const MIN_ID_CHARS: usize = 12;
const LEGAL_FORMS: [&str; 30] = [
    "bv",
    "nv",
    "ltd",
    "limited",
    "gmbh",
    "inc",
    "llc",
    "plc",
    "ag",
    "co",
    "corp",
    "corporation",
    "srl",
    "spa",
    "sa",
    "as",
    "ab",
    "oy",
    "kft",
    "pte",
    "bvba",
    "holding",
    "holdings",
    "group",
    "company",
    "industries",
    "international",
    "services",
    "systems",
    "engineering",
];
const ROLE_WORDS: [&str; 35] = [
    "manager",
    "engineer",
    "developer",
    "contributor",
    "controller",
    "director",
    "coordinator",
    "officer",
    "inspector",
    "supervisor",
    "operator",
    "technician",
    "assistant",
    "administrator",
    "department",
    "team",
    "vendor",
    "customer",
    "supplier",
    "client",
    "buyer",
    "package",
    "project",
    "sales",
    "purchase",
    "purchasing",
    "service",
    "document",
    "commercial",
    "initial",
    "original",
    "author",
    "contractor",
    "licensor",
    "licensee",
];
const PARTICLES: [&str; 12] = [
    "van", "de", "der", "den", "ter", "ten", "het", "von", "du", "da", "di", "le",
];

static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[ \t>]*\**(from|to|cc|van|aan)\**[ \t]*:[ \t]*\**[ \t]*(.+?)[ \t]*$").unwrap()
});
static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[ \t]*\**(subject|onderwerp|from|van|sent|date|datum|to|aan)\**[ \t]*:")
        .unwrap()
});
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+'-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});
static ID_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[ \t>]*\**(message-id|references|in-reply-to)\**[ \t]*:").unwrap()
});
static EXCHANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)/O=([^/]+)/.*?/CN=([^/<>]+?)\s*>?\s*$").unwrap());

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonsSpec {
    class: String,
    sent_by: String,
    sent_to: String,
    works_for: String,
    #[serde(default)]
    closings: Vec<String>,
    #[serde(default)]
    role_addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Org {
    pub class: String,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct Persons {
    pub class: String,
    sent_by: String,
    sent_to: String,
    works_for: String,
    closings: Vec<String>,
    role_addresses: Vec<String>,
    domains: HashMap<String, Org>,
    names: HashMap<String, Org>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub entities: Vec<EntityHit>,
    pub relations: Vec<TextRelation>,
}

struct Person {
    key: String,
    name: String,
    org: Option<Org>,
}

pub fn compile(
    spec: &PersonsSpec,
    ontology: &Ontology,
    entries: &[Entry],
) -> Result<Persons, String> {
    let class = ontology
        .class(&spec.class)
        .map_err(|e| format!("persons.class: {e}"))?;
    let property = |name: &str, value: &str| -> Result<String, String> {
        Ok(ontology
            .property(value)
            .map_err(|e| format!("persons.{name}: {e}"))?
            .iri
            .clone())
    };
    let mut domains = HashMap::new();
    let mut names = HashMap::new();
    for entry in entries.iter().filter(|e| e.class != class.iri) {
        let org = Org {
            class: entry.class.clone(),
            key: entry.key.clone(),
        };
        for text in std::iter::once(&entry.name).chain(&entry.aliases) {
            let lower = text.trim().to_lowercase();
            match domain_of(&lower) {
                Some(domain) => domains.insert(domain, org.clone()),
                None => names.insert(lower, org.clone()),
            };
        }
        names.insert(entry.key.to_lowercase(), org);
    }
    Ok(Persons {
        class: class.iri.clone(),
        sent_by: property("sent_by", &spec.sent_by)?,
        sent_to: property("sent_to", &spec.sent_to)?,
        works_for: property("works_for", &spec.works_for)?,
        closings: spec.closings.iter().map(|c| c.to_lowercase()).collect(),
        role_addresses: spec
            .role_addresses
            .iter()
            .map(|r| r.to_lowercase())
            .collect(),
        domains,
        names,
    })
}

fn domain_of(text: &str) -> Option<String> {
    let text = text.strip_prefix("www.").unwrap_or(text);
    let valid = text.contains('.')
        && !text.contains(' ')
        && !text.contains('@')
        && text
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    valid.then(|| text.to_string())
}

impl Persons {
    pub fn find(&self, text: &str) -> Found {
        let mut found = Found::default();
        let mut taken: Vec<(usize, usize)> = Vec::new();
        let top = starts_with_headers(text);
        let mut offset = 0;
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut starts = Vec::with_capacity(lines.len());
        for line in &lines {
            starts.push(offset);
            offset += line.len();
        }
        let mut in_top = top;
        for (n, line) in lines.iter().enumerate() {
            let body = line.trim_end_matches(['\n', '\r']);
            if !body.trim().is_empty() && !SUBJECT.is_match(body) && !HEADER.is_match(body) {
                in_top = false;
            }
            if ID_HEADER.is_match(body) {
                taken.push((starts[n], starts[n] + body.len()));
                continue;
            }
            if let Some(captures) = HEADER.captures(body) {
                let field = captures[1].to_lowercase();
                let value = captures.get(2).map_or(0..0, |m| m.range());
                let property = match field.as_str() {
                    "from" | "van" => &self.sent_by,
                    _ => &self.sent_to,
                };
                for (start, end) in recipients(body, value) {
                    let Some(person) = self.person_of(&body[start..end]) else {
                        continue;
                    };
                    let span = (starts[n] + start, starts[n] + end);
                    taken.push(span);
                    let index = self.push(&mut found, person, span, HEADER_METHOD, 1.0);
                    if in_top {
                        found.relations.push(TextRelation {
                            from: TextEnd::Document,
                            property: property.clone(),
                            to: TextEnd::Entity(index),
                        });
                    }
                }
                continue;
            }
            if self.is_closing(body) {
                self.signature(&lines, &starts, n, &mut found);
            }
        }
        for m in ADDRESS.find_iter(text) {
            if taken.iter().any(|&(s, e)| m.start() >= s && m.end() <= e) {
                continue;
            }
            let display = display_before(&text[..m.start()]);
            let Some(person) = self.person_at(m.as_str(), display) else {
                continue;
            };
            self.push(
                &mut found,
                person,
                (m.start(), m.end()),
                ADDRESS_METHOD,
                0.9,
            );
        }
        drop_shared(found)
    }

    pub fn works_for_key(&self, key: &str) -> Option<Org> {
        let (_, domain) = key.rsplit_once('@')?;
        self.domains.get(&domain.to_lowercase()).cloned()
    }

    pub fn works_for(&self) -> &str {
        &self.works_for
    }

    fn push(
        &self,
        found: &mut Found,
        person: Person,
        span: (usize, usize),
        method: &'static str,
        confidence: f64,
    ) -> usize {
        let index = found.entities.len();
        found.entities.push(EntityHit {
            class: self.class.clone(),
            key: person.key,
            name: person.name,
            start: span.0,
            end: span.1,
            method,
            confidence,
            props: BTreeMap::new(),
        });
        if let Some(org) = person.org {
            found.relations.push(TextRelation {
                from: TextEnd::Entity(index),
                property: self.works_for.clone(),
                to: TextEnd::Known(org.class, org.key),
            });
        }
        index
    }

    fn is_closing(&self, line: &str) -> bool {
        let lower = line.trim().trim_end_matches([',', '.', '!']).to_lowercase();
        lower.len() <= 40 && self.closings.contains(&lower)
    }

    fn is_organisation(&self, name: &str) -> bool {
        self.names.contains_key(&name.to_lowercase())
    }

    fn signature(&self, lines: &[&str], starts: &[usize], closing: usize, found: &mut Found) {
        let following = lines.iter().enumerate().skip(closing + 1);
        let candidates = following
            .filter(|(_, l)| !l.trim().is_empty())
            .take(SIGNATURE_LOOKAHEAD);
        for (n, line) in candidates {
            let body = line.trim_end_matches(['\n', '\r']);
            let trimmed = body.trim().trim_matches('*').trim();
            if let Some(name) = person_name(trimmed).filter(|n| !self.is_organisation(n)) {
                let start = starts[n] + body.find(trimmed).unwrap_or(0);
                let person = Person {
                    key: name_key(&name),
                    name,
                    org: None,
                };
                self.push(
                    found,
                    person,
                    (start, start + trimmed.len()),
                    SIGNATURE_METHOD,
                    0.7,
                );
                return;
            }
        }
    }

    fn person_of(&self, raw: &str) -> Option<Person> {
        let text = unescape(raw);
        let text = text.trim().trim_matches(['"', '\'', ' ']);
        if let Some(captures) = EXCHANGE.captures(text) {
            let org = self.names.get(&captures[1].trim().to_lowercase()).cloned();
            let name = title_case(captures[2].trim());
            return person_name(&name).map(|name| Person {
                key: name_key(&name),
                name,
                org,
            });
        }
        let address = ADDRESS.find(text).map(|m| m.as_str().to_string());
        let display = match text.find('<') {
            Some(i) => text[..i].trim().trim_matches(['"', '\'', ' ']).to_string(),
            None if address.is_some() => String::new(),
            None => text.to_string(),
        };
        let display = turn_around(&display);
        match address {
            Some(address) => self.person_at(&address, person_name(&display)),
            None => person_name(&display).map(|name| Person {
                key: name_key(&name),
                name,
                org: None,
            }),
        }
    }

    fn person_at(&self, address: &str, name: Option<String>) -> Option<Person> {
        let address = address.trim_matches(['\'', '.']).to_lowercase();
        let (local, domain) = address.rsplit_once('@')?;
        if self.is_role(local) || is_message_id(local) {
            return None;
        }
        let name = name.map(|n| in_address_order(&n, local));
        let name = name.or_else(|| name_from_local(local))?;
        Some(Person {
            org: self.domains.get(domain).cloned(),
            key: address,
            name,
        })
    }

    fn is_role(&self, local: &str) -> bool {
        let head: String = local
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        self.role_addresses.iter().any(|r| r == local || *r == head)
    }
}

/// "Merrin Gordon <gordon.merrin@...>": a display name that is the address's first.last turned
/// around takes the address's order.
fn in_address_order(name: &str, local: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match (words.as_slice(), local.split_once('.')) {
        ([a, b], Some((first, last)))
            if a.eq_ignore_ascii_case(last) && b.eq_ignore_ascii_case(first) =>
        {
            format!("{b} {a}")
        }
        _ => name.to_string(),
    }
}

fn is_message_id(local: &str) -> bool {
    local.len() >= MIN_ID_CHARS
        && local.chars().any(|c| c.is_ascii_digit())
        && local
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | '-' | '$' | '_'))
}

fn display_before(before: &str) -> Option<String> {
    let line = before.rsplit('\n').next().unwrap_or(before);
    let line = line.trim_end_matches([' ', '\\']).strip_suffix('<')?;
    let line = line.trim_end_matches([' ', '\\']);
    let start = line
        .rfind([':', ';', '(', '\t', '>', '|'])
        .map_or(0, |i| i + 1);
    let display = line[start..].trim();
    let display = match display.strip_suffix('"') {
        Some(inner) => inner.rsplit('"').next().unwrap_or(inner),
        None => display,
    };
    let display = display.trim().trim_matches(['"', '\'', ' ', '*']);
    person_name(&turn_around(display))
}

fn drop_shared(found: Found) -> Found {
    let mut names: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entity in found.entities.iter().filter(|e| e.key.contains('@')) {
        names.entry(&entity.key).or_default().push(&entity.name);
    }
    let shared: Vec<String> = names
        .into_iter()
        .filter(|(_, names)| {
            let parsed: Vec<_> = names
                .iter()
                .filter_map(|n| crate::resolve::names::person_name(&name_key(n)))
                .collect();
            parsed
                .iter()
                .any(|p| !crate::resolve::names::same_person(&parsed[0], p))
        })
        .map(|(key, _)| key.to_string())
        .collect();
    if shared.is_empty() {
        return found;
    }
    let mut index = HashMap::new();
    let mut kept = Found::default();
    for (i, entity) in found.entities.into_iter().enumerate() {
        if !shared.contains(&entity.key) {
            index.insert(i, kept.entities.len());
            kept.entities.push(entity);
        }
    }
    let end = |e: TextEnd| match e {
        TextEnd::Entity(i) => index.get(&i).map(|&j| TextEnd::Entity(j)),
        other => Some(other),
    };
    for relation in found.relations {
        if let (Some(from), Some(to)) = (end(relation.from), end(relation.to)) {
            kept.relations.push(TextRelation {
                from,
                property: relation.property,
                to,
            });
        }
    }
    kept
}

fn starts_with_headers(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| SUBJECT.is_match(l))
}

fn recipients(line: &str, value: std::ops::Range<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = value.start;
    let mut quoted = false;
    let mut angled = false;
    let bytes = line.as_bytes();
    for i in value.clone() {
        match bytes[i] {
            b'"' => quoted = !quoted,
            b'<' => angled = true,
            b'>' => angled = false,
            b';' if !quoted && !angled => {
                out.push((start, i));
                start = i + 1;
            }
            b',' if !quoted && !angled && line[start..i].contains('@') => {
                out.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push((start, value.end));
    out.into_iter()
        .map(|(s, e)| trim_range(line, s, e))
        .filter(|(s, e)| s < e)
        .collect()
}

fn trim_range(line: &str, start: usize, end: usize) -> (usize, usize) {
    let part = &line[start..end];
    let lead = part.len() - part.trim_start().len();
    let trail = part.len() - part.trim_end().len();
    (start + lead, end - trail)
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn turn_around(name: &str) -> String {
    match name.split_once(',') {
        Some((last, first))
            if !first.trim().is_empty()
                && !first.contains(',')
                && last.split_whitespace().count() <= 3 =>
        {
            format!("{} {}", first.trim(), last.trim())
        }
        _ => name.to_string(),
    }
}

pub fn person_name(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_NAME_CHARS {
        return None;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 2 || words.len() > MAX_NAME_WORDS {
        return None;
    }
    let mut capitals = 0;
    for word in &words {
        let letters = word
            .chars()
            .all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | '.'));
        if !letters {
            return None;
        }
        let bare: String = word
            .chars()
            .filter(|c| c.is_alphabetic())
            .flat_map(char::to_lowercase)
            .collect();
        if LEGAL_FORMS.contains(&bare.as_str()) || ROLE_WORDS.contains(&bare.as_str()) {
            return None;
        }
        if PARTICLES.contains(&word.to_lowercase().as_str()) {
            continue;
        }
        if !word.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        capitals += 1;
    }
    let all_caps = text
        .chars()
        .filter(|c| c.is_alphabetic())
        .all(char::is_uppercase);
    (capitals >= 2 && !all_caps).then(|| words.join(" "))
}

pub fn name_key(name: &str) -> String {
    crate::resolve::names::plain(name)
}

fn name_from_local(local: &str) -> Option<String> {
    let parts: Vec<&str> = local.split(['.', '_']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || parts.iter().any(|p| !p.chars().all(char::is_alphabetic)) {
        return None;
    }
    if parts.iter().all(|p| p.chars().count() < 2) {
        return None;
    }
    let name = parts
        .iter()
        .map(|p| {
            if PARTICLES.contains(p) {
                p.to_string()
            } else {
                capitalise(p)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    person_name(&name)
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

fn title_case(text: &str) -> String {
    text.split_whitespace()
        .map(|w| {
            let lower = w.to_lowercase();
            if PARTICLES.contains(&lower.as_str()) {
                lower
            } else {
                capitalise(w)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turned_display_name_takes_the_address_order() {
        assert_eq!(
            in_address_order("Merrin Gordon", "gordon.merrin"),
            "Gordon Merrin"
        );
        assert_eq!(
            in_address_order("Gordon Merrin", "gordon.merrin"),
            "Gordon Merrin"
        );
        assert_eq!(
            in_address_order("Jan de Vries", "jan.devries"),
            "Jan de Vries"
        );
        assert_eq!(
            in_address_order("Merrin Gordon", "gmerrin"),
            "Merrin Gordon"
        );
    }

    const OWN: &str = "urn:test#OwnCompany";
    const PERSON: &str = "urn:test#Person";

    fn persons() -> Persons {
        let org = Org {
            class: OWN.to_string(),
            key: "acme".to_string(),
        };
        Persons {
            class: PERSON.to_string(),
            sent_by: "urn:test#sentBy".to_string(),
            sent_to: "urn:test#sentTo".to_string(),
            works_for: "urn:test#worksFor".to_string(),
            closings: vec![
                "kind regards".to_string(),
                "met vriendelijke groet".to_string(),
            ],
            role_addresses: vec!["info".to_string(), "sales".to_string()],
            domains: HashMap::from([("acme.example".to_string(), org.clone())]),
            names: HashMap::from([("acme".to_string(), org)]),
        }
    }

    fn people(found: &Found) -> Vec<(&str, &str, &str)> {
        found
            .entities
            .iter()
            .map(|e| (e.key.as_str(), e.name.as_str(), e.method))
            .collect()
    }

    fn relations(found: &Found) -> Vec<(String, String)> {
        found
            .relations
            .iter()
            .map(|r| {
                let end = |e: &TextEnd| match e {
                    TextEnd::Document => "document".to_string(),
                    TextEnd::Entity(i) => found.entities[*i].key.clone(),
                    TextEnd::Known(_, key) => key.clone(),
                };
                (
                    end(&r.from),
                    format!(
                        "{}>{}",
                        r.property.trim_start_matches("urn:test#"),
                        end(&r.to)
                    ),
                )
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[test]
    fn top_headers_give_sender_and_recipients() {
        let text = "**Subject**: Pump test\n\n**From**: Jan de Vries \\<jan.devries@acme.example\\>\n\n\
            **To**: \"'mia.berg@northwind.example'\" \\<mia.berg@northwind.example\\>; Tom Hale <tom@northwind.example>\n\n\
            **Cc**: info@acme.example\n\nDear Mia,";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [
                ("jan.devries@acme.example", "Jan de Vries", "mail"),
                ("mia.berg@northwind.example", "Mia Berg", "mail"),
                ("tom@northwind.example", "Tom Hale", "mail"),
            ]
        );
        assert_eq!(
            relations(&found),
            [
                (
                    "document".to_string(),
                    "sentBy>jan.devries@acme.example".to_string()
                ),
                (
                    "document".to_string(),
                    "sentTo>mia.berg@northwind.example".to_string()
                ),
                (
                    "document".to_string(),
                    "sentTo>tom@northwind.example".to_string()
                ),
                (
                    "jan.devries@acme.example".to_string(),
                    "worksFor>acme".to_string()
                ),
            ]
        );
        let first = &found.entities[0];
        assert_eq!(
            &text[first.start..first.end],
            "Jan de Vries \\<jan.devries@acme.example\\>"
        );
    }

    #[test]
    fn quoted_headers_give_people_but_no_relation_to_the_document() {
        let text =
            "Thanks, see below.\n\n> From: Vries, Jan <jan.devries@acme.example>\n> To: Mia Berg\n";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [
                ("jan.devries@acme.example", "Jan Vries", "mail"),
                ("mia berg", "Mia Berg", "mail"),
            ]
        );
        assert_eq!(
            relations(&found),
            [(
                "jan.devries@acme.example".to_string(),
                "worksFor>acme".to_string()
            )]
        );
    }

    #[test]
    fn exchange_addresses_name_the_person_and_the_organisation() {
        let text = "**Subject**: x\n**From**: Ruud van der Sar \\</O=ACME/OU=FIRST ADMINISTRATIVE GROUP/CN=RECIPIENTS/CN= RUUD VAN DER SAR \\>\n";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [("ruud van der sar", "Ruud van der Sar", "mail")]
        );
        assert_eq!(
            relations(&found),
            [
                (
                    "document".to_string(),
                    "sentBy>ruud van der sar".to_string()
                ),
                ("ruud van der sar".to_string(), "worksFor>acme".to_string()),
            ]
        );
    }

    #[test]
    fn addresses_in_text_skip_role_addresses_and_addresses_without_a_name() {
        let text = "Mail sales@acme.example, sales.nl@acme.example or piet.jansen@acme.example, not x@y.example.";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [("piet.jansen@acme.example", "Piet Jansen", "address")]
        );
    }

    #[test]
    fn message_ids_and_content_ids_give_no_person() {
        let text = "**Message-ID**: <ab12cd34ef.5678@mail.example>\n\
            References: <jan.devries@acme.example>\n\
            In-Reply-To: <mia.berg@northwind.example>\n\
            See <0a1b2c3d4e5f6a7b8c9d@mx.example> and cid:image001.png@01d2a3b4.example\n";
        assert!(persons().find(text).entities.is_empty());
    }

    #[test]
    fn a_name_before_an_address_in_the_text_names_the_person() {
        let text = "Please contact: Eva van Dijk <e.vdijk@acme.example> or \"Hale, Tom\" \\<th@northwind.example\\>\n";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [
                ("e.vdijk@acme.example", "Eva van Dijk", "address"),
                ("th@northwind.example", "Tom Hale", "address"),
            ]
        );
    }

    #[test]
    fn a_bare_address_in_a_header_gives_no_person() {
        let text = "**Subject**: x\n**From**: Jan de Vries <jan.devries@acme.example>\n**To**: tom@northwind.example\n";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [("jan.devries@acme.example", "Jan de Vries", "mail")]
        );
    }

    #[test]
    fn an_address_with_several_people_behind_it_is_no_person() {
        let text = "**Subject**: x\n**From**: Jan de Vries <orders@northwind.example>\n\
            **To**: Mia Berg <mia.berg@acme.example>\n\nThanks.\n\n> From: Tom Hale <orders@northwind.example>\n\
            > To: Vries, Jan <jan.devries@acme.example>\n> Cc: J. de Vries <jan.devries@acme.example>\n";
        let found = persons().find(text);
        assert_eq!(
            people(&found),
            [
                ("mia.berg@acme.example", "Mia Berg", "mail"),
                ("jan.devries@acme.example", "Jan Vries", "mail"),
                ("jan.devries@acme.example", "J. de Vries", "mail"),
            ]
        );
        assert_eq!(
            relations(&found),
            [
                (
                    "document".to_string(),
                    "sentTo>mia.berg@acme.example".to_string()
                ),
                (
                    "jan.devries@acme.example".to_string(),
                    "worksFor>acme".to_string()
                ),
                (
                    "mia.berg@acme.example".to_string(),
                    "worksFor>acme".to_string()
                ),
            ]
        );
    }

    #[test]
    fn signatures_after_a_closing_in_dutch_and_english() {
        let found =
            persons().find("Tot ziens.\n\nMet vriendelijke groet,\n\nEva van Dijk\nInkoop\n");
        assert_eq!(
            people(&found),
            [("eva van dijk", "Eva van Dijk", "signature")]
        );
        let found = persons().find("Kind regards\nPROJECT MANAGER\nTom Hale\n");
        assert_eq!(people(&found), [("tom hale", "Tom Hale", "signature")]);
        let found = persons().find("Kind regards\n\nThe pump 3 team\n");
        assert!(found.entities.is_empty());
    }

    #[test]
    fn names_need_two_capitalised_words() {
        assert_eq!(person_name("Jan de Vries").as_deref(), Some("Jan de Vries"));
        assert_eq!(person_name("Jan"), None);
        assert_eq!(person_name("PROJECT MANAGER"), None);
        assert_eq!(person_name("pump skid"), None);
        assert_eq!(person_name("Pump 3"), None);
        assert_eq!(person_name("Fabrikam B.V."), None);
        assert_eq!(person_name("Package Manager"), None);
        assert_eq!(person_name("Initial Developer"), None);
        assert_eq!(person_name("R A Driair Limited"), None);
    }

    #[test]
    fn domains_come_from_gazetteer_aliases() {
        assert_eq!(
            domain_of("www.acme.example").as_deref(),
            Some("acme.example")
        );
        assert_eq!(domain_of("acme b.v."), None);
        assert_eq!(domain_of("acme"), None);
    }
}
