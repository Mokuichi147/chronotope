use serde::Serialize;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// ドメイン全体で共有するエラー。API 層ではそのまま JSON にシリアライズして返す。
#[derive(Debug, Clone, thiserror::Error, Serialize, PartialEq)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("cycle detected: {0}")]
    Cycle(String),
    #[error("depth limit exceeded: {0}")]
    DepthExceeded(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("incomparable: {0}")]
    Incomparable(String),
    #[error("storage: {0}")]
    Storage(String),
}

impl Error {
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Error::NotFound(what.to_string())
    }
    pub fn invalid(what: impl std::fmt::Display) -> Self {
        Error::Invalid(what.to_string())
    }
    pub fn forbidden(what: impl std::fmt::Display) -> Self {
        Error::Forbidden(what.to_string())
    }
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid",
            Error::Forbidden(_) => "forbidden",
            Error::Conflict(_) => "conflict",
            Error::Cycle(_) => "cycle",
            Error::DepthExceeded(_) => "depth_exceeded",
            Error::Parse(_) => "parse",
            Error::Incomparable(_) => "incomparable",
            Error::Storage(_) => "storage",
        }
    }
}
