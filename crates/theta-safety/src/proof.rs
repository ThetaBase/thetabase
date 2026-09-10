//! Proof-carrying migrations (ROADMAP-V3 M25).
//!
//! # The problem this is for
//!
//! The gate reads row impact, so a migration that touches a million rows is
//! gated because it is large. Most of those are safe: widening a column,
//! tightening a constraint every row already satisfies, narrowing a type no
//! value violates. A human reviews them because the classifier cannot tell the
//! difference between "large" and "large and dangerous".
//!
//! **A large safe migration should stop needing a human merely because it is
//! large.** So a change may arrive with a claim about what it preserves, and the
//! gate checks the claim instead of measuring the size.
//!
//! # The proof is checked, never trusted
//!
//! This is the whole design and it is the same lesson `impact.rs` already
//! learned the hard way. Row impact used to arrive in the proposal: the client
//! said how many rows its change would touch and the server believed it, so an
//! agent that wanted a drop waved through only had to say `rowsAffected: 0`.
//!
//! A caller-supplied *proof* is the identical hole with a longer name. So a
//! proposal carries a **claim**, and this module verifies it against the
//! branch's own view. Nothing the caller sends is read as evidence.
//!
//! What the caller's claim actually buys is direction: it tells the server which
//! cheap check to run. Verifying "no row violates this type" is one pass; working
//! out unprompted which of a dozen properties a migration might preserve is not.
//!
//! # A verified proof may only ever lower a gate, and only for some changes
//!
//! An unverified or failed proof leaves the gate exactly as the classifier set
//! it. A verified one may lower it, and only where the proof genuinely removes
//! the risk the gate was for:
//!
//! - **Narrowing a type no value violates** loses nothing, because there is
//!   nothing to lose. The gate was about data that would not fit; the proof says
//!   there is none.
//! - **Tightening nullability every row already satisfies** is the same
//!   argument.
//! - **A drop is still a drop.** No proof about the current contents makes
//!   removing a column reversible, so no proof lowers its gate. This is the case
//!   somebody will ask for, and the answer is no.

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;
use theta_core::{Value, ValueType};

use crate::classify::Gate;

/// What a migration claims about the data it is about to change.
///
/// A claim, not a proof. The server proves it or refuses to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "claim")]
pub enum Claim {
    /// Every value in this column already fits the type the change narrows to.
    ///
    /// If true, narrowing loses nothing.
    ///
    /// **There is deliberately no target type here.** It used to carry its own
    /// `to`, and nothing reconciled it with the change's: a caller proposing
    /// `Float -> Int` could attach a claim about `to: Float`, every float fits a
    /// float, the claim held trivially, and `apply` cleared a destructive
    /// irreversible narrowing to `AutoApply`. An external review (R3-01) built
    /// exactly that: 9,000 rows of money at `19.99`, narrowed to `Int` with no
    /// human and no shadow validation.
    ///
    /// The target now comes from the change, so the checked property cannot be
    /// weaker than the property the change needs. A field the caller supplies
    /// and the server must remember to reconcile is a field somebody eventually
    /// forgets to reconcile; a field that does not exist is not.
    NoValueViolatesType { table: String, column: String },
    /// Every row already has a value for this column.
    ///
    /// If true, making it non-null rejects nothing.
    NoRowIsMissing { table: String, column: String },
    /// The table is empty.
    ///
    /// The strongest and least interesting claim: anything is safe on no rows.
    /// Worth having because "this migration is enormous" and "this table is
    /// empty" are both true surprisingly often, and the gate currently only
    /// sees the first.
    TableIsEmpty { table: String },
}

/// What checking a claim established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Verdict {
    /// Checked against the branch's own data and found to hold.
    Holds { rows_examined: u64 },
    /// Checked and found false, with a witness.
    ///
    /// The witness is a **key**, never a value. A caller who proposed a
    /// migration already knows which rows exist; a value would put row contents
    /// into a refusal message that ends up in logs and in an agent's context.
    Fails {
        rows_examined: u64,
        first_violating_key: String,
    },
    /// The claim is not about anything this change does.
    ///
    /// Its own verdict rather than a failure: a caller that attached the wrong
    /// claim has made a different mistake from one whose claim is false, and
    /// telling them apart is the difference between fixing a typo and fixing
    /// their data.
    Irrelevant { reason: String },
}

impl Verdict {
    pub fn holds(&self) -> bool {
        matches!(self, Verdict::Holds { .. })
    }
}

/// Rows, as the checker needs to see them.
///
/// A trait rather than a concrete view so the checker can run against a shadow
/// branch as easily as a live one — and so this crate does not depend on
/// `theta-storage`, which would put the Safety Layer downstream of the thing it
/// gates.
pub trait RowSource {
    /// Every key in a table, and its row.
    fn rows(&self, table: &str) -> Vec<(String, Value)>;
}

