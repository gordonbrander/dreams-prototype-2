//! Command line interface (SPEC §13). `run` is in-process so tests can
//! drive it; nothing here exits the process.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use dialog_artifacts::{ArtifactSelector, Instruction};
use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::describe;
use crate::error::Error;
use crate::import::{self, FileResult, Options};
use crate::mcp;
use crate::notation;
use crate::note;
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

/// Asks a person to confirm `text`: true for yes.
pub type Confirm<'a> = &'a mut dyn FnMut(&str) -> io::Result<bool>;

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
pub fn run<I, T>(args: I, stdin: &mut dyn Read, stdout: &mut dyn Write, stderr: &mut dyn Write, confirm: Confirm) -> i32
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
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(stderr, "error: {e}");
            return 1;
        }
    };
    match runtime.block_on(execute(cli, stdin, stdout, confirm)) {
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

async fn execute(cli: Cli, stdin: &mut dyn Read, out: &mut dyn Write, _confirm: Confirm<'_>) -> anyhow::Result<()> {
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
        NoteCmd::Search { q, tag, prefix, limit } => search(v, dir, out, json, &q, tag, prefix, limit).await?,
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
    let s = Search::open(dir)?;
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
