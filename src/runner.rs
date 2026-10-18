//! Runners (SPEC §10.1): entities whose `dreams.runner/command` holds a
//! program and its arguments, as JSON text. The command is spawned
//! directly, never through a shell, with tokens replaced inside each
//! element. Runner commands are code, so none runs until a person
//! confirms a deploy that shows it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use dialog_artifacts::Entity;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::note::{self, Note};
use crate::text::truncate_bytes;
use crate::vault::Vault;

pub const COMMAND: &str = "dreams.runner/command";
pub const TIMEOUT: &str = "dreams.runner/timeout";
pub const DEFAULT_TIMEOUT: &str = "10m";
/// The folder an agent runs in when its task names none.
pub const DEFAULT_CWD: &str = "workspace";
const MAX_STDERR_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct Runner {
    pub entity: Entity,
    pub path: String,
    /// The program and its arguments, before tokens are replaced.
    pub command: Vec<String>,
    pub timeout: String,
    pub timeout_secs: u64,
}

impl Runner {
    pub fn from_note(note: &Note) -> Result<Runner> {
        let path = note.path().unwrap_or_default().to_string();
        let bad = || Error::invalid(format!("runner {path}: {COMMAND} must be a JSON array of strings, not empty"));
        let command: Vec<String> = serde_json::from_str(note.text(COMMAND).ok_or_else(bad)?).map_err(|_| bad())?;
        if command.is_empty() {
            return Err(bad());
        }
        let timeout = note.text(TIMEOUT).unwrap_or(DEFAULT_TIMEOUT).to_string();
        Ok(Runner { entity: note.entity.clone(), path, command, timeout_secs: parse_duration(&timeout)?, timeout })
    }

    pub async fn load(vault: &Vault, href: &str) -> Result<Runner> {
        Runner::from_note(&note::get(vault, href).await?)
    }
}

/// `30s`, `15m`, `2h`, `1d`, `1w` to seconds. Zero is an error.
pub fn parse_duration(text: &str) -> Result<u64> {
    let bad = || Error::invalid(format!("bad duration {text:?}: use a number and one of s, m, h, d, w"));
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(bad()),
    };
    if n == 0 {
        return Err(Error::invalid("duration must be greater than zero"));
    }
    Ok(n * mult)
}

/// The folder an agent runs in: `cwd`, or `workspace`. A leading `~/` is
/// the home folder; a relative path is relative to the vault folder.
pub fn work_dir(vault: &Path, cwd: Option<&str>) -> PathBuf {
    let cwd = cwd.unwrap_or(DEFAULT_CWD);
    if let (Some(rest), Some(home)) = (cwd.strip_prefix("~/"), std::env::var_os("HOME")) {
        return PathBuf::from(home).join(rest);
    }
    vault.join(cwd)
}

/// What one run makes available to the command, as tokens and as
/// environment variables.
#[derive(Debug, Clone)]
pub struct Context {
    pub vault: PathBuf,
    pub task: String,
    pub run: String,
    /// The session the agent writes as: its writes don't wake its own task.
    pub session: String,
    /// A generated MCP config that points back at this vault as `session`.
    pub mcp: PathBuf,
    /// Where a command may write its last message instead of stdout.
    pub out: PathBuf,
    pub exe: PathBuf,
}

