//! Skills (SPEC §11): notes with `dreams.skill/name`, served to MCP hosts
//! through the Skills Extension (`io.modelcontextprotocol/skills`). Each
//! skill is one file, `skill://<name>/SKILL.md`: `name` and `description`
//! frontmatter, then the note's content. When two notes share a name, the
//! most recently updated one wins.

use std::collections::HashSet;

use dialog_artifacts::ArtifactSelector;
use serde_json::{Value, json};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::error::Result;
use crate::hash::sha256_hex;
use crate::note::{self, Note};
use crate::value;
use crate::vault::{self, Vault};

/// The MCP extension identifier.
pub const EXTENSION_ID: &str = "io.modelcontextprotocol/skills";

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// `skill://<name>/SKILL.md`
    pub uri: String,
    pub name: String,
    pub description: String,
    /// The rendered SKILL.md.
    pub text: String,
}

impl Skill {
    fn from_note(n: &Note) -> Option<Skill> {
        let name = n.text("dreams.skill/name")?;
        let description = n.text("dreams.skill/description")?;
        let mut fm = Mapping::new();
        fm.insert(Yaml::String("name".into()), Yaml::String(name.into()));
        fm.insert(Yaml::String("description".into()), Yaml::String(description.into()));
        let yaml = serde_yaml_ng::to_string(&fm).expect("a mapping serializes");
        Some(Skill {
            uri: format!("skill://{name}/SKILL.md"),
            name: name.to_string(),
            description: description.to_string(),
            text: format!("---\n{yaml}---\n{}", n.text(note::CONTENT).unwrap_or_default()),
        })
    }

    /// The extension's Skill object: its SKILL.md as the one resource.
    pub fn entry(&self) -> Value {
        let digest = sha256_hex(self.text.as_bytes());
        json!({
            "uri": self.uri,
            "frontmatter": {"name": self.name, "description": self.description},
            "resources": [{"uri": self.uri, "digest": format!("sha256:{digest}"), "size": self.text.len()}],
        })
    }
}

/// Notes with a `the` (a name), newest first, one per name.
pub async fn named(vault: &Vault, the: &str) -> Result<Vec<Note>> {
    let mut notes = Vec::new();
    for a in vault.claims(ArtifactSelector::new().the(vault::relation(the)?)).await? {
        notes.push(note::load(vault, &a.of).await?);
    }
    notes.sort_by_key(|n| std::cmp::Reverse(n.updated()));
    let mut seen = HashSet::new();
    Ok(notes.into_iter().filter(|n| n.first(the).and_then(value::text).is_some_and(|name| seen.insert(name.to_string()))).collect())
}

pub async fn list(vault: &Vault) -> Result<Vec<Skill>> {
    Ok(named(vault, "dreams.skill/name").await?.iter().filter_map(Skill::from_note).collect())
}

/// The skill whose SKILL.md is at `uri`.
pub async fn find(vault: &Vault, uri: &str) -> Result<Option<Skill>> {
    Ok(list(vault).await?.into_iter().find(|s| s.uri == uri))
}
