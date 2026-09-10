//! The diff/preview object every gated proposal produces before any data moves
//! (`07-agent-safety-layer.md` §4).

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChangeId(pub String);

impl ChangeId {
    /// Deterministic id: content-addressed over the proposal, so proposing the
    /// same change twice yields the same id and cannot be replayed as a second,
    /// separately-approved change.
    pub fn of(change: &SchemaChange, branch: u64) -> Self {
        let encoded = serde_json::to_vec(change).expect("schema change is serializable");
        let hash = theta_core::ContentHash::of_fields(&[&encoded, &branch.to_le_bytes()]);
        ChangeId(format!("chg_{}", &hash.to_hex()[..24]))
    }
}

/// What the proposal will actually touch. Supplied by the storage engine's
/// estimator, never guessed by the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Impact {
    pub rows_affected: u64,
    pub estimated_cost_ms: u32,
}

impl Impact {
    pub fn new(rows_affected: u64, estimated_cost_ms: u32) -> Self {
        Self {
            rows_affected,
            estimated_cost_ms,
        }
    }

    pub const NONE: Impact = Impact {
        rows_affected: 0,
        estimated_cost_ms: 0,
    };
}

/// What must happen before this change may land on the target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Applies immediately; recorded in the audit log.
    AutoApply,
    /// One explicit confirmation (human, or a narrowly-scoped policy rule).
    Confirm,
    /// Confirmation is *not* sufficient. Must be applied to a shadow branch,
    /// validated, and explicitly promoted (`07-agent-safety-layer.md` §4-5).
    ShadowValidate,
}

/// Serialized shape matches the spec's object in `07-agent-safety-layer.md` §4,
/// because SDKs and the CLI render it directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeDiff {
    pub change_id: ChangeId,
    pub destructive: bool,
    pub rows_affected: u64,
    pub reversible: bool,
    pub estimated_cost_ms: u32,
    pub affected_schema: AffectedSchema,
    /// The gate the classifier decided on, recorded rather than recomputed.
    ///
    /// `requires_confirm` alone cannot distinguish "one confirmation is enough"
    /// from "confirmation is not sufficient", which is the exact distinction
    /// `07-agent-safety-layer.md` §4 turns on. Deriving the gate from the other
    /// fields — as this used to — is a second implementation of the decision
    /// that can silently disagree with the first, and a test written against
    /// the derivation cannot detect that the classifier changed its mind.
    pub gate: Gate,
    pub requires_confirm: bool,
    /// Populated once the change has been applied to a shadow branch for
    /// validation; `None` until then.
    pub shadow_branch_id: Option<u64>,
    /// Plain-language explanation of the gate decision, for the human reading a
    /// five-minute review rather than raw JSON.
    ///
    /// **Rendered from [`ChangeDiff::rationale`], never written by hand.** It is
    /// a projection of the decision, not a second account of it.
    pub reason: String,
    /// The same decision as structure: which rule fired, on what numbers, and
    /// what would unblock it.
    ///
    /// What an agent, a dashboard or a policy engine should read. `reason` is
    /// for a person; parsing it to recover any of this is parsing English to
    /// get back something that was structured a moment earlier.
    pub rationale: crate::rationale::GateRationale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AffectedSchema {
    pub table: String,
    pub column: Option<String>,
    pub change_type: String,
}

impl AffectedSchema {
    pub fn of(change: &SchemaChange) -> Self {
        let (table, column, change_type) = match change {
            SchemaChange::AddTable { table } => (table.name.clone(), None, "add_table"),
            SchemaChange::DropTable { table } => (table.clone(), None, "drop_table"),
            SchemaChange::AddColumn { table, field } => {
                (table.clone(), Some(field.name.clone()), "add_column")
            }
            SchemaChange::DropColumn { table, column } => {
                (table.clone(), Some(column.clone()), "drop_column")
            }
            SchemaChange::AlterColumnType { table, column, .. } => {
                (table.clone(), Some(column.clone()), "alter_column_type")
            }
            SchemaChange::SetNullable { table, column, .. } => {
                (table.clone(), Some(column.clone()), "set_nullable")
            }
            SchemaChange::AddIndex { table, index } => {
                (table.clone(), Some(index.name.clone()), "add_index")
            }
            SchemaChange::DropIndex { table, index } => {
                (table.clone(), Some(index.clone()), "drop_index")
            }
            SchemaChange::RenameColumn { table, from, .. } => {
                (table.clone(), Some(from.clone()), "rename_column")
            }
            SchemaChange::SetCrdt { table, column, .. } => {
                (table.clone(), Some(column.clone()), "set_crdt")
            }
        };
        Self {
            table,
            column,
            change_type: change_type.to_string(),
        }
    }
}
