//! Seeding: the built-in notes every vault starts with (SPEC §11).
//!
//! Each is a file under `src/seed/`, and its path there is its note's path.
//! Seeding imports them with entity ids derived from their paths, so two
//! vaults seeded apart converge on the same entities instead of conflicting
//! when they sync. Only facts that differ from the defaults are written.

use crate::error::Result;
use crate::import::{self, FileResult, Options};
use crate::note::{self, Put};
use crate::vault::Vault;

/// One seed file: its path, and its text.
macro_rules! seed {
    ($path:literal) => {
        ($path, include_str!(concat!("seed/", $path)))
    };
}

/// Every seed file. Runners come before the tasks that name them.
pub const FILES: &[(&str, &str)] = &[
    // Not `--bare`: bare mode skips the stored login. Bash runs in the
    // sandbox: it writes only in the cwd and has no network, and `dreams`
    // runs outside it to write the vault. `Edit(./**)` covers every
    // file-writing tool.
    seed!("runners/claude.json"),
    // `-c approval_policy=never` works on every Codex version; `-a` does not.
    // The workspace-write sandbox writes only in the cwd.
    seed!("runners/codex.json"),
    seed!("runners/pi.json"),
    // Not an agent: pulls every feed. The prompt is not read.
    seed!("runners/feeds.json"),
    seed!("skills/dreams.md"),
    seed!("skills/dialog.md"),
    seed!("skills/daily-note.md"),
    seed!("skills/bookmark.md"),
    seed!("skills/brief.md"),
    seed!("prompts/daily.md"),
    seed!("prompts/intention.md"),
    seed!("prompts/bookmark.md"),
    seed!("prompts/brief.md"),
    // Seeded tasks are templates: each runs only where the user deploys it.
    seed!("tasks/brief.json"),
    seed!("tasks/pull-feeds.json"),
];

/// Seeded runners are read-only over MCP (SPEC §10.5): resolving conflicts
/// runs them without a deploy.
const PROTECTED: &str = "runners/";

/// Write every built-in note whose facts differ from the default.
pub async fn seed(vault: &Vault) -> Result<Vec<FileResult>> {
    let docs: Vec<(String, String)> = FILES.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
    let results = import::import(vault, &docs, &Options { derive_ids: true, ..Default::default() }).await?;
    for (path, _) in FILES.iter().filter(|(p, _)| p.starts_with(PROTECTED)) {
        let n = note::get(vault, path).await?;
        if !n.is_protected() {
            let fields = [(note::PROTECTED.to_string(), serde_json::json!(true))].into_iter().collect();
            note::put(vault, Put { path: Some(path.to_string()), fields, ..Default::default() }, true).await?;
        }
    }
    Ok(results)
}

/// Whether `path` is a seeded note.
pub fn is_seeded(path: &str) -> bool {
    FILES.iter().any(|(p, _)| *p == path)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn files_under(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path, root, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }

    #[test]
    fn every_seed_file_is_listed_and_parses() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/seed");
        let mut on_disk = Vec::new();
        files_under(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut listed: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        listed.sort();
        assert_eq!(on_disk, listed);
        for (path, text) in FILES {
            import::parse(path, text).unwrap_or_else(|e| panic!("{path}: {e}"));
        }
    }
}
