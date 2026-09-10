//! Executor behaviour.
//!
//! The cases that matter are the ones where a database can be quietly wrong:
//! nulls, missing columns, mixed types, empty aggregates, and unstable
//! ordering. A query that returns *a* wrong answer consistently is worse than
//! one that errors, because nobody notices.

use std::collections::BTreeMap;

use theta_core::{Value, ValueType};
use theta_query::exec::{execute, Bindings, ExecError};
use theta_query::plan::{AggFunc, Aggregate, Expr, Literal, Plan, Predicate, SortOrder};
use theta_query::RowSource;

/// Fixture source, so the executor is tested without a storage engine.
#[derive(Debug, Default)]
struct Fixture {
    tables: BTreeMap<String, Vec<(String, Value)>>,
    types: BTreeMap<(String, String), ValueType>,
}

impl Fixture {
    fn with_users() -> Self {
        let mut f = Self::default();
        f.insert(
            "users",
            "1",
            &[
                ("name", text("Alice")),
                ("age", Value::Int(30)),
                ("active", Value::Bool(true)),
            ],
        );
        f.insert(
            "users",
            "2",
            &[
                ("name", text("Bob")),
                ("age", Value::Int(25)),
                ("active", Value::Bool(false)),
            ],
        );
        f.insert(
            "users",
            "3",
            &[
                ("name", text("Carol")),
                ("age", Value::Int(35)),
                ("active", Value::Bool(true)),
            ],
        );
        // Deliberately missing `age`, to pin down what a missing column does.
        f.insert(
            "users",
            "4",
            &[("name", text("Dave")), ("active", Value::Bool(true))],
        );
        f.declare("users", "age", ValueType::Int);
        f.declare("users", "name", ValueType::Text);
        f.declare("users", "active", ValueType::Bool);
        f
    }

    fn insert(&mut self, table: &str, key: &str, columns: &[(&str, Value)]) {
        let row = Value::Map(
            columns
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        );
        self.tables
            .entry(table.to_string())
            .or_default()
            .push((key.to_string(), row));
    }

    fn declare(&mut self, table: &str, column: &str, ty: ValueType) {
        self.types
            .insert((table.to_string(), column.to_string()), ty);
    }
}

impl RowSource for Fixture {
    fn scan(&self, table: &str) -> Vec<(String, Value)> {
        self.tables.get(table).cloned().unwrap_or_default()
    }

    fn row(&self, table: &str, primary_key: &str) -> Option<Value> {
        self.tables
            .get(table)?
            .iter()
            .find(|(k, _)| k == primary_key)
            .map(|(_, v)| v.clone())
    }

    fn column_type(&self, table: &str, column: &str) -> Option<ValueType> {
        self.types
            .get(&(table.to_string(), column.to_string()))
            .copied()
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn lit(value: Value) -> Expr {
    Expr::Literal(Literal(value))
}

fn scan(table: &str) -> Plan {
    Plan::Scan {
        table: table.into(),
    }
}

fn run(plan: &Plan, source: &Fixture) -> theta_query::ResultSet {
    execute(plan, source, &Bindings::new()).expect("execute")
}

/// Values of one column, in result order.
fn column_values(result: &theta_query::ResultSet, name: &str) -> Vec<Value> {
    let index = result
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column `{name}` in {:?}", result.columns));
    result.rows.iter().map(|row| row[index].clone()).collect()
}

#[test]
fn a_scan_returns_every_row_with_its_key() {
    let result = run(&scan("users"), &Fixture::with_users());
    assert_eq!(result.row_count(), 4);
    assert_eq!(
        column_values(&result, "_key"),
        vec![text("1"), text("2"), text("3"), text("4")]
    );
}

#[test]
fn scanning_a_table_that_does_not_exist_is_empty_rather_than_an_error() {
    let result = run(&scan("nonexistent"), &Fixture::with_users());
    assert!(result.is_empty());
}

#[test]
fn a_point_lookup_reads_one_row_without_walking_the_table() {
    let plan = Plan::PointLookup {
        table: "users".into(),
        key: Box::new(lit(text("2"))),
    };
    let result = run(&plan, &Fixture::with_users());
    assert_eq!(result.row_count(), 1);
    assert_eq!(column_values(&result, "name"), vec![text("Bob")]);
}

#[test]
fn a_point_lookup_that_misses_returns_no_rows() {
    let plan = Plan::PointLookup {
        table: "users".into(),
        key: Box::new(lit(text("999"))),
    };
    assert!(run(&plan, &Fixture::with_users()).is_empty());
}

#[test]
fn a_filter_keeps_only_matching_rows() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Gt {
            column: "age".into(),
            value: lit(Value::Int(28)),
        },
    };
    let result = run(&plan, &Fixture::with_users());
    assert_eq!(
        column_values(&result, "name"),
        vec![text("Alice"), text("Carol")]
    );
}

