//! `thetad` — the per-project storage engine daemon.
//!
//! One logical instance per project. There is no code path in this crate that
//! accepts a request spanning two project identifiers: cross-project isolation
//! is structural, not an ACL check (`04-threat-model-security.md` §3).

pub mod config;
pub mod describe;
pub mod dispatch;
pub mod engine;
pub mod impact;
pub mod server;
pub mod service;
pub mod session;
pub mod shadow;

/// Wall-clock milliseconds since the epoch.
///
/// Every entry point that stamps a request reads the clock here, so there is
/// one definition of "now" in the daemon rather than one per module.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub use config::Config;
pub use engine::Engine;
pub use server::{serve, ServerConfig};
pub use service::EngineHandle;
