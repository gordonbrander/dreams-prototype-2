//! Import and export (SPEC §7): documents in files to facts and back.
//!
//! A file's path is its note's path. A document is JSON, YAML, or Markdown
//! with YAML frontmatter. Two shapes are read:
//! - the spike's: flat keys, `_type: doc://schemas/<x>.json`, `doc://` refs;
//! - dreams' own (what `export` and `get_note` write): keys grouped by domain.

use std::path::{Path, PathBuf};

use dialog_artifacts::{Entity, Pick, Value};
use dialog_repository::schema::EntityExt as _;
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use crate::concepts;
use crate::error::{Error, Result};
use crate::markdown;
use crate::note::{self, Put};
use crate::value;
use crate::vault::{self, Vault};

/// The domain for keys with no better home.
pub const DEFAULT_DOMAIN: &str = "dreams.json";

#[derive(Debug, Clone)]
pub struct Options {
    /// The domain for unmapped keys of untyped documents.
    pub domain: String,
    /// Seeding: entity ids derived from paths, so that vaults seeded apart
    /// converge (SPEC §11).
    pub derive_ids: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { domain: DEFAULT_DOMAIN.into(), derive_ids: false }
    }
}

#[derive(Debug, Serialize)]
pub struct FileResult {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// Keys that were not imported, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Parse a document by its path's extension. Unknown extensions are Markdown.
pub fn parse(path: &str, text: &str) -> Result<Map<String, Json>> {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("json") => match serde_json::from_str::<Json>(text)? {
            Json::Object(m) => Ok(m),
            _ => Err(Error::invalid("a JSON document must be an object")),
        },
        Some("yaml" | "yml") => markdown::yaml_to_map(text),
        _ => markdown::parse(text),
    }
}

/// Every document file under `root`, skipping hidden entries, as (path, text).
pub fn read_dir(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    walk(root, &mut files)?;
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(root).unwrap_or(&p);
            let path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            Ok((path, std::fs::read_to_string(&p)?))
        })
        .collect()
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files)?;
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("md" | "markdown" | "json" | "yaml" | "yml")) {
            files.push(path);
        }
    }
    Ok(())
}

/// A reference to resolve once every document is in: `the` of `of` names
/// the note at `href`.
struct Ref {
    of: Entity,
    the: String,
    href: String,
    file: usize,
}

/// Import documents. `trusted`: seeding and the CLI may write protected notes.
pub async fn import(vault: &Vault, docs: &[(String, String)], opts: &Options) -> Result<Vec<FileResult>> {
    let mut results = Vec::new();
    let mut refs = Vec::new();
    for (i, (path, text)) in docs.iter().enumerate() {
        let mut result = FileResult { path: path.clone(), entity: None, skipped: vec![], error: None };
        match import_one(vault, path, text, opts, i, &mut refs, &mut result.skipped).await {
            Ok(e) => result.entity = Some(e.to_string()),
            Err(e) => result.error = Some(e.to_string()),
        }
        results.push(result);
    }
    // Second pass: `doc://` references, now that every note has an entity.
    let mut changes = Vec::new();
    for r in refs {
        let target = r.href.strip_prefix("doc://").unwrap_or(&r.href);
        let target = target.split('?').next().unwrap_or(target);
        let current = vault.values(&r.of, &r.the).await?;
        match note::find(vault, target).await? {
            Some(e) if current == [Value::Entity(e.clone())] => {}
            Some(e) => changes.push(vault::assert(&r.of, &r.the, Value::Entity(e), Pick::Last)?),
            None => {
                results[r.file].skipped.push(format!("{}: {} names no note; kept as text", r.the, r.href));
                changes.push(vault::assert(&r.of, &r.the, Value::String(r.href), Pick::Last)?);
            }
        }
    }
    vault.write(changes).await?;
    Ok(results)
}

