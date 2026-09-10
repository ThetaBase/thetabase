//! Query planning and execution.
//!
//! Two hard constraints from the specs shape this crate:
//!
//! 1. **No LLM on the hot path** (`05-prd.md` §3, `09-sla-performance.md` §1).
//!    Nothing in here may call a model, and CI asserts it — see
//!    `tests/no_llm_on_hot_path.rs` in the workspace root.
//! 2. **No string interpolation into the execution path**
//!    (`04-threat-model-security.md` §4). Plans are built from typed [`plan`]
//!    nodes with parameters bound out-of-band. The SQL-subset front end parses
//!    *into* those nodes; it never concatenates a query.

pub mod builder;
pub mod exec;
pub mod explain;
pub mod optimize;
pub mod plan;
pub mod planner;
pub mod result;
pub mod sql;
pub mod stats;

pub use builder::{col, param, table};
pub use exec::{execute, Bindings, ExecError};
pub use explain::Explain;
pub use optimize::optimize;
pub use plan::{Expr, Literal, Plan, PlanHash, Predicate};
pub use planner::{PlanCache, Planner};
pub use result::{Column, ResultError, ResultSet};
pub use stats::{estimate, Estimate, Statistics, TableStats};
pub use theta_core::RowSource;
