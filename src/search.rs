//! Full-text search: a Tantivy index beside the vault (SPEC §9).
//!
//! The index is derived and never synced. It records each note's
//! `updated`; before a search it catches up by comparing those with the
//! vault's, so it is correct without a daemon.

use std::collections::HashMap;
use std::path::Path;

use dialog_artifacts::{ArtifactSelector, Entity};
use schemars::JsonSchema;
use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RegexQuery, TermQuery};
use tantivy::schema::{FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term, doc};

use crate::error::{Error, Result};
use crate::note::{self, Note};
use crate::text::truncate;
use crate::value;
use crate::vault::{self, Vault};

const PREVIEW_CHARS: usize = 150;
const WRITER_HEAP: usize = 50_000_000;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Hit {
    pub entity: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub updated: u64,
    /// Matching text from the content with **highlights**, or its start.
    pub snippet: String,
    pub score: f32,
}

pub struct Search {
    index: Index,
    reader: IndexReader,
    f: Fields,
}

#[derive(Clone, Copy)]
struct Fields {
    entity: Field,
    path: Field,
    title: Field,
    content: Field,
    tags: Field,
    tag: Field,
    updated: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let f = Fields {
        entity: b.add_text_field("entity", STRING | STORED),
        path: b.add_text_field("path", STRING | STORED),
        title: b.add_text_field("title", TEXT | STORED),
        content: b.add_text_field("content", TEXT | STORED),
        tags: b.add_text_field("tags", TEXT),
        tag: b.add_text_field("tag", STRING | STORED),
        updated: b.add_u64_field("updated", INDEXED | STORED | FAST),
    };
    (b.build(), f)
}

