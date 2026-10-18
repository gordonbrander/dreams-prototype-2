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

/// The task version a run ran.
fn ran(run: &note::Note) -> dialog_artifacts::Value {
    run.first(task::RUN_VERSION).cloned().expect("a receipt links its version")
}

/// The newest run's prompt and version, after one requested tick.
async fn run_once(v: &Vault) -> (String, dialog_artifacts::Value) {
    task::request_run(v, "tasks/t.json").await.unwrap();
    let report = task::tick(v).await.unwrap();
    assert_eq!(report.fired.len(), 1, "{report:?}");
    assert_eq!(report.fired[0].exit_code, Some(0), "{report:?}");
    let run = task::runs(v, "tasks/t.json").await.unwrap().remove(0);
    (run.text(note::CONTENT).unwrap().to_string(), ran(&run))
}

#[tokio::test]
async fn an_edit_keeps_the_deployed_version_running() {
    let s = sandbox();
    let v = Vault::open(&s.0).await.unwrap();
    setup(&v, "1h", None).await;
    assert!(task::request_run(&v, "tasks/t.json").await.is_err(), "dormant tasks can't be requested");
    deploy(&v).await;

    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "not due for an hour");
    let (prompt, first) = run_once(&v).await;
    assert_eq!(prompt, "look");
    let runs = task::runs(&v, "tasks/t.json").await.unwrap();
    assert!(runs[0].path().unwrap().starts_with("runs/tasks/t/"));
    assert!(runs[0].is_protected());
    assert!(task::tick(&v).await.unwrap().fired.is_empty(), "the request was used up");

    // An edit shows as changed, and the deployed version keeps running.
    put(&v, "tasks/t.json", None, &[("dreams.task/prompt", json!("look again"))]).await;
    let list = task::list(&v).await.unwrap();
    let t = list.tasks.iter().find(|t| t.path == "tasks/t.json").unwrap();
    assert_eq!((t.status.as_str(), t.changed), ("deployed", true));
    assert_eq!(task::plan(&v, Some("tasks/t.json")).await.unwrap()[0].now, "changed");
    assert!(!list.scheduler.stale, "tick records itself");
    assert_eq!(run_once(&v).await, ("look".to_string(), first.clone()));

    // A redeploy runs the edit, as a new version.
    deploy(&v).await;
    let (prompt, second) = run_once(&v).await;
    assert_eq!(prompt, "look again");
    assert_ne!(second, first);

    // Versions are not tasks, and only the task is a task to a query.
    assert_eq!(task::entities(&v).await.unwrap().len(), 1);
    let q = json!({"deduce": {"find": ["t", "path"]},
        "when": [{"assert": {"concept": "task"}, "where": {"this": {"?": {"name": "t"}}, "path": {"?": {"name": "path"}}}}]});
    let rows = dreams::notation::query(&v, &q, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");

    // A version is read-only over MCP, and an altered one doesn't run.
    let dialog_artifacts::Value::Entity(version) = second else { panic!("an entity") };
    assert!(note::load(&v, &version).await.unwrap().is_protected());
    assert!(note::writable("dreams.version/source", false).is_err());
    let extra = dreams::vault::assert(&version, "dreams.task/cwd", dialog_artifacts::Value::String("/tmp".into()), dialog_artifacts::Pick::Last);
    v.write_unstamped(vec![extra.unwrap()]).await.unwrap();
    task::request_run(&v, "tasks/t.json").await.unwrap();
    let t = task::evaluate(&v, &note::resolve(&v, "tasks/t.json").await.unwrap(), dreams::vault::now_ms()).await.unwrap();
    assert!(t.error.as_deref().is_some_and(|e| e.contains("altered")), "{:?}", t.error);
    assert!(task::tick(&v).await.unwrap().fired.is_empty());
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
