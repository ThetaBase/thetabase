//! Index-nested-loop join.
//!
//! # What this is and is not
//!
//! For each row of the outer input, the inner plan is evaluated with the outer
//! row's join column bound. That is the whole algorithm — there is no hash
//! build, no join reordering, and no choice of strategy, because choosing would
//! need row counts the planner does not have and guessing them is how a query
//! optimiser starts being wrong in ways nobody can predict.
//!
//! So these tests are about *semantics*, not about speed: that every match is
//! produced, that a non-matching outer row produces nothing rather than nulls,
//! and that `LIMIT` above a join counts joined rows rather than outer ones.
//! The last is the one worth having — an implementation that budgeted the outer
//! side would silently truncate a result that had not reached the limit yet.

use std::collections::BTreeMap;

use theta_core::{RowSource, Value};
use theta_query::exec::{execute, Bindings};
use theta_query::plan::{Expr, Plan};

/// Two tables in memory: invoices, and the payments against them.
#[derive(Default)]
struct Tables {
    rows: BTreeMap<String, Vec<(String, Value)>>,
}

impl Tables {
    fn with(mut self, table: &str, rows: Vec<(&str, Value)>) -> Self {
        self.rows.insert(
            table.to_string(),
            rows.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        );
        self
    }
}

impl RowSource for Tables {
    fn scan(&self, table: &str) -> Vec<(String, Value)> {
        self.rows.get(table).cloned().unwrap_or_default()
    }