#[test]
fn a_row_missing_the_filtered_column_never_matches() {
    // Dave has no `age`. Every comparison against a missing column is false —
    // including `!=`, which is the case that trips people up.
    for predicate in [
        Predicate::Gt {
            column: "age".into(),
            value: lit(Value::Int(0)),
        },
        Predicate::Lt {
            column: "age".into(),
            value: lit(Value::Int(1000)),
        },
        Predicate::Eq {
            column: "age".into(),
            value: lit(Value::Int(30)),
        },
        Predicate::Ne {
            column: "age".into(),
            value: lit(Value::Int(30)),
        },
    ] {
        let plan = Plan::Filter {
            input: Box::new(scan("users")),
            predicate,
        };
        let result = run(&plan, &Fixture::with_users());
        assert!(
            !column_values(&result, "name").contains(&text("Dave")),
            "a row missing the column matched a comparison against it"
        );
    }
}

#[test]
fn is_null_finds_rows_whose_column_is_missing_or_null() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::IsNull {
            column: "age".into(),
        },
    };
    let result = run(&plan, &Fixture::with_users());
    assert_eq!(column_values(&result, "name"), vec![text("Dave")]);
}

#[test]
fn and_or_and_not_compose() {
    let source = Fixture::with_users();

    let both = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::And(vec![
            Predicate::Eq {
                column: "active".into(),
                value: lit(Value::Bool(true)),
            },
            Predicate::Gt {
                column: "age".into(),
                value: lit(Value::Int(30)),
            },
        ]),
    };
    assert_eq!(
        column_values(&run(&both, &source), "name"),
        vec![text("Carol")]
    );

    let either = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Or(vec![
            Predicate::Eq {
                column: "name".into(),
                value: lit(text("Alice")),
            },
            Predicate::Eq {
                column: "name".into(),
                value: lit(text("Bob")),
            },
        ]),
    };
    assert_eq!(run(&either, &source).row_count(), 2);

    let negated = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Not(Box::new(Predicate::Eq {
            column: "active".into(),
            value: lit(Value::Bool(true)),
        })),
    };
    assert_eq!(
        column_values(&run(&negated, &source), "name"),
        vec![text("Bob")]
    );
}

#[test]
fn in_matches_any_of_its_values() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::In {
            column: "age".into(),
            values: vec![lit(Value::Int(25)), lit(Value::Int(35))],
        },
    };
    assert_eq!(run(&plan, &Fixture::with_users()).row_count(), 2);
}

#[test]
fn a_projection_selects_and_orders_the_columns() {
    let plan = Plan::Project {
        input: Box::new(scan("users")),
        columns: vec!["name".into(), "age".into()],
    };
    let result = run(&plan, &Fixture::with_users());
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["name", "age"]);
    // Declared types come through rather than being inferred.
    assert_eq!(result.columns[1].ty, ValueType::Int);
}

#[test]
fn sorting_is_deterministic_even_where_values_tie() {
    let mut source = Fixture::default();
    // Three rows with the same sort value: only the tie-break makes the order
    // reproducible, and a query that reorders between runs is a bug.
    source.insert("t", "c", &[("v", Value::Int(1))]);
    source.insert("t", "a", &[("v", Value::Int(1))]);
    source.insert("t", "b", &[("v", Value::Int(1))]);

    let plan = Plan::Sort {
        input: Box::new(scan("t")),
        by: vec![("v".into(), SortOrder::Asc)],
    };
    let first = column_values(&run(&plan, &source), "_key");
    for _ in 0..5 {
        assert_eq!(column_values(&run(&plan, &source), "_key"), first);
    }
    assert_eq!(first, vec![text("a"), text("b"), text("c")]);
}

#[test]
fn sorting_puts_rows_missing_the_column_last() {
    let plan = Plan::Sort {
        input: Box::new(scan("users")),
        by: vec![("age".into(), SortOrder::Asc)],
    };
    let result = run(&plan, &Fixture::with_users());
    assert_eq!(
        column_values(&result, "name"),
        vec![text("Bob"), text("Alice"), text("Carol"), text("Dave")]
    );
}

#[test]
fn descending_sort_reverses_the_order() {
    let plan = Plan::Sort {
        input: Box::new(scan("users")),
        by: vec![("age".into(), SortOrder::Desc)],
    };
    let result = run(&plan, &Fixture::with_users());
    // Dave has no `age`. Missing sorts as greater than any value, so it leads
    // descending — Postgres's default, matched deliberately.
    assert_eq!(
        column_values(&result, "name"),
        vec![text("Dave"), text("Carol"), text("Alice"), text("Bob")]
    );
}

