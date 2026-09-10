//! Index-backed execution (ROADMAP M3, "real index structures").
//!
//! Two things have to hold, and the second is the one that decays quietly:
//!
//! 1. An indexed query answers exactly what the same query answers without an
//!    index. The index narrows candidates and the filter still runs, so a
//!    generous index costs a discarded row — never a wrong answer.
//! 2. The index is *actually consulted*. An `IndexScan` that silently degrades
//!    to a full scan still returns the right rows, so nothing fails; it just
//!    stops being an index. Every test here that claims a lookup was served
//!    counts scans to prove it.

use std::cell::Cell;
use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, IndexDef, SchemaChange, TableDef};
use theta_core::{
    Author, BranchId, CommitId, ContentHash, LogEntry, OpType, RowSource, Value, ValueType,
};
use theta_query::exec::execute;
use theta_query::plan::{Expr, Literal, Plan, Predicate};
use theta_storage::MaterializedView;

/// Wraps a source and counts what execution asked of it.
///
/// The point of an index is that `scan` is not called. Nothing else in the
/// result distinguishes "served from the index" from "scanned and filtered",
/// which is exactly why it needs counting rather than eyeballing.
struct Counting<'a> {
    inner: &'a MaterializedView,
    scans: Cell<usize>,
    index_hits: Cell<usize>,
    rows_fetched: Cell<usize>,
}

impl<'a> Counting<'a> {
    fn new(inner: &'a MaterializedView) -> Self {
        Self {
            inner,
            scans: Cell::new(0),
            index_hits: Cell::new(0),
            rows_fetched: Cell::new(0),
        }
    }
}

impl RowSource for Counting<'_> {
    fn scan(&self, table: &str) -> Vec<(String, Value)> {
        self.scans.set(self.scans.get() + 1);
        self.inner.scan(table)
    }

    fn row(&self, table: &str, primary_key: &str) -> Option<Value> {
        self.rows_fetched.set(self.rows_fetched.get() + 1);
        self.inner.row(table, primary_key)
    }

    fn column_type(&self, table: &str, column: &str) -> Option<ValueType> {
        self.inner.column_type(table, column)
    }

    fn index_candidates(
        &self,
        table: &str,
        column: &str,
        bound: &theta_core::IndexBound,
    ) -> Option<Vec<String>> {
        let found = self.inner.index_candidates(table, column, bound);
        if found.is_some() {
            self.index_hits.set(self.index_hits.get() + 1);
        }
        found
    }
}

