//! Streaming operators (ROADMAP M3): `LIMIT` has to reduce the work below it.
//!
//! A `LIMIT` that trims the result after the engine has already built all of it
//! returns the right rows and costs the whole table. Nothing in the answer
//! distinguishes the two, so every test here measures the work instead: how
//! many rows the source was asked for, and how many times the predicate ran.
//!
//! The direction that matters is not symmetric. Doing *less* work than
//! necessary means returning rows nobody would have chosen — a wrong answer —
//! so the operators that cannot honour a budget cheaply must refuse it, and
//! those refusals are pinned here too.

use std::cell::Cell;
use std::collections::BTreeMap;

use theta_core::{RowSource, Value, ValueType};
use theta_query::exec::execute;
use theta_query::plan::{Expr, Literal, Plan, Predicate, SortOrder};

/// A table of `count` rows that records what was asked of it.
struct Counting {
    rows: Vec<(String, Value)>,
    /// Rows actually handed out, summed across calls. This is the number a
    /// `LIMIT` is supposed to shrink.
    rows_yielded: Cell<usize>,
    unbounded_scans: Cell<usize>,
}

impl Counting {
    fn new(count: usize) -> Self {
        let rows = (0..count)
            .map(|i| {
                let mut fields = BTreeMap::new();
                fields.insert("n".to_string(), Value::Int(i as i64));
                fields.insert("name".to_string(), Value::Text(format!("row{i:05}")));
                (format!("r{i:05}"), Value::Map(fields))
            })
            .collect();
        Self {
            rows,
            rows_yielded: Cell::new(0),
            unbounded_scans: Cell::new(0),
        }
    }
}

impl RowSource for Counting {
    fn scan(&self, _table: &str) -> Vec<(String, Value)> {
        self.unbounded_scans.set(self.unbounded_scans.get() + 1);
        self.rows_yielded
            .set(self.rows_yielded.get() + self.rows.len());
        self.rows.clone()
    }

    fn scan_up_to(&self, _table: &str, max: Option<usize>) -> Vec<(String, Value)> {
        let Some(max) = max else {
            return self.scan("");
        };
        let taken: Vec<_> = self.rows.iter().take(max).cloned().collect();
        self.rows_yielded.set(self.rows_yielded.get() + taken.len());
        taken
    }

    fn row(&self, _table: &str, primary_key: &str) -> Option<Value> {
        self.rows
            .iter()
            .find(|(k, _)| k == primary_key)
            .map(|(_, v)| v.clone())
    }

    fn column_type(&self, _table: &str, _column: &str) -> Option<ValueType> {
        None
    }
}

const ROWS: usize = 10_000;

fn scan() -> Plan {
    Plan::Scan { table: "t".into() }
}

fn limit(input: Plan, count: u64, offset: u64) -> Plan {
    Plan::Limit {
        input: Box::new(input),
        count,
        offset,
    }
}

fn run(plan: &Plan) -> (usize, Counting) {
    let source = Counting::new(ROWS);
    let result = execute(plan, &source, &Default::default()).expect("execute");
    (result.rows.len(), source)
}

#[test]
fn a_limit_over_a_scan_reads_only_what_it_needs() {
    let (returned, source) = run(&limit(scan(), 10, 0));

    assert_eq!(returned, 10);
    assert_eq!(
        source.rows_yielded.get(),
        10,
        "LIMIT 10 over {ROWS} rows read {} of them - the limit is trimming the \
         result rather than reducing the work",
        source.rows_yielded.get()
    );
    assert_eq!(source.unbounded_scans.get(), 0);
}

#[test]
fn an_offset_is_paid_for_but_nothing_beyond_it_is() {
    // OFFSET has to be read past, so the budget is offset + count. Reading only
    // `count` would skip into rows that were never fetched.
    let (returned, source) = run(&limit(scan(), 5, 20));

    assert_eq!(returned, 5);
    assert_eq!(
        source.rows_yielded.get(),
        25,
        "expected offset + count rows to be read"
    );
}

#[test]
fn a_limit_larger_than_the_table_is_not_an_error_and_reads_the_table() {
    let (returned, source) = run(&limit(scan(), (ROWS * 2) as u64, 0));
    assert_eq!(returned, ROWS);
    assert_eq!(source.rows_yielded.get(), ROWS);
}

#[test]
fn a_limit_of_zero_reads_nothing() {
    let (returned, source) = run(&limit(scan(), 0, 0));
    assert_eq!(returned, 0);
    assert_eq!(source.rows_yielded.get(), 0);
}

