//! The ThetaBase Rust SDK.
//!
//! ```no_run
//! use thetabase::{query, Theta, ThetaConfig};
//!
//! # async fn example() -> Result<(), thetabase::Error> {
//! let theta = Theta::connect(ThetaConfig::new("127.0.0.1:7777"), "my-token");
//!
//! let alice = theta_core::Value::from_json(&serde_json::json!({
//!     "email": "alice@example.com",
//! }));
//! theta.put("users:1", &alice).await?;
//!
//! let rows = theta
//!     .query(&query::table("users").filter(query::eq("email", "alice@example.com")))
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # The three pieces, and why two of them were already here
//!
//! Every ThetaBase SDK is the same three things: generated wire types, a typed
//! query builder, and a transport binding onto the Scribe core. Rust is the one
//! language where two of the three already existed in this repository, and
//! pretending otherwise would have meant writing second copies of both.
//!
//! **Wire types.** [`wire`] re-exports `theta_proto::wire`, which
//! `theta-proto`'s build script generates from `theta.capnp` — the same schema
//! the TypeScript, Python and Go bindings are generated from. There is no
//! `generated.rs` here and there must not be one: a second copy of the wire
//! types is exactly the drift `make sdk-check` exists to catch in the other
//! bindings, and Rust avoids it by not making the copy.
//!
//! **Transport.** [`Theta`] wraps `theta_scribe::Scribe`, which already had
//! connection pooling, the read cache, batching and token refresh, and was
//! already exercised against a live `thetad`. Wrapping it rather than writing a
//! fourth client is the same argument as the WASM core: what must be identical
//! across languages is the protocol.
//!
//! **The builder** is the piece that is genuinely new here — see [`query`].
//!
//! # No WebAssembly runtime
//!
//! The other three SDKs load the Scribe core as a WASM module, because that is
//! the only way to run the same protocol code inside Node, CPython and Go.
//! Rust links it. `theta-scribe-wasm` has always declared `crate-type =
//! ["cdylib", "rlib"]`, and the rlib is what this uses: the crate is named for
//! how it is usually *compiled*, not for what it contains.
//!
//! That is a real difference and worth being precise about. This SDK does not
//! exercise the C ABI in `abi.rs`, so it proves nothing about that boundary —
//! the other three prove it, three times over. What it does prove is the same
//! thing they do, and the conformance harness holds it to the same standard:
//! identical output from identical cases, or the build fails.
//!
//! # The client surface is complete here first
//!
//! `get`, `put`, `delete`, `query`, `explain`, `propose`, `apply`, branches and
//! `status` all work. The TypeScript and Python SDKs still stub theirs, and this
//! is not because Rust got special treatment — it is because `theta-scribe` is
//! written in Rust and the other three reach it through a core that deliberately
//! contains no sockets.

pub mod query;

use serde_json::Value as Json;
use theta_core::Value;
use theta_scribe::{Scribe, ScribeConfig, ScribeError, TokenSource};

pub use query::Query;
pub use theta_scribe::{CacheStats, QueryResult, StaticToken};

/// The wire types, generated from `theta.capnp`.
///
/// Re-exported rather than restated. See the crate docs.
pub use theta_proto::wire;

/// How to reach a project.
pub type ThetaConfig = ScribeConfig;

