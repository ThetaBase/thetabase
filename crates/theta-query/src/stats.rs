//! Table statistics and cost estimation.
//!
//! EXPLAIN's numbers are what a human or the Safety Layer reasons about before
//! letting a query run (`02-api-wire-protocol.md` §4), so they have to be
//! *estimates* — honestly labelled, derived from something real — rather than
//! placeholders that look authoritative.
//!
//! The model here is deliberately simple and its assumptions are written down.
//! A cost model whose assumptions are hidden is worse than a crude one whose
//! assumptions are visible, because nobody can tell when it stops applying.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use theta_core::Value;

use crate::plan::{Expr, Plan, Predicate};

/// What is known about one table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableStats {
    pub rows: u64,
    /// Distinct values per column, used to estimate how selective an equality
    /// predicate is. Absent means unknown, and the estimator says so rather
    /// than inventing a number.
    pub distinct: BTreeMap<String, u64>,
    /// Indexes available on this table, by the column they cover.
    pub indexes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Statistics {
    tables: BTreeMap<String, TableStats>,
}

impl Statistics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn table(&self, name: &str) -> Option<&TableStats> {
        self.tables.get(name)
    }

    pub fn set_table(&mut self, name: impl Into<String>, stats: TableStats) {
        self.tables.insert(name.into(), stats);
    }

    /// Recompute statistics for one table from its rows.
    ///
    /// Exact rather than sampled: at the scale a single project's materialized
    /// view holds, a full pass is cheap and an exact answer removes a whole
    /// class of "the estimate was wrong" debugging.
    pub fn analyze(&mut self, table: &str, rows: &[(String, Value)]) {
        let mut distinct: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();

        for (_, row) in rows {
            for column in theta_core::address::columns_of(row) {
                if let Some(value) = theta_core::address::column(row, "", &column) {
                    distinct.entry(column).or_default().insert(encode(&value));
                }
            }
        }

        let existing_indexes = self
            .tables
            .get(table)
            .map(|t| t.indexes.clone())
            .unwrap_or_default();

        self.tables.insert(
            table.to_string(),
            TableStats {
                rows: rows.len() as u64,
                distinct: distinct
                    .into_iter()
                    .map(|(k, v)| (k, v.len() as u64))
                    .collect(),
                indexes: existing_indexes,
            },
        );
    }

    pub fn declare_index(&mut self, table: &str, column: &str, index: &str) {
        self.tables
            .entry(table.to_string())
            .or_default()
            .indexes
            .insert(column.to_string(), index.to_string());
    }
}

fn encode(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Cost model constants.
///
/// Rough, and deliberately so. They exist to make the *shape* of a plan's cost
/// visible — that a scan of a million rows is expensive and a point lookup is
/// not — rather than to predict milliseconds.
///
/// They are checked against a clock by `tests/cost_calibration.rs`, which
/// asserts two different things: that the *ordering* the optimizer consumes
/// matches which plan is really faster, and that each per-row constant is
/// within an order of magnitude of measurement. The band is wide on purpose —
/// the failure worth catching is a constant wrong by 100x, which makes the
/// ordering meaningless, not one that varies with the machine. When a
/// measurement drifts out of the band, re-measure and move the constant rather
/// than widening it.
///
/// Public because the suite that justifies them has to name them.
pub mod cost {
    /// Microseconds to produce one row from a scan.
    ///
    /// Measured, not chosen: ~0.96us/row in release on the calibration suite's
    /// slope, against the 0.05 originally guessed. A factor of twenty, and in a
    /// direction that matters — it had a scan costing only 2.5x a predicate
    /// evaluation when it really costs about thirty times as much, which is why
    /// the ordering it produced was not to be trusted.
    ///
    /// The gap is where the cost actually is: a scan clones a whole row map per
    /// row, while a predicate reads one field out of one. Anything that makes
    /// rows cheaper to produce — a borrowed row, a lazier source — belongs
    /// here, and this constant should follow it down.
    pub const PER_ROW_SCAN_US: f64 = 1.0;
    /// Microseconds to evaluate one predicate against one row.
    ///
    /// Measured at ~0.032us/row against 0.02 claimed, which is close enough to
    /// leave alone: within the band, and the difference is smaller than the
    /// spread between machines.
    pub const PER_ROW_PREDICATE_US: f64 = 0.02;
    /// Microseconds of fixed overhead per plan node.
    pub const NODE_OVERHEAD_US: f64 = 5.0;
    /// A point lookup is a hash probe, independent of table size.
    pub const POINT_LOOKUP_US: f64 = 1.0;
    /// Multiplier for the comparison work in a sort, per row per log-row.
    pub const SORT_US: f64 = 0.03;
}

/// What the estimator concluded about a plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub rows: u64,
    pub cost_ms: u32,
    /// False when no statistics existed for the table, so the numbers above are
    /// a default rather than a measurement. EXPLAIN reports this, because an
    /// estimate nobody can tell is a guess is worse than no estimate.
    pub from_statistics: bool,
}

