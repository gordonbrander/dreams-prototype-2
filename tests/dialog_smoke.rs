//! Pins every Dialog behaviour dreams relies on (SPEC §15, risk 1).
//! Dialog is experimental and breaks often: after bumping the tag in
//! Cargo.toml, run this file first.

use anyhow::Result;
use dialog_artifacts::{Artifact, ArtifactSelector, Changes, Entity, Instruction, Pick, Relation, Value};
use dialog_capability::Subject;
use dialog_credentials::SignerCredential;
use dialog_effects::storage::{Directory, Location};
use dialog_peer::{Allowance, OpenCredential, Peer, Session, SpaceVaultExt as _};
use dialog_query::{Any, Binding, ConceptConclusion, ConceptDescriptor, ConceptQuery, DeductiveRuleDescriptor, Output as _, Parameters, Term};
use dialog_artifacts::history::HistorySelector;
use dialog_repository::schema::EntityExt as _;
use dialog_repository::{ACCESS_BRANCH, Branch, Repository, RepositoryExt as _};
use dialog_storage::provider::storage::{CredentialStore, NativeSpace, Storage};
use dialog_varsig::Principal as _;
use futures_util::TryStreamExt as _;
use serde_json::json;

type Env = Peer<NativeSpace, Session>;

/// Open (or create) a vault at `dir`, the way `vault.rs` will.
async fn open(dir: &std::path::Path) -> Result<(Env, Branch)> {
    let base = Directory::At(dir.to_string_lossy().into_owned());
    let credentials = CredentialStore::<NativeSpace>::default();
    let key = |name: &'static str| OpenCredential::open(name).at(base.clone()).perform(&credentials);
    let system: SignerCredential = key("system").await?;
    let me: SignerCredential = key("peer").await?;
    let custodian: SignerCredential = key("custodian").await?;
    let peer = Peer::new(me.clone())
        .at(Location::new(base.clone(), "home"))
        .space(Repository::from(me.did()).branch(ACCESS_BRANCH))
        .with(Storage::<NativeSpace>::default().owned_by(system.did()))
        .grant(Allowance::storage(&system))
        .base(base)
        .build()
        .await?;
    if peer.authority().await.is_err() {
        let account = peer.state().vault("account").create().perform(&peer).await?;
        account.add(custodian.did()).perform(&peer).await?;
        account.delegate(peer.did()).perform(&peer).await?;
    }
    let session = peer.session(b"dreams").space(peer.state()).allow(Subject::any()).await?;
    let repo = peer.space("vault").open().perform(&session).await?;
    let branch = repo.branch("main").open().perform(&session).await?;
    Ok((session, branch))
}

fn fact(of: &Entity, the: &str, is: Value) -> Artifact {
    Artifact { the: the.parse::<Relation>().unwrap(), of: of.clone(), is, cause: None }
}

fn text(s: &str) -> Value {
    Value::String(s.into())
}

async fn commit(env: &Env, branch: &Branch, changes: Vec<Instruction>) -> Result<()> {
    let changes: Changes = changes.into_iter().collect();
    branch.transaction().integrate(changes).commit().publish().perform(env).await?;
    Ok(())
}

async fn claims(env: &Env, branch: &Branch, of: &Entity, the: &str) -> Result<Vec<Value>> {
    let selector = ArtifactSelector::new().of(of.clone()).the(the.parse()?);
    let found: Vec<Artifact> = branch.claims().select(selector).to_owned().perform(env).await?.try_collect().await?;
    Ok(found.into_iter().map(|a| a.is).collect())
}

/// The raw value a conclusion bound to `var`.
fn value(row: &ConceptConclusion, var: &str) -> Option<Value> {
    match row.source().lookup(&Term::<Any>::var(var)) {
        Ok(Binding::Present(v)) => Some(v),
        _ => None,
    }
}

fn query(descriptor: serde_json::Value, fields: &[&str]) -> Result<ConceptQuery> {
    let predicate: ConceptDescriptor = serde_json::from_value(descriptor)?;
    let mut terms = Parameters::new();
    terms.insert("this".into(), Term::var("this"));
    for f in fields {
        terms.insert((*f).into(), Term::var(*f));
    }
    Ok(ConceptQuery { predicate, terms })
}