/// Check a claim against the data.
///
/// Returns what it examined, so a verdict is never a bare yes: a claim that held
/// over zero rows and one that held over a million are different facts, and the
/// first is usually a table name somebody spelled wrong.
pub fn verify(claim: &Claim, change: &SchemaChange, rows: &impl RowSource) -> Verdict {
    if let Some(reason) = mismatch(claim, change) {
        return Verdict::Irrelevant { reason };
    }

    match claim {
        Claim::TableIsEmpty { table } => {
            let count = rows.rows(table).len() as u64;
            if count == 0 {
                Verdict::Holds { rows_examined: 0 }
            } else {
                Verdict::Fails {
                    rows_examined: count,
                    first_violating_key: rows
                        .rows(table)
                        .first()
                        .map(|(key, _)| key.clone())
                        .unwrap_or_default(),
                }
            }
        }

        Claim::NoRowIsMissing { table, column } => {
            let all = rows.rows(table);
            let examined = all.len() as u64;
            match all
                .iter()
                .find(|(_, row)| !theta_core::has_column(row, true, column))
            {
                None => Verdict::Holds {
                    rows_examined: examined,
                },
                Some((key, _)) => Verdict::Fails {
                    rows_examined: examined,
                    first_violating_key: key.clone(),
                },
            }
        }

        Claim::NoValueViolatesType { table, column } => {
            // From the change, never from the claim. `mismatch` has already
            // established that this is an `AlterColumnType`, so the target is
            // the one the change is actually narrowing to.
            let SchemaChange::AlterColumnType { to, .. } = change else {
                unreachable!("mismatch() rejects this claim on any other change")
            };
            let to = *to;
            let all = rows.rows(table);
            let examined = all.len() as u64;
            match all.iter().find(|(_, row)| {
                theta_core::address::column(row, "", column).is_some_and(|value| !fits(&value, to))
            }) {
                None => Verdict::Holds {
                    rows_examined: examined,
                },
                Some((key, _)) => Verdict::Fails {
                    rows_examined: examined,
                    first_violating_key: key.clone(),
                },
            }
        }
    }
}

/// Whether a claim is even about this change.
fn mismatch(claim: &Claim, change: &SchemaChange) -> Option<String> {
    // "No value violates the type" is a statement about a type change. Attached
    // to anything else there is no target type to check against, and answering
    // it would mean inventing one.
    if matches!(claim, Claim::NoValueViolatesType { .. })
        && !matches!(change, SchemaChange::AlterColumnType { .. })
    {
        return Some(
            "the claim is about the type values must fit, and the change does not \
             alter a column's type"
                .to_string(),
        );
    }

    let (claim_table, claim_column) = match claim {
        Claim::TableIsEmpty { table } => (table.as_str(), None),
        Claim::NoRowIsMissing { table, column } | Claim::NoValueViolatesType { table, column } => {
            (table.as_str(), Some(column.as_str()))
        }
    };

    let (change_table, change_column) = target(change);
    if claim_table != change_table {
        return Some(format!(
            "the claim is about `{claim_table}` and the change is about `{change_table}`"
        ));
    }
    match (claim_column, change_column) {
        (Some(claimed), Some(changed)) if claimed != changed => Some(format!(
            "the claim is about column `{claimed}` and the change is about `{changed}`"
        )),
        _ => None,
    }
}

fn target(change: &SchemaChange) -> (&str, Option<&str>) {
    match change {
        SchemaChange::AddTable { table } => (&table.name, None),
        SchemaChange::DropTable { table } => (table, None),
        SchemaChange::AddColumn { table, field } => (table, Some(&field.name)),
        SchemaChange::DropColumn { table, column }
        | SchemaChange::AlterColumnType { table, column, .. }
        | SchemaChange::SetNullable { table, column, .. }
        | SchemaChange::SetCrdt { table, column, .. } => (table, Some(column)),
        SchemaChange::RenameColumn { table, from, .. } => (table, Some(from)),
        SchemaChange::AddIndex { table, index } => (table, Some(&index.name)),
        SchemaChange::DropIndex { table, index } => (table, Some(index)),
    }
}

/// Whether a value fits a type.
fn fits(value: &Value, ty: ValueType) -> bool {
    match (value, ty) {
        (Value::Null, _) => true,
        (Value::Bool(_), ValueType::Bool) => true,
        (Value::Int(_), ValueType::Int) => true,
        // An integer fits a float; a float only fits an int if it is whole.
        (Value::Int(_), ValueType::Float) => true,
        (Value::Float(f), ValueType::Int) => f.fract() == 0.0,
        (Value::Float(_), ValueType::Float) => true,
        (Value::Text(_), ValueType::Text) => true,
        (Value::Bytes(_), ValueType::Bytes) => true,
        (Value::Timestamp(_), ValueType::Timestamp) => true,
        (Value::List(_), ValueType::List) => true,
        (Value::Map(_), ValueType::Map) => true,
        _ => false,
    }
}

