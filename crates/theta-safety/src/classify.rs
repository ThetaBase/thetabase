//! Rule-based classification. Deterministic, total, and LLM-free by construction.
//!
//! The match in [`classify_kind`] is exhaustive over [`SchemaChange`], so adding
//! a schema operation without deciding how risky it is fails to compile.

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;

use crate::diff::{AffectedSchema, ChangeDiff, ChangeId, Impact};
use crate::policy::SafetyPolicy;
use crate::rationale::{GateRationale, RationaleInputs};

pub use crate::diff::Gate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Add column/index/table, widen a type. Auto-applies, logged.
    NonDestructive,
    /// Drop, narrow, non-null without backfill, bulk change above threshold.
    Destructive,
    /// Rename, backfill of a new required field. Destructive by default; a
    /// project policy may reclassify.
    Ambiguous,
}

/// Whether a change can be undone by applying its inverse without consulting
/// data that the change itself destroys.
fn is_reversible(change: &SchemaChange) -> bool {
    match change {
        // Additive: the inverse is a drop of something that held no prior data.
        SchemaChange::AddTable { .. }
        | SchemaChange::AddColumn { .. }
        | SchemaChange::AddIndex { .. }
        | SchemaChange::SetCrdt { .. } => true,

        // Metadata-only in both directions.
        SchemaChange::RenameColumn { .. } => true,
        SchemaChange::DropIndex { .. } => true,

        // Relaxing a constraint is reversible; tightening one is not, because
        // the rows it rejects are gone.
        SchemaChange::SetNullable { nullable, .. } => *nullable,

        // The old values cannot be recovered from the new ones.
        SchemaChange::DropTable { .. } | SchemaChange::DropColumn { .. } => false,

        // Reversible only if the change is a lossless widening.
        SchemaChange::AlterColumnType { from, to, .. } => from.widens_to(*to),
    }
}

fn classify_kind(change: &SchemaChange) -> Classification {
    match change {
        SchemaChange::AddTable { .. }
        | SchemaChange::AddColumn { .. }
        | SchemaChange::AddIndex { .. }
        | SchemaChange::SetCrdt { .. } => Classification::NonDestructive,

        // Dropping an index loses no data, but it can silently collapse query
        // performance, so it is gated as a real change rather than waved through.
        SchemaChange::DropIndex { .. } => Classification::Ambiguous,

        SchemaChange::DropTable { .. } | SchemaChange::DropColumn { .. } => {
            Classification::Destructive
        }

        SchemaChange::AlterColumnType { from, to, .. } => {
            if from.widens_to(*to) {
                Classification::NonDestructive
            } else {
                Classification::Destructive
            }
        }

        // Making a column non-null without a backfill plan rejects existing rows.
        SchemaChange::SetNullable {
            nullable, backfill, ..
        } => match (nullable, backfill) {
            (true, _) => Classification::NonDestructive,
            (false, Some(_)) => Classification::Ambiguous,
            (false, None) => Classification::Destructive,
        },

        // Additive or destructive depending on whether old references survive —
        // which the schema alone cannot tell us.
        SchemaChange::RenameColumn { .. } => Classification::Ambiguous,
    }
}

