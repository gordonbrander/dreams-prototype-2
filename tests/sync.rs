//! Two devices sync through a folder remote (SPEC §12.2).

use dreams::note::{self, CONTENT, Put};
use dreams::peer;
use dreams::resolve;
use dreams::vault::Vault;

struct Dirs(Vec<std::path::PathBuf>);

impl Drop for Dirs {
    fn drop(&mut self) {
        for d in &self.0 {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn content(path: &str, text: &str) -> Put {
    Put { path: Some(path.into()), content: Some(text.into()), ..Default::default() }
}

/// Run `f` the way the CLI runs commands: on a thread with a large stack.
fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(dreams::cli::STACK)
        .spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(f))
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn two_devices_sync_through_a_folder_and_concurrent_edits_conflict() {
    run(sync_and_conflict());
}

async fn sync_and_conflict() {
    let base = std::env::temp_dir().join(format!("dreams-sync-{}", uuid::Uuid::now_v7()));
    let _dirs = Dirs(vec![base.clone()]);
    let (a_dir, b_dir, remote) = (base.join("a"), base.join("b"), base.join("remote"));
    // Open each vault twice, as separate processes would: only the first open holds repository keys.
    drop((Vault::open(&a_dir).await.unwrap(), Vault::open(&b_dir).await.unwrap()));
    let a = Vault::open(&a_dir).await.unwrap();
    let b = Vault::open(&b_dir).await.unwrap();

    peer::add_remote(&a, "vault", "origin", &remote).await.unwrap();
    note::put(&a, content("notes/x.md", "one\n"), false).await.unwrap();
    assert!(a.push().await.unwrap());

    // B joins with the delegation A leaves in the folder.
    peer::device_add(&a, &b.device()).await.unwrap();
    assert_eq!(peer::add_remote(&b, "vault", "origin", &remote).await.unwrap().repository, a.repository());
    assert!(b.pull().await.unwrap());
    assert_eq!(note::get(&b, "notes/x.md").await.unwrap().text(CONTENT), Some("one\n"));

    // Both edit without seeing each other.
    note::put(&a, content("notes/x.md", "one\nfrom a\n"), false).await.unwrap();
    note::put(&b, content("notes/x.md", "one\nfrom b\n"), false).await.unwrap();
    a.push().await.unwrap();
    assert!(b.push().await.is_err(), "B's push is stale");
    b.pull().await.unwrap();
    b.push().await.unwrap();
    a.pull().await.unwrap();

    for v in [&a, &b] {
        let conflicts = resolve::list(v, &[CONTENT.to_string()]).await.unwrap();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].values.len(), 2);
    }

    // A publishes; B follows A's public folder under a petname and links to it.
    let public = base.join("public");
    peer::add_remote(&a, peer::PUBLIC, "origin", &public).await.unwrap();
    note::put(&a, content("notes/hello.md", "hi from a\n"), false).await.unwrap();
    peer::publish(&a, "notes/hello.md").await.unwrap();
    peer::peer_grant(&a, &b.device()).await.unwrap();
    peer::follow(&b, "alice", &public).await.unwrap();
    let theirs = note::get(&b, "dreams://alice/notes/hello.md").await.unwrap();
    assert_eq!(theirs.text(CONTENT), Some("hi from a\n"));
    assert!(note::get(&b, "notes/hello.md").await.is_err(), "unpublished in B's own vault");
    note::put(&b, content("notes/reply.md", "re [[dreams://alice/notes/hello.md]]\n"), false).await.unwrap();
    let reply = note::get(&b, "notes/reply.md").await.unwrap();
    assert_eq!(reply.texts(note::XLINK), ["dreams://alice/notes/hello.md"]);
    assert!(reply.texts(note::DANGLING).is_empty());

    // Remotes are recorded, and sync covers every space.
    let names: Vec<_> = peer::remotes(&b).await.unwrap().into_iter().map(|r| r.space).collect();
    assert_eq!(names, ["peer-alice", "vault"]);
    let report = peer::sync(&b, true).await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
}
