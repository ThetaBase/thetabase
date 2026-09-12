//! Data at rest, re-exported.
//!
//! [`DataKey`] used to be declared here. It moved to `theta-core` when the
//! Safety Layer needed to seal its audit log: `specs/04` §3 recorded that the
//! log sat beside the encrypted segments in clear, and closing that meant the
//! key reaching `theta-safety` -- which has no business depending on the whole
//! storage engine for thirty-two bytes and two functions.
//!
//! Re-exported rather than moved outright so `theta_storage::DataKey` keeps
//! resolving. There is still exactly one implementation of the sealing, which
//! is the property worth protecting.

pub use theta_core::encryption::{DataKey, DATA_KEY_ENV};