async fn import_one(
    vault: &Vault,
    path: &str,
    text: &str,
    opts: &Options,
    file: usize,
    refs: &mut Vec<Ref>,
    skipped: &mut Vec<String>,
) -> Result<Entity> {
    let doc = parse(path, text)?;
    let entity = match (note::find(vault, path).await?, doc.get("entity").and_then(Json::as_str)) {
        (Some(e), _) => e,
        (None, Some(e)) => value::entity(e)?,
        (None, None) if opts.derive_ids => Entity::of(&("dreams", path)),
        (None, None) => Entity::new().map_err(Error::dialog)?,
    };
    let domain = match doc.get("_type").and_then(Json::as_str) {
        Some(t) => type_domain(t),
        None => opts.domain.clone(),
    };
    let mut put = Put { path: Some(path.into()), entity: Some(entity.to_string()), ..Default::default() };
    let mut children = Vec::new();
    for (key, v) in doc {
        if key.starts_with('_') || key == "entity" {
            continue;
        }
        match (key.as_str(), &v) {
            ("title", Json::String(s)) => put.title = Some(s.clone()),
            ("content", Json::String(s)) => put.content = Some(s.clone()),
            ("tags", Json::Array(tags)) => put.tags = Some(tags.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()),
            // dreams' own shape: a group of fields per domain.
            (group, Json::Object(fields)) if is_domain(group) => {
                for (name, v) in fields {
                    field(&mut put, &entity, &format!("{group}/{name}"), v, file, refs, &mut children, skipped)?;
                }
            }
            _ => {
                let the = format!("{domain}/{}", kebab(&key));
                field(&mut put, &entity, &the, &v, file, refs, &mut children, skipped)?;
            }
        }
    }
    let entity = note::put(vault, put, true).await?;
    vault.write(children).await?;
    Ok(entity)
}