/// Rows assumed for a table with no statistics. Small enough that an unanalyzed
/// table does not look alarming, and flagged as unmeasured either way.
const UNKNOWN_TABLE_ROWS: u64 = 1_000;

/// Selectivity assumed for an equality predicate on a column with no distinct
/// count. The textbook default.
const DEFAULT_EQ_SELECTIVITY: f64 = 0.1;

/// Selectivity assumed for a range predicate — a third of rows, the usual
/// rule of thumb.
const DEFAULT_RANGE_SELECTIVITY: f64 = 0.33;

/// Estimate a plan's output size and cost.
pub fn estimate(plan: &Plan, stats: &Statistics) -> Estimate {
    let table = plan.source_table();
    let known = stats.table(table);
    let base_rows = known.map(|t| t.rows).unwrap_or(UNKNOWN_TABLE_ROWS);

    let (rows, cost_us) = walk(plan, stats, base_rows, None);

    Estimate {
        rows,
        // Rounded up, so a sub-millisecond plan reports 1 rather than 0 — a
        // zero cost reads as "free", which nothing is.
        cost_ms: (cost_us / 1000.0).ceil().max(1.0) as u32,
        from_statistics: known.is_some(),
    }
}

/// Returns (estimated rows out, estimated microseconds).
///
/// `budget` mirrors the executor's row budget exactly — the two must agree, or
/// EXPLAIN describes a plan other than the one that runs. The rule there and
/// here is the same: a budget travels down through operators that cannot change
/// which rows come out on top, and stops at a `Sort` or an `Aggregate`, which
/// have to see everything before they know their own first row.
fn walk(plan: &Plan, stats: &Statistics, base_rows: u64, budget: Option<u64>) -> (u64, f64) {
    match plan {
        Plan::Scan { .. } => {
            // Reads only up to the budget, because `RowSource::scan_up_to` does.
            let rows = budget.map_or(base_rows, |b| base_rows.min(b));
            (
                rows,
                cost::NODE_OVERHEAD_US + rows as f64 * cost::PER_ROW_SCAN_US,
            )
        }

        // Independent of table size: that is the whole point of a point lookup,
        // and the estimate should make the difference obvious.
        Plan::PointLookup { .. } => (1, cost::NODE_OVERHEAD_US + cost::POINT_LOOKUP_US),

        // No budget below an index scan: the filter above the candidates
        // decides which survive, so stopping early could stop before the
        // matches.
        Plan::IndexScan {
            table, predicate, ..
        } => {
            let selectivity = selectivity_of(predicate, stats, table);
            let out = ((base_rows as f64 * selectivity).ceil() as u64).max(1);
            // An index scan touches only matching rows, so it is charged for
            // what it returns rather than for what it skipped.
            (
                out,
                cost::NODE_OVERHEAD_US + out as f64 * cost::PER_ROW_SCAN_US,
            )
        }

        Plan::Filter { input, predicate } => {
            // The input gets no budget: how far it must read to yield `budget`
            // survivors is exactly what the predicate decides.
            let (rows_in, cost_in) = walk(input, stats, base_rows, None);
            let table = input.source_table();
            let selectivity = selectivity_of(predicate, stats, table);
            let unbounded_out = (rows_in as f64 * selectivity).ceil() as u64;
            let out = budget.map_or(unbounded_out, |b| unbounded_out.min(b));

            // The filter stops once it has enough, so it examines roughly
            // `wanted / selectivity` rows rather than all of them. Selectivity
            // is an estimate, so this is too — but it is the right shape, and
            // charging for every input row would now overstate a plan the
            // engine really does cut short.
            let examined = match budget {
                Some(b) if selectivity > 0.0 => {
                    ((b as f64 / selectivity).ceil() as u64).min(rows_in)
                }
                _ => rows_in,
            };
            (
                out,
                cost_in + cost::NODE_OVERHEAD_US + examined as f64 * cost::PER_ROW_PREDICATE_US,
            )
        }

        Plan::Project { input, .. } => {
            let (rows, cost) = walk(input, stats, base_rows, budget);
            (rows, cost + cost::NODE_OVERHEAD_US)
        }

        // A sort refuses the budget: which rows come first is what it is
        // about to decide.
        Plan::Sort { input, .. } => {
            let (rows, cost_in) = walk(input, stats, base_rows, None);
            let n = rows.max(1) as f64;
            (
                rows,
                cost_in + cost::NODE_OVERHEAD_US + n * n.log2().max(1.0) * cost::SORT_US,
            )
        }

        Plan::Limit {
            input,
            count,
            offset,
        } => {
            // The rows skipped plus the rows kept, narrowed by any budget
            // already in force. This is the saving the engine now actually
            // makes - operators below a limit stop early - so EXPLAIN reports
            // it. While they did not, this deliberately claimed nothing.
            let needed = offset.saturating_add(*count);
            let inner = Some(budget.map_or(needed, |outer| needed.min(outer)));
            let (rows, cost) = walk(input, stats, base_rows, inner);
            (
                rows.saturating_sub(*offset).min(*count),
                cost + cost::NODE_OVERHEAD_US,
            )
        }

        Plan::Aggregate {
            input, group_by, ..
        } => {
            // An aggregate over a budgeted input would aggregate a subset.
            let (rows_in, cost_in) = walk(input, stats, base_rows, None);
            let out = match group_by.is_empty() {
                // A global aggregate always produces exactly one row.
                true => 1,
                false => estimate_groups(group_by, stats, input.source_table(), rows_in),
            };
            (
                out,
                cost_in + cost::NODE_OVERHEAD_US + rows_in as f64 * cost::PER_ROW_PREDICATE_US,
            )
        }
    }
}

