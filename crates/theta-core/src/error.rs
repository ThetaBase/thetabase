use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    /// A key that will not parse, or sealed bytes that will not open.
    ///
    /// Deliberately one variant for both. Distinguishing "wrong key" from
    /// "corrupt ciphertext" to a caller tells somebody probing which half they
    /// have right, and the distinction is not one this layer can make reliably
    /// anyway -- AEAD failure is indistinguishable by design.
    #[error("{0}")]
    Encryption(String),

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
