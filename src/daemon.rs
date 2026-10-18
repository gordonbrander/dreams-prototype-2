//! The clock (SPEC §10.6). `run` ticks in a loop; `install` makes the OS
//! start it at login and keep it alive. `tick` itself lives in `task`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::hash::sha256_hex;
use crate::task;
use crate::text::xml_escape;
use crate::vault::Vault;

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
    }
}

/// Tick and sync now, then whenever `interval` has passed or the vault's
/// head moved (checked every `poll`). Returns on ctrl-c or SIGTERM.
pub async fn run(vault: &Vault, interval: Duration, poll: Duration) -> Result<()> {
    let mut stop = Box::pin(shutdown());
    tracing::info!(vault = %vault.dir.display(), ?interval, ?poll, "daemon started");
    loop {
        match task::tick(vault).await {
            Ok(r) if !r.fired.is_empty() || !r.errors.is_empty() => tracing::info!(fired = ?r.fired, errors = ?r.errors, "tick"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "tick failed"),
        }
        match crate::peer::sync(vault, true).await {
            Ok(r) if !r.pulled.is_empty() || !r.pushed.is_empty() || !r.errors.is_empty() => {
                tracing::info!(pulled = ?r.pulled, pushed = ?r.pushed, errors = ?r.errors, "sync")
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "sync failed"),
        }
        let last_tick = Instant::now();
        let head = vault.head().await?;
        loop {
            tokio::select! {
                _ = &mut stop => {
                    tracing::info!("daemon stopping");
                    return Ok(());
                }
                _ = tokio::time::sleep(poll) => {}
            }
            if last_tick.elapsed() >= interval || vault.head().await? != head {
                break;
            }
        }
    }
}

/// The service name of the vault at `dir` on every platform: the launchd
/// label, the systemd unit without `.service`, and the stem of the log.
pub fn service_name(dir: &Path) -> String {
    format!("io.dreams.vault-{}", &sha256_hex(dir.to_string_lossy().as_bytes())[..12])
}

/// A launchd user agent that keeps the daemon running.
pub fn render_plist(label: &str, exe: &Path, dir: &Path, log: &Path, path_env: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--vault</string>
    <string>{dir}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&exe.to_string_lossy()),
        dir = xml_escape(&dir.to_string_lossy()),
        path = xml_escape(path_env),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// A systemd user service that keeps the daemon running.
pub fn render_unit(exe: &Path, dir: &Path, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=dreams daemon for {dir}\n\n[Service]\nExecStart={exe} --vault {dir} daemon\nRestart=always\nRestartSec=5\nEnvironment=PATH={path_env}\n\n[Install]\nWantedBy=default.target\n",
        exe = exe.to_string_lossy(),
        dir = dir.to_string_lossy(),
    )
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| Error::invalid("HOME is not set"))
}

fn sh(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status().map_err(|e| Error::invalid(format!("running {program}: {e}")))?;
    if status.success() { Ok(()) } else { Err(Error::invalid(format!("{program} {} failed with {status}", args.join(" ")))) }
}

fn uid(home: &Path) -> Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(home)?.uid())
}

fn plist_path(home: &Path, name: &str) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{name}.plist"))
}

fn unit_path(home: &Path, name: &str) -> PathBuf {
    home.join(".config/systemd/user").join(format!("{name}.service"))
}

/// Register the daemon of the vault at `dir` with the OS. `dir` must be absolute.
pub fn install(dir: &Path, exe: &Path, out: &mut dyn Write) -> Result<()> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let home = home()?;
    let name = service_name(dir);
    if cfg!(target_os = "macos") {
        let plist = plist_path(&home, &name);
        let log = home.join("Library/Logs/dreams").join(format!("{name}.log"));
        std::fs::create_dir_all(plist.parent().unwrap())?;
        std::fs::create_dir_all(log.parent().unwrap())?;
        std::fs::write(&plist, render_plist(&name, exe, dir, &log, &path_env))?;
        let domain = format!("gui/{}", uid(&home)?);
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{name}")]).output();
        sh("launchctl", &["bootstrap", &domain, &plist.to_string_lossy()])?;
        writeln!(out, "installed {name}\n  {}\nlogs: {}", plist.display(), log.display())?;
    } else if cfg!(target_os = "linux") {
        let unit = unit_path(&home, &name);
        std::fs::create_dir_all(unit.parent().unwrap())?;
        std::fs::write(&unit, render_unit(exe, dir, &path_env))?;
        sh("systemctl", &["--user", "daemon-reload"])?;
        sh("systemctl", &["--user", "enable", "--now", &format!("{name}.service")])?;
        writeln!(out, "installed {name}.service\n  {}\nlogs: journalctl --user -u {name}.service", unit.display())?;
    } else {
        return Err(Error::invalid(format!(
            "no installer for this platform; run this at login:\n  {} --vault {} daemon",
            exe.display(),
            dir.display()
        )));
    }
    Ok(())
}

/// Stop and remove the daemon of the vault at `dir`.
pub fn uninstall(dir: &Path, out: &mut dyn Write) -> Result<()> {
    let home = home()?;
    let name = service_name(dir);
    let file = if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{name}", uid(&home)?)]).output();
        plist_path(&home, &name)
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", &format!("{name}.service")]).output();
        unit_path(&home, &name)
    };
    match std::fs::remove_file(&file) {
        Ok(()) => writeln!(out, "removed {name}")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => writeln!(out, "no dreams service for {}", dir.display())?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_and_unit_name_the_binary_and_vault() {
        let plist = render_plist("io.dreams.vault-x", Path::new("/opt/dreams"), Path::new("/v/my vault"), Path::new("/l.log"), "/usr/bin");
        assert!(plist.contains("<string>/opt/dreams</string>\n    <string>--vault</string>\n    <string>/v/my vault</string>\n    <string>daemon</string>"));
        let unit = render_unit(Path::new("/opt/dreams"), Path::new("/v"), "/usr/bin");
        assert!(unit.contains("ExecStart=/opt/dreams --vault /v daemon\n"));
        assert_ne!(service_name(Path::new("/a")), service_name(Path::new("/b")));
    }
}
