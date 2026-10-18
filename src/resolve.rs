//! Conflicts (SPEC §6): a one-value attribute that holds more than one
//! value, because two devices set it without seeing each other. Dialog
//! already merged every attribute only one side changed, so a conflict
//! is one attribute of one entity. `content` merges line by line against
//! the value both sides superseded; what is left goes to a person or a
//! runner, and resolving retracts every value but the merged one.

use dialog_artifacts::{ArtifactSelector, Entity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::diff::{self, Edit};
use crate::error::{Error, Result};
use crate::note::{self, CONTENT, PATH, Put, TITLE};
use crate::runner::{self, Runner};
use crate::value::to_json;
use crate::vault::{self, Vault};

/// The runner `--auto` uses unless told otherwise.
pub const DEFAULT_RUNNER: &str = "runners/claude.json";
/// The attributes `list` checks when given none.
pub const DEFAULT_ATTRIBUTES: [&str; 3] = [PATH, TITLE, CONTENT];

/// One conflicted attribute of one entity.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Conflict {
    pub entity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub the: String,
    /// Every value it holds; a read under `last:` returns the first.
    pub values: Vec<Json>,
    /// The value both sides superseded, from history. Absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<Json>,
    /// For `content`: the line-by-line merge, when no line was changed by both sides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<String>,
    /// For `content`: the merge with conflict markers where both sides changed the same lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked: Option<String>,
}

/// A decision for a conflict: a whole value, or edits to `marked`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Reply {
    /// The value to keep. null removes the attribute.
    #[serde(default)]
    pub value: Option<Json>,
    /// Edits to the marked `content`, in order: each `old` is one whole
    /// marked block, copied exactly; `new` is the merged text.
    #[serde(default)]
    pub content_edits: Vec<Edit>,
}

/// Every conflict on `attributes`, by path.
pub async fn list(vault: &Vault, attributes: &[String]) -> Result<Vec<Conflict>> {
    let mut out = Vec::new();
    for the in attributes {
        let mut entities: Vec<Entity> =
            vault.claims(ArtifactSelector::new().the(vault::relation(the)?)).await?.into_iter().map(|a| a.of).collect();
        entities.sort_by_key(|e| e.to_string());
        for pair in entities.windows(2).filter(|w| w[0] == w[1]) {
            if out.iter().any(|c: &Conflict| c.entity == pair[0].to_string() && &c.the == the) {
                continue;
            }
            if let Some(c) = read_entity(vault, &pair[0], the).await? {
                out.push(c);
            }
        }
    }
    out.sort_by(|a, b| (&a.path, &a.the).cmp(&(&b.path, &b.the)));
    Ok(out)
}

/// The conflict on `the` of the note `href`, if it has one.
pub async fn read(vault: &Vault, href: &str, the: &str) -> Result<Option<Conflict>> {
    read_entity(vault, &note::resolve(vault, href).await?, the).await
}

async fn read_entity(vault: &Vault, entity: &Entity, the: &str) -> Result<Option<Conflict>> {
    let values = vault.values(entity, the).await?;
    if values.len() < 2 {
        return Ok(None);
    }
    // ponytail: the ancestor is the newest value retracted and not current;
    // exact once Dialog exposes each side's causal history.
    let ancestor = vault
        .history(entity, Some(the))
        .await?
        .into_iter()
        .rev()
        .find(|(_, asserted, v)| !asserted && !values.contains(v))
        .map(|(_, _, v)| to_json(&v));
    let path = vault.values(entity, PATH).await?.first().and_then(crate::value::text).map(str::to_string);
    let mut c = Conflict {
        entity: entity.to_string(),
        path,
        the: the.into(),
        values: values.iter().map(to_json).collect(),
        ancestor,
        merged: None,
        marked: None,
    };
    if the == CONTENT
        && let [Json::String(ours), Json::String(theirs)] = c.values.as_slice()
    {
        let base = c.ancestor.as_ref().and_then(Json::as_str).unwrap_or_default();
        match diff::merge_content(base, ours, theirs) {
            Ok(m) => c.merged = Some(m),
            Err(m) => c.marked = Some(m),
        }
    }
    Ok(Some(c))
}

impl Conflict {
    /// The value a reply decides.
    pub fn apply(&self, reply: &Reply) -> Result<Json> {
        let value = match (&self.marked, reply.content_edits.as_slice(), &reply.value) {
            (_, [], Some(v)) => v.clone(),
            (Some(marked), edits, None) if !edits.is_empty() => Json::String(diff::apply_edits(marked, edits)?),
            (None, edits, None) if !edits.is_empty() => {
                return Err(Error::invalid("`content_edits` need content with conflict markers, and it has none"));
            }
            (_, [], None) => match &self.merged {
                Some(m) => Json::String(m.clone()),
                None => return Err(Error::invalid("give `value`, or `content_edits` for marked content")),
            },
            _ => return Err(Error::invalid("give `value` or `content_edits`, not both")),
        };
        if self.the == CONTENT && value.as_str().is_some_and(diff::has_markers) {
            return Err(Error::invalid("merged content still has conflict markers"));
        }
        Ok(value)
    }
}

