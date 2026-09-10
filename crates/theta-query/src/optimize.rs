//! Optimizer passes.
//!
//! Every pass here must preserve meaning exactly. A rewrite that returns
//! *nearly* the same rows is worse than no optimizer at all, because the
//! difference shows up as data, not as an error — so each pass is written to be
//! obviously meaning-preserving, and the ones that would need cleverness to
//! prove correct are left undone.
//!
//! Passes, in order:
//!
//! 1. **Predicate pushdown** — move filters closer to the scan, so later
//!    operators see fewer rows.
//! 2. **Index selection** — turn a scan under a filter into an index scan where
//!    an index covers the predicate.
//! 3. **Limit pushdown into point lookups** — a limit over a lookup of one row
//!    is redundant.
//!
//! Deliberately absent: join ordering (there are no joins), and predicate
//! reordering by selectivity (correct only when predicates are side-effect free
//! *and* equally cheap, and the second is not yet measurable).

use crate::plan::{Plan, Predicate};
use crate::stats::Statistics;

/// Rewrite `plan` into an equivalent, cheaper one.
pub fn optimize(plan: Plan, stats: &Statistics) -> Plan {
    let plan = push_down_predicates(plan);
    let plan = select_indexes(plan, stats);
    simplify(plan)
}

/// Move filters below projections and sorts.
///
/// Filtering before sorting is a large win — the sort sees fewer rows — and is
/// meaning-preserving because neither projection nor sort changes which rows
/// exist. Pushing below a `Limit` would *not* be: `LIMIT 10` then filter and
/// filter then `LIMIT 10` are different queries.
fn push_down_predicates(plan: Plan) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => match *input {
            // A projection does not change the set of rows, only their columns.
            // The filter may reference a column the projection drops, which is
            // exactly why it belongs underneath.
            Plan::Project { input, columns } => Plan::Project {
                input: Box::new(push_down_predicates(Plan::Filter { input, predicate })),
                columns,
            },

            // Sorting does not change the set of rows either, and filtering
            // first means sorting less.
            Plan::Sort { input, by } => Plan::Sort {
                input: Box::new(push_down_predicates(Plan::Filter { input, predicate })),
                by,
            },

            other => Plan::Filter {
                input: Box::new(push_down_predicates(other)),
                predicate,
            },
        },

        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(push_down_predicates(*input)),
            columns,
        },
        Plan::Sort { input, by } => Plan::Sort {
            input: Box::new(push_down_predicates(*input)),
            by,
        },
        Plan::Limit {
            input,
            count,
            offset,
        } => Plan::Limit {
            input: Box::new(push_down_predicates(*input)),
            count,
            offset,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: Box::new(push_down_predicates(*input)),
            group_by,
            aggregates,
        },

        leaf => leaf,
    }
}

/// Turn `Filter(Scan)` into an index scan where an index covers the predicate.
fn select_indexes(plan: Plan, stats: &Statistics) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => {
            let input = select_indexes(*input, stats);

            let Plan::Scan { table } = &input else {
                return Plan::Filter {
                    input: Box::new(input),
                    predicate,
                };
            };

            let Some(index) = index_for(&predicate, stats, table) else {
                return Plan::Filter {
                    input: Box::new(input),
                    predicate,
                };
            };

            // The index scan carries the whole predicate, not just the indexed
            // part. Splitting it would be a bigger win and needs the predicate
            // algebra to be provably correct first; carrying it whole is always
            // right and still avoids the full scan.
            Plan::IndexScan {
                table: table.clone(),
                index,
                predicate,
            }
        }

        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(select_indexes(*input, stats)),
            columns,
        },
        Plan::Sort { input, by } => Plan::Sort {
            input: Box::new(select_indexes(*input, stats)),
            by,
        },
        Plan::Limit {
            input,
            count,
            offset,
        } => Plan::Limit {
            input: Box::new(select_indexes(*input, stats)),
            count,
            offset,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: Box::new(select_indexes(*input, stats)),
            group_by,
            aggregates,
        },

        leaf => leaf,
    }
}

