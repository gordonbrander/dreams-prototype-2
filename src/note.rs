//! Notes: the Markdown-shaped view of an entity (SPEC §3).
//!
//! A note is any entity with a `dreams.note/path`. Its frontmatter holds
//! every attribute of the entity, grouped by domain; its body is
//! `dreams.note/content`.

use std::collections::BTreeMap;

use dialog_artifacts::{Artifact, ArtifactSelector, Entity, Instruction, Pick, Value};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};

use crate::concepts;
use crate::error::{Error, FieldError, Result};
use crate::markdown;
use crate::notation;
use crate::value::{self, from_json, to_json};
use crate::vault::{self, Vault, assert};

pub const PATH: &str = "dreams.note/path";
pub const TITLE: &str = "dreams.note/title";
pub const CONTENT: &str = "dreams.note/content";
pub const TAG: &str = "dreams.note/tag";
pub const LINK: &str = "dreams.note/link";
pub const DANGLING: &str = "dreams.note/dangling";
pub const CREATED: &str = "dreams.note/created";
pub const PROTECTED: &str = "dreams.note/protected";

/// Relations dreams maintains itself; writes name them only through the
/// fields that drive them.
const MANAGED: [&str; 5] = [LINK, DANGLING, CREATED, vault::UPDATED, vault::BY];

/// An entity and all of its current facts.
#[derive(Debug, Clone)]
pub struct Note {
    pub entity: Entity,
    pub facts: BTreeMap<String, Vec<Value>>,
}

impl Note {
    pub fn first(&self, the: &str) -> Option<&Value> {
        self.facts.get(the).and_then(|v| v.first())
    }

    pub fn text(&self, the: &str) -> Option<&str> {
        self.first(the).and_then(value::text)
    }

    pub fn path(&self) -> Option<&str> {
        self.text(PATH)
    }

    pub fn values(&self, the: &str) -> &[Value] {
        self.facts.get(the).map_or(&[], Vec::as_slice)
    }

    pub fn texts(&self, the: &str) -> Vec<String> {
        self.values(the).iter().filter_map(value::text).map(str::to_string).collect()
    }

    pub fn updated(&self) -> u64 {
        self.values(vault::UPDATED).iter().filter_map(value::natural).max().unwrap_or_default()
    }

    pub fn is_protected(&self) -> bool {
        self.first(PROTECTED) == Some(&Value::Boolean(true))
    }

    /// Frontmatter fields grouped by domain, with `content` apart.
    pub fn to_json(&self) -> Map<String, Json> {
        let mut out = Map::new();
        out.insert("entity".into(), json!(self.entity.to_string()));
        let mut content = None;
        for (the, values) in &self.facts {
            if the == CONTENT {
                content = values.first().and_then(value::text).map(str::to_string);
                continue;
            }
            let (domain, name) = the.split_once('/').unwrap_or(("", the));
            let many = values.len() > 1 || concepts::is_many(the);
            let v = if many { Json::Array(values.iter().map(to_json).collect()) } else { to_json(&values[0]) };
            out.entry(domain).or_insert_with(|| json!({}))[name] = v;
        }
        if let Some(c) = content {
            out.insert("content".into(), json!(c));
        }
        out
    }

    pub fn markdown(&self) -> String {
        markdown::render(&self.to_json())
    }
}

/// The entity an href names: a path, `dreams:///<path>`, or an entity URI.
pub async fn resolve(vault: &Vault, href: &str) -> Result<Entity> {
    find(vault, href).await?.ok_or_else(|| Error::not_found(href))
}

pub async fn find(vault: &Vault, href: &str) -> Result<Option<Entity>> {
    if href.starts_with("did:") {
        return Ok(Some(value::entity(href)?));
    }
    let path = match href.strip_prefix("dreams://") {
        Some(rest) => rest
            .strip_prefix('/')
            .ok_or_else(|| Error::invalid(format!("{href}: other vaults are not followed yet; use dreams:///<path>")))?,
        None => href,
    };
    Ok(vault.having(PATH, Value::String(path.into())).await?.into_iter().next())
}

pub async fn load(vault: &Vault, entity: &Entity) -> Result<Note> {
    let mut facts: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for a in vault.claims(ArtifactSelector::new().of(entity.clone())).await? {
        facts.entry(a.the.to_string()).or_default().push(a.is);
    }
    Ok(Note { entity: entity.clone(), facts })
}