/// Classify a proposal and decide its gate.
///
/// `protected` is whether the *target* branch is protected; the same change is
/// gated harder on `main` than on a preview branch, which is the entire reason
/// branch-per-agent-session is cheap.
pub fn classify(
    change: &SchemaChange,
    impact: Impact,
    policy: &SafetyPolicy,
    protected: bool,
    branch_id: u64,
) -> ChangeDiff {
    let kind = classify_kind(change);
    let reversible = is_reversible(change);

    let effective = match kind {
        Classification::Ambiguous if policy.treat_ambiguous_as_safe => {
            Classification::NonDestructive
        }
        // Ambiguous is destructive until a project explicitly says otherwise.
        Classification::Ambiguous => Classification::Destructive,
        other => other,
    };

    let destructive = effective == Classification::Destructive;

    // `over_threshold` is deliberately not computed here. It depends on the row
    // impact threshold, which branch protection tightens — so deriving it
    // against the instance policy and passing it down would apply protection to
    // one threshold and not the other. `decide_gate` owns both.
    let (gate, rationale) = decide_gate(destructive, reversible, impact, policy, protected, change);

    ChangeDiff {
        change_id: ChangeId::of(change, branch_id),
        gate,
        destructive,
        rows_affected: impact.rows_affected,
        reversible,
        estimated_cost_ms: impact.estimated_cost_ms,
        affected_schema: AffectedSchema::of(change),
        requires_confirm: gate != Gate::AutoApply,
        shadow_branch_id: None,
        reason: rationale.render(),
        rationale,
    }
}

fn decide_gate(
    destructive: bool,
    reversible: bool,
    impact: Impact,
    policy: &SafetyPolicy,
    protected: bool,
    change: &SchemaChange,
) -> (Gate, GateRationale) {
    // **Branch protection is an input to the gate, not decoration.**
    //
    // It used to reach only the rationale, so `protected` and `standard`
    // produced identical gates and the monotonicity test — which asserts
    // `strictness(protected) >= strictness(standard)` — passed on equality
    // forever. A protected branch now takes at least the thresholds
    // `SafetyPolicy::protected()` sets, whatever policy the instance was
    // started with (`07-agent-safety-layer.md` §3, §4.1).
    let effective = match protected {
        true => policy.tightened_for_protected(),
        false => policy.clone(),
    };
    let policy = &effective;

    // The threshold is read through the accessor, which caps it: a project
    // policy may make this stricter and can never make it looser
    // (`07-agent-safety-layer.md` §4).
    let shadow_threshold = policy.effective_irreversible_shadow_threshold();

    // Recomputed against the effective policy, because the row-impact threshold
    // tightens too. Taking the caller's `over_threshold` here would have made
    // protection apply to one of the two thresholds and not the other.
    let over_threshold = impact.rows_affected > policy.row_impact_threshold;

    // Strongest gate first: irreversible and non-trivial impact cannot be waved
    // through by confirmation, no matter who or what is asking.
    //
    // The order of these branches is load-bearing and is *not* only asserted
    // here — `exhaustive_classification.rs` checks that every gate stays
    // reachable, because reordering them shadows the strongest rule and leaves
    // every other property intact.
    let gate = if destructive && !reversible && impact.rows_affected > shadow_threshold {
        Gate::ShadowValidate
    } else if destructive || over_threshold {
        Gate::Confirm
    } else {
        Gate::AutoApply
    };

    // A matching auto-approval is recorded so the rationale can name the rule
    // that let the change through. It does not change the gate: the branches
    // above already decided, and a policy rule cannot promote a gated change.
    let tag = AffectedSchema::of(change).change_type;
    let matched = policy
        .auto_approve
        .iter()
        .find(|r| r.change == tag && impact.rows_affected <= r.max_rows)
        .filter(|_| gate == Gate::AutoApply)
        .map(|r| (r.change.as_str(), r.max_rows));

    // One source of truth for the decision *and* its explanation.
    //
    // These used to be separate `format!` calls sitting beside each branch,
    // which is the mistake `07-agent-safety-layer.md` §4 already records about
    // `gate` and `requires_confirm`: two representations of one decision, free
    // to disagree, with nothing able to notice when they do. The prose is now
    // rendered from the rationale, so there is nothing for it to drift from.
    let rationale = GateRationale::derive(RationaleInputs {
        gate,
        destructive,
        reversible,
        rows_affected: impact.rows_affected,
        row_impact_threshold: policy.row_impact_threshold,
        shadow_threshold,
        protected,
        auto_approve: matched,
    });

    (gate, rationale)
}

