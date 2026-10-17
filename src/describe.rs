//! `describe` (SPEC §8.2): the map an agent reads before it writes a query.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use dialog_artifacts::{ArtifactSelector, Entity, Value};
use dialog_query::{ConceptDescriptor, ConceptQuery, DeductiveRule, Parameters, Term};
use serde_json::{Value as Json, json};

use crate::concepts;
use crate::error::Result;
use crate::markdown;
use crate::note;
use crate::value;
use crate::vault::{self, Vault};

/// Attributes in use, concepts and rules, limited to relations starting with `domain`.
// ponytail: scans every claim; cache counts beside the search index if vaults get big.
pub async fn describe(vault: &Vault, domain: Option<&str>) -> Result<Json> {
    let prefix = domain.unwrap_or("");
    let schemas = schema_notes(vault).await?;
    let declared = |the: &str| {
        concepts::attribute(the).or_else(|| {
            schemas.values().find_map(|c| c["with"].as_object()?.values().find(|a| a["the"] == the).cloned())
        })
    };

    // Per relation: fact count, value types seen, facts per entity.
    type Stats = (usize, BTreeSet<&'static str>, HashMap<Entity, usize>);
    let mut stats: BTreeMap<String, Stats> = BTreeMap::new();
    for a in vault.claims(ArtifactSelector::new().the_starting_with(prefix)).await? {
        let the = a.the.to_string();
        if the.starts_with("dialog.") && !prefix.starts_with("dialog.") {
            continue;
        }
        let s = stats.entry(the).or_default();
        s.0 += 1;
        s.1.insert(value::type_of(&a.is));
        *s.2.entry(a.of).or_default() += 1;
    }
    let attributes: Vec<Json> = stats
        .into_iter()
        .map(|(the, (count, types, per_entity))| {
            let multi = per_entity.values().any(|n| *n > 1);
            match declared(&the) {
                Some(mut d) => {
                    if let Some(o) = d.as_object_mut() {
                        o.remove("optional");
                        o.entry("pick").or_insert(json!("last:"));
                        o.insert("count".into(), json!(count));
                    }
                    d
                }
                None => json!({"the": the, "undeclared": true, "types": types, "multi": multi, "count": count}),
            }
        })
        .collect();

    let mut concepts_out = serde_json::Map::new();
    for (name, c) in concepts::builtins() {
        concepts_out.insert(name.into(), c);
    }
    for (path, c) in schemas {
        concepts_out.insert(path, c);
    }
    let (rules, quarantined) = rules(vault).await?;
    Ok(json!({
        "attributes": attributes,
        "concepts": concepts_out,
        "rules": rules,
        "quarantined": quarantined,
    }))
}

/// Schema notes: notes under `schemas/` whose content is a concept.
pub async fn schema_notes(vault: &Vault) -> Result<BTreeMap<String, Json>> {
    let mut out = BTreeMap::new();
    let paths = vault.claims(ArtifactSelector::new().the(vault::relation(note::PATH)?).is_starting_with("schemas/")).await?;
    for a in paths {
        let Some(path) = value::text(&a.is) else { continue };
        let content = vault.values(&a.of, note::CONTENT).await?;
        if let Some(c) = content.first().and_then(value::text).and_then(|t| markdown::yaml_to_map(t).ok())
            && c.contains_key("with")
        {
            out.insert(path.to_string(), Json::Object(c));
        }
    }
    Ok(out)
}

/// Installed rules in formal notation, and the ones Dialog set aside.
async fn rules(vault: &Vault) -> Result<(Vec<Json>, Vec<String>)> {
    let mut rules = Vec::new();
    for a in vault.claims(ArtifactSelector::new().the(vault::relation("dialog.rule/source")?)).await? {
        if let Value::Bytes(bytes) = &a.is
            && let Ok(rule) = DeductiveRule::decode(bytes)
        {
            let mut d = serde_json::to_value(rule.descriptor()).unwrap_or(Json::Null);
            d["entity"] = json!(a.of.to_string());
            rules.push(d);
        }
    }
    let predicate: ConceptDescriptor =
        serde_json::from_value(json!({"with": {"concept": {"the": "dialog.rule/quarantined", "as": "entity:"}}}))?;
    let mut terms = Parameters::new();
    terms.insert("this".into(), Term::var("this"));
    terms.insert("concept".into(), Term::var("concept"));
    let quarantined = vault
        .select(ConceptQuery { predicate, terms }, None)
        .await
        .map(|rows| rows.iter().map(|r| r.entity().to_string()).collect())
        .unwrap_or_default();
    Ok((rules, quarantined))
}

/// Write `concept` as a schema note at `path` (SPEC §5.2).
pub async fn define_concept(vault: &Vault, path: &str, concept: &Json) -> Result<Entity> {
    let concept = crate::notation::parse(concept)?;
    if !path.starts_with("schemas/") {
        return Err(crate::error::Error::invalid("a schema note's path starts with schemas/"));
    }
    serde_json::from_value::<ConceptDescriptor>(Json::Object(concept.clone())).map_err(|e| {
        crate::error::Error::query(format!("not a concept: {e}"), Some("a concept has `description` and `with`, a map of attributes"))
    })?;
    let content = serde_yaml_ng::to_string(&concept).map_err(|e| crate::error::Error::invalid(e.to_string()))?;
    let title = concept.get("description").and_then(Json::as_str).map(str::to_string);
    let put = note::Put {
        path: Some(path.into()),
        title,
        content: Some(content),
        tags: Some(vec!["schema".into()]),
        ..Default::default()
    };
    note::put(vault, put, false).await
}