impl Search {
    /// Open the index in `<vault>/index`, creating it on first use.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_space(dir, "vault")
    }

    /// The index of one space: `index` for the vault, `index-<space>` for
    /// another, such as a followed `peer-<name>`.
    pub fn open_space(dir: &Path, space: &str) -> Result<Self> {
        let path = if space == "vault" { dir.join("index") } else { dir.join(format!("index-{space}")) };
        std::fs::create_dir_all(&path)?;
        let (schema, f) = schema();
        let directory = tantivy::directory::MmapDirectory::open(&path).map_err(search_error)?;
        let index = Index::open_or_create(directory, schema).map_err(search_error)?;
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().map_err(search_error)?;
        Ok(Search { index, reader, f })
    }

    /// Delete and rebuild the index.
    pub fn reset(dir: &Path) -> Result<Self> {
        let _ = std::fs::remove_dir_all(dir.join("index"));
        Self::open(dir)
    }

    /// Bring the index up to date with the vault. Returns how many notes
    /// changed, or None when another process holds the writer.
    // ponytail: compares every (entity, updated) pair; fine to ~100k notes. Use Dialog's tree diff once it's public.
    pub async fn catch_up(&self, vault: &Vault) -> Result<Option<usize>> {
        let mut current: HashMap<Entity, u64> = HashMap::new();
        for a in vault.claims(ArtifactSelector::new().the(vault::relation(vault::UPDATED)?)).await? {
            let n = value::natural(&a.is).unwrap_or_default();
            let e = current.entry(a.of).or_default();
            *e = (*e).max(n);
        }
        let indexed = self.indexed()?;
        let mut stale: Vec<&Entity> = current.iter().filter(|(e, u)| indexed.get(&e.to_string()) != Some(u)).map(|(e, _)| e).collect();
        let gone: Vec<&String> = indexed.keys().filter(|e| !current.keys().any(|c| &c.to_string() == *e)).collect();
        if stale.is_empty() && gone.is_empty() {
            return Ok(Some(0));
        }
        let mut writer: IndexWriter = match self.index.writer(WRITER_HEAP) {
            Ok(w) => w,
            Err(tantivy::TantivyError::LockFailure(..)) => return Ok(None),
            Err(e) => return Err(search_error(e)),
        };
        stale.sort_by_key(|e| e.to_string());
        for e in &stale {
            writer.delete_term(Term::from_field_text(self.f.entity, &e.to_string()));
            let n = note::load(vault, e).await?;
            if n.path().is_some() {
                writer.add_document(self.document(&n)).map_err(search_error)?;
            }
        }
        for e in &gone {
            writer.delete_term(Term::from_field_text(self.f.entity, e));
        }
        writer.commit().map_err(search_error)?;
        self.reader.reload().map_err(search_error)?;
        Ok(Some(stale.len() + gone.len()))
    }

    fn document(&self, n: &Note) -> TantivyDocument {
        let f = self.f;
        let mut d = doc!(
            f.entity => n.entity.to_string(),
            f.path => n.path().unwrap_or_default(),
            f.content => n.text(note::CONTENT).unwrap_or_default(),
            f.updated => n.updated(),
        );
        if let Some(t) = n.text(note::TITLE) {
            d.add_text(f.title, t);
        }
        for tag in n.texts(note::TAG) {
            d.add_text(f.tags, &tag);
            d.add_text(f.tag, &tag);
        }
        d
    }

    /// Every indexed entity and the `updated` it was indexed at.
    fn indexed(&self) -> Result<HashMap<String, u64>> {
        self.reader.reload().map_err(search_error)?;
        let searcher = self.reader.searcher();
        let mut out = HashMap::new();
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(1).map_err(search_error)?;
            for id in segment.doc_ids_alive() {
                let d: TantivyDocument = store.get(id).map_err(search_error)?;
                let entity = d.get_first(self.f.entity).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let updated = d.get_first(self.f.updated).and_then(|v| v.as_u64()).unwrap_or_default();
                out.insert(entity, updated);
            }
        }
        Ok(out)
    }

    /// Search title (×10), tags (×5) and content (×1). An empty `q` lists
    /// notes newest first.
    pub fn search(&self, q: &str, tag: Option<&str>, prefix: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
        let f = self.f;
        let searcher = self.reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![f.title, f.tags, f.content]);
        parser.set_field_boost(f.title, 10.0);
        parser.set_field_boost(f.tags, 5.0);
        let text: Box<dyn Query> = if q.trim().is_empty() { Box::new(AllQuery) } else { parser.parse_query_lenient(q).0 };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text.box_clone())];
        if let Some(tag) = tag {
            let term = Term::from_field_text(f.tag, tag);
            clauses.push((Occur::Must, Box::new(TermQuery::new(term, IndexRecordOption::Basic))));
        }
        if let Some(prefix) = prefix {
            let pattern = format!("{}.*", regex_escape(prefix));
            clauses.push((Occur::Must, Box::new(RegexQuery::from_pattern(&pattern, f.path).map_err(search_error)?)));
        }
        let query = BooleanQuery::new(clauses);
        let fetch = if q.trim().is_empty() { searcher.num_docs() as usize } else { limit };
        let top = searcher.search(&query, &TopDocs::with_limit(fetch.max(1)).order_by_score()).map_err(search_error)?;
        let snippets = if q.trim().is_empty() {
            None
        } else {
            Some(SnippetGenerator::create(&searcher, &*text, f.content).map_err(search_error)?)
        };
        let mut hits = Vec::new();
        for (score, address) in top {
            let d: TantivyDocument = searcher.doc(address).map_err(search_error)?;
            let get = |field| d.get_first(field).and_then(|v| v.as_str()).map(str::to_string);
            let content = get(f.content).unwrap_or_default();
            let snippet = match &snippets {
                Some(g) => {
                    let mut s = g.snippet(&content);
                    s.set_snippet_prefix_postfix("**", "**");
                    let html = s.to_html();
                    if html.is_empty() { truncate(&content, PREVIEW_CHARS) } else { unescape(&html) }
                }
                None => truncate(&content, PREVIEW_CHARS),
            };
            hits.push(Hit {
                entity: get(f.entity).unwrap_or_default(),
                path: get(f.path).unwrap_or_default(),
                title: get(f.title),
                tags: d.get_all(f.tag).filter_map(|v| v.as_str().map(str::to_string)).collect(),
                updated: d.get_first(f.updated).and_then(|v| v.as_u64()).unwrap_or_default(),
                snippet,
                score,
            });
        }
        if q.trim().is_empty() {
            hits.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.path.cmp(&b.path)));
            hits.truncate(limit);
        }
        Ok(hits)
    }
}

fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| if "\\.+*?()|[]{}^$#&-~\"<>@".contains(c) { vec!['\\', c] } else { vec![c] }).collect()
}

/// Snippets come back as HTML; agents read Markdown-ish text.
fn unescape(html: &str) -> String {
    html.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#x27;", "'").replace("&amp;", "&")
}

fn search_error(e: impl std::fmt::Display) -> Error {
    Error::Dialog { message: format!("search index: {e}") }
}
