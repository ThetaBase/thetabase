//! What a proposed change will actually touch.
//!
//! The Safety Layer's gate turns on the row count
//! (`07-agent-safety-layer.md` §3, §4), so where that number comes from is a
//! security question, not an accounting one. It used to arrive in the proposal
//! itself: the client said how many rows its change would affect and the server
//! believed it. An agent that wanted a drop waved through as low-impact only had
//! to say `rowsAffected: 0` — the same shape of hole as a caller naming its own
//! user id.
//!
//! So the estimate is computed here, from the branch's own materialized view,
//! and nothing the caller sends is read.
//!
//! **This is exact, not sampled.** The view is a `BTreeMap`, the count is a
//! prefix range over it, and the numbers below are the real ones rather than a
//! projection. `Impact` keeps the word "estimate" because the *cost* half is
//! genuinely a model (see [`estimated_cost_ms`]), and because a future
//! sampled implementation for very large tables must be able to slot in here
//! without the callers having assumed otherwise.

use theta_core::schema::SchemaChange;
use theta_core::{RowAddress, Value};
use theta_safety::diff::Impact;
use theta_storage::MaterializedView;

/// Fixed overhead of any schema operation, in milliseconds.
///
/// Coarse and honest about it: a real cost model needs production timings,
/// which arrive with per-project metrics in M10. What this has to get right is
/// the *shape* — bigger changes cost more — because that is all the Safety
/// Layer reads it for.
const FIXED_COST_MS: u64 = 5;

/// Marginal cost per row touched.
const PER_ROW_COST_US: u64 = 20;

/// What `change` would do to `view`.
pub fn estimate(view: &MaterializedView, change: &SchemaChange) -> Impact {
    let rows = rows_affected(view, change);
    Impact::new(rows, estimated_cost_ms(rows))
}

fn estimated_cost_ms(rows: u64) -> u32 {
    let total_us = rows.saturating_mul(PER_ROW_COST_US);
    let ms = FIXED_COST_MS.saturating_add(total_us / 1_000);
    ms.try_into().unwrap_or(u32::MAX)
}

fn rows_affected(view: &MaterializedView, change: &SchemaChange) -> u64 {
    match change {
        // Additive. Nothing that already exists is touched, so the honest
        // answer is zero — not "the size of the table it was added to", which
        // would push routine `ADD COLUMN`s over the impact threshold and train
        // people to confirm without reading.
        SchemaChange::AddTable { .. }
        | SchemaChange::AddColumn { .. }
        | SchemaChange::AddIndex { .. }
        | SchemaChange::SetCrdt { .. } => 0,

        // No row loses data. It is still gated as ambiguous, on its type rather
        // than its size (`07-agent-safety-layer.md` §3).
        SchemaChange::DropIndex { .. } => 0,

        SchemaChange::DropTable { table } => rows_in(view, table),

        // Rows that actually hold the column. A column present in three rows of
        // a million-row table destroys three rows' worth of data, and saying
        // "1,000,000 rows" would be a number the reviewer cannot act on.
        SchemaChange::DropColumn { table, column }
        | SchemaChange::RenameColumn {
            table,
            from: column,
            ..
        } => rows_with_column(view, table, column),

        SchemaChange::AlterColumnType { table, column, .. } => {
            rows_with_column(view, table, column)
        }

        // Relaxing a constraint rejects nothing.
        SchemaChange::SetNullable { nullable: true, .. } => 0,

        // Tightening one rejects exactly the rows that have no value for the
        // column — those are the rows a backfill has to cover, and the rows
        // that are lost without one.
        SchemaChange::SetNullable {
            table,
            column,
            nullable: false,
            ..
        } => rows_without_column(view, table, column),
    }
}

/// Rows of one table.
///
/// A prefix range, not a scan: this runs on every proposal, and walking the
/// whole keyspace to answer "how big is `users`" would make the cost of asking
/// scale with the size of every other table.
fn rows_in(view: &MaterializedView, table: &str) -> u64 {
    count_rows(view, table, |_| true)
}

