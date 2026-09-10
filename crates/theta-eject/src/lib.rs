//! `eject`: migration from Postgres/Supabase (ROADMAP M8).
//!
//! Reflect an existing Postgres schema, map it onto the ThetaBase type lattice,
//! stream the data in, and verify that what arrived means what it meant.
//!
//! The order is the design. Reflection and planning are pure and produce a
//! document the operator reads *before* anything is written, because the
//! interesting failures of a migration are decisions — a `numeric` that will
//! become a float, a table with no primary key — and a decision is cheap to
//! change beforehand and expensive afterwards.
//!
//! Nothing here writes to the source. A migration that could alter what it is
//! reading is one nobody can safely re-run, and re-running is exactly what
//! happens when the first attempt is interrupted.
//!
//! # Why a driver rather than `psql`
//!
//! `theta-archive` drives the `at1` CLI as a subprocess, and says why: a
//! subprocess is not a linked HTTP client, so nothing can drag a network stack
//! into a dependency closure that must not have one. That reasoning is about
//! the hot path, and this crate is not on it — `crates/thetad/tests/
//! no_llm_on_hot_path.rs` lists the crates that are, and `theta-eject` is
//! deliberately not among them.
//!
//! Against that, `psql` renders results as text for a terminal, and the
//! difference between an empty string and a NULL is a formatting flag. A
//! verification pass whose whole job is detecting changes in meaning cannot
//! rest on that.

pub mod import;
pub mod migrate;
pub mod plan;
pub mod reflect;
pub mod types;
pub mod values;
pub mod verify;

pub use migrate::{migrate, Outcome, Progress, SchemaOutcome, Silent, Target};
pub use plan::{plan_excluding, Blocker, Plan, TablePlan, Warning};
pub use reflect::Schema;
pub use verify::{Finding, Report, Severity};

/// A connection to the source database.
///
/// Re-exported so callers can hold one without depending on the driver
/// directly. Which driver reads Postgres is this crate's business; the CLI's
/// business is what to do with the rows.
pub use tokio_postgres::Client as SourceClient;

#[derive(Debug, thiserror::Error)]
pub enum EjectError {
    #[error("connecting to Postgres: {0}")]
    Connect(String),

    #[error("Postgres: {0}")]
    Postgres(#[from] tokio_postgres::Error),

    /// A value that does not parse as the type its column declared.
    ///
    /// Reported rather than coerced or skipped. Coercing changes the data
    /// silently and skipping loses it silently; both produce a migration that
    /// reports success (`07-agent-safety-layer.md` §2, invariant 3).
    #[error("{table}.{column}: expected {expected}, found `{found}`")]
    Unparseable {
        table: String,
        column: String,
        expected: String,
        found: String,
    },

    #[error("the plan cannot run: {0}")]
    Blocked(String),
}

/// Connect, and put the session into the shape the reader assumes.
///
/// `DateStyle` and `TimeZone` are set explicitly because [`values`] parses
/// timestamps from their text rendering. Leaving them at the server's defaults
/// would make the meaning of a migrated timestamp depend on the configuration
/// of the machine it was read from.
pub async fn connect(url: &str) -> Result<tokio_postgres::Client, EjectError> {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .map_err(|e| EjectError::Connect(e.to_string()))?;

    // The connection drives the protocol and must be polled for the client to
    // work at all; it ends when the client is dropped.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::warn!(error = %e, "postgres connection ended");
        }
    });

    client
        .batch_execute("SET DateStyle = 'ISO, MDY'; SET TimeZone = 'UTC';")
        .await?;

    Ok(client)
}