#[tokio::test]
async fn dialog_does_what_dreams_needs() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("dreams-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let (env, branch) = open(&dir).await?;

    // Runtime relations, `last:` and `all:` on one entity.
    let note = Entity::new()?;
    let long = "x".repeat(10_000); // spills to the asset store
    commit(&env, &branch, vec![
        Instruction::Assert(fact(&note, "dreams.note/path", text("notes/a.md")), Pick::Last),
        Instruction::Assert(fact(&note, "dreams.note/title", text("first")), Pick::Last),
        Instruction::Assert(fact(&note, "dreams.note/tag", text("dream")), Pick::All),
        Instruction::Assert(fact(&note, "dreams.note/tag", text("idea")), Pick::All),
        Instruction::Assert(fact(&note, "dreams.note/content", text(&long)), Pick::Last),
    ])
    .await?;
    commit(&env, &branch, vec![Instruction::Assert(fact(&note, "dreams.note/title", text("second")), Pick::Last)]).await?;

    assert_eq!(claims(&env, &branch, &note, "dreams.note/title").await?, vec![text("second")], "last: supersedes");
    assert_eq!(claims(&env, &branch, &note, "dreams.note/tag").await?.len(), 2, "all: accumulates");
    assert_eq!(claims(&env, &branch, &note, "dreams.note/content").await?, vec![text(&long)], "long text round-trips");

    // A concept query from JSON, reading tags under all:.
    let rows: Vec<ConceptConclusion> = branch
        .select(query(json!({"with": {"tag": {"the": "dreams.note/tag", "as": "text:", "pick": "all:"}}}), &["tag"])?)
        .perform(&env)
        .try_vec()
        .await?;
    assert_eq!(rows.len(), 2, "pick all: sees every tag");

    // A one-off rule, evaluated without installing it.
    let rule: DeductiveRuleDescriptor = serde_json::from_value(json!({
        "deduce": {"with": {"path": {"the": "dreams.query/path", "as": "text:"}}},
        "when": [
            {"assert": {"with": {"is": {"the": "dreams.note/tag", "as": "text:", "pick": "all:"}}},
             "where": {"this": {"?": {"name": "this"}}, "is": "dream"}},
            {"assert": {"with": {"is": {"the": "dreams.note/path", "as": "text:"}}},
             "where": {"this": {"?": {"name": "this"}}, "is": {"?": {"name": "path"}}}}
        ]
    }))?;
    let head = rule.deduce.clone();
    let rows: Vec<ConceptConclusion> = branch
        .query()
        .with(rule.clone().compile()?)
        .select(ConceptQuery { predicate: head.clone(), terms: { let mut t = Parameters::new(); t.insert("this".into(), Term::var("this")); t.insert("path".into(), Term::var("path")); t } })
        .perform(&env)
        .try_vec()
        .await?;
    assert_eq!(rows.iter().map(|r| value(r, "path")).collect::<Vec<_>>(), vec![Some(text("notes/a.md"))]);

    // History keeps superseded values (scan, filtered by entity and relation).
    let title: Relation = "dreams.note/title".parse()?;
    let history = branch.history(&env);
    let records: Vec<_> = history.select(HistorySelector::All).try_collect().await?;
    let titles: Vec<Value> = records
        .iter()
        .map(|(_, r)| r.claim())
        .filter(|c| c.of == note && c.the == title)
        .map(|c| c.is.clone())
        .collect();
    assert!(titles.contains(&text("first")), "history has the superseded title: {titles:?}");

    // Installing a rule persists it as `dialog.rule/source` facts.
    let installed = rule.compile()?;
    branch.transaction().assert(installed).commit().publish().perform(&env).await?;
    let sources: Vec<Artifact> = branch
        .claims()
        .select(ArtifactSelector::new().the("dialog.rule/source".parse()?))
        .to_owned()
        .perform(&env)
        .await?
        .try_collect()
        .await?;
    assert_eq!(sources.len(), 1, "installed rule is stored as a fact");

    // Content-derived ids are stable.
    assert_eq!(Entity::of(&("feed", "key")), Entity::of(&("feed", "key")));
    assert_ne!(Entity::of(&("feed", "key")), Entity::of(&("feed", "other")));

    // Retract every fact of an entity.
    let all: Vec<Artifact> = branch.claims().select(ArtifactSelector::new().of(note.clone())).to_owned().perform(&env).await?.try_collect().await?;
    commit(&env, &branch, all.into_iter().map(Instruction::Retract).collect()).await?;
    assert!(claims(&env, &branch, &note, "dreams.note/path").await?.is_empty(), "retract removes");

    // A second handle on the same directory: a stale head is refused, refresh fixes it.
    let (env2, branch2) = open(&dir).await?;
    let other = Entity::new()?;
    commit(&env, &branch, vec![Instruction::Assert(fact(&other, "dreams.note/path", text("notes/b.md")), Pick::Last)]).await?;
    let stale = commit(&env2, &branch2, vec![Instruction::Assert(fact(&other, "dreams.note/title", text("b")), Pick::Last)]).await;
    if stale.is_err() {
        branch2.refresh(&env2).await?;
        commit(&env2, &branch2, vec![Instruction::Assert(fact(&other, "dreams.note/title", text("b")), Pick::Last)]).await?;
    }
    branch.refresh(&env).await?;
    assert_eq!(claims(&env, &branch, &other, "dreams.note/path").await?, vec![text("notes/b.md")], "first write kept");
    assert_eq!(claims(&env, &branch, &other, "dreams.note/title").await?, vec![text("b")], "second write landed");
    println!("stale handle refused: {}", stale.is_err());

    // Reopening the directory sees everything (persistence).
    drop((env, branch, env2, branch2));
    let (env, branch) = open(&dir).await?;
    assert_eq!(claims(&env, &branch, &other, "dreams.note/title").await?, vec![text("b")], "persists across opens");

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
