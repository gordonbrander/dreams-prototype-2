//! The `query` tool (SPEC §8.3): a Dialog deductive rule in formal
//! notation, run once without installing it.
//!
//! dreams adds a little on top of Dialog's notation:
//! - `"deduce": {"find": [...]}` names the result columns.
//! - `{"assert": {"concept": "<name>"}}` names a concept instead of inlining it.
//! - Dialog requires a rule to bind `this`. With `find`, dreams renames the
//!   first entity variable (one bound at a premise's `this`) to `this`.
//! - Dialog at this tag refuses `reduce` in a deductive rule, so dreams
//!   folds the rows itself: group by the `find` columns not reduced.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use dialog_artifacts::Value;
use dialog_query::{Any, Binding, DeductiveRuleDescriptor, Term};
use serde_json::{Map, Value as Json, json};

use crate::concepts;
use crate::error::{Error, Result};
use crate::markdown;
use crate::value::{self, to_json};
use crate::vault::Vault;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;
const TIMEOUT: Duration = Duration::from_secs(5);

pub type Row = Map<String, Json>;

/// Run `rule` once against the vault and return at most `limit` rows.
pub async fn query(vault: &Vault, rule: &Json, limit: usize) -> Result<Vec<Row>> {
    let rule = parse(rule)?;
    let concepts = resolve_concepts(vault, &rule).await?;
    let plan = plan(rule, &concepts)?;
    let compiled = plan.descriptor.clone().compile().map_err(|e| Error::query(&e, hint(&e.to_string())))?;
    let matches = tokio::time::timeout(TIMEOUT, vault.solve(compiled))
        .await
        .map_err(|_| Error::query("the query ran longer than 5 s", Some("bind more of its terms to constants")))??;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in matches {
        let mut r = Row::new();
        for (column, var) in &plan.read {
            let v = match m.lookup(&Term::<Any>::var(var.as_str())) {
                Ok(Binding::Present(v)) => to_json(&v),
                _ => Json::Null,
            };
            r.insert(column.clone(), v);
        }
        if seen.insert(Json::Object(r.clone()).to_string()) {
            out.push(r);
        }
    }
    let mut out = match &plan.reduce {
        Some(reduce) => fold(out, &plan.columns, reduce)?,
        None => out.into_iter().map(|r| project(r, &plan.columns)).collect(),
    };
    out.truncate(limit.min(MAX_LIMIT));
    Ok(out)
}

/// Accept a rule as a JSON object, or as JSON or YAML text.
pub fn parse(input: &Json) -> Result<Map<String, Json>> {
    match input {
        Json::Object(m) => Ok(m.clone()),
        Json::String(s) => markdown::yaml_to_map(s),
        _ => Err(Error::invalid("a rule is an object with `deduce` and `when`")),
    }
}

/// The concept named by a built-in name or a schema note path.
pub async fn concept(vault: &Vault, name: &str) -> Result<Json> {
    if let Some(c) = concepts::builtin(name) {
        return Ok(c);
    }
    let missing = || Error::query(format!("no concept named {name:?}"), Some("call describe to list the concepts"));
    let entity = vault.having("dreams.note/path", Value::String(name.into())).await?.into_iter().next().ok_or_else(missing)?;
    let content = vault.values(&entity, "dreams.note/content").await?;
    let text = content.first().and_then(value::text).ok_or_else(missing)?;
    let c = markdown::yaml_to_map(text)?;
    if !c.contains_key("with") {
        return Err(Error::query(format!("{name} has no `with`"), Some("a schema note's content is a concept in formal notation")));
    }
    Ok(Json::Object(c))
}

async fn resolve_concepts(vault: &Vault, rule: &Map<String, Json>) -> Result<BTreeMap<String, Json>> {
    let mut out = BTreeMap::new();
    for premise in premises(rule) {
        if let Some(name) = premise["assert"]["concept"].as_str()
            && !out.contains_key(name)
        {
            out.insert(name.to_string(), concept(vault, name).await?);
        }
    }
    Ok(out)
}

fn premises(rule: &Map<String, Json>) -> impl Iterator<Item = &Json> {
    ["when", "unless"].into_iter().filter_map(|k| rule.get(k)?.as_array()).flatten()
}

/// What `query` runs: the rule, with its head and `this` settled.
#[derive(Debug)]
pub struct Plan {
    pub descriptor: DeductiveRuleDescriptor,
    /// Each column to read from a solution, and the rule variable it reads.
    pub read: Vec<(String, String)>,
    /// The columns the caller asked for, in order.
    pub columns: Vec<String>,
    pub reduce: Option<Map<String, Json>>,
}

