//! Generates the ThetaBase SDK surfaces from `theta.capnp`.
//!
//! See `main.rs` for what this is for. The modules are public so the generator's
//! own tests can render without shelling out to the binary.

pub mod csharp;
pub mod golang;
pub mod java;
pub mod python;
pub mod ruby;
pub mod schema;
pub mod swift;
pub mod typescript;
