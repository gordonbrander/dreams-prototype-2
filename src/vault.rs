//! The vault: the one place dreams talks to Dialog storage.
//!
//! A vault folder holds two repositories (SPEC §2): `vault`, which syncs,
//! and `local`, which holds per-device state and never gets a remote. Each
//! process opens its own session; commits refresh and retry when another
//! process moved the head first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dialog_artifacts::selector::Constrained;
use dialog_artifacts::history::HistorySelector;
use dialog_artifacts::{Artifact, ArtifactSelector, Changes, Entity, Instruction, Pick, Value};
use dialog_capability::Subject;
use dialog_credentials::{Credential, SignerCredential, VerifierCredential};
use dialog_effects::credential::prelude::*;
use dialog_remote_fs::FsAddress;
use dialog_storage::provider::FileSystem;
use dialog_storage::resource::Resource as _;
use dialog_effects::storage::{Directory, Location};
use dialog_peer::{Allowance, OpenCredential, Peer, Session, SpaceVaultExt as _};
use dialog_query::{Application, ConceptConclusion, ConceptQuery, DeductiveRule, Environment, EvaluationError, Match, Output as _, Scope, Selection};
use dialog_repository::{ACCESS_BRANCH, Branch, Repository, RepositoryExt as _, SiteAddress, contact};
use dialog_storage::provider::storage::{CredentialStore, NativeSpace, Storage};
use dialog_varsig::Principal as _;
use futures_util::TryStreamExt as _;

use crate::error::{Error, Result};

pub type Env = Peer<NativeSpace, Session>;
pub type Selector = ArtifactSelector<Constrained>;

pub const UPDATED: &str = "dreams.note/updated";
pub const BY: &str = "dreams.note/by";

/// Where a remote lives.
#[derive(Debug, Clone)]
pub enum Site {
    Folder(PathBuf),
    S3 { address: dialog_remote_s3::Address, credential: dialog_remote_s3::S3Credential },
    /// A UCAN access service: the delegation chain is the credential.
    Ucan(String),
}

/// The file in a remote folder that names the repository it holds.
pub const REMOTE_MARKER: &str = "dreams-remote";

pub fn delegation_file(folder: &Path, device: &str) -> PathBuf {
    folder.join("delegations").join(format!("{}.ucan", device.replace(':', "_")))
}

fn location(folder: &Path) -> Location {
    Location::at(folder.to_string_lossy().into_owned())
}

/// How many times a commit refreshes and retries after losing a race.
const RETRIES: u32 = 8;

pub struct Vault {
    pub dir: PathBuf,
    /// The space this handle reads and writes: `vault`, `public`, or a followed `peer-<name>`.
    pub space: String,
    peer: Peer<NativeSpace>,
    /// The device's key: it signs revocations of what it delegated.
    key: SignerCredential,
    env: Env,
    repo: Arc<Repository>,
    vault: Branch,
    local: Branch,
}

impl Vault {
    /// Open the vault at `dir`, creating it, its keys and its account on first use.
    pub async fn open(dir: &Path) -> Result<Self> {
        Self::open_as(dir, "dreams").await
    }