pub fn plan(mut rule: Map<String, Json>, concepts: &BTreeMap<String, Json>) -> Result<Plan> {
    if constrains_a_variable(&Json::Object(rule.clone())) {
        // Dialog parses `{"?": {"where": ...}}` at this tag but ignores it.
        return Err(Error::query(
            "constraints inside a variable are not supported yet",
            Some(r#"use a premise instead: {"assert": ">=", "where": {"of": {"?": {"name": "x"}}, "with": 5}}, or text/like for a prefix"#),
        ));
    }
    for key in ["when", "unless"] {
        if let Some(Json::Array(list)) = rule.get_mut(key) {
            for premise in list {
                if let Some(name) = premise["assert"]["concept"].as_str().map(str::to_string) {
                    premise["assert"] = concepts[&name].clone();
                }
            }
        }
    }
    let reduce = match rule.remove("reduce") {
        Some(Json::Object(r)) => Some(r),
        Some(_) => return Err(Error::query("`reduce` must be an object", None)),
        None => None,
    };
    let deduce = rule.get("deduce").cloned().unwrap_or(Json::Null);
    let Some(find) = deduce.get("find") else {
        // A full head: the caller binds `this` and names the fields.
        let fields: Vec<String> = deduce["with"].as_object().map(|w| w.keys().cloned().collect()).unwrap_or_default();
        let descriptor = descriptor(rule)?;
        let mut columns = vec!["this".to_string()];
        columns.extend(fields);
        let read = columns.iter().map(|c| (c.clone(), c.clone())).collect();
        return Ok(Plan { descriptor, read, columns, reduce });
    };
    let columns: Vec<String> = find
        .as_array()
        .ok_or_else(|| Error::query("`find` must be a list of variable names", None))?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| Error::query("`find` must be a list of variable names", None)))
        .collect::<Result<_>>()?;
    if columns.is_empty() {
        return Err(Error::query("`find` is empty", Some("name at least one variable from `when`")));
    }
    let mut needed: Vec<String> = Vec::new();
    for c in &columns {
        if reduce.as_ref().is_none_or(|r| !r.contains_key(c)) {
            needed.push(c.clone());
        }
    }
    for spec in reduce.iter().flat_map(|r| r.values()) {
        let of = spec["of"]["?"]["name"].as_str().ok_or_else(|| {
            Error::query("each `reduce` entry needs `apply` and `of`, a variable", Some(r#"{"count": {"apply": "count", "of": {"?": {"name": "note"}}}}"#))
        })?;
        if !needed.iter().any(|n| n == of) {
            needed.push(of.to_string());
        }
    }

    let this = entity_variable(&rule).ok_or_else(|| {
        Error::query(
            "a query needs an entity to hang its rows on",
            Some(r#"give at least one premise a variable `this`, e.g. "where": {"this": {"?": {"name": "note"}}, ...}"#),
        )
    })?;
    if this != "this" {
        for key in ["when", "unless"] {
            if let Some(v) = rule.get_mut(key) {
                rename(v, &this, "this");
            }
        }
    }
    let fields: Vec<String> = needed.iter().filter(|n| **n != this).cloned().collect();
    let mut with = Map::new();
    for (i, f) in fields.iter().enumerate() {
        // The head is scaffolding (rows are read from the body), so a field
        // fed by an optional premise must not make the rule refuse.
        with.insert(f.clone(), json!({"the": format!("dreams.query/{}", slug(f, i)), "optional": true}));
    }
    {
        // Dialog's head needs one required field: carry `this` through an equality.
        with.insert("dreams-this".into(), json!({"the": "dreams.query/this", "as": "entity:"}));
        rule.get_mut("when").and_then(Json::as_array_mut).map(|w| {
            w.push(json!({"assert": "==", "where": {"this": {"?": {"name": "this"}}, "is": {"?": {"name": "dreams-this"}}}}))
        });
    }
    rule.insert("deduce".into(), json!({"with": with}));
    let read = needed.iter().map(|n| (n.clone(), if *n == this { "this".into() } else { n.clone() })).collect();
    Ok(Plan { descriptor: descriptor(rule)?, read, columns, reduce })
}

fn descriptor(rule: Map<String, Json>) -> Result<DeductiveRuleDescriptor> {
    serde_json::from_value(Json::Object(rule)).map_err(|e| {
        Error::query(
            format!("not a rule in formal notation: {e}"),
            Some("see the dialog skill: a rule has `deduce` and `when`; each premise has `assert` and `where`"),
        )
    })
}

/// The variable at the `this` of the first premise that matches facts:
/// the entity each row is about.
fn entity_variable(rule: &Map<String, Json>) -> Option<String> {
    if mentions(&Json::Object(rule.clone()), "this") {
        return Some("this".into());
    }
    rule.get("when")?.as_array()?.iter().find_map(|p| {
        p["assert"].is_object().then(|| p["where"]["this"]["?"]["name"].as_str().map(str::to_string)).flatten()
    })
}

fn constrains_a_variable(v: &Json) -> bool {
    match v {
        Json::Object(m) => m.get("?").is_some_and(|q| q.get("where").is_some()) || m.values().any(constrains_a_variable),
        Json::Array(a) => a.iter().any(constrains_a_variable),
        _ => false,
    }
}

fn mentions(v: &Json, name: &str) -> bool {
    match v {
        Json::Object(m) => m.get("?").is_some_and(|q| q["name"] == name) || m.values().any(|v| mentions(v, name)),
        Json::Array(a) => a.iter().any(|v| mentions(v, name)),
        _ => false,
    }
}

fn rename(v: &mut Json, from: &str, to: &str) {
    match v {
        Json::Object(m) => {
            if let Some(q) = m.get_mut("?")
                && q["name"] == from
            {
                q["name"] = json!(to);
            }
            m.values_mut().for_each(|v| rename(v, from, to));
        }
        Json::Array(a) => a.iter_mut().for_each(|v| rename(v, from, to)),
        _ => {}
    }
}

/// A variable name as a Dialog relation name: lowercase kebab-case.
fn slug(name: &str, i: usize) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.trim_matches('-').to_string();
    if s.starts_with(|c: char| c.is_ascii_lowercase()) && !s.contains("--") { s } else { format!("v{i}") }
}

fn project(mut row: Row, columns: &[String]) -> Row {
    columns.iter().map(|c| (c.clone(), row.remove(c).unwrap_or(Json::Null))).collect()
}

/// Group `rows` by the columns not reduced, and fold each group.
fn fold(rows: Vec<Row>, columns: &[String], reduce: &Map<String, Json>) -> Result<Vec<Row>> {
    let keys: Vec<&String> = columns.iter().filter(|c| !reduce.contains_key(*c)).collect();
    let mut groups: Vec<(Vec<Json>, Vec<Row>)> = Vec::new();
    for row in rows {
        let key: Vec<Json> = keys.iter().map(|k| row.get(*k).cloned().unwrap_or(Json::Null)).collect();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(row),
            None => groups.push((key, vec![row])),
        }
    }
    groups
        .into_iter()
        .map(|(key, group)| {
            let mut out = Row::new();
            for (k, v) in keys.iter().zip(key) {
                out.insert((*k).clone(), v);
            }
            for (field, spec) in reduce {
                let of = spec["of"]["?"]["name"].as_str().unwrap_or_default();
                let values: Vec<&Json> = group.iter().filter_map(|r| r.get(of)).filter(|v| !v.is_null()).collect();
                out.insert(field.clone(), aggregate(spec["apply"].as_str().unwrap_or_default(), &values)?);
            }
            Ok(project(out, columns))
        })
        .collect()
}

