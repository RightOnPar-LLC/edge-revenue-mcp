use thiserror::Error;

/// Crate-wide error type for the offline-first edge-payments ledger.
#[derive(Debug, Error)]
pub enum Error {
    #[error("db error: {0}")]
    Db(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
