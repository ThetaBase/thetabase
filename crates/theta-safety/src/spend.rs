//! Cost ceilings and spend budgets (ROADMAP-V3 M21).
//!
//! `specs/07` §6's breaker bounds blast radius. It does not bound spend, and an
//! agent in a loop is an expensive way to discover that.
//!
//! # Prediction refuses, measurement bills
//!
//! There are two mechanisms here and they do different jobs, which is what makes
//! the awkward case work.
//!
//! A **ceiling** refuses a query before it runs, from the planner's estimate. It
//! is the only one that can prevent a cost rather than record it.
//!
//! A **budget** meters what actually happened, after the fact, against an
//! allowance. It cannot prevent the first expensive query and it is the only
//! thing that can bound the thousandth.
//!
//! The awkward case is an estimate that is a guess. `Estimate::from_statistics`
//! is false for a table nobody has analysed, and a ceiling that refused on that
//! basis would refuse real queries on the strength of a default row count — or,
//! if it let them through, would be bypassable by not running `ANALYZE`.
//!
//! So an unmeasured estimate does not refuse. It is admitted, and the *budget*
//! catches it by metering what it actually cost. Neither mechanism has to be
//! right about everything, which is why there are two.
//!
//! # Refusing is not correcting
//!
//! A ceiling refuses the query. It does not silently add a `LIMIT`, rewrite the
//! plan, or return a partial result — `docs/INVARIANTS.md` invariant 3. A caller who
//! receives half a result set and does not know it is worse off than one who
//! receives an error, because they will act on the half.
//!
//! # Exhaustion is a refusal, never a downgrade
//!
//! The same rule [`crate::budget`] follows for review. Running out of spend
//! refuses work; it never lets something through more cheaply, and it never
//! weakens a gate. An agent that burns its own budget denies itself and opens
//! nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Work, in the unit the planner already produces.
///
/// Milliseconds of estimated or measured execution. Not currency: converting to
/// money needs a price the Control Plane holds and this crate must not, because
/// the Safety Layer deciding what something costs in pounds is the Safety Layer
/// holding a business rule.
pub type CostMs = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendPolicy {
    /// The most a single query may be estimated to cost before it is refused.
    ///
    /// Zero disables the ceiling. Disabling it is a real choice for an
    /// analytics-shaped project and is not the default.
    pub query_ceiling_ms: CostMs,
    /// Work one agent may spend per window.
    pub agent_ceiling_ms: CostMs,
    /// Work a whole project may spend per window.
    pub project_ceiling_ms: CostMs,
    pub window_ms: i64,
}

impl Default for SpendPolicy {
    /// Ten seconds for one query, an hour of work per agent per day.
    ///
    /// Chosen to catch a runaway rather than to ration ordinary work: a project
    /// that legitimately needs more will notice immediately and raise it, and a
    /// loop will hit it within minutes. The failure mode of a tight default is
    /// that everybody raises it without reading, which leaves a limit nobody
    /// believes.
    fn default() -> Self {
        Self {
            query_ceiling_ms: 10_000,
            agent_ceiling_ms: 60 * 60 * 1_000,
            project_ceiling_ms: 8 * 60 * 60 * 1_000,
            window_ms: 24 * 60 * 60 * 1_000,
        }
    }
}

/// What a ceiling decided about one query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum CeilingDecision {
    /// Run it.
    Allow,
    /// Run it, and note that nothing here could have stopped it.
    ///
    /// The estimate was not derived from statistics, so refusing on it would be
    /// refusing on a default. Distinguished from `Allow` because a caller
    /// counting how often the ceiling *could not apply* is measuring how much of
    /// their workload is unanalysed, which is actionable.
    AllowUnmeasured { estimated_ms: CostMs },
    /// Refuse it.
    Refuse {
        estimated_ms: CostMs,
        ceiling_ms: CostMs,
        reason: String,
    },
}

impl CeilingDecision {
    pub fn allowed(&self) -> bool {
        !matches!(self, CeilingDecision::Refuse { .. })
    }
}

