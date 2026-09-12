//! Storage engine: durability, branch pointers, and the materialized fold.
//!
//! The split of concerns here mirrors the architecture doc: [`log`] owns the
//! append-only entries, [`branches`] owns the copy-on-write pointers, and
//! [`view`] owns the incrementally-maintained materialized state that reads are
//! served from — because a read must never replay from genesis
//! (`01-system-architecture.md` §3.1).

pub mod anchor;
pub mod attribution;
pub mod attribution_bytes;
pub mod branches;
pub mod completeness;
pub mod durable;
pub mod encryption;
pub mod error;
pub(crate) mod fsync;
pub mod inclusion;
pub mod log;
pub mod merge;
pub mod mergequeue;
pub mod provenance;
pub mod region;
pub mod segment;
pub mod signing;
pub mod sync;
pub mod temporal;
pub mod verifier;
pub mod view;
pub mod wal;

pub use branches::BranchStore;
pub use durable::{BranchViews, DurableLogStore, Opened};
pub use encryption::{DataKey, DATA_KEY_ENV};
pub use error::{Result, StorageError};
pub use log::{LogStore, MemLogStore};
pub use merge::{merge, ConflictRef, MergeOutcome};
pub use view::MaterializedView;