fn aggregate(apply: &str, values: &[&Json]) -> Result<Json> {
    let numbers = || values.iter().filter_map(|v| v.as_f64()).collect::<Vec<_>>();
    let number = |f: f64| if f.fract() == 0.0 && f.abs() < 9e15 { json!(f as i64) } else { json!(f) };
    let order = |a: &&&Json, b: &&&Json| match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.to_string().cmp(&b.to_string()),
    };
    Ok(match apply {
        "count" => json!(values.len()),
        "count-distinct" => json!(values.iter().map(|v| v.to_string()).collect::<HashSet<_>>().len()),
        "sum" => number(numbers().iter().sum()),
        "avg" => {
            let n = numbers();
            if n.is_empty() { Json::Null } else { json!(n.iter().sum::<f64>() / n.len() as f64) }
        }
        "min" => values.iter().min_by(order).map_or(Json::Null, |v| (*v).clone()),
        "max" => values.iter().max_by(order).map_or(Json::Null, |v| (*v).clone()),
        other => {
            return Err(Error::query(
                format!("unknown fold {other:?}"),
                Some("use count, count-distinct, sum, min, max or avg"),
            ));
        }
    })
}

/// Turn a Dialog error message into advice an agent can act on.
fn hint(message: &str) -> Option<&'static str> {
    if message.contains("does not bind variable") {
        Some("every variable in `find` must appear in a `when` premise")
    } else if message.contains("unknown") || message.contains("formula") {
        Some("check the operator or formula name against the dialog skill")
    } else {
        Some("call describe and copy attribute descriptors exactly; tags, links and dangling need \"pick\": \"all:\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_becomes_a_head_and_this_is_renamed() {
        let rule = parse(&json!({
            "deduce": {"find": ["note", "path"]},
            "when": [{"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}},
                      "where": {"this": {"?": {"name": "note"}}, "is": {"?": {"name": "path"}}}}]
        }))
        .unwrap();
        let p = plan(rule, &BTreeMap::new()).unwrap();
        assert_eq!(p.read, vec![("note".to_string(), "this".to_string()), ("path".to_string(), "path".to_string())]);
        assert_eq!(p.columns, vec!["note", "path"]);
    }

    #[test]
    fn folds_group_by_the_columns_not_reduced() {
        let rows = vec![
            json!({"tag": "a", "note": "1"}),
            json!({"tag": "a", "note": "2"}),
            json!({"tag": "b", "note": "1"}),
        ]
        .into_iter()
        .map(|r| r.as_object().unwrap().clone())
        .collect();
        let reduce = json!({"count": {"apply": "count", "of": {"?": {"name": "note"}}}});
        let out = fold(rows, &["tag".into(), "count".into()], reduce.as_object().unwrap()).unwrap();
        assert_eq!(Json::from(out), json!([{"tag": "a", "count": 2}, {"tag": "b", "count": 1}]));
    }
}