/// Map one key and value onto the write.
#[allow(clippy::too_many_arguments)]
fn field(
    put: &mut Put,
    of: &Entity,
    the: &str,
    v: &Json,
    file: usize,
    refs: &mut Vec<Ref>,
    children: &mut Vec<dialog_artifacts::Instruction>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    if vault::relation(the).is_err() {
        skipped.push(format!("{the}: not a valid relation (lowercase kebab-case, 64 bytes at most)"));
        return Ok(());
    }
    match the {
        note::PATH => return Ok(()),
        note::TITLE => put.title = v.as_str().map(str::to_string),
        note::CONTENT => put.content = v.as_str().map(str::to_string),
        note::TAG | "dreams.note/tags" => {
            put.tags = Some(as_list(v).iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        }
        // Derived again on write.
        note::LINK | note::DANGLING | note::CREATED | vault::UPDATED | vault::BY => {}
        _ => match v {
            Json::Null => {}
            Json::String(s) if s.starts_with("doc://") => {
                refs.push(Ref { of: of.clone(), the: the.into(), href: s.clone(), file })
            }
            // The spike's task filters become a query.
            Json::Object(f) if the == "dreams.task/when" => {
                let concept = f.get("type").and_then(Json::as_str).map(|t| type_domain(t).trim_start_matches("dreams.").to_string());
                let get = |k: &str| f.get(k).and_then(Json::as_str);
                if f.contains_key("ids") {
                    skipped.push(format!("{the}.ids: list the notes in a query instead"));
                }
                let q = crate::notation::filter_query(get("tag"), get("glob"), concept.as_deref());
                put.fields.insert(the.into(), json!(q.to_string()));
            }
            // An ordered list of strings in a text attribute, like a runner's argv: JSON text (SPEC §7.3).
            Json::Array(_) if concepts::type_of(the).as_deref() == Some("text:") && !concepts::is_many(the) => {
                put.fields.insert(the.into(), json!(v.to_string()));
            }
            Json::Object(fields) => {
                // A nested object: a child entity derived from (parent, relation).
                let child = Entity::of(&(of.to_string(), the));
                let (domain, name) = the.split_once('/').unwrap_or((the, ""));
                for (k, v) in fields {
                    if let Some(scalar) = scalar(v) {
                        let rel = format!("{domain}.{name}/{}", kebab(k));
                        match vault::relation(&rel) {
                            Ok(_) => children.push(vault::assert(&child, &rel, scalar, Pick::Last)?),
                            Err(_) => skipped.push(format!("{rel}: not a valid relation")),
                        }
                    } else {
                        // ponytail: one level of nesting; deeper values are reported, not imported.
                        skipped.push(format!("{the}.{k}: nested too deep"));
                    }
                }
                put.fields.insert(the.into(), json!(child.to_string()));
            }
            Json::Array(items) if items.iter().any(|i| i.is_object() || i.is_array()) => {
                skipped.push(format!("{the}: lists of objects are not imported yet"));
            }
            _ => {
                put.fields.insert(the.into(), v.clone());
            }
        },
    }
    Ok(())
}

fn scalar(v: &Json) -> Option<Value> {
    if v.is_object() || v.is_array() || v.is_null() { None } else { value::from_json(v, None).ok() }
}

fn as_list(v: &Json) -> Vec<Json> {
    match v {
        Json::Array(a) => a.clone(),
        other => vec![other.clone()],
    }
}

/// `doc://schemas/<x>.json` to the domain its documents' keys go in.
fn type_domain(type_id: &str) -> String {
    let stem = type_id.trim_start_matches("doc://").trim_start_matches("schemas/");
    let stem = stem.split(['.', '?']).next().unwrap_or(stem);
    if concepts::builtin(stem).is_some() || stem == "daily" { format!("dreams.{stem}") } else { format!("me.{}", kebab(stem)) }
}

/// Whether a key names a domain: dotted, lowercase, no slash.
fn is_domain(key: &str) -> bool {
    key.contains('.') && !key.contains('/') && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-".contains(c))
}

/// `startedAt` and `started_at` to `started-at`.
pub fn kebab(key: &str) -> String {
    let mut out = String::new();
    for (i, c) in key.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('-') {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' || c == ' ' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out
}

/// Write every note under `dir` at its path, in the format its extension names.
pub async fn export(vault: &Vault, dir: &Path) -> Result<Vec<FileResult>> {
    let mut results = Vec::new();
    for a in vault.claims(dialog_artifacts::ArtifactSelector::new().the(vault::relation(note::PATH)?)).await? {
        let Some(path) = value::text(&a.is).map(str::to_string) else { continue };
        let mut result = FileResult { path: path.clone(), entity: Some(a.of.to_string()), skipped: vec![], error: None };
        let written = async {
            if path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
                return Err(Error::invalid(format!("{path} cannot be a file path")));
            }
            let n = note::load(vault, &a.of).await?;
            let text = match Path::new(&path).extension().and_then(|e| e.to_str()) {
                Some("json") => serde_json::to_string_pretty(&n.to_json())? + "\n",
                Some("yaml" | "yml") => serde_yaml_ng::to_string(&n.to_json()).map_err(|e| Error::invalid(e.to_string()))?,
                _ => n.markdown(),
            };
            let file = dir.join(&path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(file, text)?;
            Ok(())
        };
        if let Err(e) = written.await {
            result.error = Some(e.to_string());
        }
        results.push(result);
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_types_map_to_relations() {
        assert_eq!(kebab("startedAt"), "started-at");
        assert_eq!(kebab("exit_code"), "exit-code");
        assert_eq!(type_domain("doc://schemas/task.json"), "dreams.task");
        assert_eq!(type_domain("doc://schemas/task.json?rev=1-abc"), "dreams.task");
        assert_eq!(type_domain("doc://schemas/Reading List.json"), "me.reading-list");
        assert!(is_domain("dreams.note") && !is_domain("meta") && !is_domain("a/b"));
    }
}
