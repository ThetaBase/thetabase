//! Cost-model calibration (ROADMAP M3).
//!
//! The estimator's constants were chosen rather than measured, and the ROADMAP
//! said so: "the cost is an ordering signal, not a promise". This suite is what
//! turns that into a checked claim.
//!
//! What is asserted, and what deliberately is not:
//!
//! * **Ordering.** Where one plan really is cheaper than another, the estimate
//!   has to agree. This is what the optimizer consumes, and getting it backwards
//!   picks the worse plan every time.
//! * **Magnitude, loosely.** Estimated microseconds are compared against
//!   measured ones and required only to land within a wide band. A tight bound
//!   would fail on a noisy CI runner, and a suite that fails on a noisy
//!   neighbour teaches people to re-run it until it passes.
//! * **Not absolute latency.** `specs/09` owns the published numbers, measured
//!   on real hardware. Nothing here is a performance claim about the product.
//!
//! The constants live in `stats::cost`. When a measurement here drifts far
//! enough to fail, the fix is to re-measure and move the constant — not to
//! widen the band.

use std::collections::BTreeMap;
use std::time::Instant;

use theta_core::{RowSource, Value, ValueType};
use theta_query::exec::execute;
use theta_query::plan::{Expr, Literal, Plan, Predicate, SortOrder};
use theta_query::stats::{estimate, Statistics, TableStats};

struct Table {
    rows: Vec<(String, Value)>,
}

impl Table {
    fn new(count: usize) -> Self {
        let rows = (0..count)
            .map(|i| {
                let mut fields = BTreeMap::new();
                // Deliberately not `i`. With `n` ascending, sorting by it is
                // O(n) — Rust's sort detects the existing run — so "a scan is
                // cheaper than sorting the same rows" held by a margin smaller
                // than the measurement noise, and the pair failed about two
                // runs in three on a loaded host.
                //
                // A multiplicative permutation gives the sort real work while
                // keeping the *set* of values exactly `0..ROWS`, so every range
                // predicate below still matches the same number of rows. 7919
                // is coprime with 20_000, which is what makes it a permutation
                // rather than a collision.
                //
                // Deterministic, because a calibration test that shuffles
                // differently each run cannot be debugged when it does fail.
                fields.insert("n".to_string(), Value::Int(((i * 7919) % ROWS) as i64));
                fields.insert("bucket".to_string(), Value::Int((i % 10) as i64));
                fields.insert("name".to_string(), Value::Text(format!("row{i:06}")));
                (format!("r{i:06}"), Value::Map(fields))
            })
            .collect();
        Self { rows }
    }
}

impl RowSource for Table {
    fn scan(&self, _table: &str) -> Vec<(String, Value)> {
        self.rows.clone()
    }
    fn scan_up_to(&self, _table: &str, max: Option<usize>) -> Vec<(String, Value)> {
        match max {
            Some(max) => self.rows.iter().take(max).cloned().collect(),
            None => self.rows.clone(),
        }
    }
    fn row(&self, _table: &str, primary_key: &str) -> Option<Value> {
        self.rows
            .iter()
            .find(|(k, _)| k == primary_key)
            .map(|(_, v)| v.clone())
    }
    fn column_type(&self, _table: &str, _column: &str) -> Option<ValueType> {
        Some(ValueType::Int)
    }
}

const ROWS: usize = 20_000;

fn stats() -> Statistics {
    let mut stats = Statistics::default();
    stats.set_table(
        "t",
        TableStats {
            rows: ROWS as u64,
            ..Default::default()
        },
    );
    stats
}

fn scan() -> Plan {
    Plan::Scan { table: "t".into() }
}

fn gte(column: &str, value: i64) -> Predicate {
    Predicate::Gte {
        column: column.into(),
        value: Expr::Literal(Literal(Value::Int(value))),
    }
}

/// Measured microseconds, taking the best of several runs.
///
/// Best-of rather than mean: the thing being measured is how much work the plan
/// does, and every source of noise on a shared runner only ever adds. The
/// fastest run is the one least contaminated by something else.
fn measure(plan: &Plan, source: &Table) -> f64 {
    // Warm the allocator and any lazily-built state so the first run is not
    // measuring setup.
    let _ = execute(plan, source, &Default::default()).expect("execute");

    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let result = execute(plan, source, &Default::default()).expect("execute");
        let elapsed = start.elapsed().as_secs_f64() * 1e6;
        // Keep the result alive so nothing is optimized away.
        std::hint::black_box(result.rows.len());
        best = best.min(elapsed);
    }
    best
}

