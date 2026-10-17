use serde::Serialize;
use thiserror::Error;

/// One validation failure, addressed by the relation it is about.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FieldError {
    pub the: String,
    pub message: String,
}

/// Every way a vault operation can fail. Serializes with a `name` tag so
/// CLI and MCP clients can discriminate on it.
#[derive(Debug, Error, Serialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum Error {
    #[error("not found: {what}")]
    NotFound { what: String },

    #[error("{what} changed since you read it: `updated` is {current}, you expected {expected}")]
    Stale { what: String, expected: u64, current: u64 },

    #[error("validation failed against {concept}: {}", summarize(errors))]
    Validation { concept: String, errors: Vec<FieldError> },

    #[error("{what} is read-only over this connection")]
    Protected { what: String },

    #[error("query failed: {message}{}", hint.as_deref().map(|h| format!(". Hint: {h}")).unwrap_or_default())]
    Query {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },

    #[error("runner failed: {message}")]
    Runner { message: String },

    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    #[error("storage error: {message}")]
    Dialog { message: String },
}

fn summarize(errors: &[FieldError]) -> String {
    errors.iter().map(|e| format!("{}: {}", e.the, e.message)).collect::<Vec<_>>().join("; ")
}

impl Error {
    pub fn invalid(message: impl Into<String>) -> Self {
        Error::InvalidInput { message: message.into() }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Error::NotFound { what: what.into() }
    }

    pub fn protected(what: impl Into<String>) -> Self {
        Error::Protected { what: what.into() }
    }

    pub fn query(message: impl std::fmt::Display, hint: Option<&str>) -> Self {
        Error::Query { message: message.to_string(), hint: hint.map(str::to_string) }
    }

    /// Wrap any Dialog error.
    pub fn dialog(e: impl std::fmt::Display) -> Self {
        Error::Dialog { message: e.to_string() }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::invalid(format!("invalid JSON: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::invalid(format!("io: {e}"))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
