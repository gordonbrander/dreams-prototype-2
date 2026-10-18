//! Scheduled tasks (SPEC §10): notes with the Task concept's attributes
//! that wake an agent every interval, or when the rows of their `when`
//! query change. A task syncs, but runs only where it is deployed.
//!
//! A deploy freezes the task and its runner as **versions**: entities whose
//! id is the hash of their own facts, linked to what they freeze by
//! `dreams.version/source`. The deploy, in the local repository, points
//! at the task version, and the scheduler runs that version, never the
//! live note. An edit marks the task `changed`; the deployed version keeps
//! running until the next deploy. Each run writes a receipt, a Run entity
//! linking the version that ran, which syncs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use dialog_artifacts::{ArtifactSelector, Entity, Instruction, Pick, Value};
use dialog_repository::schema::EntityExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

use crate::error::{Error, Result};
use crate::notation::{self, MAX_LIMIT};
use crate::note::{self, Note, Put};
use crate::runner::{self, Context, Outcome, Runner, parse_duration};
use crate::text::truncate_bytes;
use crate::value::{self, to_json};
use crate::vault::{self, Vault, assert, now_ms};

pub const RUNNER: &str = "dreams.task/runner";
pub const EVERY: &str = "dreams.task/every";
pub const PROMPT: &str = "dreams.task/prompt";
pub const WHEN: &str = "dreams.task/when";
pub const CWD: &str = "dreams.task/cwd";
pub const RUN_TASK: &str = "dreams.run/task";
pub const RUN_VERSION: &str = "dreams.run/version";

/// Agent output above this is cut, and the run records the cut.
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
/// A scheduler that has not ticked for this long is taken as stopped.
pub const STALE_AFTER_MS: u64 = 5 * 60 * 1000;
/// How often a lease is re-read and re-tried when another process races for it.
const CLAIM_TRIES: u32 = 8;

/// A task's attributes.
#[derive(Debug, Clone)]
pub struct Task {
    pub entity: Entity,
    pub path: String,
    pub title: Option<String>,
    pub runner: Entity,
    pub every: String,
    pub every_secs: u64,
    pub prompt: String,
    /// The trigger query, as notation text.
    pub when: Option<String>,
    pub cwd: Option<String>,
}

impl Task {
    pub fn from_note(note: &Note) -> Result<Task> {
        let path = note.path().unwrap_or_default().to_string();
        let runner = match note.first(RUNNER) {
            Some(Value::Entity(e)) => e.clone(),
            _ => return Err(Error::invalid(format!("{path}: {RUNNER} must name a runner"))),
        };
        let every = note.text(EVERY).unwrap_or_default().to_string();
        Ok(Task {
            entity: note.entity.clone(),
            title: note.text(note::TITLE).map(str::to_string),
            runner,
            every_secs: parse_duration(&every)?,
            every,
            prompt: note.text(PROMPT).unwrap_or_default().to_string(),
            when: note.text(WHEN).map(str::to_string),
            cwd: note.text(CWD).map(str::to_string),
            path,
        })
    }

    /// Where its receipts go: `runs/<path without extension>/`.
    pub fn runs_prefix(&self) -> String {
        let stem = Path::new(&self.path).with_extension("");
        format!("runs/{}/", stem.to_string_lossy())
    }
}

/// Every task in the vault: notes with a runner. Versions have no path.
pub async fn entities(vault: &Vault) -> Result<Vec<Entity>> {
    let mut out = Vec::new();
    for a in vault.claims(ArtifactSelector::new().the(vault::relation(RUNNER)?)).await? {
        if !out.contains(&a.of) && !vault.values(&a.of, note::PATH).await?.is_empty() {
            out.push(a.of);
        }
    }
    Ok(out)
}

/// A live task and its runner, and the version they make.
pub struct Loaded {
    pub task: Task,
    pub runner: std::result::Result<Runner, String>,
    /// The task version, and the facts that write it with its runner
    /// version. None when the runner is missing or broken.
    pub version: Option<(Entity, Vec<Instruction>)>,
}