pub async fn get(vault: &Vault, href: &str) -> Result<Note> {
    let entity = resolve(vault, href).await?;
    let note = load(vault, &entity).await?;
    if note.facts.is_empty() {
        return Err(Error::not_found(href));
    }
    Ok(note)
}

/// A write to one note. Fields left out are left alone.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema)]
pub struct Put {
    /// The note's path, e.g. notes/sleep.md. Finds the note, or creates it.
    #[serde(default)]
    pub path: Option<String>,
    /// The entity to write, instead of finding it by path. Give `path` too to rename it.
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Markdown. [[links]] in it become dreams.note/link facts.
    #[serde(default)]
    pub content: Option<String>,
    /// Replaces the note's tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Other attributes, as {"domain/name": value}. A list replaces every
    /// value of the attribute; null removes them.
    #[serde(default)]
    pub fields: Map<String, Json>,
    /// Refuse the write unless the note then matches this concept: a
    /// built-in name or a schema note path.
    #[serde(default)]
    pub concept: Option<String>,
    /// Refuse the write if the note's `updated` is not this value.
    #[serde(default)]
    pub expect_updated: Option<u64>,
}

/// Create or update a note. `trusted` writers (the CLI, seeding) may touch
/// protected notes; MCP clients may not.
pub async fn put(vault: &Vault, input: Put, trusted: bool) -> Result<Entity> {
    let target = match (&input.entity, &input.path) {
        (Some(e), _) => Some(value::entity(e)?),
        (None, Some(p)) => find(vault, p).await?,
        (None, None) => return Err(Error::invalid("give `path` or `entity`")),
    };
    let entity = match target {
        Some(e) => e,
        None => Entity::new().map_err(Error::dialog)?,
    };
    let note = load(vault, &entity).await?;
    let created = note.facts.is_empty();
    let what = input.path.clone().unwrap_or_else(|| entity.to_string());
    if note.is_protected() && !trusted {
        return Err(Error::protected(what));
    }
    if let Some(expected) = input.expect_updated
        && note.updated() != expected
    {
        return Err(Error::Stale { what, expected, current: note.updated() });
    }

    let mut set: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if let Some(path) = &input.path {
        check_path(path)?;
        if let Some(other) = find(vault, path).await?
            && other != entity
        {
            return Err(Error::invalid(format!("{path} is another note's path")));
        }
        set.insert(PATH.into(), vec![Value::String(path.clone())]);
    } else if note.path().is_none() {
        return Err(Error::invalid("a note needs a `path`"));
    }
    if let Some(title) = &input.title {
        set.insert(TITLE.into(), vec![Value::String(title.clone())]);
    }
    if let Some(tags) = &input.tags {
        set.insert(TAG.into(), tags.iter().map(|t| Value::String(t.clone())).collect());
    }
    if let Some(content) = &input.content {
        set.insert(CONTENT.into(), vec![Value::String(content.clone())]);
        let (links, dangling) = resolve_links(vault, content).await?;
        set.insert(LINK.into(), links.into_iter().map(Value::Entity).collect());
        set.insert(DANGLING.into(), dangling.into_iter().map(Value::String).collect());
    }
    let schema = match &input.concept {
        Some(name) => Some(notation::concept(vault, name).await?),
        None => None,
    };
    for (the, json) in &input.fields {
        writable(the, trusted)?;
        let as_type = declared_type(the, schema.as_ref());
        let items: Vec<&Json> = match json {
            Json::Null => vec![],
            Json::Array(items) => items.iter().collect(),
            j => vec![j],
        };
        let mut values = Vec::new();
        for j in items {
            // An entity attribute may name its note by path.
            if as_type.as_deref() == Some("entity:")
                && let Some(href) = j.as_str()
                && !href.starts_with("did:")
            {
                values.push(Value::Entity(resolve(vault, href).await?));
            } else {
                values.push(from_json(j, as_type.as_deref())?);
            }
        }
        set.insert(the.clone(), values);
    }
    if let (Some(name), Some(schema)) = (&input.concept, &schema) {
        validate(name, schema, &note, &set)?;
    }

    let mut changes = Vec::new();
    for (the, values) in &set {
        let many = values.len() > 1 || concepts::is_many(the) || input.fields.get(the).is_some_and(Json::is_array);
        changes.extend(replace(&entity, the, note.values(the), values, many)?);
    }
    if created {
        changes.push(assert(&entity, CREATED, Value::UnsignedInt(vault::now_ms().into()), Pick::Min)?);
    }
    // A new path may be what earlier [[links]] were waiting for.
    if created || input.path.as_deref() != note.path() {
        changes.extend(adopt_dangling(vault, &entity, input.path.as_deref(), input.title.as_deref()).await?);
    }
    if changes.is_empty() {
        return Ok(entity);
    }
    vault.write(changes).await?;
    Ok(entity)
}