/// Which index, if any, can serve this predicate.
fn index_for(predicate: &Predicate, stats: &Statistics, table: &str) -> Option<String> {
    let table_stats = stats.table(table)?;

    match predicate {
        // Only equality and ranges on a single indexed column. `IS NULL` and
        // `!=` are excluded because they typically match most of the table, and
        // an index scan that reads nearly everything is slower than the scan it
        // replaced.
        Predicate::Eq { column, .. }
        | Predicate::Lt { column, .. }
        | Predicate::Lte { column, .. }
        | Predicate::Gt { column, .. }
        | Predicate::Gte { column, .. }
        | Predicate::In { column, .. } => table_stats.indexes.get(column).cloned(),

        // A conjunction can use an index for any one of its terms; the rest are
        // still checked, which is why the index scan keeps the whole predicate.
        Predicate::And(terms) => terms.iter().find_map(|term| index_for(term, stats, table)),

        // A disjunction cannot: rows matching the other branch would be missed
        // entirely, which is a wrong answer rather than a slow one. `!=`,
        // `IS NULL` and `NOT` typically match most of a table, and an index scan
        // that reads nearly everything is slower than the scan it replaced.
        Predicate::Or(_)
        | Predicate::Not(_)
        | Predicate::Ne { .. }
        | Predicate::IsNull { .. }
        | Predicate::True => None,
    }
}

/// Remove nodes that cannot change the result.
fn simplify(plan: Plan) -> Plan {
    match plan {
        // A filter that keeps everything is not a filter.
        Plan::Filter {
            input,
            predicate: Predicate::True,
        } => simplify(*input),

        // A point lookup returns at most one row, so limiting it does nothing —
        // unless the offset skips it, which is a real change and is left alone.
        Plan::Limit {
            input,
            count,
            offset,
        } => {
            let input = simplify(*input);
            match (&input, offset) {
                (Plan::PointLookup { .. }, 0) if count >= 1 => input,
                _ => Plan::Limit {
                    input: Box::new(input),
                    count,
                    offset,
                },
            }
        }

        Plan::Filter { input, predicate } => Plan::Filter {
            input: Box::new(simplify(*input)),
            predicate,
        },
        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(simplify(*input)),
            columns,
        },
        Plan::Sort { input, by } => Plan::Sort {
            input: Box::new(simplify(*input)),
            by,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: Box::new(simplify(*input)),
            group_by,
            aggregates,
        },

        leaf => leaf,
    }
}

#[cfg(test)]
mod tests {
    use crate::builder::{col, table as tbl};
    use crate::stats::TableStats;

    use super::*;

    fn stats_with_index(column: &str) -> Statistics {
        let mut stats = Statistics::new();
        stats.set_table(
            "users",
            TableStats {
                rows: 10_000,
                ..Default::default()
            },
        );
        stats.declare_index("users", column, &format!("idx_{column}"));
        stats
    }

    #[test]
    fn a_filter_is_pushed_below_a_sort() {
        // Built the other way round on purpose: sort then filter.
        let plan = Plan::Filter {
            input: Box::new(Plan::Sort {
                input: Box::new(Plan::Scan {
                    table: "users".into(),
                }),
                by: vec![("name".into(), crate::plan::SortOrder::Asc)],
            }),
            predicate: col("age").gt(30i64),
        };

        match optimize(plan, &Statistics::new()) {
            Plan::Sort { input, .. } => {
                assert!(
                    matches!(*input, Plan::Filter { .. }),
                    "filtering before sorting means sorting fewer rows"
                );
            }
            other => panic!("expected a sort on top, got {other:?}"),
        }
    }

    #[test]
    fn a_filter_is_never_pushed_below_a_limit() {
        // `LIMIT 10` then filter is a different query from filter then
        // `LIMIT 10`, so this rewrite must not happen.
        let plan = Plan::Filter {
            input: Box::new(Plan::Limit {
                input: Box::new(Plan::Scan {
                    table: "users".into(),
                }),
                count: 10,
                offset: 0,
            }),
            predicate: col("age").gt(30i64),
        };
        let optimized = optimize(plan.clone(), &Statistics::new());
        assert!(
            matches!(optimized, Plan::Filter { .. }),
            "the filter must stay above the limit"
        );
    }