fn rows_with_column(view: &MaterializedView, table: &str, column: &str) -> u64 {
    count_rows(view, table, |row| theta_core::has_column(row, true, column))
}

fn rows_without_column(view: &MaterializedView, table: &str, column: &str) -> u64 {
    count_rows(view, table, |row| {
        !theta_core::has_column(row, true, column)
    })
}

fn count_rows(view: &MaterializedView, table: &str, mut keep: impl FnMut(&Value) -> bool) -> u64 {
    let prefix = RowAddress::prefix(table);

    let plain = view
        .keys
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, value)| RowAddress::parse(key).is_some() && keep(value))
        .count();

    // CRDT-typed fields live in their own map and are just as much rows of the
    // table. Counting only `keys` would under-report a CRDT-heavy table, and
    // under-reporting is the direction that lets a change through.
    let crdt = view
        .crdts
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, state)| RowAddress::parse(key).is_some() && keep(&state.value()))
        .count();

    (plain + crdt) as u64
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use theta_core::schema::{FieldDef, IndexDef, TableDef};
    use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, ValueType};

    use super::*;

    fn entry(op: OpType, seq: u64) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash::ZERO,
            commit_id: CommitId(seq),
            branch_id: BranchId::MAIN,
            op,
            author: Author::System,
            timestamp_ms: seq as i64,
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

    /// A view holding `count` rows of `users`, every one with an `email`.
    fn users(count: u64) -> MaterializedView {
        let mut view = MaterializedView::new();
        for i in 0..count {
            view.apply(&entry(
                OpType::Put {
                    key: format!("users:{i}"),
                    value: row(&[("email", Value::Text(format!("u{i}@example.com")))]),
                },
                i,
            ));
        }
        view
    }

    fn field(name: &str) -> FieldDef {
        FieldDef {
            name: name.into(),
            ty: ValueType::Text,
            nullable: true,
            crdt: None,
            declared_at: None,
        }
    }

    #[test]
    fn dropping_a_table_counts_every_row_in_it() {
        let view = users(37);
        let impact = estimate(
            &view,
            &SchemaChange::DropTable {
                table: "users".into(),
            },
        );
        assert_eq!(impact.rows_affected, 37);
    }

    #[test]
    fn only_the_named_tables_rows_are_counted() {
        let mut view = users(10);
        for i in 0..500 {
            view.apply(&entry(
                OpType::Put {
                    key: format!("orders:{i}"),
                    value: Value::Int(i as i64),
                },
                1_000 + i,
            ));
        }

        // Counting the whole keyspace would report 510 and turn a ten-row drop
        // into a high-impact change.
        let impact = estimate(
            &view,
            &SchemaChange::DropTable {
                table: "users".into(),
            },
        );
        assert_eq!(impact.rows_affected, 10);
    }

    #[test]
    fn dropping_a_column_counts_the_rows_that_actually_hold_it() {
        let mut view = users(3);
        for i in 3..1_000 {
            view.apply(&entry(
                OpType::Put {
                    key: format!("users:{i}"),
                    value: row(&[("name", Value::Text("someone".into()))]),
                },
                i,
            ));
        }

        // 1,000 rows in the table, 3 of them with an email. Reporting 1,000
        // would be a number the reviewer cannot act on.
        let impact = estimate(
            &view,
            &SchemaChange::DropColumn {
                table: "users".into(),
                column: "email".into(),
            },
        );
        assert_eq!(impact.rows_affected, 3);
    }

    #[test]
    fn a_column_holding_null_does_not_count_as_held() {
        let mut view = MaterializedView::new();
        view.apply(&entry(
            OpType::Put {
                key: "users:1".into(),
                value: row(&[("email", Value::Null)]),
            },
            0,
        ));

        assert_eq!(
            estimate(
                &view,
                &SchemaChange::DropColumn {
                    table: "users".into(),
                    column: "email".into(),
                },
            )
            .rows_affected,
            0,
            "dropping a column that holds only nulls destroys nothing"
        );
    }

    #[test]
    fn tightening_a_constraint_counts_the_rows_it_would_reject() {
        let mut view = users(5);
        for i in 5..12 {
            view.apply(&entry(
                OpType::Put {
                    key: format!("users:{i}"),
                    value: row(&[("name", Value::Text("someone".into()))]),
                },
                i,
            ));
        }

        // Seven rows have no email; those are the rows a backfill has to cover
        // and the rows lost without one.
        let impact = estimate(
            &view,
            &SchemaChange::SetNullable {
                table: "users".into(),
                column: "email".into(),
                nullable: false,
                backfill: None,
            },
        );
        assert_eq!(impact.rows_affected, 7);
    }

    #[test]
    fn relaxing_a_constraint_rejects_nothing() {
        let view = users(1_000);
        let impact = estimate(
            &view,
            &SchemaChange::SetNullable {
                table: "users".into(),
                column: "email".into(),
                nullable: true,
                backfill: None,
            },
        );
        assert_eq!(impact.rows_affected, 0);
    }

    #[test]
    fn an_additive_change_touches_no_existing_row_however_big_the_table() {
        let view = users(100_000);

        // Reporting the table's size here would push every routine ADD COLUMN
        // over the impact threshold and train people to confirm without reading.
        for change in [
            SchemaChange::AddColumn {
                table: "users".into(),
                field: field("nickname"),
            },
            SchemaChange::AddIndex {
                table: "users".into(),
                index: IndexDef {
                    name: "by_email".into(),
                    columns: vec!["email".into()],
                    unique: false,
                },
            },
            SchemaChange::AddTable {
                table: TableDef {
                    name: "invoices".into(),
                    fields: BTreeMap::from([("total".to_string(), field("total"))]),
                    indexes: Vec::new(),
                },
            },
        ] {
            assert_eq!(
                estimate(&view, &change).rows_affected,
                0,
                "{change:?} was counted as touching existing rows"
            );
        }
    }

    #[test]
    fn a_change_to_a_table_that_does_not_exist_is_zero_rows_not_an_error() {
        let view = users(10);
        let impact = estimate(
            &view,
            &SchemaChange::DropTable {
                table: "nothing-here".into(),
            },
        );
        assert_eq!(impact.rows_affected, 0);
    }

    #[test]
    fn a_table_whose_name_prefixes_another_does_not_absorb_its_rows() {
        let mut view = users(4);
        for i in 0..900 {
            view.apply(&entry(
                OpType::Put {
                    key: format!("users_archive:{i}"),
                    value: Value::Int(i as i64),
                },
                2_000 + i,
            ));
        }

        // `users:` is the prefix, not `users`, so `users_archive` is a
        // different table. Getting this wrong would over-report by 900 rows.
        assert_eq!(
            estimate(
                &view,
                &SchemaChange::DropTable {
                    table: "users".into()
                },
            )
            .rows_affected,
            4
        );
    }

    #[test]
    fn the_estimate_is_the_same_every_time_it_is_asked_for() {
        // The gate is a function of this number, so two proposals of the same
        // change against the same view must classify identically.
        let view = users(64);
        let change = SchemaChange::DropTable {
            table: "users".into(),
        };
        let first = estimate(&view, &change);
        for _ in 0..8 {
            assert_eq!(estimate(&view, &change), first);
        }
    }

    #[test]
    fn cost_grows_with_the_rows_touched_and_never_overflows() {
        assert!(estimated_cost_ms(0) > 0, "even a no-op costs something");
        assert!(estimated_cost_ms(1_000_000) > estimated_cost_ms(1_000));
        // A row count this large is not reachable, but saturating rather than
        // wrapping is the difference between a big number and a tiny one.
        assert_eq!(estimated_cost_ms(u64::MAX), u32::MAX);
    }
}
