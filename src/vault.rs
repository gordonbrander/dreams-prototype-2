//! The vault: the one place dreams talks to Dialog storage.
//!
//! A vault folder holds two repositories (SPEC §2): `vault`, which syncs,
//! and `local`, which holds per-device state and never gets a remote. Each
//! process opens its own session; commits refresh and retry when another
//! process moved the head first.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dialog_artifacts::selector::Constrained;
use dialog_artifacts::history::HistorySelector;
use dialog_artifacts::{Artifact, ArtifactSelector, Changes, Entity, Instruction, Pick, Value};
use dialog_capability::Subject;
use dialog_credentials::SignerCredential;
use dialog_effects::storage::{Directory, Location};
use dialog_peer::{Allowance, OpenCredential, Peer, Session, SpaceVaultExt as _};
use dialog_query::{Application, ConceptConclusion, ConceptQuery, DeductiveRule, Environment, EvaluationError, Match, Output as _, Scope, Selection};
use dialog_repository::{ACCESS_BRANCH, Branch, Repository, RepositoryExt as _};
use dialog_storage::provider::storage::{CredentialStore, NativeSpace, Storage};
use dialog_varsig::Principal as _;
use futures_util::TryStreamExt as _;

use crate::error::{Error, Result};

pub type Env = Peer<NativeSpace, Session>;
pub type Selector = ArtifactSelector<Constrained>;

pub const UPDATED: &str = "dreams.note/updated";
pub const BY: &str = "dreams.note/by";

/// How many times a commit refreshes and retries after losing a race.
const RETRIES: u32 = 8;

pub struct Vault {
    pub dir: PathBuf,
    env: Env,
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
        let open = async |name: &str| -> Result<Branch> {
            let repo = peer.space(name).open().perform(&env).await.map_err(Error::dialog)?;
            repo.branch("main").open().perform(&env).await.map_err(Error::dialog)
        };
        let vault = open("vault").await?;
        let local = open("local").await?;
        Ok(Vault { dir: dir.to_path_buf(), env, vault, local })
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

    /// Commit `changes` to the local repository, unstamped.
    pub async fn write_local(&self, changes: Vec<Instruction>) -> Result<()> {
        commit(&self.env, &self.local, changes).await
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
