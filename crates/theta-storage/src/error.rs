use thiserror::Error;

pub type Result<T> = std::result::Result<T, StorageError>;

#[derive(Debug, Error)]
pub enum StorageError {
    /// A segment was asked to be released while recovery still needs it.
    ///
    /// Refused rather than obeyed. The archive's proof says the bytes can be
    /// fetched back; it says nothing about whether this process can still
    /// recover without them, and only the log knows that.
    #[error("segment {sequence} cannot be released: the checkpoint is at segment {checkpoint}, so recovery still needs it")]
    SegmentInUse { sequence: u64, checkpoint: u64 },

    /// A replayed entry names a parent the log does not contain.
    ///
    /// The hash chain's whole purpose (SEC-8). An entry's hash covers its
    /// contents, so altering any entry changes its hash and orphans whatever
    /// came after it. Reported rather than repaired: the log is the only source
    /// of truth (invariant 6), and a store that patched up its own history
    /// would be deciding which version of the past to believe.
    #[error(
        "the log does not chain: commit {commit_id} names parent {prev_hash}, \
         which is not in the log. history has been altered, or these segments \
         are from two different logs"
    )]
    BrokenChain {
        commit_id: u64,
        prev_hash: String,
        branch: u64,
    },

    /// Data at rest could not be sealed or opened.
    ///
    /// Deliberately distinct from a torn record. A checksum that passed means
    /// the bytes on disk are the bytes that were written, so a decryption
    /// failure is the wrong key rather than corruption — and recovery must not
    /// respond by truncating the log, which would destroy the data encryption
    /// exists to protect.
    #[error("encryption: {0}")]
    Encryption(String),

    #[error("branch `{0}` not found")]
    UnknownBranch(String),

    #[error("branch `{0}` already exists")]
    BranchExists(String),

    #[error("commit {0} not found")]
    UnknownCommit(String),

    #[error("append rejected: head moved from {expected} to {actual}")]
    HeadMoved { expected: String, actual: String },

    #[error(transparent)]
    Core(#[from] theta_core::CoreError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("encoding: {0}")]
    Encoding(#[from] serde_json::Error),

    #[error("corrupt segment: {detail}")]
    Corrupt { detail: String },

    #[error("segment declares format version {version}, which this build cannot read")]
    UnsupportedFormat { version: u32 },

    #[error("checkpoint is unreadable: {detail}")]
    BadCheckpoint { detail: String },

    #[error(
        "materialized view has folded {view_applied} entries but the checkpoint records \
         {checkpoint_applied}; the view is not a fold of this log"
    )]
    ViewDrift {
        view_applied: u64,
        checkpoint_applied: u64,
    },

    #[error(
        "segment {damaged} is damaged and segment(s) {orphaned} sit after it; \
         recovering would either discard or silently reorder them — operator decision required"
    )]
    OrphanedSegments { damaged: u64, orphaned: String },
}