fn estimated_us(plan: &Plan) -> f64 {
    // `estimate` reports whole milliseconds, rounded up, which is too coarse to
    // calibrate against. The ratio checks below use the row counts and the
    // relative ordering, which are exact.
    estimate(plan, &stats()).cost_ms as f64 * 1000.0
}

#[test]
fn the_estimate_orders_plans_the_way_the_engine_does() {
    let source = Table::new(ROWS);

    // Each pair is (cheaper, dearer) by construction. If the estimator ever
    // disagrees with the clock on one of these, the optimizer is choosing
    // backwards.
    let pairs: Vec<(&str, Plan, Plan)> = vec![
        (
            "a limit is cheaper than the scan it limits",
            Plan::Limit {
                input: Box::new(scan()),
                count: 10,
                offset: 0,
            },
            scan(),
        ),
        (
            "a scan is cheaper than sorting the same rows",
            scan(),
            Plan::Sort {
                input: Box::new(scan()),
                by: vec![("n".to_string(), SortOrder::Desc)],
            },
        ),
        (
            "a limited scan is cheaper than a filtered one over the whole table",
            Plan::Limit {
                input: Box::new(scan()),
                count: 5,
                offset: 0,
            },
            Plan::Filter {
                input: Box::new(scan()),
                predicate: gte("n", 0),
            },
        ),
    ];

    for (what, cheaper, dearer) in pairs {
        let (cheap_est, dear_est) = (estimated_us(&cheaper), estimated_us(&dearer));
        let (cheap_real, dear_real) = (measure(&cheaper, &source), measure(&dearer, &source));

        assert!(
            cheap_real < dear_real,
            "{what}: the premise is wrong - measured {cheap_real:.0}us vs \
             {dear_real:.0}us, so this pair is not testing what it claims"
        );
        assert!(
            cheap_est <= dear_est,
            "{what}: the estimator disagrees with the clock. Estimated \
             {cheap_est:.0}us vs {dear_est:.0}us, measured {cheap_real:.0}us vs \
             {dear_real:.0}us"
        );
    }
}

#[test]
fn a_limit_is_estimated_as_cheaper_now_that_it_actually_is() {
    // This is the entry the ROADMAP called out: while the engine materialized
    // everything under a LIMIT, the estimator deliberately claimed no saving.
    // The engine now stops early, so the estimate has to follow - an estimator
    // that under-claims sends the optimizer past the plan it should pick.
    let full = estimate(&scan(), &stats());
    let limited = estimate(
        &Plan::Limit {
            input: Box::new(scan()),
            count: 10,
            offset: 0,
        },
        &stats(),
    );

    assert_eq!(limited.rows, 10);
    assert!(
        limited.cost_ms < full.cost_ms,
        "LIMIT 10 over {ROWS} rows still estimates as {}ms against the full \
         scan's {}ms",
        limited.cost_ms,
        full.cost_ms
    );
}

#[test]
fn a_sort_under_a_limit_is_not_discounted() {
    // The sort still reads everything, so the estimate must not pretend
    // otherwise. Over-claiming a saving is the failure mode that makes an
    // optimizer pick a plan that is slower than the one it rejected.
    let sorted = Plan::Sort {
        input: Box::new(scan()),
        by: vec![("n".to_string(), SortOrder::Asc)],
    };
    let bare = estimate(&sorted, &stats());
    let limited = estimate(
        &Plan::Limit {
            input: Box::new(sorted.clone()),
            count: 1,
            offset: 0,
        },
        &stats(),
    );

    assert!(
        limited.cost_ms >= bare.cost_ms,
        "a LIMIT above a SORT was discounted: {}ms against {}ms",
        limited.cost_ms,
        bare.cost_ms
    );
}