#[test]
fn null_ordering_matches_postgres_in_both_directions() {
    let source = Fixture::with_users();
    let sorted = |order| {
        let plan = Plan::Sort {
            input: Box::new(scan("users")),
            by: vec![("age".into(), order)],
        };
        column_values(&run(&plan, &source), "name")
    };
    // Nulls last ascending, first descending. A migrating user's queries must
    // not quietly reorder.
    assert_eq!(*sorted(SortOrder::Asc).last().expect("rows"), text("Dave"));
    assert_eq!(sorted(SortOrder::Desc)[0], text("Dave"));
}

#[test]
fn limit_and_offset_page_through_a_sorted_result() {
    let sorted = Plan::Sort {
        input: Box::new(scan("users")),
        by: vec![("_key".into(), SortOrder::Asc)],
    };
    let page = Plan::Limit {
        input: Box::new(sorted),
        count: 2,
        offset: 1,
    };
    let result = run(&page, &Fixture::with_users());
    assert_eq!(column_values(&result, "_key"), vec![text("2"), text("3")]);
}

#[test]
fn an_offset_past_the_end_yields_nothing_rather_than_erroring() {
    let plan = Plan::Limit {
        input: Box::new(scan("users")),
        count: 10,
        offset: 100,
    };
    assert!(run(&plan, &Fixture::with_users()).is_empty());
}

// ---- aggregates -------------------------------------------------------------

fn agg(func: AggFunc, column: Option<&str>, alias: &str) -> Aggregate {
    Aggregate {
        func,
        column: column.map(|c| c.to_string()),
        alias: alias.to_string(),
    }
}

#[test]
fn count_star_counts_rows_and_count_column_counts_values() {
    let source = Fixture::with_users();
    let plan = Plan::Aggregate {
        input: Box::new(scan("users")),
        group_by: Vec::new(),
        aggregates: vec![
            agg(AggFunc::Count, None, "rows"),
            agg(AggFunc::Count, Some("age"), "ages"),
        ],
    };
    let result = run(&plan, &source);
    // Dave has no age, so the two counts must differ. Smoothing that over would
    // silently misreport data completeness.
    assert_eq!(column_values(&result, "rows"), vec![Value::Int(4)]);
    assert_eq!(column_values(&result, "ages"), vec![Value::Int(3)]);
}

#[test]
fn sum_of_integers_stays_integral() {
    let plan = Plan::Aggregate {
        input: Box::new(scan("users")),
        group_by: Vec::new(),
        aggregates: vec![agg(AggFunc::Sum, Some("age"), "total")],
    };
    // 30 + 25 + 35, still an Int — summing counts must not silently acquire
    // floating-point error.
    assert_eq!(
        column_values(&run(&plan, &Fixture::with_users()), "total"),
        vec![Value::Int(90)]
    );
}

#[test]
fn min_max_and_avg_ignore_missing_values() {
    let source = Fixture::with_users();
    let plan = Plan::Aggregate {
        input: Box::new(scan("users")),
        group_by: Vec::new(),
        aggregates: vec![
            agg(AggFunc::Min, Some("age"), "youngest"),
            agg(AggFunc::Max, Some("age"), "oldest"),
            agg(AggFunc::Avg, Some("age"), "mean"),
        ],
    };
    let result = run(&plan, &source);
    assert_eq!(column_values(&result, "youngest"), vec![Value::Int(25)]);
    assert_eq!(column_values(&result, "oldest"), vec![Value::Int(35)]);
    assert_eq!(column_values(&result, "mean"), vec![Value::Float(30.0)]);
}

#[test]
fn a_global_aggregate_over_an_empty_table_still_returns_one_row() {
    let source = Fixture::default();
    let plan = Plan::Aggregate {
        input: Box::new(scan("empty")),
        group_by: Vec::new(),
        aggregates: vec![agg(AggFunc::Count, None, "n")],
    };
    // `SELECT COUNT(*)` over nothing must answer 0, not answer nothing.
    let result = run(&plan, &source);
    assert_eq!(result.row_count(), 1);
    assert_eq!(column_values(&result, "n"), vec![Value::Int(0)]);
}

#[test]
fn a_grouped_aggregate_over_an_empty_table_returns_no_groups() {
    let source = Fixture::default();
    let plan = Plan::Aggregate {
        input: Box::new(scan("empty")),
        group_by: vec!["kind".into()],
        aggregates: vec![agg(AggFunc::Count, None, "n")],
    };
    // Unlike a global aggregate: no rows means no groups to report.
    assert!(run(&plan, &source).is_empty());
}

#[test]
fn the_average_of_nothing_is_null_not_zero() {
    let source = Fixture::default();
    let plan = Plan::Aggregate {
        input: Box::new(scan("empty")),
        group_by: Vec::new(),
        aggregates: vec![agg(AggFunc::Avg, Some("x"), "mean")],
    };
    assert_eq!(
        column_values(&run(&plan, &source), "mean"),
        vec![Value::Null]
    );
}