/// How many groups a GROUP BY produces.
fn estimate_groups(group_by: &[String], stats: &Statistics, table: &str, rows_in: u64) -> u64 {
    let Some(table_stats) = stats.table(table) else {
        // Without distinct counts, assume grouping reduces the row count by an
        // order of magnitude. Flagged as unmeasured by `from_statistics`.
        return (rows_in / 10).max(1);
    };

    // The product of each column's cardinality, capped at the input size: there
    // cannot be more groups than rows.
    let product = group_by
        .iter()
        .map(|column| table_stats.distinct.get(column).copied().unwrap_or(10))
        .fold(1u64, |acc, n| acc.saturating_mul(n));

    product.min(rows_in.max(1)).max(1)
}

/// Fraction of rows a predicate is expected to keep, in `0.0..=1.0`.
fn selectivity_of(predicate: &Predicate, stats: &Statistics, table: &str) -> f64 {
    match predicate {
        Predicate::True => 1.0,

        Predicate::Eq { column, .. } => match distinct_count(stats, table, column) {
            // Uniformity assumption: each distinct value covers an equal share.
            // Wrong for skewed data, and the honest fix is histograms, which is
            // more machinery than the current row counts justify.
            Some(n) if n > 0 => 1.0 / n as f64,
            _ => DEFAULT_EQ_SELECTIVITY,
        },

        Predicate::Ne { column, .. } => {
            1.0 - selectivity_of(
                &Predicate::Eq {
                    column: column.clone(),
                    value: dummy(),
                },
                stats,
                table,
            )
        }

        Predicate::Lt { .. }
        | Predicate::Lte { .. }
        | Predicate::Gt { .. }
        | Predicate::Gte { .. } => DEFAULT_RANGE_SELECTIVITY,

        Predicate::In { column, values } => {
            let each = match distinct_count(stats, table, column) {
                Some(n) if n > 0 => 1.0 / n as f64,
                _ => DEFAULT_EQ_SELECTIVITY,
            };
            (each * values.len() as f64).min(1.0)
        }

        Predicate::IsNull { .. } => DEFAULT_EQ_SELECTIVITY,

        // Independence assumption: predicates are treated as uncorrelated. Real
        // data correlates constantly (`country = 'JP' AND language = 'ja'`), so
        // this under-estimates conjunctions. Stated rather than hidden.
        Predicate::And(preds) => preds
            .iter()
            .map(|p| selectivity_of(p, stats, table))
            .fold(1.0, |acc, s| acc * s),

        Predicate::Or(preds) => {
            let none_match = preds
                .iter()
                .map(|p| 1.0 - selectivity_of(p, stats, table))
                .fold(1.0, |acc, s| acc * s);
            1.0 - none_match
        }

        Predicate::Not(inner) => 1.0 - selectivity_of(inner, stats, table),
    }
}