/// Retract every fact of the note (SPEC §3.5).
pub async fn delete(vault: &Vault, href: &str, trusted: bool) -> Result<Entity> {
    let entity = resolve(vault, href).await?;
    let note = load(vault, &entity).await?;
    if note.facts.is_empty() {
        return Err(Error::not_found(href));
    }
    if note.is_protected() && !trusted {
        return Err(Error::protected(href));
    }
    let mut all = vault.claims(ArtifactSelector::new().of(entity.clone())).await?;
    // A feed item keeps its feed and key: the marker that it was seen (SPEC §11).
    if crate::feed::is_item(&note) {
        all.retain(|a| ![crate::feed::ITEM_FEED, crate::feed::ITEM_KEY].contains(&a.the.to_string().as_str()));
    }
    vault.write(all.into_iter().map(Instruction::Retract).collect()).await?;
    Ok(entity)
}

/// Instructions that make `the` of `of` hold exactly `new`.
pub fn replace(of: &Entity, the: &str, old: &[Value], new: &[Value], many: bool) -> Result<Vec<Instruction>> {
    let mut out = Vec::new();
    let relation = vault::relation(the)?;
    // Several values of a one-value relation are a conflict (SPEC §6): a
    // write settles it by retracting every value it doesn't keep.
    let conflicted = !many && old.len() > 1;
    for v in old {
        if !new.contains(v) && (many || conflicted || new.is_empty()) {
            out.push(Instruction::Retract(Artifact { the: relation.clone(), of: of.clone(), is: v.clone(), cause: None }));
        }
    }
    for v in new.iter().filter(|v| !old.contains(v)) {
        // Over a conflict every old value is retracted above; `all:` adds
        // without electing one more to retract.
        out.push(assert(of, the, v.clone(), if conflicted { Pick::All } else { pick(the, many) })?);
    }
    Ok(out)
}

fn pick(the: &str, many: bool) -> Pick {
    if many {
        return Pick::All;
    }
    match concepts::attribute(the).and_then(|a| a["pick"].as_str().map(str::to_string)).as_deref() {
        Some("max:") => Pick::Max,
        Some("min:") => Pick::Min,
        _ => Pick::Last,
    }
}

fn check_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(Error::invalid(format!("bad path {path:?}: use relative segments like notes/sleep.md")));
    }
    Ok(())
}

/// Refuse writes to relations no client may name.
pub fn writable(the: &str, trusted: bool) -> Result<()> {
    vault::relation(the)?;
    if the.starts_with("dialog.") || the.starts_with("dreams.deploy/") {
        return Err(Error::protected(the));
    }
    if MANAGED.contains(&the) || (the == PROTECTED && !trusted) {
        return Err(Error::invalid(format!("{the} is maintained by dreams; write content, path or title instead")));
    }
    Ok(())
}

fn declared_type(the: &str, schema: Option<&Json>) -> Option<String> {
    concepts::type_of(the).or_else(|| {
        schema?["with"].as_object()?.values().find(|a| a["the"] == the)?["as"].as_str().map(str::to_string)
    })
}

/// Check that the note will match `schema` once `set` is written.
fn validate(name: &str, schema: &Json, note: &Note, set: &BTreeMap<String, Vec<Value>>) -> Result<()> {
    let mut errors = Vec::new();
    for field in schema["with"].as_object().into_iter().flat_map(|w| w.values()) {
        let Some(the) = field["the"].as_str() else { continue };
        let values = set.get(the).map_or(note.values(the), Vec::as_slice);
        if values.is_empty() && field["optional"] != true {
            errors.push(FieldError { the: the.into(), message: "required".into() });
        }
        if let Some(want) = field["as"].as_str()
            && let Some(bad) = values.iter().find(|v| value::type_of(v) != want && !(want == "integer:" && value::type_of(v) == "natural:"))
        {
            errors.push(FieldError { the: the.into(), message: format!("expected {want}, got {}", value::type_of(bad)) });
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation { concept: name.into(), errors }) }
}

