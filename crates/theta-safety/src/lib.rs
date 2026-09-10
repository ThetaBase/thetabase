//! The Agent-Safety Layer.
//!
//! `07-agent-safety-layer.md` is the spec this crate implements, and its §8 is a
//! hard constraint on the code: **no LLM inference may enter any decision path in
//! this crate.** Classification is rule-based and deterministic — a function of
//! (change kind, row impact, reversibility, branch protection) and nothing else —
//! specifically so it cannot be argued or prompt-injected into misclassifying a
//! destructive change as safe.
//!
//! The second constraint is §2: prevent, don't correct. Nothing here rewrites a
//! proposal to make it acceptable. A rejected proposal is rejected, and the
//! caller must submit a corrected one.

pub mod audit;
pub mod breaker;
pub mod budget;
pub mod classify;
pub mod diff;
pub mod policy;
pub mod proof;
pub mod rationale;
pub mod replay;
pub mod signed;
pub mod spend;
pub mod store;
pub mod triage;

pub use audit::{must_escape, AuditEntry, RiskLevel};
pub use breaker::{BreakerDecision, CircuitBreaker};
pub use classify::{classify, Classification, Gate};
pub use diff::{ChangeDiff, ChangeId, Impact};
pub use policy::SafetyPolicy;
