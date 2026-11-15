use std::path::Path;

use anyhow::{Result, bail};
use regex::Regex;
use serde::Serialize;
use sqlx::PgConnection;

use crate::db;
use crate::db::decisions::{Decision, HumanRelation, HumanValue};
use crate::db::resolve::SAME_AS;
use crate::ontology::{Ontology, Property, ValueKind};
use crate::units::{Lookup, UnitIndex, to_si};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    NotFound,
    Document,
    Class,
    Property,
    Unit,
    Value,
    Kind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Applied {
    pub id: i64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    pub applied: usize,
    pub problems: usize,
    pub decisions: Vec<Applied>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub unit_text: Option<String>,
    pub unit_iri: Option<String>,
    pub si_low: Option<f64>,
    pub si_high: Option<f64>,
}

fn number_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^\s*([-−]?[0-9][0-9.,' \u{a0}]*?)(?:\s*(?:-|–|\.\.|to|tot)\s*([-−]?[0-9][0-9.,' \u{a0}]*?))?\s*([^0-9\s].*)?$",
        )
        .expect("value pattern")
    })
}

fn number(text: &str) -> Option<f64> {
    let cleaned: String = text
        .trim()
        .replace('−', "-")
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{a0}' | '\''))
        .collect();
    let decimal = cleaned.rfind(['.', ',']);
    let both = cleaned.contains('.') && cleaned.contains(',');
    let normal: String = cleaned
        .char_indices()
        .filter_map(|(i, c)| match c {
            '.' | ',' if Some(i) == decimal => Some('.'),
            '.' | ',' if both => None,
            _ => Some(c),
        })
        .collect();
    normal.parse().ok()
}

pub fn parse_value(property: &Property, units: &UnitIndex, text: &str) -> Result<Parsed, Outcome> {
    if property.value == Some(ValueKind::Text) {
        return Ok(Parsed {
            low: None,
            high: None,
            unit_text: None,
            unit_iri: None,
            si_low: None,
            si_high: None,
        });
    }
    let Some(found) = number_pattern().captures(text) else {
        return Err(Outcome::Value);
    };
    let Some(low) = found.get(1).and_then(|m| number(m.as_str())) else {
        return Err(Outcome::Value);
    };
    let high = match found.get(2) {
        Some(m) => number(m.as_str()).ok_or(Outcome::Value)?,
        None => low,
    };
    if low > high || !property.accepts_number(low) || !property.accepts_number(high) {
        return Err(Outcome::Value);
    }
    let unit_text = found.get(3).map(|m| m.as_str().trim().to_string());
    let Some(kind) = &property.quantity_kind else {
        return Ok(Parsed {
            low: Some(low),
            high: Some(high),
            unit_text,
            unit_iri: None,
            si_low: Some(low),
            si_high: Some(high),
        });
    };
    let Some(written) = unit_text else {
        return Err(Outcome::Unit);
    };
    let Lookup::Found(unit) = units.lookup(kind, &written) else {
        return Err(Outcome::Unit);
    };
    Ok(Parsed {
        low: Some(low),
        high: Some(high),
        unit_text: Some(written),
        unit_iri: Some(unit.iri.clone()),
        si_low: Some(to_si(unit, low)),
        si_high: Some(to_si(unit, high)),
    })
}

fn rank(kind: &str) -> u8 {
    match kind {
        "same" => 0,
        "class" | "name" => 1,
        _ => 2,
    }
}

pub async fn run(config_dir: Option<&Path>, url: Option<&str>) -> Result<Report> {
    let config = crate::config::load(config_dir)?;
    if config.database.decisions.is_empty() {
        bail!("database.decisions is empty: set the view that holds the decisions");
    }
    let ontology = crate::ontology::load_or_empty(config_dir, &config.ontology)?;
    let lock = db::runs::lock(url, &config.database.schema).await?;
    let pool = db::pool::connect(url, &config.database.schema, 2).await?;
    let hash = crate::config::hash(&config)?;
    let view = config.database.decisions.clone();
    let work = async {
        let mut tx = pool.begin().await?;
        let decisions = db::decisions::read(&mut tx, &view).await?;
        let report = apply(&mut tx, &ontology, decisions).await?;
        tx.commit().await?;
        Ok(report)
    };
    let result = db::runs::track(&pool, &hash, "decisions", work).await;
    pool.close().await;
    lock.release().await;
    result
}