/// Decide whether a query may run, from the planner's estimate.
///
/// `from_statistics` is the planner's own honesty flag and is not optional here:
/// a caller that could omit it would default it, and the default that makes a
/// ceiling look effective is the one that makes it wrong.
pub fn check_ceiling(
    estimated_ms: CostMs,
    from_statistics: bool,
    policy: &SpendPolicy,
) -> CeilingDecision {
    if policy.query_ceiling_ms == 0 {
        return CeilingDecision::Allow;
    }
    if estimated_ms <= policy.query_ceiling_ms {
        return CeilingDecision::Allow;
    }
    if !from_statistics {
        // Over the ceiling on a number that came from a default row count.
        // Refusing here would refuse real work for not having run `ANALYZE`,
        // and the budget below will catch it by what it actually costs.
        return CeilingDecision::AllowUnmeasured { estimated_ms };
    }
    CeilingDecision::Refuse {
        estimated_ms,
        ceiling_ms: policy.query_ceiling_ms,
        reason: format!(
            "this query is estimated at {estimated_ms}ms against a {}ms ceiling. It is \
             refused rather than narrowed: a partial result you did not ask for is \
             worse than an error, because you would act on it.",
            policy.query_ceiling_ms
        ),
    }
}

/// Who is spending.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope", content = "id")]
pub enum SpendScope {
    Project,
    Agent(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum SpendDecision {
    Allow {
        /// Work left in the window, across the tightest scope.
        remaining_ms: CostMs,
    },
    Exhausted {
        scope: SpendScope,
        spent_ms: CostMs,
        ceiling_ms: CostMs,
        window_ms: i64,
        reason: String,
    },
}

impl SpendDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, SpendDecision::Allow { .. })
    }
}

#[derive(Debug, Clone)]
struct Spent {
    at_ms: i64,
    cost_ms: CostMs,
}

/// Work spent, per scope, in a rolling window.
#[derive(Debug, Clone)]
pub struct SpendLedger {
    policy: SpendPolicy,
    spent: BTreeMap<SpendScope, Vec<Spent>>,
}

impl SpendLedger {
    pub fn new(policy: SpendPolicy) -> Self {
        Self {
            policy,
            spent: BTreeMap::new(),
        }
    }

    pub fn policy(&self) -> &SpendPolicy {
        &self.policy
    }

    fn ceiling(&self, scope: &SpendScope) -> CostMs {
        match scope {
            SpendScope::Project => self.policy.project_ceiling_ms,
            SpendScope::Agent(_) => self.policy.agent_ceiling_ms,
        }
    }

    /// Work spent against a scope inside the window.
    pub fn spent(&self, scope: &SpendScope, now_ms: i64) -> CostMs {
        self.spent
            .get(scope)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|s| in_window(s.at_ms, now_ms, self.policy.window_ms))
                    .map(|s| s.cost_ms)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// May `scopes` spend `cost_ms`?
    ///
    /// Checked against every scope before charging any, so a refusal leaves
    /// nothing partially charged — the same shape as the review budget, for the
    /// same reason.
    pub fn may_spend(&self, scopes: &[SpendScope], cost_ms: CostMs, now_ms: i64) -> SpendDecision {
        let mut remaining = CostMs::MAX;
        for scope in scopes {
            let ceiling = self.ceiling(scope);
            let spent = self.spent(scope, now_ms);
            if spent.saturating_add(cost_ms) > ceiling {
                return SpendDecision::Exhausted {
                    scope: scope.clone(),
                    spent_ms: spent,
                    ceiling_ms: ceiling,
                    window_ms: self.policy.window_ms,
                    reason: format!(
                        "{spent}ms of {ceiling}ms is already spent in this window and this \
                         work would cost {cost_ms}ms more. The work is refused, not \
                         degraded — wait for the window to roll or raise the ceiling."
                    ),
                };
            }
            remaining = remaining.min(ceiling - spent - cost_ms);
        }
        SpendDecision::Allow {
            remaining_ms: if remaining == CostMs::MAX {
                0
            } else {
                remaining
            },
        }
    }

    /// Record work that actually happened.
    ///
    /// Separate from [`SpendLedger::may_spend`] because the two take different
    /// numbers: permission is asked with an estimate, and this is charged with a
    /// measurement. Charging the estimate would leave a project billed for what
    /// the planner guessed, which is the number this module already refuses to
    /// trust when it is unmeasured.
    pub fn record(&mut self, scopes: &[SpendScope], actual_ms: CostMs, now_ms: i64) {
        self.evict(now_ms);
        for scope in scopes {
            self.spent.entry(scope.clone()).or_default().push(Spent {
                at_ms: now_ms,
                cost_ms: actual_ms,
            });
        }
    }

    fn evict(&mut self, now_ms: i64) {
        let window = self.policy.window_ms;
        self.spent.retain(|_, entries| {
            entries.retain(|s| in_window(s.at_ms, now_ms, window));
            !entries.is_empty()
        });
    }
}