fn distinct_count(stats: &Statistics, table: &str, column: &str) -> Option<u64> {
    stats.table(table)?.distinct.get(column).copied()
}

/// A placeholder operand, used only where selectivity depends on the column
/// rather than the value.
fn dummy() -> Expr {
    Expr::Literal(crate::plan::Literal(Value::Null))
}

#[cfg(test)]
mod tests {
    use crate::builder::{col, table as tbl};

    use super::*;

    fn stats_with(rows: u64, distinct: &[(&str, u64)]) -> Statistics {
        let mut stats = Statistics::new();
        stats.set_table(
            "users",
            TableStats {
                rows,
                distinct: distinct.iter().map(|(c, n)| (c.to_string(), *n)).collect(),
                indexes: BTreeMap::new(),
            },
        );
        stats
    }

    #[test]
    fn a_scan_is_estimated_at_the_table_size() {
        let stats = stats_with(10_000, &[]);
        let estimate = estimate(&tbl("users").build(), &stats);
        assert_eq!(estimate.rows, 10_000);
        assert!(estimate.from_statistics);
    }

    #[test]
    fn an_unanalyzed_table_is_flagged_rather_than_silently_guessed() {
        let estimate = estimate(&tbl("users").build(), &Statistics::new());
        assert_eq!(estimate.rows, UNKNOWN_TABLE_ROWS);
        assert!(
            !estimate.from_statistics,
            "an estimate nobody can tell is a guess is worse than no estimate"
        );
    }

    #[test]
    fn a_point_lookup_costs_the_same_whatever_the_table_size() {
        let small = estimate(&tbl("users").key("1").build(), &stats_with(10, &[]));
        let huge = estimate(&tbl("users").key("1").build(), &stats_with(50_000_000, &[]));
        assert_eq!(small.rows, 1);
        assert_eq!(huge.rows, 1);
        assert_eq!(small.cost_ms, huge.cost_ms);
    }

    #[test]
    fn equality_on_a_high_cardinality_column_is_more_selective() {
        let stats = stats_with(10_000, &[("email", 10_000), ("plan", 3)]);

        let by_email = estimate(
            &tbl("users").filter(col("email").eq("a@b.c")).build(),
            &stats,
        );
        let by_plan = estimate(&tbl("users").filter(col("plan").eq("pro")).build(), &stats);

        assert_eq!(by_email.rows, 1, "a unique column matches one row");
        assert!(
            by_plan.rows > by_email.rows,
            "a three-valued column matches many"
        );
    }

