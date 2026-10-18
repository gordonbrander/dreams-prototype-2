//! Multiplayer (SPEC §12): remotes, devices, publishing and followed vaults.
//!
//! - A **remote** is a folder that holds one repository. The vault syncs
//!   with it: pull, then push. `public` can have remotes too.
//! - A **device** joins by a delegation the first device leaves in the
//!   remote folder (`device add <did>`); the new device's `remote add` picks it up.
//! - **Publish** copies a note into the `public` repository.
//! - **Following** someone keeps a replica of their `public` repository in a
//!   local space `peer-<petname>`, read-only, pulled from their folder. Notes
//!   in it are `dreams://<petname>/<path>`.
//!
//! Which remotes each space has is recorded in the local repository.

use std::path::{Path, PathBuf};

use dialog_artifacts::{ArtifactSelector, Entity, Instruction, Pick, Value};
use dialog_repository::schema::EntityExt as _;
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::note::{self, Note};
use crate::value;
use crate::vault::{self, Vault, assert};

const SPACE: &str = "dreams.remote/space";
const NAME: &str = "dreams.remote/name";
const FOLDER: &str = "dreams.remote/folder";
const REPOSITORY: &str = "dreams.remote/repository";
/// The space of the repository this device publishes.
pub const PUBLIC: &str = "public";
/// How many times sync pulls again after a push loses a race.
const SYNC_TRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Remote {
    /// `vault`, `public`, or `peer-<petname>`.
    pub space: String,
    pub name: String,
    pub folder: String,
    /// The DID of the repository the folder holds.
    pub repository: String,
}

fn record(space: &str, name: &str) -> Entity {
    Entity::of(&("dreams.remote", space.to_string(), name.to_string()))
}

/// Every remote of every space.
pub async fn remotes(vault: &Vault) -> Result<Vec<Remote>> {
    let mut out = Vec::new();
    for a in vault.local_claims(ArtifactSelector::new().the(vault::relation(NAME)?)).await? {
        let facts = vault.local_claims(ArtifactSelector::new().of(a.of)).await?;
        let get = |the: &str| {
            facts.iter().find(|f| f.the.to_string() == the).and_then(|f| value::text(&f.is)).unwrap_or_default().to_string()
        };
        out.push(Remote { space: get(SPACE), name: get(NAME), folder: get(FOLDER), repository: get(REPOSITORY) });
    }
    out.sort_by(|a, b| (&a.space, &a.name).cmp(&(&b.space, &b.name)));
    Ok(out)
}

/// Make `folder` a remote of `space`.
pub async fn add_remote(vault: &Vault, space: &str, name: &str, folder: &Path) -> Result<Remote> {
    let folder = std::path::absolute(folder)?;
    let repository = vault.view(space).await?.add_remote(name, &folder).await?;
    let remote = Remote { space: space.into(), name: name.into(), folder: folder.to_string_lossy().into(), repository };
    let of = record(space, name);
    let text = |s: &str| Value::String(s.into());
    vault
        .write_local(vec![
            assert(&of, SPACE, text(&remote.space), Pick::Last)?,
            assert(&of, NAME, text(&remote.name), Pick::Last)?,
            assert(&of, FOLDER, text(&remote.folder), Pick::Last)?,
            assert(&of, REPOSITORY, text(&remote.repository), Pick::Last)?,
        ])
        .await?;
    Ok(remote)
}