pub async fn apply(
    conn: &mut PgConnection,
    ontology: &Ontology,
    mut decisions: Vec<Decision>,
) -> Result<Report> {
    db::decisions::reset(conn).await?;
    decisions.sort_by_key(|d| (rank(&d.kind), d.id));
    let units = UnitIndex::new(ontology);
    let mut report = Report::default();
    let mut clustered = false;
    for decision in &decisions {
        if rank(&decision.kind) > 0 && !clustered {
            recluster(conn).await?;
            clustered = true;
        }
        let outcome = one(conn, ontology, &units, decision).await?;
        if outcome == Outcome::Applied {
            report.applied += 1;
        } else {
            report.problems += 1;
        }
        report.decisions.push(Applied {
            id: decision.id,
            outcome,
        });
    }
    if !clustered {
        recluster(conn).await?;
    }
    Ok(report)
}

async fn recluster(conn: &mut PgConnection) -> Result<()> {
    let entities = db::resolve::entities(&mut *conn).await?;
    let mut stats = crate::resolve::Stats::default();
    crate::resolve::canonical_with(conn, &entities, &mut stats).await
}

async fn other(conn: &mut PgConnection, decision: &Decision) -> Result<Option<i64>> {
    match (&decision.other_class_iri, &decision.other_key) {
        (Some(class), Some(key)) => db::decisions::entity(conn, class, key).await,
        _ => Ok(None),
    }
}

fn right(decision: &Decision) -> bool {
    decision.verdict.as_deref() == Some("right")
}

async fn one(
    conn: &mut PgConnection,
    ontology: &Ontology,
    units: &UnitIndex<'_>,
    decision: &Decision,
) -> Result<Outcome> {
    let Some(thing) = db::decisions::entity(conn, &decision.class_iri, &decision.key).await? else {
        return Ok(Outcome::NotFound);
    };
    let value = decision.value.as_deref().unwrap_or("").trim();
    match decision.kind.as_str() {
        "same" => {
            let Some(b) = other(conn, decision).await? else {
                return Ok(Outcome::NotFound);
            };
            if thing == b {
                return Ok(Outcome::Applied);
            }
            let status = if right(decision) {
                "confirmed"
            } else {
                "rejected"
            };
            let machine = db::decisions::links_between(conn, thing, b).await?;
            db::decisions::set_status(conn, &machine, status).await?;
            let link = HumanRelation {
                src: thing,
                property: SAME_AS,
                dst: b,
                status,
                document_id: None,
                page: None,
            };
            db::decisions::insert_relation(conn, &link).await?;
            Ok(Outcome::Applied)
        }
        "class" => {
            if !ontology.classes.contains_key(value) {
                return Ok(Outcome::Class);
            }
            db::decisions::override_entity(conn, thing, value, None).await?;
            Ok(Outcome::Applied)
        }
        "name" => {
            if value.is_empty() {
                return Ok(Outcome::Value);
            }
            let class = db::decisions::current_class(conn, thing).await?;
            db::decisions::override_entity(conn, thing, &class, Some(value)).await?;
            Ok(Outcome::Applied)
        }
        "alias" => {
            if value.is_empty() {
                return Ok(Outcome::Value);
            }
            db::decisions::add_alias(conn, thing, value).await?;
            Ok(Outcome::Applied)
        }
        "fact" => {
            let (Some(property), Some(b)) = (&decision.property_iri, other(conn, decision).await?)
            else {
                return Ok(Outcome::NotFound);
            };
            let ids = db::decisions::facts(conn, thing, property, b).await?;
            if ids.is_empty() {
                return Ok(Outcome::NotFound);
            }
            let status = if right(decision) {
                "confirmed"
            } else {
                "rejected"
            };
            db::decisions::set_status(conn, &ids, status).await?;
            Ok(Outcome::Applied)
        }
        "add_fact" => {
            let Some(property) = &decision.property_iri else {
                return Ok(Outcome::Property);
            };
            if !ontology.properties.contains_key(property) {
                return Ok(Outcome::Property);
            }
            let Some(b) = other(conn, decision).await? else {
                return Ok(Outcome::NotFound);
            };
            let Some(document_id) = source_document(conn, decision).await? else {
                return Ok(Outcome::Document);
            };
            let fact = HumanRelation {
                src: thing,
                property,
                dst: b,
                status: "confirmed",
                document_id: Some(document_id),
                page: decision.page,
            };
            db::decisions::insert_relation(conn, &fact).await?;
            Ok(Outcome::Applied)
        }
        "value" => {
            let Some(property) = decision
                .property_iri
                .as_deref()
                .and_then(|iri| ontology.properties.get(iri))
                .filter(|p| p.is_attribute())
            else {
                return Ok(Outcome::Property);
            };
            let parsed = match parse_value(property, units, value) {
                Ok(parsed) => parsed,
                Err(outcome) => return Ok(outcome),
            };
            let Some(document_id) = source_document(conn, decision).await? else {
                return Ok(Outcome::Document);
            };
            let Some(chunk_id) = db::decisions::chunk(conn, document_id, decision.page).await?
            else {
                return Ok(Outcome::Document);
            };
            if let Some(old) = decision.old_value.as_deref().filter(|v| !v.is_empty()) {
                db::decisions::hide_values(conn, thing, &property.iri, old).await?;
            }
            let row = HumanValue {
                entity_id: thing,
                property: &property.iri,
                value_text: value,
                unit_text: parsed.unit_text.as_deref(),
                unit_iri: parsed.unit_iri.as_deref(),
                low: parsed.low,
                high: parsed.high,
                si_low: parsed.si_low,
                si_high: parsed.si_high,
                dimension: property.dimension.as_deref(),
                chunk_id,
            };
            db::decisions::insert_value(conn, &row).await?;
            Ok(Outcome::Applied)
        }
        _ => Ok(Outcome::Kind),
    }
}

