//! Tasks: deploy, triggers, leases and receipts against a real vault.

use std::time::Duration;

use dreams::note::{self, Put};
use dreams::task;
use dreams::vault::Vault;
use serde_json::{Map, Value as Json, json};

struct Sandbox(std::path::PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sandbox() -> Sandbox {
    Sandbox(std::env::temp_dir().join(format!("dreams-tasks-{}", uuid::Uuid::now_v7())))
}

fn fields(pairs: &[(&str, Json)]) -> Map<String, Json> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

async fn put(v: &Vault, path: &str, content: Option<&str>, f: &[(&str, Json)]) {
    let input = Put { path: Some(path.into()), content: content.map(str::to_string), fields: fields(f), ..Default::default() };
    note::put(v, input, true).await.unwrap();
}

const DANGLING: &str = r#"{"deduce": {"find": ["note", "missing"]}, "when": [
  {"assert": {"with": {"is": {"the": "dreams.note/dangling", "as": "text:", "pick": "all:"}}},
   "where": {"this": {"?": {"name": "note"}}, "is": {"?": {"name": "missing"}}}}]}"#;

/// `cat` echoes the prompt: the receipt's content is what the agent got.
async fn setup(v: &Vault, every: &str, when: Option<&str>) {
    put(v, "runners/cat.json", None, &[("dreams.runner/command", json!("[\"cat\"]"))]).await;
    let mut f = vec![
        ("dreams.task/runner", json!("runners/cat.json")),
        ("dreams.task/every", json!(every)),
        ("dreams.task/prompt", json!("look")),
    ];
    if let Some(w) = when {
        f.push(("dreams.task/when", json!(w)));
    }
    put(v, "tasks/t.json", None, &f).await;
}

async fn deploy(v: &Vault) {
    let plan = task::plan(v, Some("tasks/t.json")).await.unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].describe().contains("command: cat"), "{}", plan[0].describe());
    task::apply(v, &plan).await.unwrap();
    assert!(task::plan(v, Some("tasks/t.json")).await.unwrap().is_empty(), "deployed as it is");
}

#[tokio::test]
async fn a_requested_run_writes_a_receipt_and_an_edit_needs_a_redeploy() {
    let s = sandbox();
    let v = Vault::open(&s.0).await.unwrap();
    setup(&v, "1h", None).await;
    assert!(task::request_run(&v, "tasks/t.json").await.is_err(), "dormant tasks can't be requested");
    deploy(&v).await;

    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "not due for an hour");
    task::request_run(&v, "tasks/t.json").await.unwrap();
    let report = task::tick(&v).await.unwrap();
    assert_eq!(report.fired.len(), 1, "{report:?}");
    let fired = &report.fired[0];
    assert_eq!(fired.exit_code, Some(0), "{fired:?}");
    assert!(fired.run.starts_with("runs/tasks/t/"));

    let runs = task::runs(&v, "tasks/t.json").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text(note::CONTENT), Some("look"));
    assert!(runs[0].is_protected());
    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "the request was used up");

    // An edit shows as changed and doesn't run until deployed again.
    put(&v, "tasks/t.json", None, &[("dreams.task/prompt", json!("look again"))]).await;
    let list = task::list(&v).await.unwrap();
    let t = list.tasks.iter().find(|t| t.path == "tasks/t.json").unwrap();
    assert_eq!(t.status, "changed");
    assert!(task::plan(&v, Some("tasks/t.json")).await.unwrap()[0].now == "changed");
    assert!(!list.scheduler.stale, "tick records itself");
}

#[tokio::test]
async fn a_when_query_fires_on_new_rows_but_not_on_the_tasks_own_writes() {
    let s = sandbox();
    let v = Vault::open(&s.0).await.unwrap();
    put(&v, "notes/old.md", Some("[[old-missing.md]]"), &[]).await;
    setup(&v, "1s", Some(DANGLING)).await;
    deploy(&v).await;

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "rows seen at deploy don't fire");

    put(&v, "notes/new.md", Some("see [[new-missing.md]]"), &[]).await;
    let report = task::tick(&v).await.unwrap();
    assert_eq!(report.fired.len(), 1, "{report:?}");
    let run = task::runs(&v, "tasks/t.json").await.unwrap().remove(0);
    let prompt = run.text(note::CONTENT).unwrap();
    assert!(prompt.contains("Changed since last run:\n+ ") && prompt.contains("new-missing.md"), "{prompt}");
    assert!(!prompt.contains("old-missing.md"), "{prompt}");

    // The task's own session writes a dangling link: no wake.
    let entity = note::resolve(&v, "tasks/t.json").await.unwrap();
    let agent = Vault::open_as(&s.0, &entity.to_string()).await.unwrap();
    put(&agent, "notes/mine.md", Some("[[mine-missing.md]]"), &[]).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "own writes don't wake the task");

    // Someone else's write still does.
    put(&v, "notes/other.md", Some("[[other-missing.md]]"), &[]).await;
    assert_eq!(task::tick(&v).await.unwrap().fired.len(), 1);
}
