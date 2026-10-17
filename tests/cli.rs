//! The CLI, driven in-process.

use serde_json::Value;

struct Sandbox {
    dir: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox { dir: std::env::temp_dir().join(format!("dreams-cli-{}", uuid::Uuid::now_v7())) }
    }

    /// Run `dreams --vault <dir> args...` with `stdin`; (exit code, stdout, stderr).
    fn run(&self, args: &[&str], stdin: &str) -> (i32, String, String) {
        let mut argv = vec!["dreams", "--vault", self.dir.to_str().unwrap()];
        argv.extend(args);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut no_tty = |_: &str| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal"));
        let code = dreams::cli::run(argv, &mut stdin.as_bytes(), &mut out, &mut err, &mut no_tty);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn ok(&self, args: &[&str], stdin: &str) -> String {
        let (code, out, err) = self.run(args, stdin);
        assert_eq!(code, 0, "{args:?}: {err}");
        out
    }

    fn json(&self, args: &[&str], stdin: &str) -> Value {
        let mut a = vec!["--json"];
        a.extend(args);
        serde_json::from_str(&self.ok(&a, stdin)).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_note_round_trips_through_put_get_search_and_query() {
    let s = Sandbox::new();
    assert!(s.ok(&["init"], "").contains("ok       skills/dialog.md"));
    s.ok(&["note", "put", "notes/a.md"], "---\ntitle: Alpha\ntags: [x]\n---\nhello [[notes/b.md]]\n");
    let md = s.ok(&["note", "get", "notes/a.md"], "");
    assert!(md.contains("title: Alpha") && md.ends_with("---\nhello [[notes/b.md]]\n"), "{md}");

    // What `get` prints, `put` takes back: the same facts, no new ones.
    s.ok(&["note", "put", "notes/a.md"], &md);
    assert_eq!(s.ok(&["note", "get", "notes/a.md"], ""), md);

    let hits = s.json(&["note", "search", "hello"], "");
    assert_eq!(hits[0]["path"], "notes/a.md");

    let rows = s.json(
        &["query", "-"],
        r#"{"deduce": {"find": ["n", "missing"]}, "when": [
            {"assert": {"with": {"is": {"the": "dreams.note/dangling", "as": "text:", "pick": "all:"}}},
             "where": {"this": {"?": {"name": "n"}}, "is": {"?": {"name": "missing"}}}}]}"#,
    );
    assert_eq!(rows[0]["missing"], "notes/b.md");

    // YAML works too.
    let yaml = "deduce: {find: [n]}\nwhen:\n  - assert: {with: {is: {the: dreams.note/title, as: 'text:'}}}\n    where: {this: {'?': {name: n}}, is: Alpha}\n";
    assert_eq!(s.json(&["query", "-"], yaml).as_array().unwrap().len(), 1);
}

#[test]
fn errors_are_json_with_a_name() {
    let s = Sandbox::new();
    let (code, _, err) = s.run(&["note", "get", "nope.md"], "");
    assert_eq!(code, 1);
    let e: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(e["name"], "not_found");

    let (_, _, err) = s.run(&["query", "-"], r#"{"deduce": {"find": ["x"]}, "when": []}"#);
    let e: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(e["name"], "query");
    assert!(e["hint"].is_string(), "{e}");
}

#[test]
fn import_export_and_mcp_json() {
    let s = Sandbox::new();
    let src = s.dir.with_extension("src");
    std::fs::create_dir_all(src.join("notes")).unwrap();
    std::fs::write(src.join("notes/x.md"), "---\ntitle: X\nmood: calm\n---\nbody\n").unwrap();
    std::fs::write(src.join(".hidden.md"), "skip me").unwrap();
    let report = s.json(&["import", src.to_str().unwrap()], "");
    assert_eq!(report["errors"], 0);
    assert_eq!(report["results"].as_array().unwrap().len(), 1);
    let note = s.json(&["note", "get", "notes/x.md"], "");
    assert_eq!(note["dreams.json"]["mood"], "calm", "untyped keys go to dreams.json");

    let out = s.dir.with_extension("out");
    s.ok(&["export", out.to_str().unwrap()], "");
    assert!(std::fs::read_to_string(out.join("notes/x.md")).unwrap().contains("mood: calm"));
    assert!(out.join("skills/dialog.md").exists());

    let entry = s.json(&["mcp-json"], "");
    assert_eq!(entry["args"][0], "--vault");
    assert_eq!(entry["args"][2], "serve");
    let _ = std::fs::remove_dir_all(src);
    let _ = std::fs::remove_dir_all(out);
}