#[test]
fn an_aggregate_under_a_limit_is_not_discounted() {
    let aggregated = Plan::Aggregate {
        input: Box::new(scan()),
        group_by: Vec::new(),
        aggregates: vec![theta_query::plan::Aggregate {
            func: theta_query::plan::AggFunc::Count,
            column: None,
            alias: "c".into(),
        }],
    };
    let bare = estimate(&aggregated, &stats());
    let limited = estimate(
        &Plan::Limit {
            input: Box::new(aggregated.clone()),
            count: 1,
            offset: 0,
        },
        &stats(),
    );

    assert!(
        limited.cost_ms >= bare.cost_ms,
        "a LIMIT above an aggregate was discounted"
    );
}

/// Is this a build worth timing?
///
/// A debug build measures rustc's bounds checks, not the engine — the same
/// reason `make sla` runs release-only. Rather than reporting a green that
/// measured the wrong thing, the calibration says what it skipped and why.
fn timing_is_meaningful() -> bool {
    if cfg!(debug_assertions) {
        eprintln!(
            "skipping calibration: debug build. Run `cargo test --release -p \
             theta-query --test cost_calibration`."
        );
        return false;
    }
    true
}

/// Per-row cost, as the slope between two table sizes.
///
/// Differencing two whole-plan timings is the obvious approach and a bad one:
/// the quantity wanted is small, the fixed costs around it are not, and the
/// difference of two noisy numbers is noisier than either. A slope cancels
/// every term that does not scale with rows — node overhead, allocation,
/// result materialization — and leaves the one that does.
fn per_row_slope(build: impl Fn(usize) -> (Plan, Table), small: usize, large: usize) -> f64 {
    let (plan_small, source_small) = build(small);
    let (plan_large, source_large) = build(large);
    let t_small = measure(&plan_small, &source_small);
    let t_large = measure(&plan_large, &source_large);
    (t_large - t_small) / (large - small) as f64
}

/// The band each constant has to land in.
///
/// Wide on purpose. The failure worth catching is a constant wrong by two
/// orders of magnitude, which makes the optimizer's ordering meaningless; a
/// constant that drifts 3x with the machine is doing its job. Tightening this
/// would buy a suite that fails on a busy CI runner, and a gate that fails on a
/// noisy neighbour teaches people to re-run it until it passes.
const BAND: std::ops::RangeInclusive<f64> = 0.1..=10.0;

#[test]
fn the_per_row_scan_constant_is_within_an_order_of_magnitude_of_reality() {
    if !timing_is_meaningful() {
        return;
    }

    let per_row = per_row_slope(|rows| (scan(), Table::new(rows)), 10_000, 40_000);
    let claimed = theta_query::stats::cost::PER_ROW_SCAN_US;
    let ratio = per_row / claimed;
    eprintln!("per-row scan: claimed {claimed}us, measured {per_row:.4}us ({ratio:.2}x)");

    assert!(
        BAND.contains(&ratio),
        "PER_ROW_SCAN_US claims {claimed}us/row; measured {per_row:.4}us/row \
         ({ratio:.1}x off). Re-measure and move the constant rather than \
         widening this band."
    );
}

#[test]
fn the_predicate_constant_is_within_an_order_of_magnitude_of_reality() {
    if !timing_is_meaningful() {
        return;
    }

    // Slope across the *number of predicates*, not the number of rows: one
    // conjunct against twenty isolates the per-evaluation cost from everything
    // the scan beneath does.
    const EXTRA: usize = 19;
    let build = |conjuncts: usize| {
        let plan = Plan::Filter {
            input: Box::new(scan()),
            predicate: Predicate::And((0..conjuncts).map(|_| gte("n", 0)).collect()),
        };
        (plan, Table::new(ROWS))
    };
    let (one, source_one) = build(1);
    let (many, source_many) = build(1 + EXTRA);
    let t_one = measure(&one, &source_one);
    let t_many = measure(&many, &source_many);

    let per_row = ((t_many - t_one) / (EXTRA * ROWS) as f64).max(0.0);
    let claimed = theta_query::stats::cost::PER_ROW_PREDICATE_US;
    let ratio = per_row / claimed;
    eprintln!("per-row predicate: claimed {claimed}us, measured {per_row:.4}us ({ratio:.2}x)");

    assert!(
        BAND.contains(&ratio),
        "PER_ROW_PREDICATE_US claims {claimed}us/row; measured \
         {per_row:.4}us/row ({ratio:.1}x off)"
    );
}
