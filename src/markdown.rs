//! Markdown with YAML frontmatter: the human format for one note.
//!
//! ```text
//! ---
//! dreams.note:
//!   path: notes/hello.md
//!   title: Hello
//! ---
//! <content, byte-verbatim>
//! ```
//!
//! Delimiter lines carry their own newline, so `render` then `parse` is a
//! bijection on content. Nothing is appended after the content.

use serde::Serialize;
use serde_json::{Map, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::error::Error;

/// Split into (frontmatter YAML, content). No frontmatter means the whole
/// text is content.
fn split(text: &str) -> Result<(Option<&str>, &str), Error> {
    let after_open = match text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        Some(rest) => rest,
        None => return Ok((None, text)),
    };
    let mut pos = 0;
    loop {
        let rest = &after_open[pos..];
        let (line, consumed, has_newline) = match rest.find('\n') {
            Some(i) => (&rest[..i], i + 1, true),
            None => (rest, rest.len(), false),
        };
        if line.trim_end_matches('\r') == "---" {
            return Ok((Some(&after_open[..pos]), &after_open[pos + consumed..]));
        }
        if !has_newline {
            return Err(Error::invalid("unterminated frontmatter: missing closing ---"));
        }
        pos += consumed;
    }
}

/// Parse YAML into a JSON object. Empty YAML is an empty object.
pub fn yaml_to_map(yaml: &str) -> Result<Map<String, Value>, Error> {
    let value: Value = serde_yaml_ng::from_str(yaml).map_err(|e| Error::invalid(format!("invalid YAML: {e}")))?;
    match value {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map),
        _ => Err(Error::invalid("YAML must be a mapping")),
    }
}

/// Parse a Markdown document into fields. Frontmatter sets fields; a
/// non-empty content section sets `content`.
pub fn parse(text: &str) -> Result<Map<String, Value>, Error> {
    let (frontmatter, content) = split(text)?;
    let mut map = match frontmatter {
        None => Map::new(),
        Some(yaml) => yaml_to_map(yaml)?,
    };
    if !content.is_empty() {
        if map.contains_key("content") {
            return Err(Error::invalid("frontmatter sets `content` but the document also has a body"));
        }
        map.insert("content".into(), Value::String(content.to_string()));
    }
    Ok(map)
}

/// Render fields as frontmatter, in serialization order. A non-empty string
/// `content` becomes the text after the frontmatter.
pub fn render(value: &impl Serialize) -> String {
    let Yaml::Mapping(fields) = serde_yaml_ng::to_value(value).expect("fields serialize") else {
        unreachable!("render takes a mapping")
    };
    let mut fm = Mapping::new();
    let mut content = String::new();
    for (key, value) in fields {
        if key.as_str() == Some("content")
            && let Yaml::String(s) = &value
            && !s.is_empty()
        {
            content = s.clone();
            continue;
        }
        fm.insert(key, value);
    }
    let yaml = serde_yaml_ng::to_string(&fm).expect("a mapping serializes");
    format!("---\n{yaml}---\n{content}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(body: Value) -> Map<String, Value> {
        parse(&render(&body)).unwrap()
    }

    #[test]
    fn content_variants_round_trip_exactly() {
        for content in ["a", "a\n", "\na", "a\n\nb\n", "---\nnot frontmatter\n", "  indented\n"] {
            let parsed = round_trip(json!({"title": "yes", "content": content}));
            assert_eq!(parsed["content"], content, "{content:?}");
            assert_eq!(parsed["title"], "yes");
        }
    }

    #[test]
    fn empty_and_absent_content() {
        assert_eq!(round_trip(json!({"title": "t", "content": ""}))["content"], "");
        assert!(round_trip(json!({"title": "t"})).get("content").is_none());
        assert!(render(&json!({"title": "t"})).ends_with("---\n"));
    }

    #[test]
    fn nested_values_and_quoting_round_trip() {
        let body = json!({"title": "true", "n": 42, "flag": false, "tags": ["a", "b"],
                          "meta": {"k": [1, {"z": null}]}, "content": "x"});
        let parsed = round_trip(body.clone());
        for key in ["title", "n", "flag", "tags", "meta", "content"] {
            assert_eq!(parsed[key], body[key], "{key}");
        }
    }

    #[test]
    fn plain_markdown_is_content() {
        assert_eq!(parse("just text\n").unwrap()["content"], "just text\n");
        assert!(parse("").unwrap().is_empty());
        assert_eq!(parse("--- not a delimiter").unwrap()["content"], "--- not a delimiter");
    }

    #[test]
    fn crlf_delimiters_and_empty_frontmatter() {
        let m = parse("---\r\ntitle: t\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(m["title"], "t");
        assert_eq!(m["content"], "body\r\n");
        assert!(parse("---\n---\n").unwrap().is_empty());
        assert_eq!(parse("---\n---\nx").unwrap()["content"], "x");
    }

    #[test]
    fn errors() {
        assert!(parse("---\ntitle: t\nno end").is_err());
        assert!(parse("---\ntitle: t\n").is_err());
        assert!(parse("---\ncontent: x\n---\nbody").is_err());
        assert!(parse("---\n- a list\n---\n").is_err());
        assert!(parse("---\n: [bad\n---\n").is_err());
    }
}