    fn row(&self, table: &str, key: &str) -> Option<Value> {
        self.rows
            .get(table)?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    // No declared schema in these fixtures: the join's behaviour does not
    // depend on one, and inventing types here would test the fixture.
    fn column_type(&self, _table: &str, _column: &str) -> Option<theta_core::ValueType> {
        None
    }
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// Invoices keyed by id, each naming the unit it bills.
fn tables() -> Tables {
    Tables::default()
        .with(
            "invoices",
            vec![
                // The unmatched invoice is deliberately **first**.
                //
                // With it last, a `LIMIT 2` over three invoices reads the two
                // that match before it ever reaches the one that does not, and
                // an implementation that wrongly budgets the outer side passes
                // anyway. Planting that bug is how this ordering was found: the
                // test was green against it until the row that produces nothing
                // came first.
                (
                    "inv-3",
                    map(&[("unit", text("u99")), ("amount", Value::Int(100))]),
                ),
                (
                    "inv-1",
                    map(&[("unit", text("u14")), ("amount", Value::Int(500))]),
                ),
                (
                    "inv-2",
                    map(&[("unit", text("u15")), ("amount", Value::Int(750))]),
                ),
            ],
        )
        .with(
            "units",
            vec![
                ("u14", map(&[("tenant", text("Acme"))])),
                ("u15", map(&[("tenant", text("Borden"))])),
            ],
        )
}

/// `invoices JOIN units ON invoices.unit = units.<key>`.
fn join(limit: Option<u64>) -> Plan {
    let inner = Plan::PointLookup {
        table: "units".into(),
        key: Box::new(Expr::Param {
            name: "unit_key".into(),
            ty: None,
        }),
    };
    let joined = Plan::Join {
        outer: Box::new(Plan::Scan {
            table: "invoices".into(),
        }),
        inner: Box::new(inner),
        outer_column: "unit".into(),
        binds: "unit_key".into(),
    };
    match limit {
        None => joined,
        Some(count) => Plan::Limit {
            input: Box::new(joined),
            count,
            offset: 0,
        },
    }
}

/// Rows as name→value maps, which is what the assertions are about.
///
/// A `ResultSet` is columns plus positional rows; zipping them here keeps every
/// test below readable and means a column-ordering change shows up as one
/// failure rather than six.
fn rows_of(result: theta_query::result::ResultSet) -> Vec<BTreeMap<String, Value>> {
    let names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    result
        .rows
        .into_iter()
        .map(|row| {
            names
                .iter()
                .cloned()
                .zip(row)
                .filter(|(_, v)| !matches!(v, Value::Null))
                .collect()
        })
        .collect()
}

fn run(plan: &Plan) -> Vec<BTreeMap<String, Value>> {
    rows_of(execute(plan, &tables(), &Bindings::new()).expect("executes"))
}

#[test]
fn a_join_pairs_each_outer_row_with_its_match() {
    let rows = run(&join(None));

    assert_eq!(
        rows.len(),
        2,
        "expected one row per invoice that has a unit, got {rows:#?}"
    );

    // Both sides' fields are present on one row. A join that returned only the
    // outer side would be a filter with extra steps.
    let tenants: Vec<&Value> = rows.iter().filter_map(|r| r.get("tenant")).collect();
    assert_eq!(
        tenants,
        vec![&text("Acme"), &text("Borden")],
        "the inner side's fields did not reach the joined rows"
    );
    assert!(
        rows[0].contains_key("amount"),
        "the outer side's fields were dropped"
    );
}

#[test]
fn an_outer_row_with_no_match_produces_nothing() {
    let rows = run(&join(None));

    // `inv-3` names `u99`, which does not exist. An inner join drops it; it
    // must not appear with null columns, which is what a caller reading the
    // result would take as "this unit exists and has no tenant".
    assert!(
        !rows
            .iter()
            .any(|r| r.get("amount") == Some(&Value::Int(100))),
        "the unmatched invoice came through anyway: {rows:#?}"
    );
}

#[test]
fn a_limit_above_a_join_counts_joined_rows_not_outer_rows() {
    // The bug this rules out: budgeting the outer side. Three invoices, two of
    // which match. Limit 2 must return two joined rows — an implementation that
    // passed the budget down would read two *invoices*, one of which is the
    // unmatched one, and return a single row while claiming to have honoured a
    // limit of two.
    let rows = run(&join(Some(2)));

    assert_eq!(
        rows.len(),
        2,
        "LIMIT 2 over a join returned {} rows; the budget reached the outer \
         side and truncated before the matches did",
        rows.len()
    );
}

#[test]
fn the_join_binding_is_not_reported_as_a_parameter_the_caller_must_supply() {
    // The join supplies it, one row at a time. Reporting it would make every
    // joined query look as though it were missing a parameter, and a client
    // that refused to run until every reported parameter was bound could never
    // run one.
    let plan = join(None);
    let params: Vec<&str> = plan.params().into_iter().map(|(n, _)| n).collect();
    assert!(
        !params.contains(&"unit_key"),
        "the join's own binding is reported as a caller parameter: {params:?}"
    );
}

#[test]
fn explain_shows_both_sides_and_says_the_inner_one_repeats() {
    let explained = theta_query::explain::Explain::of(&join(None), &Default::default());
    let rendered = format!("{explained:?}");

    assert!(
        rendered.contains("IndexNestedLoopJoin"),
        "EXPLAIN does not name the join: {rendered}"
    );
    // The inner side must appear. A plan showing only the outer branch hides
    // the multiplication, which is the one thing a reader of a nested loop has
    // to see.
    assert!(
        rendered.contains("PointLookup"),
        "EXPLAIN hides the inner side, so the cost of repeating it is invisible: \
         {rendered}"
    );
}

#[test]
fn two_columns_of_one_name_resolve_to_the_inner_side_rather_than_vanishing() {
    let tables = Tables::default()
        .with(
            "a",
            vec![("k1", map(&[("id", text("x")), ("name", text("outer"))]))],
        )
        .with("b", vec![("x", map(&[("name", text("inner"))]))]);

    let plan = Plan::Join {
        outer: Box::new(Plan::Scan { table: "a".into() }),
        inner: Box::new(Plan::PointLookup {
            table: "b".into(),
            key: Box::new(Expr::Param {
                name: "k".into(),
                ty: None,
            }),
        }),
        outer_column: "id".into(),
        binds: "k".into(),
    };

    let rows = rows_of(execute(&plan, &tables, &Bindings::new()).expect("executes"));
    let fields = &rows[0];

    // Stated rather than left to chance. Either answer is defensible; silently
    // producing neither is not, and that is what an implementation that built
    // the map in the wrong order would do.
    assert_eq!(
        fields.get("name"),
        Some(&text("inner")),
        "a name collision lost both sides instead of resolving to one"
    );
}