    /// Open with a session of its own, named `context`: an agent run writes
    /// as its own session so that its writes can be told apart (SPEC §10.4).
    pub async fn open_as(dir: &Path, context: &str) -> Result<Self> {
        std::fs::create_dir_all(dir.join("dialog"))?;
        let base = Directory::At(dir.join("dialog").to_string_lossy().into_owned());
        let credentials = CredentialStore::<NativeSpace>::default();
        let key = async |name: &str| -> Result<SignerCredential> {
            OpenCredential::open(name).at(base.clone()).perform(&credentials).await.map_err(Error::dialog)
        };
        let system = key("system").await?;
        let me = key("peer").await?;
        let custodian = key("custodian").await?;
        let peer = Peer::new(me.clone())
            .at(Location::new(base.clone(), "home"))
            .space(Repository::from(me.did()).branch(ACCESS_BRANCH))
            .with(Storage::<NativeSpace>::default().owned_by(system.did()))
            .grant(Allowance::storage(&system))
            .base(base)
            .build()
            .await
            .map_err(Error::dialog)?;
        if peer.authority().await.is_err() {
            let account = peer.state().vault("account").create().perform(&peer).await.map_err(Error::dialog)?;
            account.add(custodian.did()).perform(&peer).await.map_err(Error::dialog)?;
            account.delegate(peer.did()).perform(&peer).await.map_err(Error::dialog)?;
        }
        let env =
            peer.session(context.as_bytes()).space(peer.state()).allow(Subject::any()).await.map_err(Error::dialog)?;
        let repo = peer.space("vault").open().perform(&env).await.map_err(Error::dialog)?;
        let vault = repo.branch("main").open().perform(&env).await.map_err(Error::dialog)?;
        let local = peer.space("local").open().perform(&env).await.map_err(Error::dialog)?;
        let local = local.branch("main").open().perform(&env).await.map_err(Error::dialog)?;
        Ok(Vault { dir: dir.to_path_buf(), space: "vault".into(), peer, key: me, env, repo: Arc::new(repo), vault, local })
    }

    /// A handle on another space of this device, sharing this one's session
    /// and local repository: `public` (what this device publishes) or
    /// `peer-<name>` (a followed vault's replica).
    pub async fn view(&self, space: &str) -> Result<Vault> {
        let repo = self.peer.space(space).open().perform(&self.env).await.map_err(Error::dialog)?;
        let vault = repo.branch("main").open().perform(&self.env).await.map_err(Error::dialog)?;
        Ok(Vault {
            dir: self.dir.clone(),
            space: space.into(),
            peer: self.peer.clone(),
            key: self.key.clone(),
            env: self.env.clone(),
            repo: Arc::new(repo),
            vault,
            local: self.local.clone(),
        })
    }

    /// This device's DID: what another device delegates the vault to.
    pub fn device(&self) -> String {
        self.peer.did().to_string()
    }

    /// The vault repository's DID.
    pub fn repository(&self) -> String {
        self.repo.did().to_string()
    }

    /// A delegation of this vault's repository to the device `to`, as bytes.
    pub async fn delegate(&self, to: &str) -> Result<Vec<u8>> {
        let did: dialog_varsig::Did = to.parse().map_err(|e| Error::invalid(format!("not a DID: {to} ({e})")))?;
        // The repository's key is sealed to the account; the device holds the
        // account's delegation, so it re-delegates with its own key.
        let chain = self.peer.access().claim(&*self.repo).delegate(did).perform(&self.env).await.map_err(Error::dialog)?;
        chain.0.to_bytes().map_err(Error::dialog)
    }

    /// Track `site` as the remote `name`: this space's `main` pulls from and
    /// pushes to the `main` of the repository the site holds.
    ///
    /// - A folder names its repository in its `dreams-remote` file. An
    ///   empty folder becomes the space of this repository; a folder another
    ///   device set up needs that device's delegation (`device add`), which
    ///   it leaves in the folder.
    /// - An S3 bucket holds this repository unless `repository` names
    ///   another device's; its delegation then comes as a file (`accept`).
    pub async fn add_remote(&self, name: &str, site: &Site, repository: Option<&str>) -> Result<String> {
        let (address, target) = match site {
            Site::Folder(folder) => {
                std::fs::create_dir_all(folder)?;
                // A delegation another device left here for this one.
                if let Ok(chain) = std::fs::read(delegation_file(folder, &self.device())) {
                    self.accept(&chain).await?;
                }
                let marker = folder.join(REMOTE_MARKER);
                let target = match std::fs::read_to_string(&marker) {
                    Ok(did) => did.trim().to_string(),
                    Err(_) => {
                        // The folder's identity is the repository's public key only.
                        let fs = FileSystem::open(&location(folder)).await.map_err(Error::dialog)?;
                        let verifier = match self.repo.credential() {
                            Credential::Signer(s) => Credential::Verifier(VerifierCredential::from(s.0.verifier())),
                            v => v.clone(),
                        };
                        self.repo.did().credential().key("self").save(verifier).perform(&fs).await.map_err(Error::dialog)?;
                        std::fs::write(&marker, self.repository())?;
                        self.repository()
                    }
                };
                (SiteAddress::Fs(FsAddress::new(location(folder))), target)
            }
            Site::S3 { address, credential } => {
                let address = SiteAddress::S3(address.clone());
                self.peer.secrets().site(&address).save(credential.clone()).perform(&self.peer).await.map_err(Error::dialog)?;
                (address, repository.map_or_else(|| self.repository(), str::to_string))
            }
            Site::Ucan(endpoint) => (
                SiteAddress::Ucan(dialog_remote_ucan::UcanAddress::new(endpoint.clone())),
                repository.map_or_else(|| self.repository(), str::to_string),
            ),
        };
        let did: dialog_varsig::Did = target.parse().map_err(|e| Error::invalid(format!("not a repository DID: {target} ({e})")))?;
        let name = format!("{}-{name}", self.space);
        contact(&did).add_address(address).name(name.as_str()).perform(&self.env).await.map_err(Error::dialog)?;
        let remote = contact(name.as_str()).connect().repository(did).open().perform(&self.env).await.map_err(Error::dialog)?;
        let upstream = remote.branch("main").open().perform(&self.env).await.map_err(Error::dialog)?;
        self.vault.set_upstream(upstream).perform(&self.env).await.map_err(Error::dialog)?;
        Ok(target)
    }

