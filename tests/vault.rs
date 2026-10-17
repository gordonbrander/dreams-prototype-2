//! Notes, links, queries and validation against a real vault on disk.

use dreams::error::Error;
use dreams::note::{self, Put};
use dreams::notation::query;
use dreams::vault::Vault;
use serde_json::{Value as Json, json};

struct Sandbox(std::path::PathBuf);

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("dreams-test-{}", uuid::Uuid::now_v7()));
        Sandbox(dir)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn put(path: &str) -> Put {
    Put { path: Some(path.into()), ..Default::default() }
}

async fn write(v: &Vault, path: &str, content: &str, tags: &[&str]) {
    let input = Put {
        content: Some(content.into()),
        tags: Some(tags.iter().map(|t| t.to_string()).collect()),
        ..put(path)
    };
    note::put(v, input, false).await.unwrap();
}

async fn paths(v: &Vault, rule: Json, column: &str) -> Vec<String> {
    let mut out: Vec<String> =
        query(v, &rule, 100).await.unwrap().into_iter().map(|r| r[column].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

fn backlinks_to(path: &str) -> Json {
    json!({"deduce": {"find": ["source", "path"]}, "when": [
        {"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}}, "where": {"this": {"?": {"name": "target"}}, "is": path}},
        {"assert": {"with": {"is": {"the": "dreams.note/link", "as": "entity:", "pick": "all:"}}}, "where": {"this": {"?": {"name": "source"}}, "is": {"?": {"name": "target"}}}},
        {"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}}, "where": {"this": {"?": {"name": "source"}}, "is": {"?": {"name": "path"}}}}]})
}

#[tokio::test]
async fn notes_round_trip_and_partial_updates_leave_other_fields_alone() {
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    write(&v, "notes/a.md", "hello\n", &["x", "y"]).await;
    note::put(&v, Put { title: Some("A".into()), ..put("notes/a.md") }, false).await.unwrap();

    let n = note::get(&v, "notes/a.md").await.unwrap();
    assert_eq!(n.text(note::CONTENT), Some("hello\n"));
    assert_eq!(n.text(note::TITLE), Some("A"));
    let mut tags = n.texts(note::TAG);
    tags.sort();
    assert_eq!(tags, ["x", "y"]);
    assert!(n.updated() > 0);
    assert_eq!(n.text(dreams::vault::BY), Some(v.session().as_str()));
    let md = n.markdown();
    assert!(md.contains("dreams.note:") && md.ends_with("---\nhello\n"), "{md}");

    // Same entity by dreams:/// and by did.
    let by_uri = note::get(&v, "dreams:///notes/a.md").await.unwrap();
    assert_eq!(by_uri.entity, n.entity);
    assert_eq!(note::get(&v, &n.entity.to_string()).await.unwrap().entity, n.entity);

    // Tags are replaced as a set.
    note::put(&v, Put { tags: Some(vec!["y".into()]), ..put("notes/a.md") }, false).await.unwrap();
    assert_eq!(note::get(&v, "notes/a.md").await.unwrap().texts(note::TAG), ["y"]);

    note::delete(&v, "notes/a.md", false).await.unwrap();
    assert!(matches!(note::get(&v, "notes/a.md").await, Err(Error::NotFound { .. })));
}

#[tokio::test]
async fn links_survive_renames_and_dangling_links_resolve_when_the_note_appears() {
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    write(&v, "a.md", "see [[b]] and [[c]]", &[]).await;
    assert_eq!(note::get(&v, "a.md").await.unwrap().texts(note::DANGLING).len(), 2);

    write(&v, "b.md", "I am b", &[]).await;
    let a = note::get(&v, "a.md").await.unwrap();
    assert_eq!(a.texts(note::DANGLING), ["c"], "b's arrival resolved its dangling link");
    assert_eq!(paths(&v, backlinks_to("b.md"), "path").await, ["a.md"]);

    // Rename b: the link is to the entity, so it follows.
    let b = note::get(&v, "b.md").await.unwrap();
    note::put(&v, Put { entity: Some(b.entity.to_string()), path: Some("notes/b.md".into()), ..Default::default() }, false)
        .await
        .unwrap();
    assert_eq!(paths(&v, backlinks_to("notes/b.md"), "path").await, ["a.md"]);
}

#[tokio::test]
async fn protection_staleness_paths_and_validation_are_enforced() {
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    let fields = json!({"dreams.note/protected": true}).as_object().unwrap().clone();
    note::put(&v, Put { fields, ..put("runners/x.json") }, true).await.unwrap();
    let edit = Put { title: Some("t".into()), ..put("runners/x.json") };
    assert!(matches!(note::put(&v, edit, false).await, Err(Error::Protected { .. })));
    assert!(matches!(note::delete(&v, "runners/x.json", false).await, Err(Error::Protected { .. })));

    write(&v, "a.md", "one", &[]).await;
    let stale = Put { content: Some("two".into()), expect_updated: Some(1), ..put("a.md") };
    assert!(matches!(note::put(&v, stale, false).await, Err(Error::Stale { .. })));

    write(&v, "b.md", "", &[]).await;
    let a = note::get(&v, "a.md").await.unwrap();
    let clash = Put { entity: Some(a.entity.to_string()), path: Some("b.md".into()), ..Default::default() };
    assert!(note::put(&v, clash, false).await.is_err(), "two notes can't share a path");

    let bad = Put { concept: Some("bookmark".into()), ..put("bookmarks/x.md") };
    assert!(matches!(note::put(&v, bad, false).await, Err(Error::Validation { .. })));
    let fields = json!({"dreams.bookmark/url": "https://example.com"}).as_object().unwrap().clone();
    note::put(&v, Put { concept: Some("bookmark".into()), fields, ..put("bookmarks/x.md") }, false).await.unwrap();

    let managed = json!({"dreams.note/updated": 5}).as_object().unwrap().clone();
    assert!(note::put(&v, Put { fields: managed, ..put("a.md") }, false).await.is_err());
}

#[tokio::test]
async fn every_recipe_in_the_dialog_skill_runs() {
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    write(&v, "notes/sleep.md", "rest. see [[nowhere]]", &["dream", "idea"]).await;
    write(&v, "a.md", "see [[notes/sleep.md]]", &["dream"]).await;
    write(&v, "tasks/brief.json", "", &[]).await;

    let skill = include_str!("../src/seed/skills/dialog.md");
    let recipes = skill.split("## Recipes").nth(1).unwrap().split("## Write facts").next().unwrap();
    let mut ran = 0;
    for block in recipes.split("```json\n").skip(1) {
        let rule: Json = serde_json::from_str(block.split("```").next().unwrap()).unwrap();
        let rows = query(&v, &rule, 100).await.unwrap_or_else(|e| panic!("{e}\n{rule:#}"));
        ran += 1;
        let _ = rows;
    }
    assert_eq!(ran, 9, "the skill has nine recipes");

    let tagged = paths(&v, query_tag("dream"), "path").await;
    assert_eq!(tagged, ["a.md", "notes/sleep.md"]);
    let counts = query(&v, &json!({"deduce": {"find": ["tag", "count"]}, "when": [
        {"assert": {"with": {"is": {"the": "dreams.note/tag", "as": "text:", "pick": "all:"}}},
         "where": {"this": {"?": {"name": "note"}}, "is": {"?": {"name": "tag"}}}}],
        "reduce": {"count": {"apply": "count", "of": {"?": {"name": "note"}}}}}), 100).await.unwrap();
    assert!(counts.contains(&json!({"tag": "dream", "count": 2}).as_object().unwrap().clone()), "{counts:?}");
}

fn query_tag(tag: &str) -> Json {
    json!({"deduce": {"find": ["note", "path"]}, "when": [
        {"assert": {"with": {"is": {"the": "dreams.note/tag", "as": "text:", "pick": "all:"}}}, "where": {"this": {"?": {"name": "note"}}, "is": tag}},
        {"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}}, "where": {"this": {"?": {"name": "note"}}, "is": {"?": {"name": "path"}}}}]})
}

#[tokio::test]
async fn two_handles_on_one_vault_see_each_others_writes() {
    let s = Sandbox::new();
    let a = Vault::open(&s.0).await.unwrap();
    let b = Vault::open(&s.0).await.unwrap();
    write(&a, "from-a.md", "a", &[]).await;
    write(&b, "from-b.md", "b", &[]).await;
    assert!(note::get(&a, "from-b.md").await.is_ok());
    assert!(note::get(&b, "from-a.md").await.is_ok());
}

#[tokio::test]
async fn search_ranks_titles_first_filters_and_catches_up() {
    use dreams::search::Search;
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    write(&v, "notes/body.md", "a long note about sleep and dreams", &["health"]).await;
    note::put(&v, Put { title: Some("Sleep".into()), content: Some("rest".into()), ..put("journal/title.md") }, false)
        .await
        .unwrap();
    let search = Search::open(&s.0).unwrap();
    assert_eq!(search.catch_up(&v).await.unwrap(), Some(2));
    assert_eq!(search.catch_up(&v).await.unwrap(), Some(0), "nothing changed");

    let hits = search.search("sleep", None, None, 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["journal/title.md", "notes/body.md"]);
    assert!(hits[1].snippet.contains("**sleep**"), "{}", hits[1].snippet);
    assert_eq!(search.search("sleep", Some("health"), None, 10).unwrap().len(), 1);
    assert_eq!(search.search("sleep", None, Some("journal/"), 10).unwrap()[0].path, "journal/title.md");

    // A write from another handle shows up after catch-up; a delete disappears.
    let other = Vault::open(&s.0).await.unwrap();
    write(&other, "notes/new.md", "fresh sleep", &[]).await;
    note::delete(&other, "notes/body.md", false).await.unwrap();
    search.catch_up(&v).await.unwrap();
    let listed = search.search("", None, None, 10).unwrap();
    assert_eq!(listed[0].path, "notes/new.md", "empty query lists newest first");
    assert!(!listed.iter().any(|h| h.path == "notes/body.md"));
}

#[tokio::test]
async fn open_ended_concepts_from_invented_facts_to_rules() {
    use dreams::describe::{define_concept, describe};
    use dreams::notation::define_rule;
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();

    // 1. Invent: facts in a new domain, no definition.
    let fields = json!({"me.recipes/cuisine": "thai", "me.recipes/ingredient": ["rice", "lime"]}).as_object().unwrap().clone();
    note::put(&v, Put { fields, ..put("recipes/pad-thai.md") }, false).await.unwrap();

    // 2. Discover: describe reports them undeclared, and that ingredient is many-valued.
    let d = describe(&v, Some("me.recipes")).await.unwrap();
    let attrs = d["attributes"].as_array().unwrap();
    let ingredient = attrs.iter().find(|a| a["the"] == "me.recipes/ingredient").unwrap();
    assert_eq!(ingredient["undeclared"], true);
    assert_eq!(ingredient["multi"], true);
    assert_eq!(ingredient["types"], json!(["text:"]));

    // 3. Name: define a concept, then query by name and validate writes.
    let recipe = json!({"description": "A recipe", "with": {
        "cuisine": {"description": "Cuisine", "the": "me.recipes/cuisine", "as": "text:"},
        "ingredient": {"description": "One ingredient", "the": "me.recipes/ingredient", "as": "text:", "pick": "all:"}}});
    define_concept(&v, "schemas/recipe.yaml", &recipe).await.unwrap();
    let d = describe(&v, Some("me.recipes")).await.unwrap();
    assert_eq!(d["concepts"]["schemas/recipe.yaml"]["description"], "A recipe");
    let ingredient = d["attributes"].as_array().unwrap().iter().find(|a| a["the"] == "me.recipes/ingredient").unwrap().clone();
    assert_eq!(ingredient["pick"], "all:", "declared now");

    let rows = query(&v, &json!({"deduce": {"find": ["r", "i"]}, "when": [
        {"assert": {"concept": "schemas/recipe.yaml"}, "where": {"this": {"?": {"name": "r"}}, "ingredient": {"?": {"name": "i"}}}}]}), 100)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let missing = Put { concept: Some("schemas/recipe.yaml".into()), ..put("recipes/empty.md") };
    assert!(matches!(note::put(&v, missing, false).await, Err(Error::Validation { .. })));

    // 4. Install a rule; its conclusions read like stored facts.
    let rule = json!({
        "deduce": {"description": "A Thai recipe", "with": {"dish": {"the": "me.recipes.thai/dish", "as": "text:"}}},
        "when": [
            {"assert": {"with": {"is": {"the": "me.recipes/cuisine", "as": "text:"}}}, "where": {"this": {"?": {"name": "this"}}, "is": "thai"}},
            {"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}}, "where": {"this": {"?": {"name": "this"}}, "is": {"?": {"name": "dish"}}}}]});
    define_rule(&v, &rule).await.unwrap();
    let thai = query(&v, &json!({"deduce": {"find": ["r", "dish"]}, "when": [
        {"assert": {"with": {"is": {"the": "me.recipes.thai/dish", "as": "text:"}}}, "where": {"this": {"?": {"name": "r"}}, "is": {"?": {"name": "dish"}}}}]}), 100)
        .await
        .unwrap();
    assert_eq!(thai.len(), 1, "{thai:?}");
    assert_eq!(thai[0]["dish"], "recipes/pad-thai.md");
    assert_eq!(describe(&v, None).await.unwrap()["rules"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_spike_brief_task_imports_as_the_facts_in_spec_section_4() {
    use dreams::import::{Options, export, import, read_dir};
    let s = Sandbox::new();
    let v = Vault::open(&s.0).await.unwrap();
    let docs = vec![
        ("tasks/brief.json".to_string(), include_str!("../src/seed/tasks/brief.json").to_string()),
        ("runners/claude.json".to_string(), include_str!("../src/seed/runners/claude.json").to_string()),
        ("tasks/inbox.json".to_string(), r#"{"_type": "doc://schemas/task.json", "runner": "doc://runners/claude.json",
            "every": "5m", "prompt": "triage", "when": {"tag": "inbox"}, "startedAt": "x", "Bad Key!": 1}"#.to_string()),
    ];
    let report = import(&v, &docs, &Options::default()).await.unwrap();
    assert!(report.iter().all(|r| r.error.is_none()), "{report:?}");

    let task = note::get(&v, "tasks/brief.json").await.unwrap();
    let runner = note::get(&v, "runners/claude.json").await.unwrap();
    let mut relations: Vec<&str> = task.facts.keys().map(String::as_str).collect();
    relations.sort();
    assert_eq!(relations, [
        "dreams.note/by", "dreams.note/created", "dreams.note/path", "dreams.note/title", "dreams.note/updated",
        "dreams.task/every", "dreams.task/prompt", "dreams.task/runner",
    ]);
    assert_eq!(task.first("dreams.task/runner"), Some(&dialog_artifacts::Value::Entity(runner.entity.clone())));
    assert_eq!(task.text("dreams.task/every"), Some("1d"));
    let argv: Vec<String> = serde_json::from_str(runner.text("dreams.runner/command").unwrap()).unwrap();
    assert_eq!(argv[0], "claude", "argv kept in order as JSON text");

    // The spike's `when` filter became a query that finds the tagged notes.
    let inbox = note::get(&v, "tasks/inbox.json").await.unwrap();
    let when: Json = serde_json::from_str(inbox.text("dreams.task/when").unwrap()).unwrap();
    write(&v, "inbox/one.md", "x", &["inbox"]).await;
    assert_eq!(paths(&v, when, "path").await, ["inbox/one.md"]);
    assert_eq!(inbox.text("dreams.task/started-at"), Some("x"), "camelCase keys become kebab-case");
    assert!(report[2].skipped.iter().any(|s| s.contains("bad-key!")), "{:?}", report[2].skipped);

    // Export and re-import into a fresh vault: same facts, same entities.
    let out = s.0.join("export");
    export(&v, &out).await.unwrap();
    let s2 = Sandbox::new();
    let v2 = Vault::open(&s2.0).await.unwrap();
    import(&v2, &read_dir(&out).unwrap(), &Options::default()).await.unwrap();
    let again = note::get(&v2, "tasks/brief.json").await.unwrap();
    assert_eq!(again.entity, task.entity);
    for the in ["dreams.note/title", "dreams.task/every", "dreams.task/prompt", "dreams.task/runner"] {
        assert_eq!(again.facts.get(the), task.facts.get(the), "{the}");
    }
}