    #[test]
    fn an_indexed_equality_becomes_an_index_scan() {
        let plan = tbl("users").filter(col("email").eq("a@b.c")).build();
        match optimize(plan, &stats_with_index("email")) {
            Plan::IndexScan { index, .. } => assert_eq!(index, "idx_email"),
            other => panic!("expected an index scan, got {other:?}"),
        }
    }

    #[test]
    fn a_predicate_with_no_index_stays_a_scan() {
        let plan = tbl("users").filter(col("nickname").eq("x")).build();
        assert!(matches!(
            optimize(plan, &stats_with_index("email")),
            Plan::Filter { .. }
        ));
    }

    #[test]
    fn a_conjunction_uses_an_index_for_one_term_and_keeps_checking_the_rest() {
        let plan = tbl("users")
            .filter(col("email").eq("a@b.c"))
            .filter(col("age").gt(30i64))
            .build();

        match optimize(plan, &stats_with_index("email")) {
            Plan::IndexScan {
                index, predicate, ..
            } => {
                assert_eq!(index, "idx_email");
                // The whole predicate is carried, so the non-indexed term is
                // still applied. Dropping it would return extra rows.
                match predicate {
                    Predicate::And(terms) => assert_eq!(terms.len(), 2),
                    other => panic!("expected the full predicate, got {other:?}"),
                }
            }
            other => panic!("expected an index scan, got {other:?}"),
        }
    }

    #[test]
    fn a_disjunction_never_uses_an_index() {
        // Serving one branch from an index would miss rows matching the other,
        // which is a wrong answer rather than a slow one.
        let plan = tbl("users")
            .filter(crate::builder::any([
                col("email").eq("a@b.c"),
                col("nickname").eq("x"),
            ]))
            .build();
        assert!(matches!(
            optimize(plan, &stats_with_index("email")),
            Plan::Filter { .. }
        ));
    }

    #[test]
    fn an_always_true_filter_is_removed() {
        let plan = Plan::Filter {
            input: Box::new(Plan::Scan {
                table: "users".into(),
            }),
            predicate: Predicate::True,
        };
        assert_eq!(
            optimize(plan, &Statistics::new()),
            Plan::Scan {
                table: "users".into()
            }
        );
    }

    #[test]
    fn a_limit_over_a_point_lookup_is_dropped() {
        let plan = tbl("users").key("1").limit(10).build();
        assert!(matches!(
            optimize(plan, &Statistics::new()),
            Plan::PointLookup { .. }
        ));
    }

    #[test]
    fn a_limit_with_an_offset_over_a_point_lookup_is_kept() {
        // `OFFSET 1` over one row returns nothing, so removing the limit would
        // change the answer.
        let plan = tbl("users").key("1").limit(10).offset(1).build();
        assert!(matches!(
            optimize(plan, &Statistics::new()),
            Plan::Limit { .. }
        ));
    }

    #[test]
    fn optimizing_is_idempotent() {
        let stats = stats_with_index("email");
        let plan = tbl("users")
            .filter(col("email").eq("a@b.c"))
            .order_by("name")
            .limit(10)
            .build();

        let once = optimize(plan, &stats);
        let twice = optimize(once.clone(), &stats);
        assert_eq!(once, twice, "a second pass must be a no-op");
    }

    #[test]
    fn optimizing_never_changes_which_table_is_read() {
        let stats = stats_with_index("email");
        for plan in [
            tbl("users").build(),
            tbl("users").filter(col("email").eq("x")).build(),
            tbl("users")
                .filter(col("email").eq("x"))
                .order_by("a")
                .limit(5)
                .build(),
            tbl("users")
                .aggregate(["plan"], [crate::builder::count()])
                .build(),
        ] {
            let before = plan.source_table().to_string();
            assert_eq!(optimize(plan, &stats).source_table(), before);
        }
    }
}