/// The gate a produced diff carries.
///
/// Reads the decision the classifier recorded. It used to re-derive it from the
/// diff's other fields, which meant the gate had two implementations that could
/// disagree — and a test asserting on the derived value could not tell that the
/// classifier had stopped agreeing with it.
pub fn gate_of(diff: &ChangeDiff) -> Gate {
    diff.gate
}

#[cfg(test)]
mod tests {
    use theta_core::schema::{FieldDef, IndexDef};
    use theta_core::ValueType;

    use super::*;

    fn field(name: &str, ty: ValueType) -> FieldDef {
        FieldDef {
            name: name.into(),
            ty,
            nullable: true,
            crdt: None,
            declared_at: None,
        }
    }

    fn diff_for(change: &SchemaChange, rows: u64) -> ChangeDiff {
        classify(
            change,
            Impact::new(rows, 10),
            &SafetyPolicy::protected(),
            true,
            0,
        )
    }

    #[test]
    fn adding_a_column_auto_applies() {
        let d = diff_for(
            &SchemaChange::AddColumn {
                table: "users".into(),
                field: field("nickname", ValueType::Text),
            },
            0,
        );
        assert!(!d.destructive);
        assert!(!d.requires_confirm);
    }

    #[test]
    fn dropping_a_populated_column_demands_shadow_validation() {
        let d = diff_for(
            &SchemaChange::DropColumn {
                table: "users".into(),
                column: "email".into(),
            },
            14_032,
        );
        assert!(d.destructive && !d.reversible);
        assert_eq!(gate_of(&d), Gate::ShadowValidate);
    }

    #[test]
    fn widening_is_safe_but_narrowing_is_not() {
        let widen = diff_for(
            &SchemaChange::AlterColumnType {
                table: "orders".into(),
                column: "total".into(),
                from: ValueType::Int,
                to: ValueType::Float,
            },
            100,
        );
        let narrow = diff_for(
            &SchemaChange::AlterColumnType {
                table: "orders".into(),
                column: "total".into(),
                from: ValueType::Float,
                to: ValueType::Int,
            },
            100,
        );
        assert!(!widen.destructive);
        assert!(narrow.destructive);
    }

    #[test]
    fn rename_is_destructive_until_a_policy_says_otherwise() {
        let change = SchemaChange::RenameColumn {
            table: "users".into(),
            from: "email".into(),
            to: "email_address".into(),
        };
        assert!(diff_for(&change, 10).destructive);

        let mut lenient = SafetyPolicy::protected();
        lenient.treat_ambiguous_as_safe = true;
        assert!(!classify(&change, Impact::new(10, 1), &lenient, true, 0).destructive);
    }

    #[test]
    fn non_null_without_a_backfill_is_destructive() {
        let no_plan = SchemaChange::SetNullable {
            table: "users".into(),
            column: "email".into(),
            nullable: false,
            backfill: None,
        };
        assert_eq!(classify_kind(&no_plan), Classification::Destructive);
    }

    #[test]
    fn a_wide_non_destructive_change_still_needs_confirmation() {
        let d = diff_for(
            &SchemaChange::AddIndex {
                table: "events".into(),
                index: IndexDef {
                    name: "idx".into(),
                    columns: vec!["ts".into()],
                    unique: false,
                },
            },
            5_000_000,
        );
        assert!(!d.destructive);
        assert!(
            d.requires_confirm,
            "blast radius applies regardless of type"
        );
    }

    #[test]
    fn change_ids_are_deterministic_and_distinct_per_change() {
        let a = SchemaChange::DropTable { table: "a".into() };
        let b = SchemaChange::DropTable { table: "b".into() };
        assert_eq!(ChangeId::of(&a, 0), ChangeId::of(&a, 0));
        assert_ne!(ChangeId::of(&a, 0), ChangeId::of(&b, 0));
        assert_ne!(ChangeId::of(&a, 0), ChangeId::of(&a, 1));
    }
}
