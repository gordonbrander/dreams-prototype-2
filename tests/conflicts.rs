//! Conflicts: what a sync merge leaves, found, merged and settled.

use dialog_artifacts::{Artifact, Instruction, Pick, Value};
use dreams::note::{self, CONTENT, Put};
use dreams::resolve::{self, Reply};
use dreams::vault::{self, Vault};

struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a sync of two devices' edits leaves: the shared value retracted
/// by both, and both new values asserted.
async fn merged_edits(v: &Vault, path: &str, base: &str, ours: &str, theirs: &str) {
    let put = Put { path: Some(path.into()), content: Some(base.into()), ..Default::default() };
    let e = note::put(v, put, false).await.unwrap();
    let relation = vault::relation(CONTENT).unwrap();
    v.write(vec![
        Instruction::Retract(Artifact { the: relation, of: e.clone(), is: Value::String(base.into()), cause: None }),
        vault::assert(&e, CONTENT, Value::String(ours.into()), Pick::All).unwrap(),
        vault::assert(&e, CONTENT, Value::String(theirs.into()), Pick::All).unwrap(),
    ])
    .await
    .unwrap();
}

#[tokio::test]
async fn content_conflicts_merge_by_lines_or_take_edits_and_then_disappear() {
    let d = Dir(std::env::temp_dir().join(format!("dreams-conflicts-{}", uuid::Uuid::now_v7())));
    let v = Vault::open(&d.0).await.unwrap();
    merged_edits(&v, "notes/clean.md", "one\ntwo\nthree\n", "ONE\ntwo\nthree\n", "one\ntwo\nTHREE\n").await;
    merged_edits(&v, "notes/marked.md", "one\n", "one\na\n", "one\nb\n").await;

    let all = resolve::list(&v, &[CONTENT.to_string()]).await.unwrap();
    let paths: Vec<_> = all.iter().map(|c| c.path.as_deref().unwrap()).collect();
    assert_eq!(paths, ["notes/clean.md", "notes/marked.md"]);
    assert_eq!(all[0].ancestor, Some(serde_json::json!("one\ntwo\nthree\n")));
    assert_eq!(all[0].merged.as_deref(), Some("ONE\ntwo\nTHREE\n"));
    let marked = all[1].marked.clone().unwrap();
    assert!(!resolve::diffs(&all[1]).is_empty());

    resolve::resolve(&v, "notes/clean.md", CONTENT, &Reply::default(), false).await.unwrap();
    let edit = resolve::parse_reply(&serde_json::json!({"content_edits": [{"old": marked.split_once("one\n").unwrap().1, "new": "a\nb\n"}]}).to_string()).unwrap();
    resolve::resolve(&v, "notes/marked.md", CONTENT, &edit, false).await.unwrap();

    assert!(resolve::list(&v, &[CONTENT.to_string()]).await.unwrap().is_empty());
    assert_eq!(note::get(&v, "notes/clean.md").await.unwrap().text(CONTENT), Some("ONE\ntwo\nTHREE\n"));
    assert_eq!(note::get(&v, "notes/marked.md").await.unwrap().text(CONTENT), Some("one\na\nb\n"));
}
