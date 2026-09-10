//! Review as a budget rather than a queue (ROADMAP-V3 M17).
//!
//! # The problem this exists for
//!
//! Every gate in [`crate::classify`] ends in the same place: a human decides.
//! That is correct and it is the product. It is also unbounded — nothing in the
//! Safety Layer limits how many decisions an agent can *create*, and a queue
//! with unbounded arrival and a human service rate has one steady state, which
//! is a backlog nobody reads.
//!
//! The failure mode is not that changes land unreviewed. It is worse and
//! quieter: review becomes ceremonial. A human facing four hundred pending
//! proposals approves them in batches by feel, which is the same as no gate
//! while looking like a gate that works.
//!
//! So the scarce resource is metered directly. **Human attention is a budget an
//! agent spends, and when it runs out the agent stops rather than the queue
//! grows.**
//!
//! # Why this does not weaken any gate
//!
//! Exhausting a budget **refuses** a proposal. It never approves one, never
//! downgrades a gate, and never converts a `ShadowValidate` into something a
//! confirmation clears. The direction is the one `docs/INVARIANTS.md` invariant 3
//! requires: a refused proposal is refused, and the caller submits a corrected
//! one — or waits for the window to roll.
//!
//! It is also the reason exhaustion cannot be used as an attack. An agent that
//! deliberately burns the budget denies itself; it does not open a path for
//! anything to land unreviewed.
//!
//! # Why auto-applied changes are free
//!
//! [`ReviewCost::of`] charges zero for [`Gate::AutoApply`]. An agent doing
//! nothing that needs a human is never throttled by this, no matter how much of
//! it it does. That is what makes this a *review* budget and not a rate limit —
//! and it is the property that keeps the incentive pointing the right way: the
//! cheapest way to stay under budget is to propose changes that do not need
//! review, which is the behaviour we want anyway.
//!
//! Volume of safe writes is already bounded, by [`crate::breaker`]. The two
//! meter different scarcities and neither substitutes for the other.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::diff::Gate;

/// What one gated change costs against a reviewer's attention.
///
/// A pure function of the gate — the same discipline as classification itself
/// (`docs/INVARIANTS.md` invariant 2). Nothing about the identifier, the proposer, or
/// the change's contents reaches this, so a proposal cannot make itself cheap
/// by how it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCost(pub u32);

impl ReviewCost {
    /// `AutoApply` is free; `Confirm` is the unit; `ShadowValidate` costs more
    /// because it is a shadow run *plus* a promotion decision, and the human is
    /// reading validation output rather than answering yes or no.
    ///
    /// The ratio is a default, not a measurement, and it is stated as one:
    /// unlike the breaker's ceiling there is no corpus to calibrate against,
    /// because the quantity is somebody's attention. What is *not* a guess is
    /// the ordering, and that is what the tests pin — a stricter gate always
    /// costs at least as much as a weaker one. A project that measures its own
    /// review times can set [`BudgetPolicy::shadow_cost`] from data.
    pub fn of(gate: Gate, policy: &BudgetPolicy) -> Self {
        ReviewCost(match gate {
            Gate::AutoApply => 0,
            Gate::Confirm => policy.confirm_cost,
            Gate::ShadowValidate => policy.shadow_cost,
        })
    }

    pub fn is_free(&self) -> bool {
        self.0 == 0
    }
}

