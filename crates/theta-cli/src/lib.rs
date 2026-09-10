//! The `theta` CLI.
//!
//! This is where the product's interface actually lives — the dashboard is for
//! billing only (`06-provisioning-identity-flow.md` §5).

pub mod client;
pub mod data;
pub mod environment;
pub mod login;
pub mod store;

pub use client::{ControlPlaneClient, ResolveOutcome};
pub use data::{DataClient, DataError};
pub use store::{Backend, CachedContext, CredentialStore, Credentials, FileReason, StoreError};
pub mod review;