pub async fn load(vault: &Vault, entity: &Entity) -> Result<Loaded> {
    let note = note::load(vault, entity).await?;
    if note.facts.is_empty() {
        return Err(Error::not_found(entity.to_string()));
    }
    let task = Task::from_note(&note)?;
    let runner_note = note::load(vault, &task.runner).await?;
    let runner = if runner_note.facts.is_empty() {
        Err(format!("the runner of {} does not exist", task.path))
    } else {
        Runner::from_note(&runner_note).map_err(|e| e.to_string())
    };
    let version = match runner {
        Ok(_) => Some(make_version(&note, &runner_note)?),
        Err(_) => None,
    };
    Ok(Loaded { task, runner, version })
}

// ---- versions -------------------------------------------------------------

/// Links a version to the task or runner it freezes.
pub const SOURCE: &str = "dreams.version/source";

/// What a version copies from its task, besides the runner.
const TASK_FIELDS: [&str; 4] = [EVERY, PROMPT, WHEN, CWD];
const RUNNER_FIELDS: [&str; 2] = [runner::COMMAND, runner::TIMEOUT];

/// A version's id: the hash of every one of its facts.
fn version_id(facts: &[(String, Value)]) -> Entity {
    let mut pairs: Vec<(&str, String)> = facts.iter().map(|(the, is)| (the.as_str(), to_json(is).to_string())).collect();
    pairs.sort();
    Entity::of(&("dreams.version", pairs))
}

/// Freeze the current value of each of `fields` of `note`, plus `extra`.
fn freeze(note: &Note, fields: &[&str], extra: Option<(&str, Value)>) -> Result<(Entity, Vec<Instruction>)> {
    let mut facts = vec![(SOURCE.to_string(), Value::Entity(note.entity.clone()))];
    facts.extend(fields.iter().filter_map(|the| note.first(the).map(|v| (the.to_string(), v.clone()))));
    facts.extend(extra.map(|(the, v)| (the.to_string(), v)));
    let id = version_id(&facts);
    let changes = facts.into_iter().map(|(the, is)| assert(&id, &the, is, Pick::Last)).collect::<Result<_>>()?;
    Ok((id, changes))
}

/// The version of `task` with `runner`, and the facts that write both
/// versions. The task version links the runner version, so its id
/// covers the runner's command too.
fn make_version(task: &Note, runner: &Note) -> Result<(Entity, Vec<Instruction>)> {
    let (r, mut changes) = freeze(runner, &RUNNER_FIELDS, None)?;
    let (t, more) = freeze(task, &TASK_FIELDS, Some((RUNNER, Value::Entity(r))))?;
    changes.extend(more);
    Ok((t, changes))
}

/// A version's facts, refused when they no longer hash to its id.
async fn thaw(vault: &Vault, version: &Entity) -> Result<Note> {
    let note = note::load(vault, version).await?;
    let facts: Vec<(String, Value)> =
        note.facts.iter().flat_map(|(the, vs)| vs.iter().map(move |v| (the.clone(), v.clone()))).collect();
    if facts.is_empty() || version_id(&facts) != *version {
        return Err(Error::invalid(format!("version {version} is missing or was altered; deploy again")));
    }
    Ok(note)
}

/// The task and runner a version froze. The task keeps the live task's
/// entity, path and title, so runs keep its session and receipt folder.
/// A deleted runner stops the version too.
async fn load_version(vault: &Vault, version: &Entity, live: &Note) -> Result<(Task, Runner)> {
    let mut task = Task::from_note(&thaw(vault, version).await?)?;
    let runner = thaw(vault, &task.runner).await?;
    if let Some(Value::Entity(source)) = runner.first(SOURCE)
        && vault.values(source, note::PATH).await?.is_empty()
    {
        return Err(Error::invalid("its runner was deleted"));
    }
    task.entity = live.entity.clone();
    task.path = live.path().unwrap_or_default().to_string();
    task.title = live.text(note::TITLE).map(str::to_string);
    Ok((task, Runner::from_note(&runner)?))
}

// ---- deploy state (local repository) -----------------------------------

