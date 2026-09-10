use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("log entry {entry} does not chain onto {expected}")]
    BrokenChain { entry: String, expected: String },

    #[error("type mismatch on field `{field}`: declared {declared}, write was {actual}")]
    TypeMismatch {
        field: String,
        declared: String,
        actual: String,
    },

    #[error("branch {0} not found")]
    UnknownBranch(u64),

    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}
