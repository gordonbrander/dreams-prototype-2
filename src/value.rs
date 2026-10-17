//! Dialog values to and from JSON, the format of every tool and file.

use dialog_artifacts::{Entity, Value};
use serde_json::Value as Json;

use crate::error::{Error, Result};

/// A value as JSON. Entities and symbols are their URI strings; integers
/// that don't fit JSON numbers are strings.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::String(s) => Json::String(s.clone()),
        Value::Entity(e) => Json::String(e.to_string()),
        Value::Symbol(r) => Json::String(r.to_string()),
        Value::Boolean(b) => Json::Bool(*b),
        Value::UnsignedInt(n) => u64::try_from(*n).map(Json::from).unwrap_or_else(|_| Json::String(n.to_string())),
        Value::SignedInt(n) => i64::try_from(*n).map(Json::from).unwrap_or_else(|_| Json::String(n.to_string())),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Bytes(b) | Value::Record(b) => Json::from(b.clone()),
    }
}

/// A JSON scalar as a value. `as_type` is an attribute's declared type
/// (`text:`, `entity:`, ...); without one, a string is text and a whole
/// number is natural, or integer when negative.
pub fn from_json(json: &Json, as_type: Option<&str>) -> Result<Value> {
    let wrong = |want: &str| Error::invalid(format!("expected {want}, got {json}"));
    Ok(match (as_type, json) {
        (Some("entity:"), Json::String(s)) => Value::Entity(entity(s)?),
        (Some("entity:"), _) => return Err(wrong("an entity URI")),
        (Some("text:") | Some("symbol:"), Json::String(s)) => Value::String(s.clone()),
        (Some("text:") | Some("symbol:"), _) => return Err(wrong("text")),
        (Some("boolean:"), Json::Bool(b)) => Value::Boolean(*b),
        (Some("boolean:"), _) => return Err(wrong("a boolean")),
        (Some("natural:"), Json::Number(n)) => Value::UnsignedInt(n.as_u64().ok_or_else(|| wrong("a natural"))?.into()),
        (Some("natural:"), _) => return Err(wrong("a natural")),
        (Some("integer:"), Json::Number(n)) => Value::SignedInt(n.as_i64().ok_or_else(|| wrong("an integer"))?.into()),
        (Some("integer:"), _) => return Err(wrong("an integer")),
        (Some("float:"), Json::Number(n)) => Value::Float(n.as_f64().ok_or_else(|| wrong("a float"))?),
        (Some("float:"), _) => return Err(wrong("a float")),
        (_, Json::String(s)) => Value::String(s.clone()),
        (_, Json::Bool(b)) => Value::Boolean(*b),
        (_, Json::Number(n)) => match (n.as_u64(), n.as_i64()) {
            (Some(u), _) => Value::UnsignedInt(u.into()),
            (None, Some(i)) => Value::SignedInt(i.into()),
            _ => Value::Float(n.as_f64().unwrap_or_default()),
        },
        (_, Json::Null) => return Err(Error::invalid("null is not a value; leave the field out")),
        (_, Json::Array(_) | Json::Object(_)) => return Err(wrong("a scalar")),
    })
}

/// The type name of a value, as `describe` reports it.
pub fn type_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "text:",
        Value::Entity(_) => "entity:",
        Value::Symbol(_) => "symbol:",
        Value::Boolean(_) => "boolean:",
        Value::UnsignedInt(_) => "natural:",
        Value::SignedInt(_) => "integer:",
        Value::Float(_) => "float:",
        Value::Bytes(_) => "bytes:",
        Value::Record(_) => "record:",
    }
}

pub fn entity(s: &str) -> Result<Entity> {
    s.parse().map_err(|e| Error::invalid(format!("not an entity URI: {s} ({e})")))
}

pub fn text(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

pub fn natural(value: &Value) -> Option<u64> {
    match value {
        Value::UnsignedInt(n) => u64::try_from(*n).ok(),
        Value::SignedInt(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trips_through_values() {
        for (json, as_type) in [(json!("hi"), None), (json!(5), None), (json!(-5), None), (json!(1.5), None), (json!(true), None)] {
            assert_eq!(to_json(&from_json(&json, as_type).unwrap()), json);
        }
        let e = from_json(&json!("did:key:z6Mkabc"), Some("entity:")).unwrap();
        assert!(matches!(e, Value::Entity(_)));
        assert_eq!(type_of(&from_json(&json!(5), Some("integer:")).unwrap()), "integer:");
        assert!(from_json(&json!("x"), Some("natural:")).is_err());
        assert!(from_json(&json!(null), None).is_err());
    }
}