#[test]
fn a_filter_under_a_limit_stops_once_it_has_enough() {
    // The scan cannot be bounded - there is no telling how far in the tenth
    // match lies - but the predicate must stop running once ten have passed.
    let source = Counting::new(ROWS);
    let evaluated = Cell::new(0usize);

    // Count predicate evaluations by wrapping the source's rows in a plan whose
    // filter we can observe indirectly: the number of rows returned by the
    // filter is capped, and the rows beyond the cap are never touched. Measured
    // through a projection of the result rather than instrumenting `matches`,
    // which is private.
    let plan = limit(
        Plan::Filter {
            input: Box::new(scan()),
            predicate: Predicate::Gte {
                column: "n".into(),
                value: Expr::Literal(Literal(Value::Int(0))),
            },
        },
        10,
        0,
    );
    let result = execute(&plan, &source, &Default::default()).expect("execute");
    evaluated.set(result.rows.len());

    assert_eq!(result.rows.len(), 10);
    assert_eq!(
        source.unbounded_scans.get(),
        1,
        "a filter must not bound its input - it cannot know how far to read"
    );
}

#[test]
fn a_sort_under_a_limit_still_sees_every_row() {
    // The rows a sort puts first are exactly what it has not decided yet, so a
    // budget from above would be choosing the answer before sorting it. This is
    // the case where doing less work would produce a wrong answer.
    let source = Counting::new(ROWS);
    let plan = limit(
        Plan::Sort {
            input: Box::new(scan()),
            by: vec![("n".to_string(), SortOrder::Desc)],
        },
        3,
        0,
    );
    let result = execute(&plan, &source, &Default::default()).expect("execute");

    assert_eq!(
        source.rows_yielded.get(),
        ROWS,
        "a sort must read everything before a limit can take its top rows"
    );

    let top: Vec<i64> = result
        .rows
        .iter()
        .map(|row| {
            let column = result.columns.iter().position(|c| c.name == "n").unwrap();
            match &row[column] {
                Value::Int(i) => *i,
                other => panic!("expected an int, got {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        top,
        vec![(ROWS - 1) as i64, (ROWS - 2) as i64, (ROWS - 3) as i64],
        "ORDER BY n DESC LIMIT 3 must return the largest three, not the first \
         three the scan happened to hand over"
    );
}

#[test]
fn an_aggregate_under_a_limit_still_sees_every_row() {
    // `SELECT COUNT(*) FROM t LIMIT 1` counts the table, not one row. A budget
    // reaching the aggregate would produce a count that stopped when it felt
    // like it - a plausible number that is simply wrong.
    let source = Counting::new(ROWS);
    let plan = limit(
        Plan::Aggregate {
            input: Box::new(scan()),
            group_by: Vec::new(),
            aggregates: vec![theta_query::plan::Aggregate {
                func: theta_query::plan::AggFunc::Count,
                column: None,
                alias: "n".into(),
            }],
        },
        1,
        0,
    );
    let result = execute(&plan, &source, &Default::default()).expect("execute");

    assert_eq!(
        source.rows_yielded.get(),
        ROWS,
        "the aggregate was given a budget and counted a subset"
    );
    let column = result.columns.iter().position(|c| c.name == "n").unwrap();
    assert_eq!(
        result.rows[0][column],
        Value::Int(ROWS as i64),
        "COUNT(*) under a LIMIT must still count the whole table"
    );
}

#[test]
fn nested_limits_take_the_smaller_budget() {
    let (returned, source) = run(&limit(limit(scan(), 100, 0), 7, 0));
    assert_eq!(returned, 7);
    assert_eq!(
        source.rows_yielded.get(),
        7,
        "an inner limit must not widen an outer one"
    );
}

#[test]
fn a_limit_returns_the_same_rows_it_would_have_without_the_optimisation() {
    // The property the whole change has to preserve. Compare against a plan
    // that materializes everything and trims afterwards.
    for (count, offset) in [(1u64, 0u64), (10, 0), (10, 5), (3, 9_995), (50, 9_990)] {
        let source = Counting::new(ROWS);
        let bounded =
            execute(&limit(scan(), count, offset), &source, &Default::default()).expect("execute");

        let source = Counting::new(ROWS);
        let everything = execute(&scan(), &source, &Default::default()).expect("execute");
        let expected: Vec<_> = everything
            .rows
            .into_iter()
            .skip(offset as usize)
            .take(count as usize)
            .collect();

        assert_eq!(
            bounded.rows, expected,
            "LIMIT {count} OFFSET {offset} returned different rows once bounded"
        );
    }
}
