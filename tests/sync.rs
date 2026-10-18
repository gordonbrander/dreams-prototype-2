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

    peer::add_remote(&a, "vault", "origin", remote.to_str().unwrap(), None).await.unwrap();
    note::put(&a, content("notes/x.md", "one\n"), false).await.unwrap();
    assert!(a.push().await.unwrap());

    // B joins with the delegation A leaves in the folder.
    peer::device_add(&a, &b.device()).await.unwrap();
    assert_eq!(peer::add_remote(&b, "vault", "origin", remote.to_str().unwrap(), None).await.unwrap().repository, a.repository());
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
    peer::add_remote(&a, peer::PUBLIC, "origin", public.to_str().unwrap(), None).await.unwrap();
    note::put(&a, content("notes/hello.md", "hi from a\n"), false).await.unwrap();
    peer::publish(&a, "notes/hello.md").await.unwrap();
    peer::peer_grant(&a, &b.device()).await.unwrap();
    peer::follow(&b, "alice", public.to_str().unwrap(), None).await.unwrap();
    let theirs = note::get(&b, "dreams://alice/notes/hello.md").await.unwrap();
    assert_eq!(theirs.text(CONTENT), Some("hi from a\n"));
    let alice = peer::followed(&b, "alice").await.unwrap();
    let index = dreams::search::Search::open_space(&b_dir, &alice.space).unwrap();
    index.catch_up(&alice).await.unwrap();
    let hits = index.search("hi", None, None, 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["notes/hello.md"]);
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

#[test]
fn two_devices_sync_through_an_s3_bucket() {
    run(sync_s3());
}

async fn sync_s3() {
    use dialog_remote_s3::helpers::LocalS3;
    let server = LocalS3::start_with_auth("dreams-key", "dreams-secret", &["vault"]).await.unwrap();
    // SAFETY: set before any other thread reads these, and only ever to these values.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "dreams-key");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "dreams-secret");
    }
    let location = format!("s3://vault?endpoint={}&path-style=true", server.endpoint);
    let base = std::env::temp_dir().join(format!("dreams-s3-{}", uuid::Uuid::now_v7()));
    let _dirs = Dirs(vec![base.clone()]);
    drop((Vault::open(&base.join("a")).await.unwrap(), Vault::open(&base.join("b")).await.unwrap()));
    let a = Vault::open(&base.join("a")).await.unwrap();
    let b = Vault::open(&base.join("b")).await.unwrap();

    peer::add_remote(&a, "vault", "cloud", &location, None).await.unwrap();
    note::put(&a, content("notes/s3.md", "through a bucket\n"), false).await.unwrap();
    assert_eq!(peer::sync(&a, true).await.unwrap().pushed, ["vault"]);

    // The delegation travels as a file; B names A's repository.
    let files = peer::device_add(&a, &b.device()).await.unwrap();
    b.accept(&std::fs::read(&files[0]).unwrap()).await.unwrap();
    peer::add_remote(&b, "vault", "cloud", &location, Some(&a.repository())).await.unwrap();
    let report = peer::sync(&b, true).await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(note::get(&b, "notes/s3.md").await.unwrap().text(CONTENT), Some("through a bucket\n"));

    note::put(&b, content("notes/back.md", "from b\n"), false).await.unwrap();
    peer::sync(&b, true).await.unwrap();
    peer::sync(&a, true).await.unwrap();
    assert!(note::get(&a, "notes/back.md").await.is_ok());
}

#[test]
fn two_devices_sync_through_a_ucan_access_service() {
    run(sync_ucan());
}

async fn sync_ucan() {
    use dialog_remote_s3::helpers::LocalS3;
    use dialog_remote_ucan_s3::helpers::UcanAccessServer;
    let s3 = LocalS3::start_with_auth("k", "s", &["vault"]).await.unwrap();
    let service = UcanAccessServer::start(s3, "vault", "k", "s").await.unwrap();
    let location = format!("ucan+{}", service.endpoint);
    let base = std::env::temp_dir().join(format!("dreams-ucan-{}", uuid::Uuid::now_v7()));
    let _dirs = Dirs(vec![base.clone()]);
    drop((Vault::open(&base.join("a")).await.unwrap(), Vault::open(&base.join("b")).await.unwrap()));
    let a = Vault::open(&base.join("a")).await.unwrap();
    let b = Vault::open(&base.join("b")).await.unwrap();

    peer::add_remote(&a, "vault", "cloud", &location, None).await.unwrap();
    note::put(&a, content("notes/ucan.md", "through a service\n"), false).await.unwrap();
    let report = peer::sync(&a, true).await.unwrap();
    assert_eq!(report.pushed, ["vault"], "{report:?}");

    let files = peer::device_add(&a, &b.device()).await.unwrap();
    b.accept(&std::fs::read(&files[0]).unwrap()).await.unwrap();
    peer::add_remote(&b, "vault", "cloud", &location, Some(&a.repository())).await.unwrap();
    let report = peer::sync(&b, true).await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(note::get(&b, "notes/ucan.md").await.unwrap().text(CONTENT), Some("through a service\n"));

    // A revokes B: the service refuses B from then on, and A still syncs.
    let revoked = peer::revoke(&a, "vault", &b.device()).await.unwrap();
    assert_eq!(revoked[0].outcome, "revoked", "{revoked:?}");
    note::put(&a, content("notes/after.md", "after the revocation\n"), false).await.unwrap();
    let report = peer::sync(&a, true).await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let report = peer::sync(&b, true).await.unwrap();
    assert!(!report.errors.is_empty(), "B is refused: {report:?}");
    assert!(note::get(&b, "notes/after.md").await.is_err());
}
