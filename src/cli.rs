//! Command line interface (SPEC §13). `run` is in-process so tests can
//! drive it; nothing here exits the process.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use dialog_artifacts::{ArtifactSelector, Instruction};
use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::daemon;
use crate::describe;
use crate::feed;
use crate::resolve::{self, Reply};
use crate::runner::{self, Runner};
use crate::task;
use crate::error::Error;
use crate::import::{self, FileResult, Options};
use crate::mcp;
use crate::notation;
use crate::note::{self, Put};
use crate::peer;
use crate::search::Search;
use crate::seed;
use crate::value::to_json;
use crate::vault::{self, Vault};

/// dreams: a memory substrate for agents, on DialogDB.
#[derive(Parser)]
#[command(name = "dreams", version, about)]
pub struct Cli {
    /// The vault folder. Default: ~/.dreams
    #[arg(long, global = true, env = "DREAMS_VAULT")]
    vault: Option<PathBuf>,
    /// Print JSON (the same structures the MCP tools return) instead of human output.
    #[arg(long, global = true)]
    json: bool,
    /// Write as a session of this name. A task's agent runs with its own, so
    /// that its writes can be told apart and do not wake it.
    #[arg(long, global = true, env = "DREAMS_SESSION", default_value = "dreams")]
    session: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the vault if needed and seed the built-in runners, skills, prompts and tasks.
    Init,
    /// Seed the built-in notes again: write every fact that differs from the default.
    RestoreDefaults,
    /// Serve MCP (2026-07-28, stateless) over stdio.
    Serve,
    /// Print how a host starts `serve` on this vault, as one `mcpServers` entry.
    /// For example: claude mcp add-json dreams "$(dreams mcp-json)"
    McpJson,
    /// Notes: entities with a path.
    #[command(subcommand)]
    Note(NoteCmd),
    /// Run a query in Dialog's formal notation (JSON or YAML) from a file, or `-` for stdin.
    Query {
        file: PathBuf,
        #[arg(long, default_value_t = notation::DEFAULT_LIMIT)]
        limit: usize,
    },
    /// The attributes, concepts and rules in the vault.
    Describe { domain: Option<String> },
    /// Installed deductive rules.
    #[command(subcommand)]
    Rule(RuleCmd),
    /// Import .md, .json and .yaml files under a folder (or one file). A file's relative path is its note's path.
    Import {
        path: PathBuf,
        /// Domain for keys of untyped documents.
        #[arg(long, default_value = import::DEFAULT_DOMAIN)]
        domain: String,
    },
    /// Write every note to a folder, at its path.
    Export { dir: PathBuf },
    /// Rebuild the search index.
    Reindex,
    /// Notes updated at or after a time (Unix ms).
    Changes {
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Report husks, path collisions and set-aside rules.
    Doctor,
    /// Scheduled tasks.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Runners: the commands that start agents.
    #[command(subcommand)]
    Runner(RunnerCmd),
    /// Feeds pulled into the vault.
    #[command(subcommand)]
    Feed(FeedCmd),
    /// Fire every due task once, then exit.
    Tick,
    /// Folders this vault syncs with.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Pull every remote's changes.
    Pull,
    /// Pull, then push to every remote of the vault and of `public`.
    Sync,
    /// This device, and the devices that may sync this vault.
    #[command(subcommand)]
    Device(DeviceCmd),
    /// Other people's vaults you follow, by petname.
    #[command(subcommand)]
    Peer(PeerCmd),
    /// Copy a note into your public repository and push it.
    Publish { note: String },
    /// Run the scheduler: tick every --interval, and when the vault changes.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonCmd>,
        #[arg(long, default_value = "1m")]
        interval: String,
        #[arg(long, default_value = "2s")]
        poll: String,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum TaskCmd {
    /// Write a task. It runs only after `task deploy`.
    Add {
        path: String,
        #[arg(long)]
        runner: String,
        #[arg(long)]
        every: String,
        /// The prompt, or `-` to read it from stdin.
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Fire only when this query's rows change: a file in formal notation.
        #[arg(long)]
        when: Option<PathBuf>,
        /// Fire when a note with this tag changes.
        #[arg(long)]
        tag: Option<String>,
        /// Fire when a note whose path matches this pattern changes.
        #[arg(long)]
        glob: Option<String>,
        /// Fire when a note of this concept changes.
        #[arg(long = "type")]
        concept: Option<String>,
    },
    /// Every task and its state on this device.
    List,
    /// Run a task (or every task) as it is now on this device, after you confirm.
    Deploy {
        task: Option<String>,
        /// Don't ask.
        #[arg(long)]
        yes: bool,
    },
    /// Stop running a task on this device.
    Disable { task: String },
    /// Delete a task note. Its receipts stay.
    Rm { task: String },
    /// Evaluate one task now: whether it is due, and the rows that changed.
    Check { task: String },
    /// Fire a task on the next tick; with --now, here and now, deployed or not.
    Run {
        task: String,
        #[arg(long)]
        now: bool,
    },
    /// A task's receipts, newest first.
    Runs { task: String },
}

#[derive(Subcommand)]
enum RunnerCmd {
    /// Write a runner: dreams runner add runners/x.json -- program args...
    Add {
        path: String,
        #[arg(long)]
        timeout: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Every runner.
    List,
}

#[derive(Subcommand)]
enum FeedCmd {
    /// Add a feed. Default path: feeds/<origin>.md.
    Add {
        url: String,
        #[arg(long, default_value = "rss")]
        kind: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        instructions: Option<String>,
    },
    /// Every feed.
    List,
    /// Pull one feed, or every feed, and print the new items.
    Pull { feed: Option<String> },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Sync with LOCATION: a folder, ucan+https://<access service>, or s3://<bucket>?endpoint=<url>&region=<r>
    /// (credentials from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY). An empty
    /// folder becomes this vault's remote; a folder another device set up
    /// needs its `dreams device add` first.
    Add {
        name: String,
        location: String,
        /// A remote of the public repository instead.
        #[arg(long)]
        public: bool,
        /// For an S3 remote another device set up: its repository DID.
        #[arg(long)]
        repository: Option<String>,
        /// A delegation file from that device's `dreams device add`.
        #[arg(long)]
        delegation: Option<PathBuf>,
    },
    List,
    Rm {
        name: String,
        #[arg(long)]
        public: bool,
    },
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// Print this device's DID, for `dreams device add` on another device.
    Id,
    /// Let the device DID sync this vault: leaves a delegation in each remote folder.
    Add { did: String },
    /// Stop the device DID syncing: enforced by UCAN access services only.
    Revoke { did: String },
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Follow the vault published at LOCATION (a folder, or s3:// with
    /// --repository and --delegation) as NAME. Its notes are dreams://NAME/<path>.
    Add {
        name: String,
        location: String,
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        delegation: Option<PathBuf>,
    },
    List,
    Rm { name: String },
    /// Let the device DID follow what you publish.
    Grant { did: String },
    /// Stop the device DID following what you publish: enforced by UCAN access services only.
    Revoke { did: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon at login and keep it running (launchd or systemd).
    Install,
    /// Stop it and remove it.
    Uninstall,
}

#[derive(Subcommand)]
enum NoteCmd {
    /// Create or update the note at PATH from a document in FILE (or stdin):
    /// Markdown with frontmatter, JSON or YAML, by PATH's extension.
    Put { path: String, file: Option<PathBuf> },
    /// Print a note as Markdown (or JSON with --json).
    Get { note: String },
    /// Delete a note: retract every fact of its entity.
    Rm {
        note: String,
    },
    /// List notes, newest first.
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Full-text search.
    Search {
        q: String,
        /// Search a followed vault, by petname.
        #[arg(long)]
        peer: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Every value a note's attributes have held, oldest first.
    History {
        note: String,
        #[arg(long)]
        attribute: Option<String>,
    },
    /// Notes whose title, content or path hold several values after a sync.
    Conflicts {
        /// Check these attributes instead.
        #[arg(long)]
        attribute: Vec<String>,
    },
    /// Each conflicting value as a diff from the value before both changes.
    Diff {
        note: String,
        #[arg(long, default_value = note::CONTENT)]
        attribute: String,
    },
    /// Settle a conflict with the value in FILE (`-` for stdin), with
    /// code's line merge when it is clean, or with --auto, a runner's merge.
    Resolve {
        note: String,
        file: Option<PathBuf>,
        #[arg(long, default_value = note::CONTENT)]
        attribute: String,
        #[arg(long)]
        auto: bool,
        #[arg(long, default_value = resolve::DEFAULT_RUNNER)]
        runner: String,
    },
}

#[derive(Subcommand)]
enum RuleCmd {
    /// Install a deductive rule from a file in formal notation (or `-`).
    Add { file: PathBuf },
    /// List installed rules.
    List,
    /// Uninstall a rule by its entity.
    Rm { entity: String },
}

/// Stack for the threads that run commands.
pub const STACK: usize = 64 << 20;

/// Asks a person to confirm `text`: true for yes.
pub type Confirm<'a> = &'a mut (dyn FnMut(&str) -> io::Result<bool> + Send);

/// Ask on the controlling terminal, not on stdin, which can hold a task prompt.
pub fn tty_confirm(text: &str) -> io::Result<bool> {
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    write!(tty, "{text} [y/N] ")?;
    tty.flush()?;
    let mut line = String::new();
    io::BufReader::new(&tty).read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Parse `args` (including argv[0]) and run. Returns the exit code.
pub fn run<I, T>(
    args: I,
    stdin: &mut (dyn Read + Send),
    stdout: &mut (dyn Write + Send),
    stderr: &mut dyn Write,
    confirm: Confirm,
) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let out: &mut dyn Write = if e.use_stderr() { stderr } else { stdout };
            let _ = write!(out, "{}", e.render());
            return e.exit_code();
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(STACK).build() {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(stderr, "error: {e}");
            return 1;
        }
    };
    // Dialog's sync futures are deep: run on a thread with room for them.
    let result = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, || runtime.block_on(execute(cli, stdin, stdout, confirm)))
            .expect("a thread")
            .join()
            .expect("the command thread")
    });
    match result {
        Ok(()) => 0,
        Err(err) => {
            if let Some(io_err) = err.downcast_ref::<io::Error>()
                && io_err.kind() == io::ErrorKind::BrokenPipe
            {
                return 0;
            }
            match err.downcast_ref::<Error>() {
                Some(e) => {
                    let mut v = serde_json::to_value(e).unwrap_or_default();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("message".into(), json!(e.to_string()));
                    }
                    let _ = writeln!(stderr, "{v}");
                }
                None => {
                    let _ = writeln!(stderr, "error: {err:#}");
                }
            }
            1
        }
    }
}

fn vault_dir(cli: &Cli) -> Result<PathBuf, Error> {
    let dir = match &cli.vault {
        Some(d) => d.clone(),
        None => std::env::home_dir().ok_or_else(|| Error::invalid("no home folder; pass --vault"))?.join(".dreams"),
    };
    std::path::absolute(&dir).map_err(|e| Error::invalid(format!("{}: {e}", dir.display())))
}

/// Open the vault, seeding it when this call creates it.
async fn open(dir: &Path, session: &str) -> Result<Vault, Error> {
    let created = !dir.join("dialog").exists();
    let v = Vault::open_as(dir, session).await?;
    if created {
        seed::seed(&v).await?;
    }
    Ok(v)
}

async fn execute(cli: Cli, stdin: &mut dyn Read, out: &mut dyn Write, confirm: Confirm<'_>) -> anyhow::Result<()> {
    let json = cli.json;
    let dir = vault_dir(&cli)?;
    match cli.command {
        Command::Init => {
            let v = Vault::open_as(&dir, &cli.session).await?;
            let seeded = seed::seed(&v).await?;
            writeln!(out, "initialized {}", dir.display())?;
            report(out, json, seeded)?;
        }
        Command::RestoreDefaults => {
            let v = open(&dir, &cli.session).await?;
            report(out, json, seed::seed(&v).await?)?;
        }
        Command::Serve => {
            install_tracing("warn");
            let v = open(&dir, &cli.session).await?;
            let search = Search::open(&dir)?;
            mcp::serve(v, search).await?;
        }
        Command::McpJson => {
            let exe = std::env::current_exe()?;
            let session = (cli.session != "dreams").then_some(cli.session.as_str());
            print_json(out, &mcp::server_entry(&exe, &dir, session))?;
        }
        Command::Note(cmd) => note_cmd(cmd, &open(&dir, &cli.session).await?, &dir, stdin, out, json).await?,
        Command::Query { file, limit } => {
            let v = open(&dir, &cli.session).await?;
            let text = read_text(&file, stdin)?;
            let rows = notation::query(&v, &Json::String(text), limit).await?;
            print_json(out, &rows)?;
        }
        Command::Describe { domain } => {
            let v = open(&dir, &cli.session).await?;
            print_json(out, &describe::describe(&v, domain.as_deref()).await?)?;
        }
        Command::Rule(cmd) => {
            let v = open(&dir, &cli.session).await?;
            match cmd {
                RuleCmd::Add { file } => {
                    let id = notation::define_rule(&v, &Json::String(read_text(&file, stdin)?)).await?;
                    writeln!(out, "{id}")?;
                }
                RuleCmd::List => print_json(out, &describe::describe(&v, Some("dialog.rule")).await?["rules"])?,
                RuleCmd::Rm { entity } => {
                    let e = crate::value::entity(&entity)?;
                    let claims = v.claims(ArtifactSelector::new().of(e)).await?;
                    if claims.is_empty() {
                        return Err(Error::not_found(entity).into());
                    }
                    v.write(claims.into_iter().map(Instruction::Retract).collect()).await?;
                }
            }
        }
        Command::Import { path, domain } => {
            let v = open(&dir, &cli.session).await?;
            let docs = if path.is_dir() {
                import::read_dir(&path)?
            } else {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                vec![(name, std::fs::read_to_string(&path)?)]
            };
            let results = import::import(&v, &docs, &Options { domain, derive_ids: false }).await?;
            report(out, json, results)?;
        }
        Command::Export { dir: to } => {
            let v = open(&dir, &cli.session).await?;
            report(out, json, import::export(&v, &to).await?)?;
        }
        Command::Reindex => {
            let v = open(&dir, &cli.session).await?;
            let n = Search::reset(&dir)?.catch_up(&v).await?;
            writeln!(out, "indexed {}", n.unwrap_or_default())?;
        }
        Command::Changes { since, limit } => {
            let v = open(&dir, &cli.session).await?;
            let rows = mcp::changes(&v, since, limit).await?;
            if json {
                print_json(out, &rows)?;
            } else {
                for r in rows {
                    writeln!(out, "{}  {}", r["updated"], r["path"].as_str().unwrap_or("(no path)"))?;
                }
            }
        }
        Command::Doctor => {
            let v = open(&dir, &cli.session).await?;
            let report = doctor(&v).await?;
            print_json(out, &report)?;
        }
        Command::Task(cmd) => task_cmd(cmd, &open(&dir, &cli.session).await?, stdin, out, json, confirm).await?,
        Command::Runner(cmd) => {
            let v = open(&dir, &cli.session).await?;
            match cmd {
                RunnerCmd::Add { path, timeout, title, command } => {
                    let mut fields = serde_json::Map::new();
                    fields.insert(runner::COMMAND.into(), json!(serde_json::to_string(&command)?));
                    if let Some(t) = timeout {
                        runner::parse_duration(&t)?;
                        fields.insert(runner::TIMEOUT.into(), json!(t));
                    }
                    let put = Put { path: Some(path.clone()), title, fields, concept: Some("runner".into()), ..Default::default() };
                    note::put(&v, put, true).await?;
                    writeln!(out, "wrote {path}")?;
                }
                RunnerCmd::List => {
                    let mut runners = Vec::new();
                    for a in v.claims(ArtifactSelector::new().the(vault::relation(runner::COMMAND)?)).await? {
                        let n = note::load(&v, &a.of).await?;
                        runners.push(match Runner::from_note(&n) {
                            Ok(r) => json!({"path": r.path, "command": r.command, "timeout": r.timeout, "protected": n.is_protected()}),
                            Err(e) => json!({"path": n.path(), "error": e.to_string()}),
                        });
                    }
                    print_json(out, &runners)?;
                }
            }
        }
        Command::Feed(cmd) => {
            let v = open(&dir, &cli.session).await?;
            match cmd {
                FeedCmd::Add { url, kind, path, title, instructions } => {
                    feed::Kind::parse(&kind)?;
                    let path = match path {
                        Some(p) => p,
                        None => feed::default_path(&url)?,
                    };
                    let mut fields = serde_json::Map::new();
                    fields.insert(feed::URL.into(), json!(url));
                    fields.insert(feed::KIND.into(), json!(kind));
                    if let Some(i) = instructions {
                        fields.insert(feed::INSTRUCTIONS.into(), json!(i));
                    }
                    let put = Put { path: Some(path.clone()), title, fields, concept: Some("feed".into()), ..Default::default() };
                    note::put(&v, put, true).await?;
                    writeln!(out, "wrote {path}")?;
                }
                FeedCmd::List => {
                    let feeds: Vec<Json> = feed::feeds(&v, None)
                        .await?
                        .into_iter()
                        .map(|f| json!({"path": f.path, "url": f.url, "title": f.title}))
                        .collect();
                    print_json(out, &feeds)?;
                }
                FeedCmd::Pull { feed: only } => print_json(out, &feed::pull(&v, only.as_deref()).await?)?,
            }
        }
        Command::Remote(cmd) => {
            let v = open(&dir, &cli.session).await?;
            let space = |public: bool| if public { peer::PUBLIC } else { "vault" };
            match cmd {
                RemoteCmd::Add { name, location, public, repository, delegation } => {
                    if let Some(file) = delegation {
                        v.accept(&std::fs::read(file)?).await?;
                    }
                    print_json(out, &peer::add_remote(&v, space(public), &name, &location, repository.as_deref()).await?)?
                }
                RemoteCmd::List => print_json(out, &peer::remotes(&v).await?)?,
                RemoteCmd::Rm { name, public } => peer::rm_remote(&v, space(public), &name).await?,
            }
        }
        Command::Pull => print_json(out, &peer::sync(&open(&dir, &cli.session).await?, false).await?)?,
        Command::Sync => print_json(out, &peer::sync(&open(&dir, &cli.session).await?, true).await?)?,
        Command::Device(cmd) => {
            let v = open(&dir, &cli.session).await?;
            match cmd {
                DeviceCmd::Id => writeln!(out, "{}", v.device())?,
                DeviceCmd::Add { did } => {
                    for f in peer::device_add(&v, &did).await? {
                        writeln!(out, "wrote {}", f.display())?;
                    }
                    writeln!(out, "On that device: dreams remote add <name> <folder>")?;
                    writeln!(out, "For an S3 remote, copy the file there and add --repository {} --delegation <file>", v.repository())?;
                }
                DeviceCmd::Revoke { did } => print_json(out, &peer::revoke(&v, "vault", &did).await?)?,
            }
        }
        Command::Peer(cmd) => {
            let v = open(&dir, &cli.session).await?;
            match cmd {
                PeerCmd::Add { name, location, repository, delegation } => {
                    if let Some(file) = delegation {
                        v.accept(&std::fs::read(file)?).await?;
                    }
                    print_json(out, &peer::follow(&v, &name, &location, repository.as_deref()).await?)?
                }
                PeerCmd::List => {
                    let peers: Vec<_> =
                        peer::remotes(&v).await?.into_iter().filter(|r| r.space.starts_with("peer-")).collect();
                    print_json(out, &peers)?;
                }
                PeerCmd::Rm { name } => peer::rm_remote(&v, &format!("peer-{name}"), &name).await?,
                PeerCmd::Revoke { did } => print_json(out, &peer::revoke(&v, peer::PUBLIC, &did).await?)?,
                PeerCmd::Grant { did } => {
                    for f in peer::peer_grant(&v, &did).await? {
                        writeln!(out, "wrote {}", f.display())?;
                    }
                }
            }
        }
        Command::Publish { note: href } => {
            let v = open(&dir, &cli.session).await?;
            print_json(out, &peer::publish(&v, &href).await?)?;
        }
        Command::Tick => {
            let v = open(&dir, &cli.session).await?;
            print_json(out, &task::tick(&v).await?)?;
        }
        Command::Daemon { action, interval, poll } => match action {
            Some(DaemonCmd::Install) => {
                open(&dir, &cli.session).await?;
                daemon::install(&dir, &std::env::current_exe()?, out)?;
            }
            Some(DaemonCmd::Uninstall) => daemon::uninstall(&dir, out)?,
            None => {
                install_tracing("info");
                let v = open(&dir, &cli.session).await?;
                let secs = |t: &str| runner::parse_duration(t).map(std::time::Duration::from_secs);
                daemon::run(&v, secs(&interval)?, secs(&poll)?).await?;
            }
        },
    }
    Ok(())
}

async fn task_cmd(
    cmd: TaskCmd,
    v: &Vault,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    json: bool,
    confirm: Confirm<'_>,
) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Add { path, runner, every, prompt, title, cwd, when, tag, glob, concept } => {
            runner::parse_duration(&every)?;
            let prompt = if prompt == "-" { read_text(Path::new("-"), stdin)? } else { prompt };
            let when = match when {
                Some(file) => Some(read_text(&file, stdin)?),
                None if tag.is_some() || glob.is_some() || concept.is_some() => {
                    Some(notation::filter_query(tag.as_deref(), glob.as_deref(), concept.as_deref()).to_string())
                }
                None => None,
            };
            let mut fields = serde_json::Map::new();
            fields.insert(task::RUNNER.into(), json!(runner));
            fields.insert(task::EVERY.into(), json!(every));
            fields.insert(task::PROMPT.into(), json!(prompt));
            if let Some(w) = when {
                notation::parse(&Json::String(w.clone()))?;
                fields.insert(task::WHEN.into(), json!(w));
            }
            if let Some(c) = cwd {
                fields.insert(task::CWD.into(), json!(c));
            }
            let put = Put { path: Some(path.clone()), title, fields, concept: Some("task".into()), ..Default::default() };
            note::put(v, put, true).await?;
            writeln!(out, "wrote {path}; it runs here after `dreams task deploy {path}`")?;
        }
        TaskCmd::List => {
            let list = task::list(v).await?;
            if json {
                print_json(out, &list)?;
            } else {
                let rows: Vec<Vec<String>> = list
                    .tasks
                    .iter()
                    .map(|t| {
                        let every = t.task.as_ref().map(|x| x.every.clone()).unwrap_or_default();
                        vec![t.path.clone(), t.status.clone(), every, t.due.to_string(), t.error.clone().unwrap_or_default()]
                    })
                    .collect();
                table(out, &["TASK", "STATUS", "EVERY", "DUE", "ERROR"], &rows)?;
                if list.scheduler.stale {
                    writeln!(out, "\nNo scheduler has ticked in five minutes: run `dreams daemon install`.")?;
                }
            }
        }
        TaskCmd::Deploy { task: href, yes } => {
            let plan = task::plan(v, href.as_deref()).await?;
            if plan.is_empty() {
                writeln!(out, "nothing to deploy")?;
                return Ok(());
            }
            let text: String = plan.iter().map(task::Deploy::describe).collect::<Vec<_>>().join("\n");
            write!(out, "{text}")?;
            if !yes && !confirm("Deploy these tasks on this device?")? {
                writeln!(out, "nothing deployed")?;
                return Ok(());
            }
            for path in task::apply(v, &plan).await? {
                writeln!(out, "deployed {path}")?;
            }
        }
        TaskCmd::Disable { task: href } => {
            task::disable(v, &href).await?;
            writeln!(out, "disabled {href}")?;
        }
        TaskCmd::Rm { task: href } => {
            note::delete(v, &href, true).await?;
            writeln!(out, "deleted {href}")?;
        }
        TaskCmd::Check { task: href } => {
            let e = note::resolve(v, &href).await?;
            print_json(out, &task::evaluate(v, &e, vault::now_ms()).await?)?;
        }
        TaskCmd::Run { task: href, now } => {
            if now {
                let e = note::resolve(v, &href).await?;
                let eval = task::evaluate(v, &e, vault::now_ms()).await?;
                print_json(out, &task::fire(v, &eval, vault::now_ms(), true).await?)?;
            } else {
                task::request_run(v, &href).await?;
                writeln!(out, "{href} fires on the next tick")?;
            }
        }
        TaskCmd::Runs { task: href } => {
            let runs: Vec<Json> = task::runs(v, &href).await?.iter().map(|n| Json::Object(n.to_json())).collect();
            print_json(out, &runs)?;
        }
    }
    Ok(())
}