fn entry(op: OpType) -> LogEntry {
    LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(0),
        branch_id: BranchId::MAIN,
        op,
        author: Author::System,
        timestamp_ms: 0,
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

/// A `users` table of `count` rows, `age` running 0..count, indexed on `age`
/// unless `indexed` is false.
fn view_with(count: i64, indexed: bool) -> MaterializedView {
    let mut view = MaterializedView::new();
    view.apply(&entry(OpType::Schema {
        change: SchemaChange::AddTable {
            table: TableDef {
                name: "users".into(),
                fields: [(
                    "age".to_string(),
                    FieldDef {
                        name: "age".into(),
                        ty: ValueType::Int,
                        nullable: true,
                        crdt: None,
                        declared_at: None,
                    },
                )]
                .into_iter()
                .collect(),
                indexes: Vec::new(),
            },
        },
    }));

    if indexed {
        view.apply(&entry(OpType::Schema {
            change: SchemaChange::AddIndex {
                table: "users".into(),
                index: IndexDef {
                    name: "users_age".into(),
                    columns: vec!["age".into()],
                    unique: false,
                },
            },
        }));
    }

    for i in 0..count {
        view.apply(&entry(OpType::Put {
            key: format!("users:u{i}"),
            value: row(&[
                ("age", Value::Int(i)),
                ("name", Value::Text(format!("user{i}"))),
            ]),
        }));
    }
    view
}

fn index_scan(predicate: Predicate) -> Plan {
    Plan::IndexScan {
        table: "users".into(),
        index: "users_age".into(),
        predicate,
    }
}

fn eq(column: &str, value: Value) -> Predicate {
    Predicate::Eq {
        column: column.into(),
        value: Expr::Literal(Literal(value)),
    }
}

fn run(view: &MaterializedView, plan: &Plan) -> (Vec<String>, usize, usize) {
    let source = Counting::new(view);
    let result = execute(plan, &source, &Default::default()).expect("execute");
    // An empty result carries no columns to look a name up in, and "no rows"
    // is a legitimate answer here rather than a broken one.
    if result.rows.is_empty() {
        return (Vec::new(), source.scans.get(), source.index_hits.get());
    }
    let column = result
        .columns
        .iter()
        .position(|c| c.name == "name")
        .unwrap_or_else(|| {
            panic!(
                "no `name` column in {:?}",
                result.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        });
    let names = result
        .rows
        .iter()
        .map(|row| match &row[column] {
            Value::Text(t) => t.clone(),
            other => panic!("expected a name, got {other:?}"),
        })
        .collect();
    (names, source.scans.get(), source.index_hits.get())
}

#[test]
fn an_equality_lookup_is_served_from_the_index_without_scanning() {
    let view = view_with(200, true);
    let (names, scans, hits) = run(&view, &index_scan(eq("age", Value::Int(42))));

    assert_eq!(names, vec!["user42".to_string()]);
    assert_eq!(hits, 1, "the index was never consulted");
    assert_eq!(
        scans, 0,
        "an indexed equality must not walk the table - this is the whole point \
         of the index, and a degraded scan returns the same rows so nothing \
         else here would notice"
    );
}

#[test]
fn an_indexed_query_answers_exactly_what_an_unindexed_one_answers() {
    // The property that matters most: adding an index changes performance and
    // nothing else. Run the same predicates against a view with and without.
    let indexed = view_with(120, true);
    let plain = view_with(120, false);

    let predicates = vec![
        eq("age", Value::Int(0)),
        eq("age", Value::Int(119)),
        eq("age", Value::Int(9_999)),
        Predicate::Lt {
            column: "age".into(),
            value: Expr::Literal(Literal(Value::Int(5))),
        },
        Predicate::Gte {
            column: "age".into(),
            value: Expr::Literal(Literal(Value::Int(117))),
        },
        Predicate::In {
            column: "age".into(),
            values: vec![
                Expr::Literal(Literal(Value::Int(3))),
                Expr::Literal(Literal(Value::Int(77))),
            ],
        },
        // Neither of these narrows to a range, so both fall back to a scan on
        // both views - and must still agree.
        Predicate::Ne {
            column: "age".into(),
            value: Expr::Literal(Literal(Value::Int(1))),
        },
        Predicate::IsNull {
            column: "age".into(),
        },
    ];

    for predicate in predicates {
        let plan = index_scan(predicate.clone());
        let (with, _, _) = run(&indexed, &plan);
        let (without, _, _) = run(&plain, &plan);
        assert_eq!(
            with, without,
            "the index changed the answer for {predicate:?}"
        );
    }
}

#[test]
fn a_range_is_swept_rather_than_scanned() {
    let view = view_with(300, true);
    let (names, scans, hits) = run(
        &view,
        &index_scan(Predicate::Lt {
            column: "age".into(),
            value: Expr::Literal(Literal(Value::Int(3))),
        }),
    );

    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["user0", "user1", "user2"]);
    assert_eq!(hits, 1);
    assert_eq!(scans, 0, "a bounded range must not walk the whole table");
}

#[test]
fn a_predicate_no_index_can_narrow_falls_back_to_a_scan() {
    // `!=` matches nearly everything, so sweeping the index to find it would
    // cost more than the scan it replaced. Falling back is correct behaviour,
    // not a failure, and this pins that it still *happens*.
    let view = view_with(50, true);
    let (names, scans, hits) = run(
        &view,
        &index_scan(Predicate::Ne {
            column: "age".into(),
            value: Expr::Literal(Literal(Value::Int(0))),
        }),
    );

    assert_eq!(names.len(), 49);
    assert_eq!(hits, 0, "no index should have claimed this predicate");
    assert_eq!(scans, 1, "it has to have scanned instead");
}

#[test]
fn a_column_with_no_index_falls_back_to_a_scan() {
    let view = view_with(50, true);
    let (names, scans, hits) = run(&view, &index_scan(eq("name", Value::Text("user7".into()))));

    assert_eq!(names, vec!["user7".to_string()]);
    assert_eq!(hits, 0, "`name` carries no index");
    assert_eq!(scans, 1);
}

#[test]
fn an_updated_row_leaves_no_entry_under_its_old_value() {
    // The classic index bug: maintain on insert, forget on update, and the
    // index keeps pointing at a value the row no longer holds. The filter
    // cannot catch it, because the row it points at is real.
    let mut view = view_with(10, true);
    view.apply(&entry(OpType::Put {
        key: "users:u3".into(),
        value: row(&[
            ("age", Value::Int(999)),
            ("name", Value::Text("user3".into())),
        ]),
    }));

    let (stale, _, _) = run(&view, &index_scan(eq("age", Value::Int(3))));
    assert!(
        stale.is_empty(),
        "the index still points at the row's old age: {stale:?}"
    );

    let (moved, _, hits) = run(&view, &index_scan(eq("age", Value::Int(999))));
    assert_eq!(moved, vec!["user3".to_string()]);
    assert_eq!(hits, 1);
}

#[test]
fn a_deleted_row_leaves_no_entry_behind() {
    let mut view = view_with(10, true);
    view.apply(&entry(OpType::Delete {
        key: "users:u4".into(),
    }));

    let (found, _, _) = run(&view, &index_scan(eq("age", Value::Int(4))));
    assert!(
        found.is_empty(),
        "a deleted row is still indexed: {found:?}"
    );
}

#[test]
fn an_index_declared_after_the_rows_still_finds_them() {
    // An index built only from future writes answers questions about the past
    // by omission, which is the one direction a re-filter cannot rescue.
    let mut view = view_with(30, false);
    view.apply(&entry(OpType::Schema {
        change: SchemaChange::AddIndex {
            table: "users".into(),
            index: IndexDef {
                name: "users_age".into(),
                columns: vec!["age".into()],
                unique: false,
            },
        },
    }));

    let (names, scans, hits) = run(&view, &index_scan(eq("age", Value::Int(17))));
    assert_eq!(names, vec!["user17".to_string()]);
    assert_eq!(hits, 1);
    assert_eq!(scans, 0);
}

#[test]
fn dropping_an_index_returns_the_query_to_a_scan_with_the_same_answer() {
    let mut view = view_with(30, true);
    view.apply(&entry(OpType::Schema {
        change: SchemaChange::DropIndex {
            table: "users".into(),
            index: "users_age".into(),
        },
    }));

    let (names, scans, hits) = run(&view, &index_scan(eq("age", Value::Int(11))));
    assert_eq!(
        names,
        vec!["user11".to_string()],
        "the answer must not change"
    );
    assert_eq!(hits, 0);
    assert_eq!(scans, 1);
}

#[test]
fn an_integer_query_finds_a_row_stored_as_a_float() {
    // The executor compares Int and Float to each other, so `age = 5` matches a
    // stored 5.0. An index that keyed them separately would miss it - and miss
    // it silently, since the row simply would not come back.
    let mut view = view_with(10, true);
    view.apply(&entry(OpType::Put {
        key: "users:f1".into(),
        value: row(&[
            ("age", Value::Float(5.0)),
            ("name", Value::Text("float-five".into())),
        ]),
    }));

    let (mut names, _, hits) = run(&view, &index_scan(eq("age", Value::Int(5))));
    names.sort();
    assert_eq!(names, vec!["float-five".to_string(), "user5".to_string()]);
    assert_eq!(hits, 1);
}

#[test]
fn a_conjunction_may_use_one_index_and_filter_the_rest() {
    let view = view_with(100, true);
    let plan = index_scan(Predicate::And(vec![
        eq("age", Value::Int(7)),
        eq("name", Value::Text("user7".into())),
    ]));
    let (names, scans, hits) = run(&view, &plan);

    assert_eq!(names, vec!["user7".to_string()]);
    assert_eq!(hits, 1, "the indexed conjunct should have been used");
    assert_eq!(scans, 0);

    // And the un-indexable conjunct still excludes rows.
    let plan = index_scan(Predicate::And(vec![
        eq("age", Value::Int(7)),
        eq("name", Value::Text("somebody-else".into())),
    ]));
    let (names, _, _) = run(&view, &plan);
    assert!(names.is_empty(), "the filter did not apply: {names:?}");
}

#[test]
fn a_disjunction_is_only_served_when_every_branch_can_be() {
    let view = view_with(100, true);

    // Both sides indexable: the union is safe to serve.
    let (mut both, scans, hits) = run(
        &view,
        &index_scan(Predicate::Or(vec![
            eq("age", Value::Int(1)),
            eq("age", Value::Int(2)),
        ])),
    );
    both.sort();
    assert_eq!(both, vec!["user1".to_string(), "user2".to_string()]);
    assert_eq!(hits, 2);
    assert_eq!(scans, 0);

    // One side is on an unindexed column. Serving only the other would return a
    // subset and silently lose rows, so this has to scan.
    let (mut mixed, scans, _) = run(
        &view,
        &index_scan(Predicate::Or(vec![
            eq("age", Value::Int(1)),
            eq("name", Value::Text("user9".into())),
        ])),
    );
    mixed.sort();
    assert_eq!(
        mixed,
        vec!["user1".to_string(), "user9".to_string()],
        "a partially-indexable OR lost rows"
    );
    assert_eq!(scans, 1, "it had to fall back to a scan");
}