async fn source_document(conn: &mut PgConnection, decision: &Decision) -> Result<Option<i64>> {
    match (&decision.source, &decision.path) {
        (Some(source), Some(path)) => db::decisions::document(conn, source, path).await,
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: &str = r#"
        @prefix ex: <http://example.org/onto#> .
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix qudt: <http://qudt.org/schema/qudt/> .
        @prefix unit: <http://qudt.org/vocab/unit/> .
        @prefix qk: <http://qudt.org/vocab/quantitykind/> .
        @prefix qkdv: <http://qudt.org/vocab/dimensionvector/> .
        ex:Pump a owl:Class .
        qk:Pressure a qudt:QuantityKind ; qudt:hasDimensionVector qkdv:P .
        unit:BAR a qudt:Unit ; qudt:symbol "bar" ;
            qudt:conversionMultiplier 100000.0 ; qudt:hasDimensionVector qkdv:P .
        ex:pressure a owl:DatatypeProperty ; rdfs:domain ex:Pump ; qudt:hasQuantityKind qk:Pressure .
    "#;

    fn ontology() -> Ontology {
        let config = crate::config::OntologyConfig {
            files: Vec::new(),
            languages: vec!["en".to_string()],
            include: vec!["http://example.org/onto#Pump".to_string()],
            exclude: Vec::new(),
            document_class: String::new(),
        };
        let loaded = crate::ontology::load_sources(&[("t.ttl".to_string(), TTL.into())], &config);
        assert!(!loaded.has_errors(), "{:?}", loaded.findings);
        loaded.ontology
    }

    #[test]
    fn values_are_read_with_their_unit_into_si() {
        let onto = ontology();
        let units = UnitIndex::new(&onto);
        let pressure = &onto.properties["http://example.org/onto#pressure"];
        let one = parse_value(pressure, &units, "6 bar").unwrap();
        assert_eq!(
            (one.low, one.high, one.si_low),
            (Some(6.0), Some(6.0), Some(600000.0))
        );
        assert_eq!(
            one.unit_iri.as_deref(),
            Some("http://qudt.org/vocab/unit/BAR")
        );
        let range = parse_value(pressure, &units, "6 - 10 bar").unwrap();
        assert_eq!(range.si_high, Some(1000000.0));
        assert_eq!(
            parse_value(pressure, &units, "6 bananen"),
            Err(Outcome::Unit)
        );
        assert_eq!(parse_value(pressure, &units, "6"), Err(Outcome::Unit));
        assert_eq!(parse_value(pressure, &units, "veel"), Err(Outcome::Value));
        assert_eq!(
            parse_value(pressure, &units, "10 - 6 bar"),
            Err(Outcome::Value)
        );
    }

    #[test]
    fn numbers_read_both_decimal_marks() {
        assert_eq!(number("1770"), Some(1770.0));
        assert_eq!(number("1770,5"), Some(1770.5));
        assert_eq!(number("1.770,5"), Some(1770.5));
        assert_eq!(number("1,770.5"), Some(1770.5));
        assert_eq!(number("−3"), Some(-3.0));
        assert_eq!(number("abc"), None);
    }

    #[test]
    fn same_comes_first_then_names_then_the_rest() {
        let mut kinds = vec!["value", "name", "fact", "same", "class"];
        kinds.sort_by_key(|k| rank(k));
        assert_eq!(kinds, vec!["same", "name", "class", "value", "fact"]);
    }

    #[test]
    fn the_pattern_splits_number_range_and_unit() {
        let found = number_pattern().captures("10 - 20 bar").unwrap();
        assert_eq!(found.get(1).unwrap().as_str().trim(), "10");
        assert_eq!(found.get(2).unwrap().as_str().trim(), "20");
        assert_eq!(found.get(3).unwrap().as_str(), "bar");
        let one = number_pattern().captures("1770 m³/h").unwrap();
        assert_eq!(one.get(1).unwrap().as_str().trim(), "1770");
        assert!(one.get(2).is_none());
        assert_eq!(one.get(3).unwrap().as_str(), "m³/h");
        assert!(number_pattern().captures("bananen").is_none());
    }
}