    #[test]
    fn a_scan_of_a_large_table_costs_more_than_a_scan_of_a_small_one() {
        let small = estimate(&tbl("users").build(), &stats_with(100, &[]));
        let large = estimate(&tbl("users").build(), &stats_with(5_000_000, &[]));
        assert!(
            large.cost_ms > small.cost_ms,
            "cost must reflect the shape of the work"
        );
    }

    #[test]
    fn conjunctions_narrow_and_disjunctions_widen() {
        let stats = stats_with(10_000, &[("a", 10), ("b", 10)]);

        let and = estimate(
            &tbl("users")
                .filter(col("a").eq(1i64))
                .filter(col("b").eq(1i64))
                .build(),
            &stats,
        );
        let single = estimate(&tbl("users").filter(col("a").eq(1i64)).build(), &stats);

        assert!(and.rows < single.rows, "AND must narrow");
        assert!(and.rows >= 1, "an estimate of zero rows is never useful");
    }

    #[test]
    fn a_global_aggregate_estimates_exactly_one_row() {
        let plan = tbl("users")
            .aggregate(Vec::<String>::new(), [crate::builder::count()])
            .build();
        assert_eq!(estimate(&plan, &stats_with(10_000, &[])).rows, 1);
    }

    #[test]
    fn grouping_estimates_from_the_columns_cardinality() {
        let stats = stats_with(10_000, &[("plan", 3)]);
        let plan = tbl("users")
            .aggregate(["plan"], [crate::builder::count()])
            .build();
        assert_eq!(estimate(&plan, &stats).rows, 3);
    }

    #[test]
    fn there_can_never_be_more_groups_than_rows() {
        let stats = stats_with(5, &[("a", 100), ("b", 100)]);
        let plan = tbl("users")
            .aggregate(["a", "b"], [crate::builder::count()])
            .build();
        assert!(estimate(&plan, &stats).rows <= 5);
    }

    #[test]
    fn a_limit_bounds_the_estimated_output() {
        let stats = stats_with(10_000, &[]);
        assert_eq!(estimate(&tbl("users").limit(10).build(), &stats).rows, 10);
    }

    #[test]
    fn sorting_costs_more_than_not_sorting() {
        let stats = stats_with(100_000, &[]);
        let unsorted = estimate(&tbl("users").build(), &stats);
        let sorted = estimate(&tbl("users").order_by("name").build(), &stats);
        assert!(sorted.cost_ms > unsorted.cost_ms);
    }

    #[test]
    fn analyze_counts_rows_and_distinct_values() {
        let rows: Vec<(String, Value)> = (0..100)
            .map(|i| {
                (
                    i.to_string(),
                    Value::Map(BTreeMap::from([
                        ("id".to_string(), Value::Int(i)),
                        // Only two distinct values across a hundred rows.
                        (
                            "plan".to_string(),
                            Value::Text(if i % 2 == 0 { "free" } else { "pro" }.into()),
                        ),
                    ])),
                )
            })
            .collect();

        let mut stats = Statistics::new();
        stats.analyze("users", &rows);

        let table = stats.table("users").expect("analyzed");
        assert_eq!(table.rows, 100);
        assert_eq!(table.distinct.get("id"), Some(&100));
        assert_eq!(table.distinct.get("plan"), Some(&2));
    }

    #[test]
    fn analyze_preserves_declared_indexes() {
        let mut stats = Statistics::new();
        stats.declare_index("users", "email", "idx_email");
        stats.analyze("users", &[]);
        // Re-analyzing must not drop what indexes exist; they are not derived
        // from the rows.
        assert_eq!(
            stats.table("users").expect("analyzed").indexes.get("email"),
            Some(&"idx_email".to_string())
        );
    }

    #[test]
    fn cost_is_never_reported_as_zero() {
        // Zero reads as "free", and nothing is.
        let estimate = estimate(&tbl("tiny").key("1").build(), &stats_with(1, &[]));
        assert!(estimate.cost_ms >= 1);
    }
}