#[test]
fn grouping_partitions_rows_and_is_ordered_deterministically() {
    let source = Fixture::with_users();
    let plan = Plan::Aggregate {
        input: Box::new(scan("users")),
        group_by: vec!["active".into()],
        aggregates: vec![agg(AggFunc::Count, None, "n")],
    };
    let result = run(&plan, &source);
    assert_eq!(result.row_count(), 2);

    let counts = column_values(&result, "n");
    assert!(counts.contains(&Value::Int(3)), "three active users");
    assert!(counts.contains(&Value::Int(1)), "one inactive user");

    // Group order must be stable across runs.
    for _ in 0..5 {
        assert_eq!(column_values(&run(&plan, &source), "n"), counts);
    }
}

// ---- parameters -------------------------------------------------------------

#[test]
fn a_bound_parameter_filters_without_ever_becoming_query_text() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Eq {
            column: "name".into(),
            value: Expr::Param {
                name: "who".into(),
                ty: Some(ValueType::Text),
            },
        },
    };
    let mut bindings = Bindings::new();
    // A value that would be catastrophic if interpolated. It is simply a Text
    // value here, and no code path turns it into query text.
    bindings.insert("who".into(), text("Alice'; DROP TABLE users; --"));

    let result = execute(&plan, &Fixture::with_users(), &bindings).expect("execute");
    assert!(
        result.is_empty(),
        "no user has that name, so nothing matches"
    );

    bindings.insert("who".into(), text("Alice"));
    let result = execute(&plan, &Fixture::with_users(), &bindings).expect("execute");
    assert_eq!(result.row_count(), 1);
}

#[test]
fn an_unbound_parameter_fails_before_any_row_is_read() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Eq {
            column: "name".into(),
            value: Expr::Param {
                name: "who".into(),
                ty: Some(ValueType::Text),
            },
        },
    };
    assert_eq!(
        execute(&plan, &Fixture::with_users(), &Bindings::new()),
        Err(ExecError::UnboundParameter { name: "who".into() })
    );
}

#[test]
fn a_parameter_bound_to_the_wrong_type_is_refused() {
    let plan = Plan::Filter {
        input: Box::new(scan("users")),
        predicate: Predicate::Eq {
            column: "age".into(),
            value: Expr::Param {
                name: "age".into(),
                ty: Some(ValueType::Int),
            },
        },
    };
    let mut bindings = Bindings::new();
    bindings.insert("age".into(), text("thirty"));

    assert!(matches!(
        execute(&plan, &Fixture::with_users(), &bindings),
        Err(ExecError::ParameterType { .. })
    ));
}

// ---- mixed types ------------------------------------------------------------

#[test]
fn comparing_across_incomparable_types_matches_nothing_rather_than_guessing() {
    let mut source = Fixture::default();
    source.insert("t", "1", &[("v", text("10"))]);
    source.insert("t", "2", &[("v", Value::Int(10))]);

    let plan = Plan::Filter {
        input: Box::new(scan("t")),
        predicate: Predicate::Eq {
            column: "v".into(),
            value: lit(Value::Int(10)),
        },
    };
    // The text "10" is not the integer 10. Coercing them would be exactly the
    // silent reinterpretation the data model forbids.
    let result = run(&plan, &source);
    assert_eq!(column_values(&result, "_key"), vec![text("2")]);
}

#[test]
fn integers_and_floats_compare_because_that_widening_is_lossless() {
    let mut source = Fixture::default();
    source.insert("t", "1", &[("v", Value::Int(10))]);
    source.insert("t", "2", &[("v", Value::Float(10.5))]);

    let plan = Plan::Filter {
        input: Box::new(scan("t")),
        predicate: Predicate::Gte {
            column: "v".into(),
            value: lit(Value::Float(10.0)),
        },
    };
    assert_eq!(run(&plan, &source).row_count(), 2);
}

#[test]
fn a_scalar_row_exposes_its_value_column() {
    let mut source = Fixture::default();
    source
        .tables
        .insert("counters".into(), vec![("hits".into(), Value::Int(42))]);

    let result = run(&scan("counters"), &source);
    assert_eq!(column_values(&result, "value"), vec![Value::Int(42)]);
    assert_eq!(column_values(&result, "_key"), vec![text("hits")]);
}

#[test]
fn results_encode_to_arrow_after_execution() {
    // The executor and the wire encoding have to agree on column types, so this
    // composes them rather than testing each in isolation.
    let result = run(&scan("users"), &Fixture::with_users());
    assert!(!result.to_arrow_ipc().expect("encode").is_empty());
}
