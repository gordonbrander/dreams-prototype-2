//! Prompts (SPEC §11): notes with `dreams.prompt/name`, served as MCP
//! prompts. A prompt is instructions only: it declares no arguments. The
//! user's own text reaches the model beside it.

use crate::error::Result;
use crate::note;
use crate::skill::named;
use crate::vault::Vault;

#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    pub content: String,
}

pub async fn list(vault: &Vault) -> Result<Vec<Prompt>> {
    Ok(named(vault, "dreams.prompt/name")
        .await?
        .iter()
        .filter_map(|n| {
            Some(Prompt {
                name: n.text("dreams.prompt/name")?.into(),
                description: n.text("dreams.prompt/description")?.into(),
                content: n.text(note::CONTENT)?.into(),
            })
        })
        .collect())
}

pub async fn find(vault: &Vault, name: &str) -> Result<Option<Prompt>> {
    Ok(list(vault).await?.into_iter().find(|p| p.name == name))
}
