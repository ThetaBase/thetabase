//! Scoped session tokens.
//!
//! This crate exists so that minting (the Control Plane) and verification
//! (`thetad`) share one implementation of the token format. Two
//! implementations of a security-critical format drift, and the drift is
//! discovered in production.
//!
//! # The isolation guarantee
//!
//! `04-threat-model-security.md` §2 requires that a token minted for project A
//! be *cryptographically incapable* of authenticating against project B — "not
//! merely not shown it". A shared secret plus a scope check does not satisfy
//! that: anyone holding the secret can mint a token for any project.
//!
//! So every project has its own Ed25519 keypair. A `thetad` instance holds only
//! its own project's **public** key, which means:
//!
//! * It cannot mint tokens at all, only verify them. A compromised storage node
//!   cannot issue credentials for itself or anyone else.
//! * A token signed with project A's key fails verification against project B's
//!   key, whatever its payload claims. Forging the `project_id` field does not
//!   help, because the signature is checked with the *verifier's* key.
//!
//! # Why there is no algorithm field
//!
//! JWT lets a token declare how it should be verified, which is the root of the
//! `alg: none` and RS256/HS256 confusion families. Here the verifier decides:
//! Ed25519, always. The only thing a token declares about its own format is a
//! version prefix, and an unrecognised version is refused rather than guessed
//! at.

pub mod identity;
pub mod keys;
pub mod oauth;
pub mod revocation;
pub mod token;

pub use identity::{IdentityScope, IdentityToken, DEFAULT_IDENTITY_TTL_MS};
pub use keys::{KeyId, KeysetError, ProjectKeys, PublicKeyset, SigningKeyset, WireKeyset};
pub use revocation::{
    RevocationDecision, RevocationError, RevocationList, RevocationSync, SignedRevocationList,
};
pub use token::{SessionToken, TokenError, TokenScope};