/// Whether a sample is still inside the window.
///
/// Written as an addition on the sample, matching [`crate::breaker`] and
/// [`crate::budget`], so the three read the same way.
///
/// Being precise about why, because the obvious reason does not apply here.
/// Those two use **unsigned** timestamps, where subtracting a window from a
/// small `now` saturates at zero and silently drops everything recorded at
/// `t = 0` — a bug the breaker has a comment about and the review budget
/// reintroduced anyway. These timestamps are `i64`, where the subtraction goes
/// negative instead, so the subtracted form would be correct too.
///
/// Recorded rather than left as a repeated warning: a planted violation swapped
/// this for the subtracted form and every test still passed, which is what
/// happens when a comment carries a caution from a different type.
fn in_window(at_ms: i64, now_ms: i64, window_ms: i64) -> bool {
    at_ms.saturating_add(window_ms) > now_ms
}

/// What the billing layer should do about a project over its limit.
///
/// `theta-control`'s `Standing::Over` is deliberately reporting-only: a billing
/// module that could silently drop data would be the worst kind of coupling.
/// This is the *instance* side of that decision, and it keeps the same
/// separation — the Control Plane says a project is over, and the instance
/// decides what that means for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverLimit {
    /// Report it and serve the request. The v1 position.
    Report,
    /// Refuse *new work* and keep serving reads.
    ///
    /// The enforcing position, and the only enforcing one on offer. Refusing
    /// reads would make a billing state into data loss by another name: a
    /// customer over their limit must always be able to get their data out
    /// (`specs/06` §8).
    RefuseWrites,
}

impl OverLimit {
    /// Whether a read may proceed. Always true, on purpose.
    ///
    /// A function rather than a constant so that a future variant has to answer
    /// the question rather than inherit an answer.
    pub fn allows_reads(self) -> bool {
        true
    }