/// The scopes a proposal is charged against.
///
/// A proposal is charged to **every** applicable scope and refused if **any**
/// of them is exhausted. Charging one and not the others would leave the
/// obvious hole: an agent that has spent its own budget starts a new branch, or
/// a fleet of agents that are each under budget saturates a project's reviewers
/// between them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope", content = "id")]
pub enum BudgetScope {
    /// Everything in the project, whoever proposed it. The ceiling that matters,
    /// because it is the one that corresponds to a real reviewer's day.
    Project,
    /// One target branch. Stops a single branch monopolising review.
    Branch(u64),
    /// One agent identity. Stops one misbehaving agent starving the others.
    Agent(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetPolicy {
    /// Cost of a change gated at `Confirm`. The unit.
    pub confirm_cost: u32,
    /// Cost of a change gated at `ShadowValidate`.
    pub shadow_cost: u32,

    /// Review units a whole project may spend per window.
    pub project_ceiling: u32,
    /// Review units one branch may spend per window.
    pub branch_ceiling: u32,
    /// Review units one agent may spend per window.
    pub agent_ceiling: u32,

    /// Length of the rolling window.
    pub window_ms: i64,

    /// How long a reserved proposal holds its budget before expiring
    /// (ROADMAP-V3 M17, "expiring proposals").
    pub proposal_ttl_ms: i64,
}

impl Default for BudgetPolicy {
    /// Deliberately generous, because the first thing a budget that is too tight
    /// does is teach people to raise it, and a limit nobody believes is worse
    /// than no limit.
    ///
    /// 60 confirmations a day is more review than any human does carefully; the
    /// point of the ceiling is to catch the runaway case, where an agent
    /// generates hundreds, not to ration ordinary work.
    fn default() -> Self {
        Self {
            confirm_cost: 1,
            shadow_cost: 5,
            project_ceiling: 60,
            // Between the agent and project ceilings, deliberately.
            //
            // These were both 20, and integration found the consequence: on a
            // project where everyone works on `main`, the branch ceiling binds
            // at exactly the moment the first agent's own ceiling does, and
            // then blocks *every other agent* on that branch. The scope that
            // exists to stop one branch monopolising review was instead letting
            // one agent monopolise a branch.
            //
            // A branch ceiling only does its job when it sits above what a
            // single agent can spend and below what the project can.
            branch_ceiling: 40,
            agent_ceiling: 20,
            window_ms: 24 * 60 * 60 * 1_000,
            proposal_ttl_ms: 4 * 60 * 60 * 1_000,
        }
    }
}

impl BudgetPolicy {
    fn ceiling(&self, scope: &BudgetScope) -> u32 {
        match scope {
            BudgetScope::Project => self.project_ceiling,
            BudgetScope::Branch(_) => self.branch_ceiling,
            BudgetScope::Agent(_) => self.agent_ceiling,
        }
    }
}

/// Handle to a held reservation. Opaque, and required to settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReservationId(pub u64);

impl ReservationId {
    /// The id handed back for a change that costs nothing.
    ///
    /// It is a distinct value rather than an absent one so a caller never has to
    /// branch on whether its change happened to be free — settling this is
    /// always a no-op and always succeeds.
    ///
    /// It exists because the first version inferred "this was free" from "this
    /// id is not held", which is also true of an id that was *already settled*.
    /// That made a double-settle silently succeed, and a double-settle that
    /// succeeds is a refund of budget that was only ever reserved once.
    pub const FREE: ReservationId = ReservationId(0);

    pub fn is_free(&self) -> bool {
        *self == ReservationId::FREE
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum BudgetDecision {
    /// Budget reserved. The proposal may be created.
    Granted {
        reservation: ReservationId,
        cost: u32,
        /// Smallest remaining headroom across the charged scopes, so a caller
        /// can warn before it hits zero rather than after.
        remaining: u32,
    },
    /// Refused. Carries which scope ran out and by how much, because a refusal a
    /// caller cannot act on is a refusal that gets retried in a loop.
    Exhausted {
        scope: BudgetScope,
        cost: u32,
        spent: u32,
        ceiling: u32,
        window_ms: i64,
        reason: String,
    },
}

impl BudgetDecision {
    pub fn is_granted(&self) -> bool {
        matches!(self, BudgetDecision::Granted { .. })
    }

    pub fn reservation(&self) -> Option<ReservationId> {
        match self {
            BudgetDecision::Granted { reservation, .. } => Some(*reservation),
            BudgetDecision::Exhausted { .. } => None,
        }
    }
}

/// Why a reservation stopped being held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    /// The proposal was reviewed and applied. The budget stays spent — this is
    /// attention that was actually consumed.
    Approved,
    /// The proposal was reviewed and refused. Also spent: saying no takes as
    /// long as saying yes, and refunding it would make rejected proposals free
    /// to generate.
    Rejected,
    /// Nobody looked at it before it expired. **Refunded**, because attention
    /// that was never spent should not be charged.
    Expired,
    /// The proposer withdrew it before anyone looked. Refunded, for the same
    /// reason — and this is the path that makes a well-behaved agent cheaper
    /// than a careless one.
    Withdrawn,
}

impl Settlement {
    fn refunds(self) -> bool {
        matches!(self, Settlement::Expired | Settlement::Withdrawn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettleError {
    /// No such reservation, or it has already been settled. Settling twice
    /// would refund budget that was only ever reserved once.
    Unknown(ReservationId),
    /// The reservation outlived `proposal_ttl_ms`. The proposal is dead and
    /// cannot be approved — see [`ReviewBudget::settle`].
    Expired {
        reservation: ReservationId,
        age_ms: i64,
        ttl_ms: i64,
    },
}

impl std::fmt::Display for SettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettleError::Unknown(id) => {
                write!(f, "reservation {} is unknown or already settled", id.0)
            }
            SettleError::Expired {
                reservation,
                age_ms,
                ttl_ms,
            } => write!(
                f,
                "reservation {} expired {}ms ago (ttl {}ms); the proposal must be \
                 re-submitted and re-reviewed",
                reservation.0,
                age_ms.saturating_sub(*ttl_ms),
                ttl_ms
            ),
        }
    }
}

impl std::error::Error for SettleError {}

#[derive(Debug, Clone)]
struct Reservation {
    at_ms: i64,
    cost: u32,
    scopes: Vec<BudgetScope>,
}

/// A spend recorded against a scope, retained until it leaves the window.
#[derive(Debug, Clone)]
struct Spend {
    at_ms: i64,
    cost: u32,
}

/// Rolling review budgets, keyed by scope.
///
/// Time is injected rather than read from a clock, so expiry and window
/// rollover are deterministically testable — the same choice
/// [`crate::breaker`] makes and for the same reason.
#[derive(Debug)]
pub struct ReviewBudget {
    policy: BudgetPolicy,
    /// Settled-and-spent history per scope, trimmed to the window.
    spent: HashMap<BudgetScope, Vec<Spend>>,
    /// Currently-held reservations, which count against the ceiling exactly as
    /// spend does. A reservation that did not count would let an agent hold a
    /// hundred proposals open and never be refused.
    held: HashMap<ReservationId, Reservation>,
    next_id: u64,
}

impl ReviewBudget {
    pub fn new(policy: BudgetPolicy) -> Self {
        Self {
            policy,
            spent: HashMap::new(),
            held: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn policy(&self) -> &BudgetPolicy {
        &self.policy
    }

    /// Reserve budget for a proposal gated at `gate`.
    ///
    /// A free change (`AutoApply`) is granted without recording anything: it
    /// consumes no attention, so there is nothing to meter and nothing to
    /// settle. Callers may settle the returned reservation anyway — settling a
    /// zero-cost reservation is a no-op rather than an error, so a caller does
    /// not have to branch on whether its change happened to be free.
    pub fn reserve(&mut self, gate: Gate, scopes: &[BudgetScope], now_ms: i64) -> BudgetDecision {
        self.expire(now_ms);

        let cost = ReviewCost::of(gate, &self.policy).0;

        // Checked against every scope *before* charging any of them. Charging as
        // we go and unwinding on refusal would leave a partial charge if the
        // unwind were ever wrong; not charging until the answer is known means
        // there is nothing to unwind.
        let mut headroom = u32::MAX;
        for scope in scopes {
            let ceiling = self.policy.ceiling(scope);
            let committed = self.committed(scope, now_ms);
            if committed.saturating_add(cost) > ceiling {
                return BudgetDecision::Exhausted {
                    scope: scope.clone(),
                    cost,
                    spent: committed,
                    ceiling,
                    window_ms: self.policy.window_ms,
                    reason: format!(
                        "this change costs {cost} review unit(s) and {committed} of {ceiling} \
                         are already committed in the current window. The proposal is refused, \
                         not downgraded \u{2014} resolve outstanding reviews or wait for the \
                         window to roll."
                    ),
                };
            }
            headroom = headroom.min(ceiling - committed - cost);
        }

        // A free change is not recorded at all. Recording it would grow `held`
        // without bound for a caller that never settles what it never had to.
        let id = if cost > 0 {
            let id = ReservationId(self.next_id);
            self.next_id += 1;
            self.held.insert(
                id,
                Reservation {
                    at_ms: now_ms,
                    cost,
                    scopes: scopes.to_vec(),
                },
            );
            id
        } else {
            ReservationId::FREE
        };

        BudgetDecision::Granted {
            reservation: id,
            cost,
            remaining: if headroom == u32::MAX { 0 } else { headroom },
        }
    }

    /// Resolve a reservation.
    ///
    /// **An expired reservation cannot be approved.** `settle` refuses it rather
    /// than accepting it late, and that is the whole point of expiry being here
    /// rather than in a cleanup job: a proposal whose budget was released but
    /// which could still be promoted is strictly worse than no expiry at all,
    /// because the reviewer's decision is being applied to a change that has sat
    /// unreviewed for hours while the schema moved underneath it.
    ///
    /// Expiring it here also means the refund and the invalidation are the same
    /// event and cannot disagree.
    pub fn settle(
        &mut self,
        reservation: ReservationId,
        settlement: Settlement,
        now_ms: i64,
    ) -> Result<(), SettleError> {
        // Sweep first, so a caller arriving late gets `Expired` rather than
        // `Unknown` — the difference matters, because one says "too late" and
        // the other says "never existed".
        let expired_now = self.expire(now_ms);
        if let Some(age_ms) = expired_now.get(&reservation) {
            return Err(SettleError::Expired {
                reservation,
                age_ms: *age_ms,
                ttl_ms: self.policy.proposal_ttl_ms,
            });
        }

        if reservation.is_free() {
            return Ok(());
        }

        let Some(held) = self.held.remove(&reservation) else {
            // Either it never existed or it has already been settled, and both
            // must refuse. Accepting a second settle would refund a spend.
            return Err(SettleError::Unknown(reservation));
        };

        if !settlement.refunds() {
            for scope in held.scopes {
                self.spent.entry(scope).or_default().push(Spend {
                    at_ms: now_ms,
                    cost: held.cost,
                });
            }
        }
        Ok(())
    }

    /// Units committed against a scope right now: settled spend inside the
    /// window, plus everything currently held.
    pub fn committed(&self, scope: &BudgetScope, now_ms: i64) -> u32 {
        let settled: u32 = self
            .spent
            .get(scope)
            .map(|v| {
                v.iter()
                    .filter(|s| in_window(s.at_ms, now_ms, self.policy.window_ms))
                    .map(|s| s.cost)
                    .sum()
            })
            .unwrap_or(0);
        let held: u32 = self
            .held
            .values()
            .filter(|r| r.scopes.contains(scope))
            .map(|r| r.cost)
            .sum();
        settled.saturating_add(held)
    }

    /// Reservations currently held. Surfaced so a `status` RPC can show what a
    /// budget is being held *by* — a ceiling with no visibility into who is
    /// holding it produces a support ticket rather than an action.
    pub fn outstanding(&self) -> usize {
        self.held.len()
    }

    /// Drop reservations past their TTL and trim spend that has left the window.
    /// Returns the ids expired on this sweep, with their ages.
    fn expire(&mut self, now_ms: i64) -> HashMap<ReservationId, i64> {
        let ttl = self.policy.proposal_ttl_ms;
        let mut expired = HashMap::new();
        self.held.retain(|id, r| {
            let age = now_ms.saturating_sub(r.at_ms);
            // `>` not `>=`: a reservation exactly at its TTL has not yet
            // outlived it. The same off-by-one cost the breaker a bug.
            if age > ttl {
                expired.insert(*id, age);
                false
            } else {
                true
            }
        });

        let window_ms = self.policy.window_ms;
        self.spent.retain(|_, v| {
            v.retain(|s| in_window(s.at_ms, now_ms, window_ms));
            !v.is_empty()
        });

        expired
    }
}

/// Whether a sample at `at_ms` is still inside the window ending at `now_ms`.
///
/// Written as an addition on the sample, matching `breaker.rs` and
/// `spend.rs` so the three read alike.
///
/// The clock here was originally `u64`, where the subtracted form saturates at
/// zero near the start of a process and silently drops every sample recorded at
/// `t = 0` — a bug `breaker.rs` warns about and this file reintroduced anyway.
/// It is now `i64`, matching every other clock in the engine, so the subtraction
/// would go negative instead and both forms are correct. The shape is kept for
/// consistency; the hazard it guarded against belongs to the old type.
fn in_window(at_ms: i64, now_ms: i64, window_ms: i64) -> bool {
    at_ms.saturating_add(window_ms) > now_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<BudgetScope> {
        vec![
            BudgetScope::Project,
            BudgetScope::Branch(7),
            BudgetScope::Agent("agent-a".into()),
        ]
    }

    fn budget() -> ReviewBudget {
        ReviewBudget::new(BudgetPolicy {
            project_ceiling: 10,
            branch_ceiling: 4,
            agent_ceiling: 6,
            ..BudgetPolicy::default()
        })
    }

    #[test]
    fn an_auto_applied_change_costs_nothing_however_many_of_them_there_are() {
        let mut b = budget();
        for i in 0..10_000i64 {
            let d = b.reserve(Gate::AutoApply, &scopes(), i);
            assert!(
                d.is_granted(),
                "a change needing no human must never be throttled by a review budget"
            );
        }
        assert_eq!(b.committed(&BudgetScope::Project, 10_000), 0);
    }

    #[test]
    fn the_tightest_scope_is_the_one_that_refuses() {
        let mut b = budget();
        // Branch ceiling is 4 and the agent's is 6, so the branch runs out first.
        for _ in 0..4 {
            assert!(b.reserve(Gate::Confirm, &scopes(), 0).is_granted());
        }
        match b.reserve(Gate::Confirm, &scopes(), 0) {
            BudgetDecision::Exhausted { scope, ceiling, .. } => {
                assert_eq!(scope, BudgetScope::Branch(7));
                assert_eq!(ceiling, 4);
            }
            other => panic!("expected exhaustion on the branch scope, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_proposal_is_refused_and_never_downgraded() {
        // The property that makes this safe to add at all: exhaustion has one
        // outcome, and it is not "let it through with a weaker gate".
        let mut b = ReviewBudget::new(BudgetPolicy {
            project_ceiling: 0,
            branch_ceiling: 0,
            agent_ceiling: 0,
            ..BudgetPolicy::default()
        });
        for gate in [Gate::Confirm, Gate::ShadowValidate] {
            let d = b.reserve(gate, &scopes(), 0);
            assert!(
                !d.is_granted(),
                "{gate:?} must be refused at a zero ceiling"
            );
            assert!(d.reservation().is_none());
        }
        // ...and a free change still passes, because refusing those would make
        // an exhausted budget a total outage.
        assert!(b.reserve(Gate::AutoApply, &scopes(), 0).is_granted());
    }

    #[test]
    fn a_held_reservation_counts_against_the_ceiling_before_it_is_settled() {
        // Otherwise an agent opens a hundred proposals, settles none, and is
        // never refused — the exact backlog this exists to prevent.
        let mut b = budget();
        for _ in 0..4 {
            assert!(b.reserve(Gate::Confirm, &scopes(), 0).is_granted());
        }
        assert_eq!(b.outstanding(), 4);
        assert!(!b.reserve(Gate::Confirm, &scopes(), 0).is_granted());
    }

    #[test]
    fn withdrawing_refunds_and_approving_does_not() {
        let mut b = budget();
        let held = b
            .reserve(Gate::Confirm, &scopes(), 0)
            .reservation()
            .unwrap();
        b.settle(held, Settlement::Withdrawn, 10).unwrap();
        assert_eq!(b.committed(&BudgetScope::Branch(7), 10), 0);

        let held = b
            .reserve(Gate::Confirm, &scopes(), 20)
            .reservation()
            .unwrap();
        b.settle(held, Settlement::Approved, 30).unwrap();
        assert_eq!(b.committed(&BudgetScope::Branch(7), 30), 1);
    }

    #[test]
    fn rejecting_a_proposal_still_costs_what_reviewing_it_cost() {
        // Refunding rejections would make generating bad proposals free, which
        // inverts the incentive this whole mechanism exists to set.
        let mut b = budget();
        let held = b
            .reserve(Gate::Confirm, &scopes(), 0)
            .reservation()
            .unwrap();
        b.settle(held, Settlement::Rejected, 10).unwrap();
        assert_eq!(b.committed(&BudgetScope::Project, 10), 1);
    }

    #[test]
    fn an_expired_proposal_cannot_be_approved_late() {
        // The safety-relevant half of expiry. Releasing the budget while leaving
        // the proposal promotable would be worse than not expiring it at all.
        let mut b = budget();
        let ttl = b.policy().proposal_ttl_ms;
        let held = b
            .reserve(Gate::Confirm, &scopes(), 0)
            .reservation()
            .unwrap();

        let err = b.settle(held, Settlement::Approved, ttl + 1).unwrap_err();
        assert!(
            matches!(err, SettleError::Expired { .. }),
            "expected Expired, got {err:?}"
        );
        // And the budget came back.
        assert_eq!(b.committed(&BudgetScope::Branch(7), ttl + 1), 0);
    }

    #[test]
    fn a_reservation_exactly_at_its_ttl_has_not_expired() {
        let mut b = budget();
        let ttl = b.policy().proposal_ttl_ms;
        let held = b
            .reserve(Gate::Confirm, &scopes(), 0)
            .reservation()
            .unwrap();
        assert!(
            b.settle(held, Settlement::Approved, ttl).is_ok(),
            "a proposal at exactly its deadline is still inside it"
        );
    }

    #[test]
    fn settling_twice_does_not_refund_twice() {
        let mut b = budget();
        let held = b
            .reserve(Gate::Confirm, &scopes(), 0)
            .reservation()
            .unwrap();
        b.settle(held, Settlement::Approved, 10).unwrap();
        let again = b.settle(held, Settlement::Withdrawn, 20);
        assert!(
            matches!(again, Err(SettleError::Unknown(_))),
            "a second settle must not be able to refund a spend, got {again:?}"
        );
        assert_eq!(b.committed(&BudgetScope::Project, 20), 1);
    }

    #[test]
    fn spend_leaves_the_window_and_the_budget_recovers() {
        let mut b = budget();
        let window = b.policy().window_ms;
        for _ in 0..4 {
            let held = b
                .reserve(Gate::Confirm, &scopes(), 0)
                .reservation()
                .unwrap();
            b.settle(held, Settlement::Approved, 0).unwrap();
        }
        assert!(!b.reserve(Gate::Confirm, &scopes(), 1).is_granted());
        assert!(
            b.reserve(Gate::Confirm, &scopes(), window + 1).is_granted(),
            "a full window later the budget is available again"
        );
    }

    #[test]
    fn a_stricter_gate_never_costs_less_than_a_weaker_one() {
        // The ratio between the costs is a default and may be tuned. The
        // ordering is not tunable, and this is what pins it: a change that needs
        // more review must never be cheaper to propose.
        let p = BudgetPolicy::default();
        let auto = ReviewCost::of(Gate::AutoApply, &p).0;
        let confirm = ReviewCost::of(Gate::Confirm, &p).0;
        let shadow = ReviewCost::of(Gate::ShadowValidate, &p).0;
        assert!(
            auto <= confirm && confirm <= shadow,
            "{auto} {confirm} {shadow}"
        );
        assert_eq!(auto, 0, "a change needing no human must be free");
    }

    #[test]
    fn the_ceilings_are_ordered_so_every_scope_can_bind() {
        // Found by integration rather than by unit test. `branch_ceiling` and
        // `agent_ceiling` were both 20, which meant that on a project where
        // everyone works on one branch the branch ceiling bound at exactly the
        // moment the first agent's did — and then blocked every other agent.
        //
        // A scope whose ceiling is not strictly between the ones either side of
        // it cannot express anything the others do not already say.
        let policy = BudgetPolicy::default();
        assert!(
            policy.agent_ceiling < policy.branch_ceiling,
            "one agent must not be able to exhaust a whole branch"
        );
        assert!(
            policy.branch_ceiling < policy.project_ceiling,
            "one branch must not be able to exhaust a whole project"
        );
    }

    #[test]
    fn a_second_agent_can_keep_working_on_a_branch_the_first_exhausted_itself_on() {
        // The consequence of the ordering above, stated as the behaviour rather
        // than as the arithmetic.
        let mut b = ReviewBudget::new(BudgetPolicy::default());
        let branch = BudgetScope::Branch(0);
        let first = vec![
            BudgetScope::Project,
            branch.clone(),
            BudgetScope::Agent("a".into()),
        ];
        let second = vec![BudgetScope::Project, branch, BudgetScope::Agent("b".into())];

        let mut spent = 0;
        while b.reserve(Gate::Confirm, &first, 0).is_granted() {
            spent += 1;
            assert!(spent < 1_000, "the first agent never ran out");
        }

        assert!(
            b.reserve(Gate::Confirm, &second, 0).is_granted(),
            "a second agent was blocked by the first agent's spend on a shared branch"
        );
    }

    #[test]
    fn one_agents_exhaustion_does_not_starve_another() {
        // The reason `Agent` is a scope at all. Without it a single misbehaving
        // agent consumes the project ceiling and every other agent stops.
        let mut b = budget();
        let a = vec![BudgetScope::Project, BudgetScope::Agent("a".into())];
        let c = vec![BudgetScope::Project, BudgetScope::Agent("c".into())];
        for _ in 0..6 {
            assert!(b.reserve(Gate::Confirm, &a, 0).is_granted());
        }
        assert!(!b.reserve(Gate::Confirm, &a, 0).is_granted());
        assert!(
            b.reserve(Gate::Confirm, &c, 0).is_granted(),
            "a different agent still has its own budget"
        );
    }
}