/// Forget a remote.
// ponytail: Dialog keeps the branch's upstream until another remote replaces it; sync skips forgotten remotes.
pub async fn rm_remote(vault: &Vault, space: &str, name: &str) -> Result<()> {
    let facts = vault.local_claims(ArtifactSelector::new().of(record(space, name))).await?;
    if facts.is_empty() {
        return Err(Error::not_found(format!("remote {name} of {space}")));
    }
    vault.write_local(facts.into_iter().map(Instruction::Retract).collect()).await
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct SyncReport {
    /// Spaces that received changes.
    pub pulled: Vec<String>,
    /// Spaces whose changes went out.
    pub pushed: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// Pull every space that has a remote; with `push`, then push `vault` and
/// `public`. A push that lost a race pulls and pushes again. Followed
/// spaces are only pulled.
pub async fn sync(vault: &Vault, push: bool) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let mut spaces: Vec<String> = remotes(vault).await?.into_iter().map(|r| r.space).collect();
    spaces.dedup();
    for space in spaces {
        let view = vault.view(&space).await?;
        let result: Result<()> = async {
            if view.pull().await? {
                report.pulled.push(space.clone());
            }
            if !push || space.starts_with("peer-") {
                return Ok(());
            }
            for i in 0..SYNC_TRIES {
                match view.push().await {
                    Ok(true) => {
                        report.pushed.push(space.clone());
                        return Ok(());
                    }
                    Ok(false) => return Ok(()),
                    Err(e) if i + 1 == SYNC_TRIES => return Err(e),
                    Err(_) => {
                        view.pull().await?;
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            report.errors.push((space, e.to_string()));
        }
    }
    Ok(report)
}

/// Let another device sync this vault: leave a delegation for it in each
/// of the vault's remote folders.
pub async fn device_add(vault: &Vault, device: &str) -> Result<Vec<PathBuf>> {
    grant(vault, "vault", device).await
}

/// Let someone follow what this device publishes: leave a delegation of the
/// `public` repository for them in each of its remote folders.
// ponytail: grants the whole public repository, write included; narrow to reads when Dialog's read-only claims settle.
pub async fn peer_grant(vault: &Vault, device: &str) -> Result<Vec<PathBuf>> {
    grant(vault, PUBLIC, device).await
}

async fn grant(vault: &Vault, space: &str, device: &str) -> Result<Vec<PathBuf>> {
    let view = vault.view(space).await?;
    let mut out = Vec::new();
    for r in remotes(vault).await?.into_iter().filter(|r| r.space == space) {
        out.push(view.grant(device, Path::new(&r.folder)).await?);
    }
    if out.is_empty() {
        return Err(Error::invalid(format!("the {space} space has no remote; add one first")));
    }
    Ok(out)
}

fn petname_space(name: &str) -> Result<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(Error::invalid(format!("bad petname {name:?}: use lowercase letters, digits and -")));
    }
    Ok(format!("peer-{name}"))
}

/// Follow the vault published in `folder`, under the petname `name`.
pub async fn follow(vault: &Vault, name: &str, folder: &Path) -> Result<Remote> {
    let space = petname_space(name)?;
    let remote = add_remote(vault, &space, name, folder).await?;
    vault.view(&space).await?.pull().await?;
    Ok(remote)
}

/// A followed vault, by petname.
pub async fn followed(vault: &Vault, name: &str) -> Result<Vault> {
    let space = petname_space(name)?;
    if !remotes(vault).await?.iter().any(|r| r.space == space) {
        return Err(Error::not_found(format!("no followed vault named {name}; see `dreams peer list`")));
    }
    vault.view(&space).await
}

/// Copy a note into the `public` repository, then push it.
pub async fn publish(vault: &Vault, href: &str) -> Result<SyncReport> {
    let note = note::get(vault, href).await?;
    let public = vault.view(PUBLIC).await?;
    let old = note::load(&public, &note.entity).await?;
    let mut changes = Vec::new();
    for (the, values) in &note.facts {
        if the == vault::UPDATED || the == vault::BY {
            continue;
        }
        let many = values.len() > 1 || crate::concepts::is_many(the);
        changes.extend(note::replace(&note.entity, the, old.values(the), values, many)?);
    }
    public.write(changes).await?;
    sync(vault, true).await
}

/// A note in a followed vault: `dreams://<petname>/<path>`.
pub async fn get_remote(vault: &Vault, href: &str) -> Result<Option<Note>> {
    let Some(rest) = href.strip_prefix("dreams://") else { return Ok(None) };
    let Some((name, path)) = rest.split_once('/') else { return Ok(None) };
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(note::get(&followed(vault, name).await?, path).await?))
}