const VERSION: &str = "dreams.deploy/version";
const ENABLED: &str = "dreams.deploy/enabled";
const ENABLED_AT: &str = "dreams.deploy/enabled-at";
const LAST_RUN: &str = "dreams.deploy/last-run";
const LEASE_UNTIL: &str = "dreams.deploy/lease-until";
const REQUESTED: &str = "dreams.deploy/requested";
const ROWS: &str = "dreams.deploy/rows";
const SESSION: &str = "dreams.deploy/session";
const PENDING: &str = "dreams.deploy/pending";
const LAST_TICK: &str = "dreams.scheduler/last-tick";

/// This device's schedule for one task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct State {
    /// The task version this device runs; empty before a deploy.
    pub version: String,
    pub enabled: bool,
    /// Unix ms. The schedule counts from here until the first run.
    pub enabled_at: u64,
    /// Unix ms when the last run started; 0 before the first.
    pub last_run: u64,
    /// Unix ms; a run holds the task until then.
    pub lease_until: u64,
    /// The next tick fires the task, whatever its schedule says.
    pub requested: bool,
    /// The `when` query's rows at the last run.
    #[serde(skip)]
    pub rows: Vec<Json>,
    /// The DID the task's runs write as.
    #[serde(skip)]
    pub session: String,
    /// The run in progress: what to record when its receipt exists.
    #[serde(skip)]
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub run: String,
    pub started: u64,
    pub rows: Vec<Json>,
}

fn state_entity(task: &Entity) -> Entity {
    Entity::of(&("dreams.deploy", task.to_string()))
}

pub async fn state(vault: &Vault, task: &Entity) -> Result<Option<State>> {
    let claims = vault.local_claims(ArtifactSelector::new().of(state_entity(task))).await?;
    if claims.is_empty() {
        return Ok(None);
    }
    let facts: BTreeMap<String, Value> = claims.into_iter().map(|a| (a.the.to_string(), a.is)).collect();
    let text = |the: &str| facts.get(the).and_then(value::text).unwrap_or_default().to_string();
    let n = |the: &str| facts.get(the).and_then(value::natural).unwrap_or_default();
    let yes = |the: &str| facts.get(the) == Some(&Value::Boolean(true));
    let rows = text(ROWS);
    let pending = text(PENDING);
    Ok(Some(State {
        version: text(VERSION),
        enabled: yes(ENABLED),
        enabled_at: n(ENABLED_AT),
        last_run: n(LAST_RUN),
        lease_until: n(LEASE_UNTIL),
        requested: yes(REQUESTED),
        rows: if rows.is_empty() { Vec::new() } else { serde_json::from_str(&rows)? },
        session: text(SESSION),
        pending: if pending.is_empty() { None } else { Some(serde_json::from_str(&pending)?) },
    }))
}

/// Every fact of `state`, as assertions.
// ponytail: rewrites every field, rows included, on each change; split rows out if they grow large.
fn state_changes(task: &Entity, s: &State) -> Result<Vec<Instruction>> {
    let of = state_entity(task);
    let text = |s: String| Value::String(s);
    let n = |n: u64| Value::UnsignedInt(n.into());
    let pending = match &s.pending {
        Some(p) => serde_json::to_string(p)?,
        None => String::new(),
    };
    [
        ("dreams.deploy/task", Value::Entity(task.clone())),
        (VERSION, text(s.version.clone())),
        (ENABLED, Value::Boolean(s.enabled)),
        (ENABLED_AT, n(s.enabled_at)),
        (LAST_RUN, n(s.last_run)),
        (LEASE_UNTIL, n(s.lease_until)),
        (REQUESTED, Value::Boolean(s.requested)),
        (ROWS, text(serde_json::to_string(&s.rows)?)),
        (SESSION, text(s.session.clone())),
        (PENDING, text(pending)),
    ]
    .into_iter()
    .map(|(the, v)| assert(&of, the, v, Pick::Last))
    .collect()
}

async fn save(vault: &Vault, task: &Entity, s: &State) -> Result<()> {
    vault.write_local(state_changes(task, s)?).await
}