    /// Pull the upstream's changes into the vault. `true` when any came.
    pub async fn pull(&self) -> Result<bool> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        Ok(self.vault.pull().perform(&self.env).await.map_err(Error::dialog)?.is_some())
    }

    /// Push the vault to its upstream. Fails when the upstream moved since the last pull.
    pub async fn push(&self) -> Result<bool> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        Ok(self.vault.push().perform(&self.env).await.map_err(Error::dialog)?.is_some())
    }

    /// Leave a delegation of this space's repository to `device` in `folder`,
    /// where `add_remote` on that device picks it up.
    pub async fn grant(&self, device: &str, folder: &Path) -> Result<PathBuf> {
        let file = delegation_file(folder, device);
        std::fs::create_dir_all(file.parent().expect("a folder"))?;
        std::fs::write(&file, self.delegate(device).await?)?;
        Ok(file)
    }

    /// A signed revocation of the delegation in `chain` that this device
    /// issued: what an access service is posted to refuse it from then on.
    pub async fn revocation(&self, chain: &[u8]) -> Result<Vec<u8>> {
        let chain = dialog_ucan_core::DelegationChain::try_from(chain).map_err(Error::dialog)?;
        let mine = self.peer.did();
        let issued = chain
            .proofs()
            .find(|d| *d.issuer() == mine)
            .ok_or_else(|| Error::invalid("this device issued no delegation in that chain"))?;
        let cid = issued.to_cid();
        let revoker = dialog_credentials::Signer::from(self.key.clone());
        let revocation = dialog_ucan_core::revocation::builder::RevocationBuilder::new(revoker, cid)
            .try_build()
            .await
            .map_err(Error::dialog)?;
        let delegations = std::collections::HashMap::from([(cid, Arc::new(issued.clone()))]);
        let document = dialog_ucan_core::RevocationChain::assemble(revocation, delegations).map_err(Error::dialog)?;
        document.to_bytes().map_err(Error::dialog)
    }

    /// Keep a delegation another device made for this one.
    pub async fn accept(&self, chain: &[u8]) -> Result<()> {
        let chain = dialog_ucan_core::DelegationChain::try_from(chain).map_err(Error::dialog)?;
        self.peer.access().save(chain.into()).perform(&self.env).await.map_err(Error::dialog)?;
        Ok(())
    }

    /// The DID this process writes as: the value of `dreams.note/by`.
    pub fn session(&self) -> String {
        self.env.did().to_string()
    }

    /// Claims in the vault matching `selector`, at the current head.
    pub async fn claims(&self, selector: Selector) -> Result<Vec<Artifact>> {
        read(&self.env, &self.vault, selector).await
    }

    /// Claims in the local repository.
    pub async fn local_claims(&self, selector: Selector) -> Result<Vec<Artifact>> {
        read(&self.env, &self.local, selector).await
    }

    /// The values of `the` on `of`.
    pub async fn values(&self, of: &Entity, the: &str) -> Result<Vec<Value>> {
        let claims = self.claims(ArtifactSelector::new().of(of.clone()).the(relation(the)?)).await?;
        Ok(claims.into_iter().map(|a| a.is).collect())
    }

    /// Entities whose `the` is `is`.
    pub async fn having(&self, the: &str, is: Value) -> Result<Vec<Entity>> {
        let claims = self.claims(ArtifactSelector::new().the(relation(the)?).is(is)).await?;
        Ok(claims.into_iter().map(|a| a.of).collect())
    }

    /// Commit `changes` to the vault. Every entity that gets an assertion is
    /// stamped with `updated` and `by` (SPEC §3.2): search catch-up and
    /// `changes` rely on it. Pure retractions stamp nothing, so a deleted
    /// entity leaves no facts behind.
    pub async fn write(&self, mut changes: Vec<Instruction>) -> Result<()> {
        let mut touched: Vec<Entity> = Vec::new();
        for change in &changes {
            if let Instruction::Assert(a, _) = change
                && !touched.contains(&a.of)
            {
                touched.push(a.of.clone());
            }
        }
        let now = Value::UnsignedInt(now_ms().into());
        let by = Value::String(self.session());
        for of in touched {
            changes.push(assert(&of, UPDATED, now.clone(), Pick::Max)?);
            changes.push(assert(&of, BY, by.clone(), Pick::Last)?);
        }
        commit(&self.env, &self.vault, changes).await
    }

    /// Commit `changes` to the vault, unstamped: for values, such as
    /// versions, whose facts must be exactly what was hashed.
    pub async fn write_unstamped(&self, changes: Vec<Instruction>) -> Result<()> {
        commit(&self.env, &self.vault, changes).await
    }

    /// Commit `changes` to the local repository, unstamped.
    pub async fn write_local(&self, changes: Vec<Instruction>) -> Result<()> {
        commit(&self.env, &self.local, changes).await
    }

    /// Commit `changes` to the local repository once, on the head the last
    /// `local_claims` read: a compare-and-swap. `false` when another
    /// process moved the head since; read again and decide again.
    pub async fn swap_local(&self, changes: Vec<Instruction>) -> Result<bool> {
        let batch: Changes = changes.into_iter().collect();
        match self.local.transaction().integrate(batch).commit().publish().perform(&self.env).await {
            Ok(_) => Ok(true),
            Err(_) => {
                self.local.refresh(&self.env).await.map_err(Error::dialog)?;
                Ok(false)
            }
        }
    }

    /// The vault's head revision, for noticing that something changed.
    pub async fn head(&self) -> Result<String> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        Ok(format!("{:?}", self.vault.revision()))
    }

    /// Run a concept query, with `rule` layered over the vault for this
    /// query only when given (SPEC §8.3).
    pub async fn select(&self, query: ConceptQuery, rule: Option<DeductiveRule>) -> Result<Vec<ConceptConclusion>> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        let rows = match rule {
            Some(rule) => self.vault.query().with(rule).select(query).perform(&self.env).try_vec().await,
            None => self.vault.select(query).perform(&self.env).try_vec().await,
        };
        rows.map_err(|e| Error::query(e, None))
    }

    /// Every binding of the variables in `rule`'s body, one `Match` per
    /// solution. Unlike a concept query, rows are tuples: a head reads its
    /// fields back per entity and loses which values went together.
    pub async fn solve(&self, rule: DeductiveRule) -> Result<Vec<Match>> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        dialog_query::Output::try_vec(self.vault.select(Body(rule)).perform(&self.env)).await.map_err(|e| Error::query(e, None))
    }

    /// Install a deductive rule: it is stored as facts and syncs.
    pub async fn install(&self, rule: DeductiveRule) -> Result<()> {
        let mut backoff = Backoff::new();
        loop {
            let tried = self.vault.transaction().assert(rule.clone()).commit().publish().perform(&self.env).await;
            match tried {
                Ok(_) => return Ok(()),
                Err(e) => backoff.wait(&self.env, &self.vault, e).await?,
            }
        }
    }

    /// Every value `the` of `of` has held, oldest first, with whether it
    /// was asserted or retracted.
    // ponytail: scans the whole history region; filter by key range if Dialog exposes one.
    pub async fn history(&self, of: &Entity, the: Option<&str>) -> Result<Vec<(String, bool, Value)>> {
        self.vault.refresh(&self.env).await.map_err(Error::dialog)?;
        let the = the.map(relation).transpose()?;
        let history = self.vault.history(&self.env);
        let records: Vec<_> = history.select(HistorySelector::All).try_collect().await.map_err(Error::dialog)?;
        Ok(records
            .into_iter()
            .filter(|(_, r)| &r.claim().of == of && the.as_ref().is_none_or(|t| &r.claim().the == t))
            .map(|(_, r)| (r.claim().the.to_string(), r.is_assertion(), r.claim().is.clone()))
            .collect())
    }
}

