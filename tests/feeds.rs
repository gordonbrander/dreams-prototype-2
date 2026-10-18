//! Feed items: dedup by key, the seen marker, and convergence across vaults.

use dreams::feed::{self, Feed, Kind};
use dreams::note::{self, Put};
use dreams::vault::Vault;
use serde_json::json;

const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title>
  <item><title>Two</title><guid>g-2</guid><description>&lt;p&gt;Second &lt;b&gt;post&lt;/b&gt;&lt;/p&gt;</description></item>
  <item><title>One</title><guid>g-1</guid><description>First</description></item>
</channel></rss>"#;

struct Dirs(Vec<std::path::PathBuf>);

impl Drop for Dirs {
    fn drop(&mut self) {
        for d in &self.0 {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A vault with the same feed note, seeded the same way on every device.
async fn vault_with_feed(dirs: &mut Dirs) -> (Vault, Feed) {
    let dir = std::env::temp_dir().join(format!("dreams-feeds-{}", uuid::Uuid::now_v7()));
    dirs.0.push(dir.clone());
    let v = Vault::open(&dir).await.unwrap();
    let fields = [("dreams.feed/url", json!("https://example.com/rss")), ("dreams.feed/kind", json!("rss"))];
    let input = Put {
        path: Some("feeds/example.md".into()),
        entity: Some("did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".into()),
        fields: fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ..Default::default()
    };
    note::put(&v, input, true).await.unwrap();
    let f = feed::feeds(&v, None).await.unwrap().remove(0);
    (v, f)
}

#[tokio::test]
async fn items_are_new_once_deleted_items_stay_deleted_and_vaults_converge() {
    let mut dirs = Dirs(vec![]);
    let (a, fa) = vault_with_feed(&mut dirs).await;
    assert_eq!(fa.kind, Kind::Rss);
    let drafts = feed::parse(&fa, RSS.as_bytes()).unwrap();
    let new = feed::apply(&a, &fa, drafts.clone()).await.unwrap();
    assert_eq!(new.len(), 2);
    assert_eq!(new[1].description, "Second post");
    assert!(feed::apply(&a, &fa, drafts.clone()).await.unwrap().is_empty(), "a second pull sees nothing new");

    // A deleted item stays deleted.
    note::delete(&a, &new[0].path, false).await.unwrap();
    assert!(feed::apply(&a, &fa, drafts.clone()).await.unwrap().is_empty());
    assert!(note::get(&a, &new[0].path).await.is_err());

    // Another device pulling the same feed writes the same entities.
    let (b, fb) = vault_with_feed(&mut dirs).await;
    feed::apply(&b, &fb, drafts.clone()).await.unwrap();
    let ea = note::get(&a, &new[1].path).await.unwrap().entity;
    let eb = note::get(&b, &new[1].path).await.unwrap().entity;
    assert_eq!(ea, eb);
}
