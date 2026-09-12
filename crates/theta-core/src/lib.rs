//! ThetaBase core primitives.
//!
//! Everything in this crate is deterministic and side-effect free: the log is the
//! ground truth (`docs/specs/03-data-model-consistency.md` §2.1) and every higher
//! layer is a fold over it. Nothing here performs I/O.

pub mod address;
pub mod branch;
pub mod crdt;
// The AEAD is optional so Scribe's WebAssembly core can drop it: that core
// encodes and decodes frames and never seals anything, and the sealing chain
// ends at a `getrandom` that will not build for wasm32 at all. See the
// `encryption` feature in Cargo.toml.
#[cfg(feature = "encryption")]
pub mod encryption;
pub mod error;
pub mod hash;
pub mod index;
pub mod log;
pub mod schema;
pub mod value;

/// The environment variable an instance reads its data-encryption key from
/// (SEC-2, `specs/04` §3).
///
/// Defined here, in the crate both halves already depend on, because the
/// Control Plane sets it and the storage engine reads it and they share no
/// other code. Two constants would be two chances to disagree, and the symptom
/// of disagreement is a database that quietly writes in the clear while
/// everything reports success — the exact failure SEC-2 was raised for.
///
/// The value is 64 hex characters: a 256-bit key.
pub const DATA_KEY_ENV: &str = "THETA_DATA_KEY";

/// The variable an instance may read its **public** keyset from, as JSON.
///
/// Defined beside [`DATA_KEY_ENV`] for the same reason: the Control Plane
/// writes it and `thetad` reads it, and they share no other code.
///
/// # Why there are two routes to the same keyset
///
/// The keyset is public, so the original delivery was a file in the instance's
/// own data directory — correct, and simpler than an environment variable,
/// *when the Control Plane and the instance share a filesystem*. On a machine
/// platform they do not: each instance is its own microVM with its own volume,
/// and a file the Control Plane writes lands in the Control Plane's container
/// where nothing will ever read it. The symptom is an instance that starts
/// cleanly and then refuses every request, because it has no keys to verify
/// tokens against.
///
/// So the file remains for the shared-filesystem topology and for an operator
/// starting `thetad` by hand, and this carries it everywhere else.
/// [`Runtime::shares_filesystem`] is what decides which applies.
///
/// Unlike [`DATA_KEY_ENV`], nothing here is secret. It is delivered as a
/// platform secret only because that is the mechanism a machine platform gives
/// for getting a value into a process.
///
/// [`Runtime::shares_filesystem`]: https://docs.rs/theta-control
pub const PUBLIC_KEYSET_ENV: &str = "THETA_PUBLIC_KEYSET";

pub use address::{has_column, RowAddress, RowSource, KEY_COLUMN, VALUE_COLUMN};
pub use branch::{Branch, BranchId};
pub use error::{CoreError, Result};
pub use hash::ContentHash;
pub use index::{encoded_range, IndexBound};
pub use log::{AgentProvenance, Author, CommitId, CrdtOp, LogEntry, OpType};
pub use value::{Value, ValueType};

#[cfg(feature = "encryption")]
pub use encryption::DataKey;
