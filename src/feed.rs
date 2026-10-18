//! Feeds (SPEC §11): entities with `dreams.feed/url` and `dreams.feed/kind`.
//! A pull fetches each feed and writes one item entity per entry. An item's
//! entity id comes from (feed, key), so two devices pulling the same feed
//! converge on the same entities. An item whose key fact exists was seen;
//! deleting an item keeps its feed and key facts as that marker.
//!
//! Fetching is kept apart from the vault: `gather` fetches and parses
//! with no commit in progress, `apply` writes.

use std::path::Path;
use std::time::Duration;

use dialog_artifacts::{ArtifactSelector, Entity};
use dialog_repository::schema::EntityExt as _;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, json};

use crate::error::{Error, Result};
use crate::hash::sha256_hex;
use crate::note::{self, Note, Put};
use crate::vault::{self, Vault};

pub const URL: &str = "dreams.feed/url";
pub const KIND: &str = "dreams.feed/kind";
pub const INSTRUCTIONS: &str = "dreams.feed/instructions";
pub const ITEM_FEED: &str = "dreams.feed-item/feed";
pub const ITEM_KEY: &str = "dreams.feed-item/key";

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;
/// Line width for page text. Wide, so text wraps rarely.
const TEXT_WIDTH: usize = 100;
/// Characters in a new item's `description`.
pub const DESCRIPTION_CHARS: usize = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// RSS or Atom: one item per entry.
    Rss,
    /// One web page, as text.
    Html,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Kind> {
        match text {
            "rss" => Ok(Kind::Rss),
            "html" => Ok(Kind::Html),
            _ => Err(Error::invalid(format!("unknown feed kind {text:?}: use rss or html"))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Feed {
    pub entity: Entity,
    pub path: String,
    pub url: String,
    pub kind: Kind,
    pub title: Option<String>,
    pub instructions: Option<String>,
}

impl Feed {
    pub fn from_note(note: &Note) -> Result<Feed> {
        let path = note.path().unwrap_or_default().to_string();
        let url = note.text(URL).ok_or_else(|| Error::invalid(format!("{path} has no {URL}")))?.to_string();
        Ok(Feed {
            entity: note.entity.clone(),
            kind: Kind::parse(note.text(KIND).unwrap_or_default())?,
            title: note.text(note::TITLE).map(str::to_string),
            instructions: note.text(INSTRUCTIONS).map(str::to_string),
            url,
            path,
        })
    }

    /// Where the items go: the feed's path without its extension.
    pub fn base(&self) -> String {
        Path::new(&self.path).with_extension("").to_string_lossy().into_owned()
    }
}

/// The path `feed add` uses when it is given none: `feeds/<origin-slug>.md`.
pub fn default_path(url: &str) -> Result<String> {
    match crate::slug::origin_slug(url) {
        s if s.is_empty() => Err(Error::invalid(format!("cannot make a path from {url:?}; pass --path"))),
        s => Ok(format!("feeds/{s}.md")),
    }
}

/// One item to write.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub key: String,
    pub path: String,
    pub title: Option<String>,
    pub content: Option<String>,
    /// Item attributes other than feed and key, by name: url, guid, published.
    pub fields: Vec<(&'static str, String)>,
}

/// Turn fetched bytes into the items to write. Pure.
pub fn parse(feed: &Feed, bytes: &[u8]) -> Result<Vec<Draft>> {
    match feed.kind {
        Kind::Rss => parse_entries(feed, bytes),
        Kind::Html => {
            let content = html2text::config::plain()
                .string_from_read(bytes, TEXT_WIDTH)
                .map_err(|e| Error::invalid(format!("reading {}: {e}", feed.url)))?;
            Ok(vec![Draft {
                key: "page".into(),
                path: format!("{}/page.md", feed.base()),
                title: Some(feed.title.clone().unwrap_or_else(|| feed.url.clone())),
                content: Some(content),
                fields: vec![("url", feed.url.clone())],
            }])
        }
    }
}

/// One item per entry, oldest first. Feeds list the newest first.
fn parse_entries(feed: &Feed, bytes: &[u8]) -> Result<Vec<Draft>> {
    // No generated ids: feed-rs falls back to a random UUID, which would
    // make each pull see every entry as new.
    let parsed = feed_rs::parser::Builder::new()
        .id_generator(|_, _, _| String::new())
        .build()
        .parse(bytes)
        .map_err(|e| Error::invalid(format!("reading {}: {e}", feed.url)))?;
    let mut drafts = Vec::new();
    for entry in parsed.entries.into_iter().rev() {
        let link = entry.links.first().map(|l| l.href.clone());
        let title = entry.title.map(|t| t.content.trim().to_string()).filter(|t| !t.is_empty());
        let published = entry.published.or(entry.updated).map(|d| d.to_rfc3339());
        let content = entry.content.and_then(|c| c.body).or(entry.summary.map(|s| s.content));
        let key = if !entry.id.is_empty() {
            entry.id.clone()
        } else if let Some(l) = &link {
            l.clone()
        } else {
            format!("{}\n{}", title.as_deref().unwrap_or_default(), published.as_deref().unwrap_or_default())
        };
        if key.trim().is_empty() {
            continue;
        }
        let fields = [("guid", Some(entry.id).filter(|g| !g.is_empty())), ("url", link), ("published", published)]
            .into_iter()
            .filter_map(|(k, v)| Some((k, v?)))
            .collect();
        drafts.push(Draft { path: format!("{}/{}.md", feed.base(), sha256_hex(key.as_bytes())), key, title, content, fields });
    }
    Ok(drafts)
}

/// GET `url`. A status that is not 2xx is an error.
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("dreams/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let fail = |e: ureq::Error| Error::invalid(format!("fetching {url}: {e}"));
    agent.get(url).call().map_err(fail)?.body_mut().with_config().limit(MAX_BODY_BYTES).read_to_vec().map_err(fail)
}

/// A new item as a pull reports it.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct NewItem {
    pub path: String,
    pub title: String,
    /// The start of the content as plain text.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PullError {
    pub feed: String,
    pub error: String,
}

/// The new items of one feed, with what an agent needs to process them.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct FeedItems {
    pub feed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The feed's instructions for the agent that processes its items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub items: Vec<NewItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
pub struct PullReport {
    /// Each feed that has new items, in path order.
    pub feeds: Vec<FeedItems>,
    /// Feeds that failed. The others were still pulled.
    pub errors: Vec<PullError>,
}

/// The feeds to pull: one, or every feed in path order.
pub async fn feeds(vault: &Vault, only: Option<&str>) -> Result<Vec<Feed>> {
    let entities: Vec<Entity> = match only {
        Some(href) => vec![note::resolve(vault, href).await?],
        None => vault.claims(ArtifactSelector::new().the(vault::relation(URL)?)).await?.into_iter().map(|a| a.of).collect(),
    };
    let mut out = Vec::new();
    for e in entities {
        out.push(Feed::from_note(&note::load(vault, &e).await?)?);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The item entity for `key` of `feed`: the same on every device.
pub fn item_entity(feed: &Entity, key: &str) -> Entity {
    Entity::of(&(feed.to_string(), key.to_string()))
}

/// Write the items not seen before, and return them.
/// - An item whose key fact exists was seen: skip it, even when the feed edited it.
/// - An html page whose text changed is new again.
pub async fn apply(vault: &Vault, feed: &Feed, drafts: Vec<Draft>) -> Result<Vec<NewItem>> {
    let mut new = Vec::new();
    for d in drafts {
        let entity = item_entity(&feed.entity, &d.key);
        let item = note::load(vault, &entity).await?;
        let seen = !item.values(ITEM_KEY).is_empty();
        if seen && (feed.kind == Kind::Rss || item.text(note::CONTENT) == d.content.as_deref() || item.path().is_none()) {
            continue;
        }
        let mut fields = Map::new();
        fields.insert(ITEM_FEED.into(), json!(feed.entity.to_string()));
        fields.insert(ITEM_KEY.into(), json!(d.key));
        for (name, v) in &d.fields {
            fields.insert(format!("dreams.feed-item/{name}"), json!(v));
        }
        let put = Put {
            entity: Some(entity.to_string()),
            path: Some(d.path.clone()),
            title: d.title.clone(),
            content: d.content.clone(),
            fields,
            ..Default::default()
        };
        note::put(vault, put, true).await?;
        let content = d.content.unwrap_or_default();
        let text = match feed.kind {
            // Feed content is often HTML.
            Kind::Rss => html2text::config::plain_no_decorate().string_from_read(content.as_bytes(), 10_000).unwrap_or(content),
            Kind::Html => content,
        };
        new.push(NewItem {
            path: d.path,
            title: d.title.unwrap_or_default(),
            description: crate::text::truncate(&text, DESCRIPTION_CHARS),
        });
    }
    Ok(new)
}

/// Fetch, parse and write one feed or every feed. Fetches run on a
/// blocking thread with no commit in progress.
pub async fn pull(vault: &Vault, only: Option<&str>) -> Result<PullReport> {
    let mut report = PullReport::default();
    for feed in feeds(vault, only).await? {
        let f = feed.clone();
        let fetched = tokio::task::spawn_blocking(move || fetch(&f.url).and_then(|b| parse(&f, &b)))
            .await
            .map_err(|e| Error::invalid(e.to_string()))?;
        let applied = match fetched {
            Ok(drafts) => apply(vault, &feed, drafts).await,
            Err(e) => Err(e),
        };
        match applied {
            Ok(items) if items.is_empty() => {}
            Ok(items) => report.feeds.push(FeedItems {
                feed: feed.path.clone(),
                title: feed.title,
                instructions: feed.instructions,
                items,
            }),
            Err(e) => report.errors.push(PullError { feed: feed.path, error: e.to_string() }),
        }
    }
    Ok(report)
}

/// Whether `note` is a feed item: deleting it keeps the seen marker.
pub fn is_item(note: &Note) -> bool {
    !note.values(ITEM_KEY).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn feed(kind: Kind) -> Feed {
        Feed {
            entity: Entity::new().unwrap(),
            path: "feeds/example-com.md".into(),
            url: "https://example.com/feed".into(),
            kind,
            title: None,
            instructions: None,
        }
    }

    pub const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://example.com/</link>
  <item><title>Newest</title><guid>g-2</guid><link>https://example.com/2</link>
    <description>&lt;p&gt;Second &lt;b&gt;post&lt;/b&gt;&lt;/p&gt;</description>
    <pubDate>Tue, 22 Sep 2026 10:00:00 GMT</pubDate></item>
  <item><title>No guid</title><link>https://example.com/1</link><description>First</description></item>
  <item><title>Only a title</title></item>
</channel></rss>"#;

    #[test]
    fn rss_items_are_oldest_first_with_stable_keys() {
        let f = feed(Kind::Rss);
        let drafts = parse(&f, RSS.as_bytes()).unwrap();
        let titles: Vec<&str> = drafts.iter().map(|d| d.title.as_deref().unwrap()).collect();
        assert_eq!(titles, ["Only a title", "No guid", "Newest"]);
        let newest = &drafts[2];
        assert_eq!(newest.key, "g-2");
        assert_eq!(newest.path, format!("feeds/example-com/{}.md", sha256_hex(b"g-2")));
        assert!(newest.fields.contains(&("url", "https://example.com/2".into())));
        assert_eq!(drafts[1].key, "https://example.com/1", "no guid: the link is the key");
        assert_eq!(drafts[0].key, "Only a title\n", "no guid or link: title and date");
        assert_eq!(parse(&f, RSS.as_bytes()).unwrap(), drafts, "a second parse gives the same drafts");
    }

    #[test]
    fn html_is_one_page_of_text() {
        let a = parse(&feed(Kind::Html), b"<html><body><h1>Hello</h1><p>World</p></body></html>").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].path, "feeds/example-com/page.md");
        let content = a[0].content.as_deref().unwrap();
        assert!(content.contains("Hello") && content.contains("World") && !content.contains("<p>"), "{content}");
    }

    #[test]
    fn default_paths_use_the_origin_slug() {
        assert_eq!(default_path("https://news.ycombinator.com/rss").unwrap(), "feeds/news-ycombinator-com.md");
        assert!(default_path("https://").is_err());
    }
}