/// What a verified claim does to a gate.
///
/// Takes the classifier's gate and returns the gate that applies. **Never
/// raises**, and lowers only where the proof removes the risk the gate was for.
pub fn apply(gate: Gate, change: &SchemaChange, verdict: &Verdict) -> (Gate, String) {
    if !verdict.holds() {
        return (
            gate,
            "the gate is unchanged: no claim was proved".to_string(),
        );
    }

    let Verdict::Holds { rows_examined } = verdict else {
        unreachable!("holds() was just checked")
    };

    match change {
        // Nothing is lost, because there was nothing that would not fit.
        SchemaChange::AlterColumnType { .. }
        | SchemaChange::SetNullable {
            nullable: false, ..
        } => (
            Gate::AutoApply,
            format!(
                "checked against {rows_examined} rows on this branch: none violate the \
                 constraint, so the change loses nothing and is not gated for its size"
            ),
        ),

        // A drop is still a drop. No claim about the current contents makes
        // removing them reversible, and this is the case somebody will ask for.
        SchemaChange::DropColumn { .. } | SchemaChange::DropTable { .. } => (
            gate,
            "the gate is unchanged: a proof about what a column contains does not \
             make removing it reversible"
                .to_string(),
        ),

        _ => (
            gate,
            "the gate is unchanged: no rule lowers it for this kind of change".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use theta_core::RowAddress;

    struct Rows(Vec<(String, Value)>);

    impl RowSource for Rows {
        fn rows(&self, table: &str) -> Vec<(String, Value)> {
            self.0
                .iter()
                .filter(|(key, _)| {
                    RowAddress::parse(key).is_some_and(|address| address.table == table)
                })
                .cloned()
                .collect()
        }
    }

    fn row(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn narrowing() -> SchemaChange {
        SchemaChange::AlterColumnType {
            table: "orders".into(),
            column: "total".into(),
            from: ValueType::Float,
            to: ValueType::Int,
        }
    }

    #[test]
    fn a_narrowing_no_value_violates_stops_being_gated_for_its_size() {
        // The point of the whole module: a large safe migration should not need
        // a human merely because it is large.
        let rows = Rows(
            (0..1_000)
                .map(|i| {
                    (
                        format!("orders:{i}"),
                        row(&[("total", Value::Float(i as f64))]),
                    )
                })
                .collect(),
        );

        let verdict = verify(
            &Claim::NoValueViolatesType {
                table: "orders".into(),
                column: "total".into(),
            },
            &narrowing(),
            &rows,
        );
        assert_eq!(
            verdict,
            Verdict::Holds {
                rows_examined: 1_000
            }
        );

        let (gate, why) = apply(Gate::ShadowValidate, &narrowing(), &verdict);
        assert_eq!(gate, Gate::AutoApply);
        assert!(why.contains("1000 rows"));
    }

    #[test]
    fn one_violating_row_fails_the_claim_and_names_the_key() {
        let rows = Rows(vec![
            ("orders:1".into(), row(&[("total", Value::Float(1.0))])),
            ("orders:2".into(), row(&[("total", Value::Float(1.5))])),
        ]);

        let verdict = verify(
            &Claim::NoValueViolatesType {
                table: "orders".into(),
                column: "total".into(),
            },
            &narrowing(),
            &rows,
        );
        match verdict {
            Verdict::Fails {
                first_violating_key,
                ..
            } => assert_eq!(first_violating_key, "orders:2"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_witness_is_a_key_and_never_a_value() {
        // A refusal message ends up in logs and in an agent's context. A caller
        // who proposed a migration already knows which rows exist; putting the
        // contents of one into an error is a leak for no gain.
        let rows = Rows(vec![(
            "orders:1".into(),
            row(&[("total", Value::Text("bob@example.com".into()))]),
        )]);
        let verdict = verify(
            &Claim::NoValueViolatesType {
                table: "orders".into(),
                column: "total".into(),
            },
            &narrowing(),
            &rows,
        );
        let encoded = serde_json::to_string(&verdict).unwrap();
        assert!(
            !encoded.contains("bob@example.com"),
            "a row value reached the verdict: {encoded}"
        );
    }

    #[test]
    fn a_failed_claim_leaves_the_gate_exactly_where_the_classifier_put_it() {
        let verdict = Verdict::Fails {
            rows_examined: 10,
            first_violating_key: "orders:2".into(),
        };
        let (gate, _) = apply(Gate::ShadowValidate, &narrowing(), &verdict);
        assert_eq!(gate, Gate::ShadowValidate);
    }

    #[test]
    fn no_proof_lowers_the_gate_on_a_drop() {
        // The case somebody will ask for, and the answer is no: no claim about
        // what a column currently contains makes removing it reversible.
        let drop = SchemaChange::DropColumn {
            table: "orders".into(),
            column: "total".into(),
        };
        let verdict = Verdict::Holds {
            rows_examined: 1_000,
        };
        let (gate, why) = apply(Gate::ShadowValidate, &drop, &verdict);
        assert_eq!(gate, Gate::ShadowValidate);
        assert!(why.contains("does not"));
    }

    #[test]
    fn a_claim_about_another_table_is_irrelevant_rather_than_false() {
        // A caller that attached the wrong claim has made a different mistake
        // from one whose claim is false, and telling them apart is the
        // difference between fixing a typo and fixing their data.
        let rows = Rows(vec![]);
        let verdict = verify(
            &Claim::TableIsEmpty {
                table: "customers".into(),
            },
            &narrowing(),
            &rows,
        );
        assert!(matches!(verdict, Verdict::Irrelevant { .. }), "{verdict:?}");
        assert!(!verdict.holds());
    }

    #[test]
    fn a_claim_about_another_column_is_irrelevant_too() {
        let rows = Rows(vec![]);
        let verdict = verify(
            &Claim::NoRowIsMissing {
                table: "orders".into(),
                column: "currency".into(),
            },
            &narrowing(),
            &rows,
        );
        assert!(matches!(verdict, Verdict::Irrelevant { .. }), "{verdict:?}");
    }

    #[test]
    fn a_claim_that_held_over_no_rows_says_so() {
        // A claim that held over zero rows and one that held over a million are
        // different facts, and the first is usually a table name somebody spelled
        // wrong.
        let rows = Rows(vec![]);
        let verdict = verify(
            &Claim::TableIsEmpty {
                table: "orders".into(),
            },
            &narrowing(),
            &rows,
        );
        assert_eq!(verdict, Verdict::Holds { rows_examined: 0 });
    }

    #[test]
    fn tightening_nullability_every_row_satisfies_is_not_gated_for_its_size() {
        let change = SchemaChange::SetNullable {
            table: "orders".into(),
            column: "total".into(),
            nullable: false,
            backfill: None,
        };
        let rows = Rows(
            (0..500)
                .map(|i| {
                    (
                        format!("orders:{i}"),
                        row(&[("total", Value::Int(i as i64))]),
                    )
                })
                .collect(),
        );

        let verdict = verify(
            &Claim::NoRowIsMissing {
                table: "orders".into(),
                column: "total".into(),
            },
            &change,
            &rows,
        );
        assert!(verdict.holds());
        assert_eq!(apply(Gate::Confirm, &change, &verdict).0, Gate::AutoApply);
    }

    #[test]
    fn a_row_missing_the_column_fails_a_no_row_is_missing_claim() {
        let change = SchemaChange::SetNullable {
            table: "orders".into(),
            column: "total".into(),
            nullable: false,
            backfill: None,
        };
        let rows = Rows(vec![
            ("orders:1".into(), row(&[("total", Value::Int(1))])),
            (
                "orders:2".into(),
                row(&[("currency", Value::Text("gbp".into()))]),
            ),
        ]);

        let verdict = verify(
            &Claim::NoRowIsMissing {
                table: "orders".into(),
                column: "total".into(),
            },
            &change,
            &rows,
        );
        match verdict {
            Verdict::Fails {
                first_violating_key,
                ..
            } => assert_eq!(first_violating_key, "orders:2"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_claim_is_checked_against_the_data_and_not_taken_on_the_callers_word() {
        // The hole `impact.rs` already closed once, with a longer name. A
        // caller-supplied proof the server accepted would be exactly the
        // caller-supplied row count that let an agent say `rowsAffected: 0`.
        //
        // There is no call that accepts a verdict from outside: `apply` takes a
        // `Verdict`, and the only way to obtain one is `verify`, which reads the
        // rows. This asserts the consequence — a false claim over real data
        // produces a failure however confidently it was asserted.
        let rows = Rows(vec![(
            "orders:1".into(),
            row(&[("total", Value::Float(1.5))]),
        )]);
        let verdict = verify(
            &Claim::TableIsEmpty {
                table: "orders".into(),
            },
            &SchemaChange::DropTable {
                table: "orders".into(),
            },
            &rows,
        );
        assert!(
            !verdict.holds(),
            "the table is not empty and saying so does not make it so"
        );
    }
}