/// Link targets in Markdown: `[[target]]`, `[[target|label]]`,
/// `dreams:///path` and bare `did:key:` URIs.
pub fn link_targets(content: &str) -> Vec<String> {
    let prose = without_code(content);
    let content = prose.as_str();
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(i) = rest.find("[[") {
        rest = &rest[i + 2..];
        let Some(j) = rest.find("]]") else { break };
        let raw = rest[..j].split('|').next().unwrap_or_default();
        let target = raw.trim();
        if !target.is_empty() && !raw.contains('\n') {
            out.push(target.to_string());
        }
        rest = &rest[j + 2..];
    }
    for prefix in ["dreams:///", "did:key:"] {
        let mut rest = content;
        while let Some(i) = rest.find(prefix) {
            let tail = &rest[i..];
            let end = tail.find(|c: char| c.is_whitespace() || ")]>\"'`,".contains(c)).unwrap_or(tail.len());
            let uri = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
            let target = uri.strip_prefix("dreams:///").unwrap_or(uri);
            if !target.is_empty() && !out.iter().any(|t| t == target) {
                out.push(target.to_string());
            }
            rest = &tail[end..];
        }
    }
    out
}

/// Markdown with code fences and inline code spans blanked out: a link
/// written as an example is not a link.
fn without_code(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fenced = false;
    for line in md.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            out.push('\n');
            continue;
        }
        if fenced {
            out.push('\n');
            continue;
        }
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
                out.push(' ');
            } else {
                out.push(if in_code { ' ' } else { c });
            }
        }
    }
    out
}

/// Split link targets into the notes they name and the ones that name none.
async fn resolve_links(vault: &Vault, content: &str) -> Result<(Vec<Entity>, Vec<String>)> {
    let mut links = Vec::new();
    let mut dangling = Vec::new();
    for target in link_targets(content) {
        match link_target(vault, &target).await? {
            Some(e) if !links.contains(&e) => links.push(e),
            Some(_) => {}
            None if !dangling.contains(&target) => dangling.push(target),
            None => {}
        }
    }
    Ok((links, dangling))
}

async fn link_target(vault: &Vault, target: &str) -> Result<Option<Entity>> {
    if target.starts_with("did:") {
        return Ok(value::entity(target).ok());
    }
    for path in [target.to_string(), format!("{target}.md")] {
        if let Some(e) = vault.having(PATH, Value::String(path)).await?.into_iter().next() {
            return Ok(Some(e));
        }
    }
    Ok(vault.having(TITLE, Value::String(target.into())).await?.into_iter().next())
}

/// Turn other notes' dangling links to `path` or `title` into links to `entity`.
async fn adopt_dangling(vault: &Vault, entity: &Entity, path: Option<&str>, title: Option<&str>) -> Result<Vec<Instruction>> {
    let mut names: Vec<String> = Vec::new();
    if let Some(p) = path {
        names.push(p.to_string());
        if let Some(stem) = p.strip_suffix(".md") {
            names.push(stem.to_string());
        }
    }
    names.extend(title.map(str::to_string));
    let mut out = Vec::new();
    for name in names {
        let wanting = vault.claims(ArtifactSelector::new().the(vault::relation(DANGLING)?).is(Value::String(name.clone()))).await?;
        for a in wanting {
            out.push(assert(&a.of, LINK, Value::Entity(entity.clone()), Pick::All)?);
            out.push(Instruction::Retract(a));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_targets_are_found() {
        let md = "See [[sleep]] and [[notes/a.md|A]], dreams:///b.md, and did:key:z6Mkabc. Not [[\nbroken]], `[[code]]`,\n```\n[[fenced]]\n```\n";
        assert_eq!(link_targets(md), vec!["sleep", "notes/a.md", "b.md", "did:key:z6Mkabc"]);
    }

    #[test]
    fn paths_must_be_relative() {
        assert!(check_path("notes/a.md").is_ok());
        for bad in ["", "/a", "a//b", "../a", "a/./b"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