// ---- deploy -------------------------------------------------------------

/// One task to deploy: exactly what will run, for a person to confirm.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Deploy {
    pub path: String,
    pub entity: String,
    /// The task version a deploy freezes: the id of exactly what is shown.
    pub version: String,
    pub runner: String,
    pub command: Vec<String>,
    pub every: String,
    pub when: Option<String>,
    pub prompt: String,
    pub cwd: Option<String>,
    pub timeout: String,
    /// What is deployed now: dormant, deployed, disabled, or changed (another version is deployed).
    pub now: String,
}

impl Deploy {
    /// The text a person confirms.
    pub fn describe(&self) -> String {
        let mut text = format!("{}  (now {})\n", self.path, self.now);
        let _ = writeln!(text, "  every:   {}", self.every);
        if let Some(w) = &self.when {
            let _ = writeln!(text, "  when:    {}", w.replace('\n', " "));
        }
        let _ = writeln!(text, "  runner:  {}", self.runner);
        let _ = writeln!(text, "  command: {}", self.command.join(" "));
        let _ = writeln!(text, "  cwd:     {}", self.cwd.as_deref().unwrap_or("workspace (default)"));
        let _ = writeln!(text, "  timeout: {}", self.timeout);
        text.push_str("  prompt:\n");
        for line in self.prompt.lines() {
            let _ = writeln!(text, "    {line}");
        }
        text
    }
}

/// dormant (never deployed here), deployed, or disabled.
fn status(state: Option<&State>) -> &'static str {
    match state {
        None => "dormant",
        Some(s) if s.version.is_empty() => "dormant",
        Some(s) if s.enabled => "deployed",
        Some(_) => "disabled",
    }
}

/// Whether the live task makes a version other than the deployed one.
fn changed(state: Option<&State>, live: Option<&str>) -> bool {
    state.is_some_and(|s| !s.version.is_empty() && live != Some(s.version.as_str()))
}

/// What a deploy would change: one task, or every task that is not
/// deployed and enabled as it is now.
pub async fn plan(vault: &Vault, href: Option<&str>) -> Result<Vec<Deploy>> {
    let entities = match href {
        Some(h) => vec![note::resolve(vault, h).await?],
        None => entities(vault).await?,
    };
    let mut out = Vec::new();
    for entity in entities {
        let Loaded { task, runner, version } = load(vault, &entity).await?;
        let runner = runner.map_err(Error::invalid)?;
        let version = version.map(|(v, _)| v.to_string()).unwrap_or_default();
        let current = state(vault, &entity).await?;
        if current.as_ref().is_some_and(|s| s.enabled && s.version == version) {
            continue;
        }
        if let Some(when) = &task.when {
            notation::parse(&Json::String(when.clone()))?;
        }
        let now = if changed(current.as_ref(), Some(&version)) { "changed" } else { status(current.as_ref()) };
        out.push(Deploy {
            path: task.path.clone(),
            entity: entity.to_string(),
            now: now.into(),
            version,
            runner: runner.path,
            command: runner.command,
            every: task.every,
            when: task.when,
            prompt: task.prompt,
            cwd: task.cwd,
            timeout: runner.timeout,
        });
    }
    Ok(out)
}