impl Context {
    /// A context with a fresh scratch folder for `{mcp}` and `{out}`.
    pub fn new(vault: &Path, task: &str, run: &str, session: &str) -> Result<Context> {
        let scratch = std::env::temp_dir().join(format!("dreams-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&scratch)?;
        let ctx = Context {
            vault: vault.to_path_buf(),
            task: task.into(),
            run: run.into(),
            session: session.into(),
            mcp: scratch.join("mcp.json"),
            out: scratch.join("last-message"),
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dreams")),
        };
        std::fs::write(&ctx.mcp, ctx.mcp_config())?;
        Ok(ctx)
    }

    /// Remove the scratch folder.
    pub fn clean(&self) {
        if let Some(dir) = self.mcp.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn pairs(&self) -> [(&'static str, String); 7] {
        [
            ("vault", self.vault.to_string_lossy().into_owned()),
            ("task", self.task.clone()),
            ("run", self.run.clone()),
            ("session", self.session.clone()),
            ("mcp", self.mcp.to_string_lossy().into_owned()),
            ("out", self.out.to_string_lossy().into_owned()),
            ("exe", self.exe.to_string_lossy().into_owned()),
        ]
    }

    /// A command as the argv to spawn, with `{vault}`, `{task}`, `{run}`,
    /// `{session}`, `{mcp}`, `{out}` and `{exe}` replaced inside each element.
    pub fn resolve(&self, command: &[String]) -> Vec<String> {
        let pairs = self.pairs();
        command
            .iter()
            .map(|arg| pairs.iter().fold(arg.clone(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value)))
            .collect()
    }

    /// The same values as `DREAMS_*` variables, plus `PATH` with this
    /// binary's folder first so an agent's shell finds `dreams`.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .pairs()
            .into_iter()
            .filter(|(name, _)| *name != "exe" && *name != "out")
            .map(|(name, value)| (format!("DREAMS_{}", name.to_ascii_uppercase()), value))
            .collect();
        let mut path = self.exe.parent().map(Path::to_path_buf).unwrap_or_default().to_string_lossy().into_owned();
        if let Ok(existing) = std::env::var("PATH")
            && !existing.is_empty()
        {
            path = format!("{path}:{existing}");
        }
        env.push(("PATH".into(), path));
        env
    }

    pub fn mcp_config(&self) -> String {
        json!({ "mcpServers": { "dreams": crate::mcp::server_entry(&self.exe, &self.vault, Some(&self.session)) } })
            .to_string()
    }
}

/// What came back from the command.
#[derive(Debug, Default)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
}

impl Outcome {
    pub fn failed(error: impl Into<String>) -> Outcome {
        Outcome { spawn_error: Some(error.into()), ..Default::default() }
    }

    /// Why the command failed: it did not start, it timed out, or it exited
    /// non-zero (with the tail of its stderr). Empty on success.
    pub fn failures(&self, timeout: Duration) -> Vec<String> {
        let mut errors: Vec<String> = self.spawn_error.iter().cloned().collect();
        if self.timed_out {
            errors.push(format!("timeout after {}s", timeout.as_secs()));
        }
        if !self.timed_out && self.spawn_error.is_none() && self.exit_code != Some(0) {
            let start = self.stderr.len().saturating_sub(MAX_STDERR_BYTES);
            let (tail, _) = truncate_bytes(&self.stderr[start..], MAX_STDERR_BYTES);
            let tail = tail.trim();
            errors.push(match self.exit_code {
                Some(code) if tail.is_empty() => format!("exit code {code}"),
                Some(code) => format!("exit code {code}: {tail}"),
                None if tail.is_empty() => "killed by signal".to_string(),
                None => format!("killed by signal: {tail}"),
            });
        }
        errors
    }
}

/// Spawn the resolved argv with the prompt on stdin. Stdin and stdout are
/// driven together so neither pipe fills. On timeout the child is killed.
pub async fn spawn(argv: &[String], env: &[(String, String)], cwd: &Path, prompt: &str, timeout: Duration) -> Outcome {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::failed(format!("cannot start {}: {e}", argv[0])),
    };
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let prompt = prompt.as_bytes().to_vec();
    let feed = async move {
        // A command that never reads stdin closes the pipe; that is not an error.
        let _ = stdin.write_all(&prompt).await;
        let _ = stdin.shutdown().await;
    };
    match tokio::time::timeout(timeout, async { tokio::join!(feed, child.wait_with_output()) }).await {
        Ok((_, Ok(output))) => Outcome {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            ..Default::default()
        },
        Ok((_, Err(e))) => Outcome::failed(format!("waiting for {}: {e}", argv[0])),
        Err(_) => Outcome { timed_out: true, ..Default::default() },
    }
}

/// The command's last message: the `{out}` file when the command wrote
/// one, else stdout.
pub fn last_message(out_file: &Path, stdout: &[u8]) -> Vec<u8> {
    match std::fs::read(out_file) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => stdout.to_vec(),
    }
}

/// Run `runner` once, outside the scheduler, with `prompt` on stdin, and
/// return its last message. A command that does not start, times out, or
/// exits non-zero is an error.
pub async fn invoke(runner: &Runner, vault: &Path, name: &str, prompt: &str) -> Result<String> {
    let ctx = Context::new(vault, name, "", name)?;
    let cwd = work_dir(vault, None);
    std::fs::create_dir_all(&cwd)?;
    let timeout = Duration::from_secs(runner.timeout_secs);
    let outcome = spawn(&ctx.resolve(&runner.command), &ctx.env(), &cwd, prompt, timeout).await;
    let message = last_message(&ctx.out, &outcome.stdout);
    ctx.clean();
    let errors = outcome.failures(timeout);
    if !errors.is_empty() {
        return Err(Error::Runner { message: format!("{}: {}", runner.path, errors.join("; ")) });
    }
    Ok(String::from_utf8_lossy(&message).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            vault: "/v".into(),
            task: "tasks/t.json".into(),
            run: "runs/tasks/t/1.md".into(),
            session: "did:key:zT".into(),
            mcp: "/tmp/r/mcp.json".into(),
            out: "/tmp/r/out".into(),
            exe: "/bin/dreams".into(),
        }
    }

    #[test]
    fn tokens_are_replaced_per_element() {
        let argv: Vec<String> =
            ["x", "--vault={vault}", "{task}:{run}", "{missing}", "a {out} b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(ctx().resolve(&argv), ["x", "--vault=/v", "tasks/t.json:runs/tasks/t/1.md", "{missing}", "a /tmp/r/out b"]);
    }

    #[test]
    fn env_names_the_vault_and_session_and_prepends_exe_dir() {
        let env = ctx().env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap();
        assert_eq!(get("DREAMS_VAULT"), "/v");
        assert_eq!(get("DREAMS_SESSION"), "did:key:zT");
        assert_eq!(get("DREAMS_MCP"), "/tmp/r/mcp.json");
        assert!(get("PATH").starts_with("/bin"));
        let cfg: serde_json::Value = serde_json::from_str(&ctx().mcp_config()).unwrap();
        assert_eq!(cfg["mcpServers"]["dreams"]["args"][3], "did:key:zT");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("1w").unwrap(), 604_800);
        for bad in ["0m", "15", "m", "", "1.5h", "-1m"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn work_dir_is_cwd_or_workspace_in_the_vault() {
        assert_eq!(work_dir(Path::new("/v"), None), Path::new("/v/workspace"));
        assert_eq!(work_dir(Path::new("/v"), Some("/abs")), Path::new("/abs"));
    }
}
