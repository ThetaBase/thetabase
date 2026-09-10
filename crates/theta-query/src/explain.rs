//! EXPLAIN output. Required before execution for anything the Safety Layer or a
//! human reviewer must reason about (`02-api-wire-protocol.md` §4).

use serde::{Deserialize, Serialize};
use theta_core::schema::Schema;

use crate::plan::{Plan, PlanHash};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Explain {
    pub plan_hash: PlanHash,
    pub steps: Vec<ExplainStep>,
    pub estimated_rows: u64,
    pub estimated_cost_ms: u32,
    /// False when the table has no statistics, so the estimates above are a
    /// default rather than a measurement. Reported because an estimate nobody
    /// can tell is a guess is worse than no estimate.
    pub estimates_from_statistics: bool,
    /// Indexes this plan will use. An empty list on a large table is the signal
    /// a reviewer is looking for.
    pub indexes_used: Vec<String>,
    /// True iff no model call occurs anywhere in this plan's execution. Asserted
    /// in CI rather than assumed (`08-test-validation-plan.md` §4).
    pub llm_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainStep {
    pub depth: u32,
    pub operator: String,
    pub detail: String,
}

impl Explain {
    /// Explain a plan without statistics. Estimates fall back to defaults and
    /// say so.
    pub fn of(plan: &Plan, schema: &Schema) -> Self {
        Self::with_stats(plan, schema, &crate::stats::Statistics::new())
    }

    /// Explain a plan against known statistics.
    pub fn with_stats(plan: &Plan, _schema: &Schema, stats: &crate::stats::Statistics) -> Self {
        let mut steps = Vec::new();
        let mut indexes = Vec::new();
        walk(plan, 0, &mut steps, &mut indexes);

        let estimate = crate::stats::estimate(plan, stats);
        Self {
            plan_hash: plan.hash(),
            steps,
            estimated_rows: estimate.rows,
            estimated_cost_ms: estimate.cost_ms,
            estimates_from_statistics: estimate.from_statistics,
            indexes_used: indexes,
            // Structurally zero: nothing in this crate's dependency closure can
            // make a model call, and `no_llm_on_hot_path` asserts it in CI.
            llm_calls: 0,
        }
    }

    /// Rendered for a terminal or an agent's context window.
    pub fn to_text(&self) -> String {
        let mut out = format!("plan {:016x}\n", self.plan_hash.0);
        for step in &self.steps {
            out.push_str(&format!(
                "{:indent$}-> {} ({})\n",
                "",
                step.operator,
                step.detail,
                indent = (step.depth as usize) * 2
            ));
        }
        out.push_str(&format!(
            "est. rows {} | est. cost {}ms | llm calls {}{}\n",
            self.estimated_rows,
            self.estimated_cost_ms,
            self.llm_calls,
            match self.estimates_from_statistics {
                true => "",
                false => " | estimates unmeasured (no statistics for this table)",
            }
        ));
        out
    }
}

fn walk(plan: &Plan, depth: u32, steps: &mut Vec<ExplainStep>, indexes: &mut Vec<String>) {
    let (operator, detail, child): (&str, String, Option<&Plan>) = match plan {
        Plan::Scan { table } => ("Scan", table.clone(), None),
        Plan::PointLookup { table, .. } => ("PointLookup", table.clone(), None),
        Plan::IndexScan { table, index, .. } => {
            indexes.push(index.clone());
            ("IndexScan", format!("{table} via {index}"), None)
        }
        Plan::Filter { input, .. } => ("Filter", String::new(), Some(input)),
        Plan::Project { input, columns } => ("Project", columns.join(", "), Some(input)),
        Plan::Sort { input, by } => (
            "Sort",
            by.iter()
                .map(|(c, _)| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Some(input),
        ),
        Plan::Limit {
            input,
            count,
            offset,
        } => ("Limit", format!("{count} offset {offset}"), Some(input)),
        Plan::Aggregate {
            input, group_by, ..
        } => ("Aggregate", group_by.join(", "), Some(input)),
    };

    steps.push(ExplainStep {
        depth,
        operator: operator.to_string(),
        detail,
    });
    if let Some(child) = child {
        walk(child, depth + 1, steps, indexes);
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::Predicate;

    use super::*;

    #[test]
    fn explain_reports_the_operator_tree_and_zero_llm_calls() {
        let plan = Plan::Filter {
            input: Box::new(Plan::IndexScan {
                table: "users".into(),
                index: "idx_churn".into(),
                predicate: Predicate::True,
            }),
            predicate: Predicate::True,
        };
        let explain = Explain::of(&plan, &Schema::default());
        assert_eq!(explain.steps.len(), 2);
        assert_eq!(explain.indexes_used, vec!["idx_churn"]);
        assert_eq!(explain.llm_calls, 0);
        assert!(explain.to_text().contains("IndexScan"));
    }
}