/// Anything that can go wrong.
///
/// Rendering is separated from the rest because it fails before anything leaves
/// the process: a refused identifier is a bug in the caller's query, not a
/// problem with the database, and a caller retrying it forever would never
/// succeed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Scribe(#[from] ScribeError),

    #[error(transparent)]
    Render(#[from] query::RenderError),

    #[error("a bound parameter did not survive rendering: {0}")]
    Parameter(#[from] serde_json::Error),
}

/// A connection to one ThetaBase project.
pub struct Theta {
    scribe: Scribe,
}

impl Theta {
    /// Connect using a static token.
    pub fn connect(config: ThetaConfig, token: impl Into<String>) -> Self {
        Self::with_token_source(config, std::sync::Arc::new(StaticToken(token.into())))
    }

    /// Connect using a token source that can refresh.
    pub fn with_token_source(config: ThetaConfig, token: std::sync::Arc<dyn TokenSource>) -> Self {
        Self {
            scribe: Scribe::connect(config, token),
        }
    }

    /// A view of the same project on another branch.
    pub fn on_branch(&self, branch_id: u64) -> Self {
        Self {
            scribe: self.scribe.on_branch(branch_id),
        }
    }

    /// The Scribe underneath, for anything this surface does not cover.
    ///
    /// Exposed deliberately. A wrapper that hid the thing it wraps would force
    /// a fork the first time somebody needed a method it had not got round to.
    pub fn scribe(&self) -> &Scribe {
        &self.scribe
    }

    // ---- data ---------------------------------------------------------------

    /// Point lookup. Hot path: no model call, ever.
    pub async fn get(&self, key: &str) -> Result<Option<Value>, Error> {
        Ok(self.scribe.get(key).await?)
    }

    /// Write a value. Returns the commit id.
    pub async fn put(&self, key: &str, value: &Value) -> Result<String, Error> {
        Ok(self.scribe.put(key, value).await?)
    }

    /// Delete a key. Returns the commit id.
    pub async fn delete(&self, key: &str) -> Result<String, Error> {
        Ok(self.scribe.delete(key).await?)
    }

    /// Write several keys in one round trip.
    pub async fn put_many(&self, writes: &[(String, Value)]) -> Result<Vec<String>, Error> {
        Ok(self.scribe.put_many(writes).await?)
    }

    // ---- queries ------------------------------------------------------------

    /// Run a typed query.
    ///
    /// Takes a [`Query`] and nothing else. There is deliberately no method here
    /// that accepts a SQL string: an SDK that offers one invites exactly the
    /// injection the typed path exists to prevent. A caller who genuinely needs
    /// to send text can reach [`Theta::scribe`] and be explicit about it.
    pub async fn query(&self, query: &Query) -> Result<QueryResult, Error> {
        let rendered = query.render()?;
        let params = bind(&rendered)?;
        Ok(self
            .scribe
            .query_with(&rendered.sql, &borrow(&params))
            .await?)
    }

    /// EXPLAIN a query without running it — what a reviewer reads before
    /// approving.
    ///
    /// The bound parameters go with it. A plan for `WHERE email = $p0` with
    /// nothing bound to `$p0` is a plan for a different query than the one that
    /// will run, and an explanation of something adjacent is worse than none.
    pub async fn explain(&self, query: &Query) -> Result<Json, Error> {
        let rendered = query.render()?;
        let params = bind(&rendered)?;
        Ok(self
            .scribe
            .explain_with(&rendered.sql, &borrow(&params))
            .await?)
    }

    // ---- schema -------------------------------------------------------------

    /// Submit a schema change. Always returns a diff; never applies anything.
    ///
    /// A change the rules put at the shadow gate is applied to an ephemeral
    /// shadow branch and validated there as part of this call, so the diff comes
    /// back already saying what the checks found. Landing it still takes an
    /// explicit confirmation.
    pub async fn propose(
        &self,
        change: &theta_core::schema::SchemaChange,
    ) -> Result<wire::ChangeDiffWire, Error> {
        Ok(self.scribe.propose(change).await?)
    }

    /// Confirm a proposed change, by id.
    ///
    /// Takes no change body and no branch, and that is not an oversight: the
    /// server applies what it classified under this id, on the branch that
    /// proposal targeted. A caller who could supply either could confirm one
    /// change and execute another (`docs/specs/07-agent-safety-layer.md` §5.1).
    pub async fn apply(&self, change_id: &str, confirm: bool) -> Result<String, Error> {
        Ok(self.scribe.apply(change_id, confirm).await?)
    }

    // ---- branches -----------------------------------------------------------

    pub async fn create_branch(&self, name: &str, from: u64) -> Result<u64, Error> {
        Ok(self.scribe.create_branch(name, from).await?)
    }

    pub async fn merge(&self, source: u64, target: u64) -> Result<wire::MergeResultWire, Error> {
        Ok(self.scribe.merge(source, target).await?)
    }

    pub async fn status(&self) -> Result<wire::ProjectStatusWire, Error> {
        Ok(self.scribe.status().await?)
    }

    pub async fn cache_stats(&self) -> CacheStats {
        self.scribe.cache_stats().await
    }
}

/// Turn a rendered query's parameters back into values.
///
/// The renderer emits canonical JSON *text* per parameter, because that is what
/// crosses the WASM boundary for every other SDK. Parsing it back here rather
/// than adding a second, Rust-only render path keeps one renderer — the whole
/// point of the core. The cost is one parse per parameter per query, which is
/// nothing next to a round trip.
fn bind(rendered: &query::Rendered) -> Result<Vec<(String, Value)>, Error> {
    rendered
        .params
        .iter()
        .map(|(name, json)| {
            let plain: Json = serde_json::from_str(json)?;
            // `Value::from_json`, the same conversion the WASM path uses for
            // every other SDK. A Rust-only conversion here would be a second
            // reading of what a parameter means, and the two would disagree the
            // first time either moved.
            Ok((name.clone(), Value::from_json(&plain)))
        })
        .collect()
}

/// Borrow the names, since that is the shape Scribe takes.
fn borrow(params: &[(String, Value)]) -> Vec<(&str, Value)> {
    params
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_parameters_parse_back_to_the_values_they_came_from() {
        // The one place this SDK re-reads the core's output. A parameter that
        // failed to round-trip would reach the server as text and be compared
        // against a column as a string.
        let rendered = query::table("users")
            .filter(query::eq("age", 30))
            .filter(query::eq("name", "O'Brien"))
            .render()
            .expect("renders");

        let bound = bind(&rendered).expect("every parameter parses");
        let values: Vec<&Value> = bound.iter().map(|(_, v)| v).collect();

        assert!(values.iter().any(|v| matches!(v, Value::Int(30))));
        assert!(values
            .iter()
            .any(|v| matches!(v, Value::Text(t) if t == "O'Brien")));
    }

    #[test]
    fn the_wire_types_are_the_generated_ones_rather_than_a_copy() {
        // A second copy of a wire type is the drift the other bindings need a
        // gate to catch. This asserts the re-export actually points at
        // `theta_proto` — if someone declared a local `ChangeDiff`, this stops
        // compiling rather than quietly shadowing it.
        fn assert_same(_: &wire::ChangeDiffWire) {}
        let from_proto = theta_proto::wire::ChangeDiffWire::default();
        assert_same(&from_proto);
    }
}
