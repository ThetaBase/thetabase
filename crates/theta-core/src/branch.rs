//! Branches: a pointer to a commit plus a private write-ahead segment.
//! Creation is O(1) — copy a pointer, never data (`01-system-architecture.md` §3.2).

use serde::{Deserialize, Serialize};

use crate::hash::ContentHash;
use crate::log::CommitId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BranchId(pub u64);

impl BranchId {
    /// Every project's trunk. Protected by default.
    pub const MAIN: BranchId = BranchId(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    /// `main`, `prod`, or any branch flagged production-like. Destructive changes
    /// cannot land here unreviewed.
    Protected,
    Standard,
    /// Created by the Safety Layer to validate a destructive change; discarded
    /// after promotion or rejection.
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    pub id: BranchId,
    pub name: String,
    pub kind: BranchKind,
    /// Commit this branch currently points at.
    pub head: ContentHash,
    /// Commit this branch diverged from; `None` for the root branch.
    pub fork_point: Option<ContentHash>,
    pub next_commit: CommitId,
}

impl Branch {
    pub fn is_protected(&self) -> bool {
        matches!(self.kind, BranchKind::Protected)
    }
}
