//! Scribe — the ThetaBase edge runtime.
//!
//! Runs inside the caller's app (edge or origin) and is what every generated
//! SDK sits on: connection pooling, a local read cache, request batching, and
//! token refresh (`01-system-architecture.md` §2).
//!
//! # What the cache does and does not promise
//!
//! Caching reads is only safe if it cannot break the guarantees the database
//! makes. Two rules keep it honest:
//!
//! * **Read-your-writes is never violated.** Any write through this Scribe
//!   invalidates the affected key locally before the write is acknowledged to
//!   the caller, so a subsequent read cannot serve a value the caller has
//!   already replaced (`03-data-model-consistency.md` §3.1).
//! * **Another writer's update may be served stale, for up to the configured
//!   TTL.** That is a real weakening and it is stated plainly rather than
//!   implied away: within a branch ThetaBase offers strong *eventual*
//!   consistency, not linearizability, so a bounded staleness window is
//!   consistent with what the engine already guarantees — but it is opt-in, and
//!   a zero TTL disables the cache entirely.

pub mod cache;
pub mod pool;
pub mod token;

mod client;

pub use cache::{CacheStats, ReadCache};
pub use client::{QueryResult, Scribe, ScribeConfig, ScribeError};
pub use token::{StaticToken, TokenSource};