/// A rule's body as a query whose conclusions are its raw bindings.
#[derive(Clone)]
struct Body(DeductiveRule);

impl Application for Body {
    type Conclusion = Match;

    fn evaluate<'a, E, M: Selection + 'a>(self, selection: M, env: &'a E) -> impl Selection + 'a
    where
        E: Scope<'a>,
    {
        self.0.plan(&Environment::new()).evaluate(selection, env)
    }

    fn realize(&self, input: Match) -> std::result::Result<Match, EvaluationError> {
        Ok(input)
    }
}

/// Parse a relation with Dialog's query-layer rules (lowercase kebab-case
/// names, 64 bytes at most), which are stricter than storage's: a fact
/// stored under a name the query layer rejects could never be queried.
pub fn relation(the: &str) -> Result<dialog_artifacts::Relation> {
    let bad = |e: &dyn std::fmt::Display| Error::invalid(format!("bad relation {the:?}: {e}"));
    the.parse::<dialog_query::Relation>().map_err(|e| bad(&e))?;
    the.parse().map_err(|e| bad(&e))
}

/// An assertion of `the` = `is` on `of`, under `pick`.
pub fn assert(of: &Entity, the: &str, is: Value, pick: Pick) -> Result<Instruction> {
    Ok(Instruction::Assert(Artifact { the: relation(the)?, of: of.clone(), is, cause: None }, pick))
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

async fn read(env: &Env, branch: &Branch, selector: Selector) -> Result<Vec<Artifact>> {
    // ponytail: refresh on every read so a long-running server sees other processes' commits.
    branch.refresh(env).await.map_err(Error::dialog)?;
    let stream = branch.claims().select(selector).to_owned().perform(env).await.map_err(Error::dialog)?;
    stream.try_collect().await.map_err(Error::dialog)
}

async fn commit(env: &Env, branch: &Branch, changes: Vec<Instruction>) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut backoff = Backoff::new();
    loop {
        let batch: Changes = changes.iter().map(copy).collect();
        match branch.transaction().integrate(batch).commit().publish().perform(env).await {
            Ok(_) => return Ok(()),
            Err(e) => backoff.wait(env, branch, e).await?,
        }
    }
}

fn copy(change: &Instruction) -> Instruction {
    match change {
        Instruction::Assert(a, pick) => Instruction::Assert(a.clone(), pick.clone()),
        Instruction::Retract(a) => Instruction::Retract(a.clone()),
    }
}

/// Retrying a commit: a stale head or a busy lock means another process
/// committed first, so wait, refresh the head and try again.
// Plain loops, not a retry(closure) helper: async closures capturing
// references make futures rustc can't prove Send, which MCP handlers need.
struct Backoff {
    tries: u32,
    wait: Duration,
}

impl Backoff {
    fn new() -> Self {
        Backoff { tries: 0, wait: Duration::from_millis(10) }
    }

    async fn wait(&mut self, env: &Env, branch: &Branch, error: impl std::fmt::Display) -> Result<()> {
        self.tries += 1;
        if self.tries > RETRIES {
            return Err(Error::dialog(error));
        }
        tokio::time::sleep(self.wait).await;
        self.wait *= 2;
        branch.refresh(env).await.map_err(Error::dialog)
    }
}