/// Settle a conflict: `value` becomes the attribute's only value.
pub async fn resolve(vault: &Vault, href: &str, the: &str, reply: &Reply, trusted: bool) -> Result<Json> {
    let c = read(vault, href, the).await?.ok_or_else(|| Error::invalid(format!("{href} has no conflict on {the}")))?;
    let value = c.apply(reply)?;
    let mut put = Put { entity: Some(c.entity.clone()), ..Default::default() };
    match (the, &value) {
        (CONTENT, Json::String(s)) => put.content = Some(s.clone()),
        (TITLE, Json::String(s)) => put.title = Some(s.clone()),
        (PATH, Json::String(s)) => put.path = Some(s.clone()),
        (CONTENT | TITLE | PATH, _) => return Err(Error::invalid(format!("{the} must be text"))),
        _ => put.fields = Map::from_iter([(the.to_string(), value.clone())]),
    }
    note::put(vault, put, trusted).await?;
    Ok(value)
}

/// Settle a conflict without a person: code's merge when it is clean,
/// else the runner's reply to `merge_prompt`.
pub async fn auto(vault: &Vault, href: &str, the: &str, runner: &str) -> Result<Json> {
    let c = read(vault, href, the).await?.ok_or_else(|| Error::invalid(format!("{href} has no conflict on {the}")))?;
    let reply = if c.merged.is_some() {
        Reply::default()
    } else {
        let runner = Runner::load(vault, runner).await?;
        let text = runner::invoke(&runner, &vault.dir, &format!("resolve/{href}"), &merge_prompt(&c)).await?;
        parse_reply(&text)?
    };
    resolve(vault, href, the, &reply, true).await
}

fn pretty(v: &Json) -> String {
    serde_json::to_string_pretty(v).expect("values serialize")
}

/// The instructions and the work left, for a runner's stdin.
pub fn merge_prompt(c: &Conflict) -> String {
    let mut text = format!(
        "You merge conflicting values of the attribute \"{}\" of the note \"{}\" in a vault.\n\
         Two devices changed it at the same time, and a sync brought both values together.\n\n",
        c.the,
        c.path.as_deref().unwrap_or(&c.entity)
    );
    if let Some(a) = &c.ancestor {
        text.push_str(&format!("The value before both changes:\n<ancestor>\n{}\n</ancestor>\n", pretty(a)));
    }
    for v in &c.values {
        text.push_str(&format!("<side>\n{}\n</side>\n", pretty(v)));
    }
    if let Some(m) = &c.marked {
        text.push_str(&format!(
            "\nThe text is merged line by line. Where both sides changed the same lines, it has a marked block. \
             Each marker line starts with 7 or more of one character: \"<<<<<<< ours\" starts one side's lines, \
             \"||||||| original\" the ancestor's, \"=======\" the other side's, and \">>>>>>> theirs\" ends the block.\n\
             <marked>\n{m}</marked>\n\n\
             For each marked block, write one edit in \"content_edits\". \"old\" is the whole block, copied exactly. \
             \"new\" is the merged text: keep the additions from every side, and remove the markers.\n"
        ));
    } else {
        text.push_str("\nCombine the changes of every side if you can. If you cannot, choose one. Put it in \"value\".\n");
    }
    text.push_str(
        "Do not use any tools, and do not change the vault.\n\
         Reply with exactly one JSON object, and write no other text: \
         {\"value\": <value>} or {\"content_edits\": [{\"old\": \"<block>\", \"new\": \"<merged text>\"}]}\n",
    );
    text
}

/// The runner's reply: the outermost JSON object in the text, so a code
/// fence or a sentence around it does no harm.
pub fn parse_reply(text: &str) -> Result<Reply> {
    let bad = |why: &str| Error::Runner {
        message: format!("{why}; the reply began: {:?}", text.chars().take(200).collect::<String>()),
    };
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err(bad("the reply has no JSON object"));
    };
    if end < start {
        return Err(bad("the reply has no JSON object"));
    }
    serde_json::from_str(&text[start..=end]).map_err(|e| bad(&format!("the reply is not a merge: {e}")))
}

/// Unified diffs from the ancestor (or the first value) to each value.
pub fn diffs(c: &Conflict) -> Vec<String> {
    let as_text = |v: &Json| v.as_str().map_or_else(|| pretty(v) + "\n", str::to_string);
    let (base, name) = match &c.ancestor {
        Some(a) => (as_text(a), "ancestor"),
        None => (as_text(&c.values[0]), "value 1"),
    };
    c.values
        .iter()
        .enumerate()
        .map(|(i, v)| diff::unified(&base, &as_text(v), name, &format!("value {}", i + 1)))
        .filter(|d| !d.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conflict(marked: Option<&str>, merged: Option<&str>) -> Conflict {
        Conflict {
            entity: "did:key:x".into(),
            path: Some("notes/a.md".into()),
            the: CONTENT.into(),
            values: vec![json!("a\n"), json!("b\n")],
            ancestor: Some(json!("base\n")),
            merged: merged.map(str::to_string),
            marked: marked.map(str::to_string),
        }
    }

    #[test]
    fn replies_apply_to_marked_content_or_give_a_value() {
        let block = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
        let c = conflict(Some(block), None);
        let reply = parse_reply(&format!("ok: {}", json!({"content_edits": [{"old": block, "new": "a\nb\n"}]}))).unwrap();
        assert_eq!(c.apply(&reply).unwrap(), json!("a\nb\n"));
        assert!(c.apply(&Reply { value: Some(json!(block)), ..Default::default() }).is_err(), "markers left");
        assert!(c.apply(&Reply::default()).is_err());
        assert_eq!(conflict(None, Some("m\n")).apply(&Reply::default()).unwrap(), json!("m\n"));
        assert!(parse_reply("no json").is_err());
        assert!(merge_prompt(&c).contains("<marked>"));
    }
}