/// Whether every task in `plan` still makes the version shown.
pub async fn is_current(vault: &Vault, plan: &[Deploy]) -> Result<bool> {
    for d in plan {
        match load(vault, &value::entity(&d.entity)?).await {
            Ok(l) if l.version.as_ref().is_some_and(|(v, _)| v.to_string() == d.version) => {}
            Ok(_) | Err(Error::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Deploy a confirmed plan: write each version to the vault, and point
/// this device's deploy at it. Fails when a task or runner changed since
/// the plan was made. A first deploy starts the schedule now, and takes
/// the `when` query's current rows as seen, so it does not fire on
/// history; a redeploy keeps the rows unless `when` changed.
pub async fn apply(vault: &Vault, plan: &[Deploy]) -> Result<Vec<String>> {
    if !is_current(vault, plan).await? {
        return Err(Error::invalid("a task or runner changed since you confirmed; deploy again"));
    }
    for d in plan {
        let entity = value::entity(&d.entity)?;
        if let Some((_, facts)) = load(vault, &entity).await?.version {
            vault.write_unstamped(facts).await?;
        }
        let mut s = state(vault, &entity).await?.unwrap_or(State { enabled_at: now_ms(), ..Default::default() });
        let old_when = match value::entity(&s.version) {
            Ok(v) if !s.version.is_empty() => thaw(vault, &v).await.ok().and_then(|n| n.text(WHEN).map(str::to_string)),
            _ => None,
        };
        if s.version.is_empty() || old_when != d.when {
            s.rows = match &d.when {
                Some(w) => rows(vault, w).await?,
                None => Vec::new(),
            };
        }
        s.version = d.version.clone();
        s.enabled = true;
        save(vault, &entity, &s).await?;
    }
    Ok(plan.iter().map(|d| d.path.clone()).collect())
}

/// Stop running a task on this device. It keeps its rows.
pub async fn disable(vault: &Vault, href: &str) -> Result<()> {
    let entity = note::resolve(vault, href).await?;
    let mut s = state(vault, &entity).await?.ok_or_else(|| Error::invalid(format!("{href} is not deployed here")))?;
    s.enabled = false;
    save(vault, &entity, &s).await
}

/// Ask the scheduler to fire a deployed task on its next tick.
pub async fn request_run(vault: &Vault, href: &str) -> Result<()> {
    let entity = note::resolve(vault, href).await?;
    let mut s = state(vault, &entity)
        .await?
        .filter(|s| s.enabled)
        .ok_or_else(|| Error::invalid(format!("{href} is not deployed here; deploy it first")))?;
    s.requested = true;
    save(vault, &entity, &s).await
}

// ---- evaluation -----------------------------------------------------------

/// A `when` query's rows, sorted so that two runs compare.
async fn rows(vault: &Vault, when: &str) -> Result<Vec<Json>> {
    let mut rows: Vec<Json> =
        notation::query(vault, &Json::String(when.into()), MAX_LIMIT).await?.into_iter().map(Json::Object).collect();
    rows.sort_by_key(|r| r.to_string());
    Ok(rows)
}

/// Everything `task list` shows and `tick` decides on.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Evaluation {
    pub path: String,
    pub entity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// dormant (never deployed here), deployed, or disabled.
    pub status: String,
    /// The task or its runner was edited since the deploy. The deployed
    /// version keeps running; the edit runs after the next deploy.
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    pub running: bool,
    /// The interval has passed since the last run.
    pub time_due: bool,
    /// The next tick fires it.
    pub due: bool,
    /// Rows of the `when` query that appeared since the last run.
    pub added: Vec<Json>,
    /// Rows that went away.
    pub removed: Vec<Json>,
    /// Why it cannot run: its runner or its query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub task: Option<Task>,
    #[serde(skip)]
    #[schemars(skip)]
    pub runner: Option<Runner>,
    /// The task version that runs: the deployed one, or for a task not
    /// deployed here, the one the live task makes. Empty without a runner.
    #[serde(skip)]
    #[schemars(skip)]
    pub version: String,
    /// `version` is stored already; else firing writes it first.
    #[serde(skip)]
    #[schemars(skip)]
    pub stored: bool,
    /// The query's rows now, when they were read: the next `rows` once a run finishes.
    #[serde(skip)]
    #[schemars(skip)]
    pub rows: Option<Vec<Json>>,
}

/// Whether any entity in `row` was last written by `session`.
async fn written_by(vault: &Vault, row: &Json, session: &str) -> Result<bool> {
    for v in row.as_object().into_iter().flat_map(|r| r.values()) {
        if let Some(s) = v.as_str()
            && s.starts_with("did:")
            && let Ok(e) = value::entity(s)
            && vault.values(&e, vault::BY).await?.iter().any(|b| value::text(b) == Some(session))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn evaluate(vault: &Vault, entity: &Entity, now: u64) -> Result<Evaluation> {
    let note = note::load(vault, entity).await?;
    if note.facts.is_empty() {
        return Err(Error::not_found(entity.to_string()));
    }
    let state = state(vault, entity).await?;
    let live = load(vault, entity).await;
    let live_version = live.as_ref().ok().and_then(|l| l.version.as_ref()).map(|(v, _)| v.to_string());
    let deployed = state.as_ref().map(|s| s.version.clone()).filter(|v| !v.is_empty());
    // A deployed task runs its version; any other runs as it is now.
    let (run, version, stored) = match &deployed {
        Some(v) => {
            let run = match value::entity(v) {
                Ok(e) => load_version(vault, &e, &note).await,
                Err(e) => Err(e),
            };
            (run.map_err(|e| e.to_string()), v.clone(), true)
        }
        None => {
            let run = match &live {
                Ok(l) => l.runner.clone().map(|r| (l.task.clone(), r)),
                Err(e) => Err(e.to_string()),
            };
            (run, live_version.clone().unwrap_or_default(), false)
        }
    };
    let mut error = run.as_ref().err().cloned();
    let (task, runner) = match run {
        Ok((t, r)) => (Some(t), Some(r)),
        Err(_) => (live.as_ref().ok().map(|l| l.task.clone()), None),
    };
    let status = status(state.as_ref());
    let (mut running, mut time_due, mut due) = (false, false, false);
    let (mut added, mut removed, mut rows_now) = (Vec::new(), Vec::new(), None);
    if let (Some(s), Some(task)) = (&state, &task) {
        running = s.lease_until > now;
        let since = if s.last_run > 0 { s.last_run } else { s.enabled_at };
        time_due = now.saturating_sub(since) >= task.every_secs * 1000;
        let mut changed = true;
        if let Some(when) = &task.when
            && status == "deployed"
            && error.is_none()
            && (time_due || s.requested)
        {
            match rows(vault, when).await {
                Ok(now_rows) => {
                    for r in now_rows.iter().filter(|r| !s.rows.contains(r)) {
                        if s.session.is_empty() || !written_by(vault, r, &s.session).await? {
                            added.push(r.clone());
                        }
                    }
                    for r in s.rows.iter().filter(|r| !now_rows.contains(r)) {
                        if s.session.is_empty() || !written_by(vault, r, &s.session).await? {
                            removed.push(r.clone());
                        }
                    }
                    changed = !added.is_empty() || !removed.is_empty();
                    rows_now = Some(now_rows);
                }
                Err(e) => error = Some(format!("the when query failed: {e}")),
            }
        }
        due = status == "deployed" && !running && error.is_none() && (s.requested || (time_due && changed));
    }
    Ok(Evaluation {
        path: note.path().unwrap_or_default().into(),
        entity: entity.to_string(),
        title: note.text(note::TITLE).map(str::to_string),
        status: status.into(),
        changed: changed(state.as_ref(), live_version.as_deref()),
        state,
        running,
        time_due,
        due,
        added,
        removed,
        error,
        task,
        runner,
        version,
        stored,
        rows: rows_now,
    })
}

/// The prompt as the agent receives it: the task's text, then, for a task
/// with `when`, the rows that changed.
pub fn prompt_text(eval: &Evaluation) -> String {
    let Some(task) = &eval.task else { return String::new() };
    let mut text = task.prompt.clone();
    if task.when.is_none() {
        return text;
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n---\nChanged since last run:\n");
    for r in &eval.added {
        let _ = writeln!(text, "+ {r}");
    }
    for r in &eval.removed {
        let _ = writeln!(text, "- {r}");
    }
    text
}

// ---- firing -------------------------------------------------------------

/// The result of one firing.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Fired {
    pub task: String,
    pub run: String,
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Take the lease: a compare-and-swap on the local repository. `None` when
/// another process holds it, or ran the task since `eval`. `force` takes
/// it anyway. A task with no state gets a disabled one, so a manual run of
/// a dormant task does not start its schedule.
async fn claim(vault: &Vault, eval: &Evaluation, task: &Task, now: u64, force: bool) -> Result<Option<Pending>> {
    let lease = eval.runner.as_ref().map_or(0, |r| r.timeout_secs) * 1000 + 60_000;
    for _ in 0..CLAIM_TRIES {
        let current = state(vault, &task.entity).await?;
        let mut s = current.clone().unwrap_or(State { enabled_at: now, ..Default::default() });
        let seen = eval.state.as_ref().map_or(0, |s| s.last_run);
        if !force && (s.lease_until > now || s.last_run != seen) {
            return Ok(None);
        }
        let pending = Pending {
            run: format!("{}{}.md", task.runs_prefix(), uuid::Uuid::now_v7()),
            started: now,
            rows: eval.rows.clone().unwrap_or_else(|| s.rows.clone()),
        };
        s.lease_until = now + lease;
        s.requested = false;
        s.pending = Some(pending.clone());
        if vault.swap_local(state_changes(&task.entity, &s)?).await? {
            return Ok(Some(pending));
        }
    }
    Ok(None)
}

/// Record a finished run in the local repository: the rows it consumed
/// become the ones seen, and the lease is released.
async fn settle(vault: &Vault, task: &Entity, pending: &Pending, session: Option<String>) -> Result<()> {
    let mut s = state(vault, task).await?.unwrap_or_default();
    s.last_run = pending.started;
    s.rows = pending.rows.clone();
    s.lease_until = 0;
    s.pending = None;
    if let Some(session) = session {
        s.session = session;
    }
    save(vault, task, &s).await
}

/// Claim, spawn, record. `None` when another process claimed the task
/// first. Every failure after the claim lands in the receipt's `error`.
pub async fn fire(vault: &Vault, eval: &Evaluation, now: u64, force: bool) -> Result<Option<Fired>> {
    let task = eval.task.as_ref().ok_or_else(|| Error::invalid(eval.error.clone().unwrap_or_default()))?;
    let Some(pending) = claim(vault, eval, task, now, force).await? else {
        return Ok(None);
    };
    // A task not deployed here runs as it is now: store that version, so
    // the receipt can link it.
    if !eval.stored
        && let Ok(Loaded { version: Some((v, facts)), .. }) = load(vault, &task.entity).await
        && v.to_string() == eval.version
    {
        vault.write_unstamped(facts).await?;
    }
    // The run writes as the task's own session, so its writes don't wake it.
    let context = task.entity.to_string();
    let agent = Vault::open_as(&vault.dir, &context).await?;
    let ctx = Context::new(&vault.dir, &task.path, &pending.run, &context)?;
    let cwd = runner::work_dir(&vault.dir, task.cwd.as_deref());
    let (outcome, timeout) = match (&eval.runner, std::fs::create_dir_all(&cwd)) {
        (None, _) => (Outcome::failed(eval.error.clone().unwrap_or_default()), Duration::ZERO),
        (Some(_), Err(e)) => (Outcome::failed(format!("creating {}: {e}", cwd.display())), Duration::ZERO),
        (Some(r), Ok(())) => {
            let timeout = Duration::from_secs(r.timeout_secs);
            (runner::spawn(&ctx.resolve(&r.command), &ctx.env(), &cwd, &prompt_text(eval), timeout).await, timeout)
        }
    };
    let (content, cut) = truncate_bytes(&runner::last_message(&ctx.out, &outcome.stdout), MAX_CONTENT_BYTES);
    ctx.clean();
    let mut errors = outcome.failures(timeout);
    if cut {
        errors.push(format!("output truncated to {MAX_CONTENT_BYTES} bytes"));
    }
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let mut fields = Map::new();
    fields.insert(RUN_TASK.into(), json!(task.entity.to_string()));
    if !eval.version.is_empty() {
        fields.insert(RUN_VERSION.into(), json!(eval.version));
    }
    fields.insert("dreams.run/peer".into(), json!(vault.session()));
    fields.insert("dreams.run/started".into(), json!(pending.started));
    fields.insert("dreams.run/finished".into(), json!(now_ms()));
    fields.insert("dreams.run/exit-code".into(), json!(outcome.exit_code));
    fields.insert("dreams.run/error".into(), json!(error));
    fields.insert(note::PROTECTED.into(), json!(true));
    let receipt = Put {
        path: Some(pending.run.clone()),
        title: Some(format!("{} run", task.title.as_deref().unwrap_or(&task.path))),
        content: Some(content),
        tags: Some(vec!["run".into()]),
        fields,
        ..Default::default()
    };
    note::put(&agent, receipt, true).await?;
    settle(vault, &task.entity, &pending, Some(agent.session())).await?;
    Ok(Some(Fired { task: task.path.clone(), run: pending.run, exit_code: outcome.exit_code, error }))
}

/// Finish runs whose receipt was written before a crash: the next tick
/// moves the rows and releases the lease without running again.
async fn recover(vault: &Vault, entity: &Entity) -> Result<()> {
    if let Some(s) = state(vault, entity).await?
        && let Some(p) = &s.pending
        && note::find(vault, &p.run).await?.is_some()
    {
        settle(vault, entity, p, None).await?;
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct TickReport {
    pub fired: Vec<Fired>,
    /// Due tasks another process claimed first.
    pub skipped: Vec<String>,
    /// Tasks whose firing failed before a receipt could record it.
    pub errors: Vec<(String, String)>,
}

/// One pass over every task. Sequential; one failure never stops the others.
pub async fn tick(vault: &Vault) -> Result<TickReport> {
    let mut report = TickReport::default();
    let now = now_ms();
    for entity in entities(vault).await? {
        recover(vault, &entity).await?;
        let eval = match evaluate(vault, &entity, now).await {
            Ok(e) if e.due => e,
            Ok(_) => continue,
            Err(e) => {
                report.errors.push((entity.to_string(), e.to_string()));
                continue;
            }
        };
        match fire(vault, &eval, now, false).await {
            Ok(Some(f)) => report.fired.push(f),
            Ok(None) => report.skipped.push(eval.path.clone()),
            Err(e) => report.errors.push((eval.path.clone(), e.to_string())),
        }
    }
    let of = Entity::of(&("dreams", "scheduler"));
    vault.write_local(vec![assert(&of, LAST_TICK, Value::UnsignedInt(now_ms().into()), Pick::Last)?]).await?;
    Ok(report)
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Scheduler {
    /// Unix ms of the last tick on this device; absent when it never ticked.
    pub last_tick: Option<u64>,
    /// No tick in five minutes: deployed tasks do not fire.
    pub stale: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskList {
    pub tasks: Vec<Evaluation>,
    pub scheduler: Scheduler,
}

pub async fn scheduler(vault: &Vault) -> Result<Scheduler> {
    let of = Entity::of(&("dreams", "scheduler"));
    let claims = vault.local_claims(ArtifactSelector::new().of(of).the(vault::relation(LAST_TICK)?)).await?;
    let last_tick = claims.first().and_then(|a| value::natural(&a.is));
    Ok(Scheduler { last_tick, stale: last_tick.is_none_or(|t| now_ms().saturating_sub(t) > STALE_AFTER_MS) })
}

/// Every task, evaluated now, with the scheduler's status.
pub async fn list(vault: &Vault) -> Result<TaskList> {
    let now = now_ms();
    let mut tasks = Vec::new();
    for entity in entities(vault).await? {
        tasks.push(evaluate(vault, &entity, now).await?);
    }
    tasks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TaskList { tasks, scheduler: scheduler(vault).await? })
}

/// The receipts of a task, newest first.
pub async fn runs(vault: &Vault, href: &str) -> Result<Vec<Note>> {
    let entity = note::resolve(vault, href).await?;
    let mut out = Vec::new();
    for run in vault.having(RUN_TASK, Value::Entity(entity)).await? {
        out.push(note::load(vault, &run).await?);
    }
    out.sort_by_key(|n| std::cmp::Reverse(n.path().unwrap_or_default().to_string()));
    Ok(out)
}
