//! Wire protocol definitions and version negotiation.
//!
//! The Cap'n Proto schema in `schema/theta.capnp` is the single source of truth
//! for every SDK binding. Generated Rust bindings are not built here yet — the
//! `capnp` compiler is a build prerequisite and codegen is wired up in ROADMAP
//! M2. Until then this crate owns the parts of the protocol that are pure
//! policy: version negotiation and status codes.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod frame;
pub mod wire;

/// Bindings generated from `schema/theta.capnp` at build time.
///
/// The generated code refers to itself as `crate::theta_capnp`, so it has to
/// live at the crate root under exactly this name. Prefer the typed views in
/// [`wire`] over reaching in here directly.
#[allow(clippy::all, dead_code, unused_qualifications, clippy::pedantic)]
#[rustfmt::skip]
pub mod theta_capnp {
    include!(concat!(env!("OUT_DIR"), "/theta_capnp.rs"));
}

pub use frame::{FrameError, MAX_FRAME_BYTES};
pub use wire::{Hello, Request, RequestBody, Response, ResponseBody, Welcome};

/// Bumped only for changes that are *not* additive. Additive changes (new
/// optional fields with new ordinals) keep the version and stay compatible with
/// older SDKs mid-rollout (`02-api-wire-protocol.md` §5).
pub const PROTOCOL_VERSION: u32 = 1;

/// Oldest protocol version this build still speaks.
pub const MIN_SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum StatusCode {
    Ok = 0,
    /// Rejected by the Safety Layer; the caller must submit a corrected proposal.
    Rejected = 1,
    /// The blast-radius breaker is open.
    BreakerOpen = 2,
    /// Token is not scoped to this project/environment.
    Unauthorized = 3,
    /// Change requires confirmation or shadow validation before it can land.
    ConfirmationRequired = 4,
    Conflict = 5,
    Internal = 6,
    /// The instance is at its request or connection limit. Distinct from
    /// `BreakerOpen`, which is about the size of a caller's writes rather than
    /// how many callers there are.
    Busy = 7,
}

impl StatusCode {
    /// Decode a status byte off the wire.
    ///
    /// An unrecognised code becomes `Internal` rather than an error: a newer
    /// server may send a code this build predates, and the additive-by-default
    /// rule (`02-api-wire-protocol.md` §5) means that must degrade to "something
    /// went wrong", not to a dropped connection.
    pub fn from_u8(code: u8) -> Self {
        match code {
            0 => StatusCode::Ok,
            1 => StatusCode::Rejected,
            2 => StatusCode::BreakerOpen,
            3 => StatusCode::Unauthorized,
            4 => StatusCode::ConfirmationRequired,
            5 => StatusCode::Conflict,
            7 => StatusCode::Busy,
            _ => StatusCode::Internal,
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, StatusCode::Ok)
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum NegotiationError {
    #[error("client protocol v{client} is older than the minimum supported v{minimum}")]
    ClientTooOld { client: u32, minimum: u32 },

    #[error("client protocol v{client} is newer than this server's v{server}")]
    ClientTooNew { client: u32, server: u32 },
}

/// Agree a protocol version, or refuse.
///
/// Deliberately refuses rather than guessing: a silently-incompatible pairing is
/// worse than a failed connect (`02-api-wire-protocol.md` §5).
pub fn negotiate(client_version: u32) -> Result<u32, NegotiationError> {
    if client_version < MIN_SUPPORTED_VERSION {
        return Err(NegotiationError::ClientTooOld {
            client: client_version,
            minimum: MIN_SUPPORTED_VERSION,
        });
    }
    if client_version > PROTOCOL_VERSION {
        return Err(NegotiationError::ClientTooNew {
            client: client_version,
            server: PROTOCOL_VERSION,
        });
    }
    Ok(client_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_negotiate() {
        assert_eq!(negotiate(PROTOCOL_VERSION), Ok(PROTOCOL_VERSION));
    }

    #[test]
    fn incompatible_versions_are_refused_not_guessed() {
        assert!(matches!(
            negotiate(0),
            Err(NegotiationError::ClientTooOld { .. })
        ));
        assert!(matches!(
            negotiate(PROTOCOL_VERSION + 1),
            Err(NegotiationError::ClientTooNew { .. })
        ));
    }
}