async fn note_cmd(
    cmd: NoteCmd,
    v: &Vault,
    dir: &Path,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    json: bool,
) -> anyhow::Result<()> {
    match cmd {
        NoteCmd::Put { path, file } => {
            let text = match &file {
                Some(f) => std::fs::read_to_string(f)?,
                None => {
                    let mut s = String::new();
                    stdin.read_to_string(&mut s)?;
                    s
                }
            };
            let results = import::import(v, &[(path, text)], &Options::default()).await?;
            if let Some(e) = results.iter().find_map(|r| r.error.clone()) {
                return Err(Error::invalid(e).into());
            }
            report(out, json, results)?;
        }
        NoteCmd::Get { note: href } => {
            let n = note::get(v, &href).await?;
            if json { print_json(out, &n.to_json())? } else { write!(out, "{}", n.markdown())? }
        }
        NoteCmd::Rm { note: href } => {
            note::delete(v, &href, true).await?;
            writeln!(out, "deleted {href}")?;
        }
        NoteCmd::List { tag, prefix, limit } => search(v, dir, out, json, "", tag, prefix, limit).await?,
        NoteCmd::Search { q, peer: None, tag, prefix, limit } => search(v, dir, out, json, &q, tag, prefix, limit).await?,
        NoteCmd::Search { q, peer: Some(name), tag, prefix, limit } => {
            let followed = crate::peer::followed(v, &name).await?;
            search(&followed, dir, out, json, &q, tag, prefix, limit).await?
        }
        NoteCmd::History { note: href, attribute } => {
            let e = note::resolve(v, &href).await?;
            let rows: Vec<Json> = v
                .history(&e, attribute.as_deref())
                .await?
                .into_iter()
                .map(|(the, asserted, is)| json!({"the": the, "asserted": asserted, "is": to_json(&is)}))
                .collect();
            print_json(out, &rows)?;
        }
        NoteCmd::Conflicts { attribute } => {
            let attrs = if attribute.is_empty() {
                resolve::DEFAULT_ATTRIBUTES.iter().map(|a| a.to_string()).collect()
            } else {
                attribute
            };
            print_json(out, &resolve::list(v, &attrs).await?)?;
        }
        NoteCmd::Diff { note: href, attribute } => {
            let c = resolve::read(v, &href, &attribute).await?.ok_or_else(|| Error::invalid(format!("{href} has no conflict on {attribute}")))?;
            for d in resolve::diffs(&c) {
                write!(out, "{d}")?;
            }
        }
        NoteCmd::Resolve { note: href, file, attribute, auto, runner } => {
            let value = if auto {
                resolve::auto(v, &href, &attribute, &runner).await?
            } else {
                let reply = match file {
                    Some(f) => {
                        let text = read_text(&f, stdin)?;
                        let value = if attribute == note::CONTENT { json!(text) } else { serde_json::from_str(&text).unwrap_or(json!(text)) };
                        Reply { value: Some(value), ..Default::default() }
                    }
                    None => Reply::default(),
                };
                resolve::resolve(v, &href, &attribute, &reply, true).await?
            };
            if json { print_json(out, &value)? } else { writeln!(out, "resolved {href} {attribute}")? }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn search(
    v: &Vault,
    dir: &Path,
    out: &mut dyn Write,
    json: bool,
    q: &str,
    tag: Option<String>,
    prefix: Option<String>,
    limit: usize,
) -> anyhow::Result<()> {
    let s = Search::open_space(dir, &v.space)?;
    s.catch_up(v).await?;
    let hits = s.search(q, tag.as_deref(), prefix.as_deref(), limit)?;
    if json {
        print_json(out, &hits)?;
    } else {
        let rows: Vec<Vec<String>> =
            hits.iter().map(|h| vec![h.path.clone(), h.title.clone().unwrap_or_default(), crate::text::truncate(&h.snippet, 80)]).collect();
        table(out, &["PATH", "TITLE", "TEXT"], &rows)?;
    }
    Ok(())
}

/// Husks (facts but no path), paths held by several entities, and rules Dialog set aside.
async fn doctor(v: &Vault) -> Result<Json, Error> {
    let mut paths: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut with_path = std::collections::HashSet::new();
    for a in v.claims(ArtifactSelector::new().the(vault::relation(note::PATH)?)).await? {
        with_path.insert(a.of.clone());
        if let Some(p) = crate::value::text(&a.is) {
            paths.entry(p.to_string()).or_default().push(a.of.to_string());
        }
    }
    let mut husks = std::collections::BTreeSet::new();
    for a in v.claims(ArtifactSelector::new().the_starting_with("dreams.note/")).await? {
        if !with_path.contains(&a.of) {
            husks.insert(a.of.to_string());
        }
    }
    let collisions: Vec<Json> =
        paths.into_iter().filter(|(_, es)| es.len() > 1).map(|(p, es)| json!({"path": p, "entities": es})).collect();
    let quarantined = describe::describe(v, Some("dreams.note/path")).await?["quarantined"].clone();
    Ok(json!({"husks": husks, "path_collisions": collisions, "quarantined_rules": quarantined}))
}

pub fn install_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
}

fn read_text(file: &Path, stdin: &mut dyn Read) -> Result<String, Error> {
    if file == Path::new("-") {
        let mut s = String::new();
        stdin.read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(std::fs::read_to_string(file)?)
    }
}

fn report(out: &mut dyn Write, json: bool, results: Vec<FileResult>) -> io::Result<()> {
    if json {
        return print_json(out, &json!({"results": results, "errors": results.iter().filter(|r| r.error.is_some()).count()}));
    }
    for r in results {
        match &r.error {
            Some(e) => writeln!(out, "error    {}: {e}", r.path)?,
            None => writeln!(out, "ok       {}", r.path)?,
        }
        for s in &r.skipped {
            writeln!(out, "  skipped {s}")?;
        }
    }
    Ok(())
}

fn print_json<T: Serialize>(out: &mut dyn Write, value: &T) -> io::Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value).expect("values serialize"))
}

fn table(out: &mut dyn Write, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let render_row = |cells: &[&str]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            line.extend(std::iter::repeat_n(' ', widths[i] - cell.chars().count()));
        }
        line.trim_end().to_string()
    };
    writeln!(out, "{}", render_row(headers))?;
    for row in rows {
        writeln!(out, "{}", render_row(&row.iter().map(String::as_str).collect::<Vec<_>>()))?;
    }
    Ok(())
}