    pub fn allows_writes(self) -> bool {
        matches!(self, OverLimit::Report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<SpendScope> {
        vec![SpendScope::Project, SpendScope::Agent("agent-a".into())]
    }

    fn ledger() -> SpendLedger {
        SpendLedger::new(SpendPolicy {
            agent_ceiling_ms: 1_000,
            project_ceiling_ms: 5_000,
            ..SpendPolicy::default()
        })
    }

    #[test]
    fn an_expensive_query_with_a_real_estimate_is_refused() {
        let policy = SpendPolicy::default();
        let decision = check_ceiling(60_000, true, &policy);
        match decision {
            CeilingDecision::Refuse {
                estimated_ms,
                ceiling_ms,
                ..
            } => {
                assert_eq!(estimated_ms, 60_000);
                assert_eq!(ceiling_ms, 10_000);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_expensive_query_with_a_guessed_estimate_is_admitted_and_flagged() {
        // The awkward case. `from_statistics` is false for an unanalysed table,
        // and refusing on that basis would refuse real work on the strength of a
        // default row count. The budget catches it by what it actually costs.
        let policy = SpendPolicy::default();
        let decision = check_ceiling(60_000, false, &policy);
        assert!(decision.allowed());
        assert!(
            matches!(decision, CeilingDecision::AllowUnmeasured { .. }),
            "an unmeasured admission must be distinguishable from an ordinary one: \
             counting them measures how much of a workload is unanalysed"
        );
    }

    #[test]
    fn a_refusal_says_it_refused_rather_than_narrowing() {
        // Invariant 3. A caller who receives half a result set and does not know
        // it is worse off than one who receives an error.
        let policy = SpendPolicy::default();
        let CeilingDecision::Refuse { reason, .. } = check_ceiling(60_000, true, &policy) else {
            panic!("expected a refusal");
        };
        assert!(reason.contains("refused rather than narrowed"));
    }

    #[test]
    fn a_ceiling_of_zero_disables_it_rather_than_refusing_everything() {
        // Disabling is a real choice for an analytics-shaped project. A zero that
        // meant "refuse all" would make the disabled state unreachable and the
        // configuration a trap.
        let policy = SpendPolicy {
            query_ceiling_ms: 0,
            ..SpendPolicy::default()
        };
        assert_eq!(
            check_ceiling(u64::MAX, true, &policy),
            CeilingDecision::Allow
        );
    }

    #[test]
    fn the_tightest_scope_is_the_one_that_refuses() {
        let mut ledger = ledger();
        ledger.record(&scopes(), 900, 0);

        match ledger.may_spend(&scopes(), 200, 1) {
            SpendDecision::Exhausted { scope, .. } => {
                assert_eq!(
                    scope,
                    SpendScope::Agent("agent-a".into()),
                    "the agent's 1000ms runs out before the project's 5000ms"
                );
            }
            other => panic!("expected exhaustion on the agent, got {other:?}"),
        }
    }

    #[test]
    fn exhaustion_refuses_and_never_degrades() {
        // The same rule the review budget follows. Running out never lets
        // something through more cheaply.
        let mut ledger = ledger();
        ledger.record(&scopes(), 1_000, 0);
        let decision = ledger.may_spend(&scopes(), 1, 1);
        assert!(!decision.allowed());
        let SpendDecision::Exhausted { reason, .. } = decision else {
            unreachable!()
        };
        assert!(reason.contains("refused, not"));
    }

    #[test]
    fn permission_is_asked_with_an_estimate_and_charged_with_a_measurement() {
        // Charging the estimate would bill a project for what the planner
        // guessed, which is the number this module already refuses to trust when
        // it is unmeasured.
        let mut ledger = ledger();
        assert!(ledger.may_spend(&scopes(), 800, 0).allowed());
        // It actually took 100ms.
        ledger.record(&scopes(), 100, 0);
        assert_eq!(ledger.spent(&SpendScope::Agent("agent-a".into()), 0), 100);
    }

    #[test]
    fn spend_leaves_the_window_and_the_allowance_recovers() {
        let mut ledger = ledger();
        let window = ledger.policy().window_ms;
        ledger.record(&scopes(), 1_000, 0);
        assert!(!ledger.may_spend(&scopes(), 1, 1).allowed());
        assert!(
            ledger.may_spend(&scopes(), 1, window + 1).allowed(),
            "a full window later the allowance is available again"
        );
    }

    #[test]
    fn spend_recorded_at_time_zero_is_not_silently_dropped() {
        // Near the start of a process, work recorded at `t = 0` must still be in
        // the window a millisecond later. Trivially true with signed
        // timestamps and the reason the breaker's `u64` version needed care.
        let mut ledger = ledger();
        ledger.record(&scopes(), 500, 0);
        assert_eq!(
            ledger.spent(&SpendScope::Agent("agent-a".into()), 1),
            500,
            "work recorded at t=0 must still be in the window at t=1"
        );
    }

    #[test]
    fn work_older_than_the_window_leaves_it_and_newer_work_does_not() {
        // What the window actually has to get right, checked at the boundary
        // rather than at an arbitrary interior point.
        let mut ledger = ledger();
        let window = ledger.policy().window_ms;

        ledger.record(&scopes(), 400, 1_000);
        let agent = SpendScope::Agent("agent-a".into());

        assert_eq!(
            ledger.spent(&agent, 1_000 + window - 1),
            400,
            "one millisecond before the window closes it is still counted"
        );
        assert_eq!(
            ledger.spent(&agent, 1_000 + window),
            0,
            "and exactly at the boundary it has left"
        );
    }

    #[test]
    fn one_agents_exhaustion_does_not_stop_another() {
        let mut ledger = ledger();
        let a = vec![SpendScope::Project, SpendScope::Agent("a".into())];
        let b = vec![SpendScope::Project, SpendScope::Agent("b".into())];

        ledger.record(&a, 1_000, 0);
        assert!(!ledger.may_spend(&a, 1, 1).allowed());
        assert!(
            ledger.may_spend(&b, 1, 1).allowed(),
            "a different agent has its own allowance"
        );
    }

    #[test]
    fn the_project_ceiling_still_binds_when_no_single_agent_has_exhausted_theirs() {
        // The reason `Project` is a scope at all: four agents each well inside
        // their own allowance can exhaust the project's between them.
        let mut ledger = ledger();
        for name in ["a", "b", "c", "d", "e"] {
            let scopes = vec![SpendScope::Project, SpendScope::Agent(name.into())];
            ledger.record(&scopes, 900, 0);
        }
        let sixth = vec![SpendScope::Project, SpendScope::Agent("f".into())];
        match ledger.may_spend(&sixth, 900, 1) {
            SpendDecision::Exhausted { scope, .. } => assert_eq!(scope, SpendScope::Project),
            other => panic!("expected the project ceiling to bind, got {other:?}"),
        }
    }

    #[test]
    fn being_over_a_billing_limit_never_stops_a_customer_reading_their_data() {
        // A billing state must not become data loss by another name. Asserted
        // over every variant, so a new one has to answer the question.
        for state in [OverLimit::Report, OverLimit::RefuseWrites] {
            assert!(
                state.allows_reads(),
                "{state:?} must still let a customer read"
            );
        }
        assert!(!OverLimit::RefuseWrites.allows_writes());
        assert!(OverLimit::Report.allows_writes());
    }
}
