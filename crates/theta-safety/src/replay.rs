//! Replaying an agent session's decisions (ROADMAP-V3 M26).
//!
//! # What can and cannot be replayed
//!
//! Re-running an agent session exactly is impossible and will stay impossible: a
//! model is nondeterministic, so the same prompt against the same state can
//! produce a different proposal. Anything built on exact replay is built on
//! something that does not exist.
//!
//! What *is* deterministic is everything downstream of the proposal.
//! Classification is a pure function of change kind, row impact, reversibility
//! and branch protection (`docs/INVARIANTS.md` invariant 2) — that is the property
//! `exhaustive_classification.rs` checks — so given the proposals a session
//! actually made, **the gate outcomes are reproducible exactly**.
//!
//! So this replays the decisions rather than the session. It answers: given
//! these proposals against this state, under this policy, what would the Safety
//! Layer do?
//!
//! # The two questions it is for
//!
//! **Forensics.** An agent did something in March and somebody is asking why it
//! was allowed. Replaying the decisions answers that from the record rather than
//! from memory, and answers it with the policy that was in force rather than the
//! one in force today.
//!
//! **Regression-testing a rule change.** Somebody proposes tightening a
//! threshold. Replaying real history against the new policy says exactly which
//! past changes it would have caught and which previously-fine changes it would
//! now gate — which is the difference between a rule change somebody argued for
//! and one somebody measured.
//!
//! That second use is why [`Divergence`] distinguishes *directions*. A rule
//! change that gates more is a review-load question; one that gates less is a
//! safety question, and reporting them as one number would hide the second
//! inside the first.

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;

use crate::classify::{classify, Gate};
use crate::diff::Impact;
use crate::policy::SafetyPolicy;

/// One decision as it was actually taken.
///
/// Recorded from the audit trail rather than reconstructed: the impact is the
/// number the server measured at the time, and re-measuring it against today's
/// data would answer a different question from the one being asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedDecision {
    pub change_id: String,
    pub change: SchemaChange,
    /// What the server measured then. Not re-derived.
    pub impact: Impact,
    pub protected: bool,
    pub branch_id: u64,
    /// The gate that was actually applied.
    pub gate: Gate,
    pub at_ms: i64,
}

/// How a replayed decision differs from the recorded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// The replay agrees with what happened.
    Same,
    /// The replay gates it harder than it was gated.
    ///
    /// A review-load question. Under a proposed policy this is what it would
    /// newly catch; in forensics it means the rules have been tightened since.
    Stricter,
    /// The replay gates it *less* than it was gated.
    ///
    /// A **safety** question, and the direction that matters. Under a proposed
    /// policy this is what the change would stop catching; in forensics it means
    /// something that was reviewed then would sail through now.
    ///
    /// Reported separately rather than folded into a difference count, because a
    /// single number lets a hundred harmless tightenings hide one loosening.
    Looser,
}

/// One replayed decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayedDecision {
    pub change_id: String,
    pub recorded_gate: Gate,
    pub replayed_gate: Gate,
    pub divergence: Divergence,
    /// Why the replayed gate is what it is, in the rationale's own words.
    pub reason: String,
}

/// What a whole replay found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    pub decisions: Vec<ReplayedDecision>,
    pub agreed: usize,
    pub stricter: usize,
    /// **The number to look at.** Kept as its own field rather than derivable
    /// from the list, because a report somebody skims should put the safety
    /// direction where it cannot be missed.
    pub looser: usize,
}

impl ReplayReport {
    /// Whether replaying found anything that would now be allowed through more
    /// easily than it was.
    pub fn has_loosened(&self) -> bool {
        self.looser > 0
    }

    /// The decisions that would now be gated less. The ones to read.
    pub fn loosened(&self) -> impl Iterator<Item = &ReplayedDecision> {
        self.decisions
            .iter()
            .filter(|d| d.divergence == Divergence::Looser)
    }

    /// A line for a person deciding whether to ship a rule change.
    pub fn summary(&self) -> String {
        if self.decisions.is_empty() {
            return "nothing to replay: no decisions were recorded".into();
        }
        format!(
            "{} decisions replayed: {} unchanged, {} would now be gated harder, \
             {} would now be gated LESS",
            self.decisions.len(),
            self.agreed,
            self.stricter,
            self.looser
        )
    }
}

/// Strictness order, so a divergence has a direction.
fn strictness(gate: Gate) -> u8 {
    match gate {
        Gate::AutoApply => 0,
        Gate::Confirm => 1,
        Gate::ShadowValidate => 2,
    }
}

/// Replay a session's decisions under a policy.
///
/// Pass the policy that was in force to ask a forensic question, or a proposed
/// one to ask what a rule change would do. The function is the same; only the
/// policy differs, which is the point — a separate "what-if" path would be a
/// second implementation of the decision, free to disagree with the first.
pub fn replay(decisions: &[RecordedDecision], policy: &SafetyPolicy) -> ReplayReport {
    let mut replayed = Vec::with_capacity(decisions.len());
    let (mut agreed, mut stricter, mut looser) = (0, 0, 0);

    for recorded in decisions {
        let diff = classify(
            &recorded.change,
            recorded.impact,
            policy,
            recorded.protected,
            recorded.branch_id,
        );

        let divergence = match strictness(diff.gate).cmp(&strictness(recorded.gate)) {
            std::cmp::Ordering::Equal => {
                agreed += 1;
                Divergence::Same
            }
            std::cmp::Ordering::Greater => {
                stricter += 1;
                Divergence::Stricter
            }
            std::cmp::Ordering::Less => {
                looser += 1;
                Divergence::Looser
            }
        };

        replayed.push(ReplayedDecision {
            change_id: recorded.change_id.clone(),
            recorded_gate: recorded.gate,
            replayed_gate: diff.gate,
            divergence,
            reason: diff.reason,
        });
    }

    ReplayReport {
        decisions: replayed,
        agreed,
        stricter,
        looser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::schema::{FieldDef, SchemaChange};
    use theta_core::ValueType;

    fn drop_table(rows: u64) -> RecordedDecision {
        RecordedDecision {
            change_id: format!("chg_drop_{rows}"),
            change: SchemaChange::DropTable {
                table: "orders".into(),
            },
            impact: Impact::new(rows, 10),
            protected: true,
            branch_id: 0,
            gate: Gate::Confirm,
            at_ms: 1_000,
        }
    }

    fn add_column(rows: u64) -> RecordedDecision {
        RecordedDecision {
            change_id: format!("chg_add_{rows}"),
            change: SchemaChange::AddColumn {
                table: "orders".into(),
                field: FieldDef {
                    name: "currency".into(),
                    ty: ValueType::Text,
                    nullable: true,
                    crdt: None,
                    declared_at: None,
                },
            },
            impact: Impact::new(rows, 10),
            protected: true,
            branch_id: 0,
            gate: Gate::AutoApply,
            at_ms: 2_000,
        }
    }

    #[test]
    fn replaying_the_policy_that_was_in_force_reproduces_what_happened() {
        // The property that makes this worth anything: classification is a pure
        // function of the recorded facts, so the same facts under the same
        // policy give the same answer. If this ever failed, every other use of
        // replay would be reporting noise.
        let policy = SafetyPolicy::protected();
        let recorded: Vec<RecordedDecision> = [10u64, 50, 200]
            .iter()
            .map(|rows| {
                let mut decision = drop_table(*rows);
                // Take the gate from the classifier itself, which is what the
                // server would have recorded.
                decision.gate = classify(
                    &decision.change,
                    decision.impact,
                    &policy,
                    decision.protected,
                    decision.branch_id,
                )
                .gate;
                decision
            })
            .collect();

        let report = replay(&recorded, &policy);
        assert_eq!(report.agreed, 3);
        assert_eq!(report.looser, 0);
        assert_eq!(report.stricter, 0);
        assert!(!report.has_loosened());
    }

    #[test]
    fn a_tightened_rule_reports_what_it_would_newly_catch() {
        // The regression-test use. "This threshold change catches four more
        // things" is a measurement; "this feels safer" is not.
        let was = SafetyPolicy {
            row_impact_threshold: 10_000,
            ..SafetyPolicy::protected()
        };
        let proposed = SafetyPolicy {
            row_impact_threshold: 10,
            ..SafetyPolicy::protected()
        };

        let mut decision = add_column(500);
        decision.gate = classify(&decision.change, decision.impact, &was, true, 0).gate;
        assert_eq!(decision.gate, Gate::AutoApply, "it was fine before");

        let report = replay(std::slice::from_ref(&decision), &proposed);
        assert_eq!(report.stricter, 1);
        assert_eq!(report.looser, 0);
        assert!(report.summary().contains("1 would now be gated harder"));
    }

    #[test]
    fn a_loosened_rule_is_reported_separately_because_it_is_the_dangerous_one() {
        // A single "differences" count would let a hundred harmless tightenings
        // hide one loosening. The direction that matters gets its own field.
        let was = SafetyPolicy {
            row_impact_threshold: 10,
            ..SafetyPolicy::protected()
        };
        let proposed = SafetyPolicy {
            row_impact_threshold: 100_000,
            ..SafetyPolicy::protected()
        };

        let mut decision = add_column(500);
        decision.gate = classify(&decision.change, decision.impact, &was, true, 0).gate;
        assert_eq!(decision.gate, Gate::Confirm, "it was gated before");

        let report = replay(std::slice::from_ref(&decision), &proposed);
        assert!(report.has_loosened());
        assert_eq!(report.looser, 1);
        assert_eq!(report.loosened().count(), 1);
        assert!(report.summary().contains("1 would now be gated LESS"));
    }

    #[test]
    fn one_loosening_is_visible_among_many_tightenings() {
        // The scenario the separate field exists for, checked rather than
        // assumed: a rule change that gates fifty more things and one fewer
        // must not read as "51 differences, mostly good".
        let was = SafetyPolicy {
            row_impact_threshold: 400,
            ..SafetyPolicy::protected()
        };
        let proposed = SafetyPolicy {
            row_impact_threshold: 100,
            treat_ambiguous_as_safe: true,
            ..SafetyPolicy::protected()
        };

        let mut recorded: Vec<RecordedDecision> = (0..50)
            .map(|i| {
                let mut d = add_column(300 + i);
                d.change_id = format!("chg_add_{i}");
                d.gate = classify(&d.change, d.impact, &was, true, 0).gate;
                d
            })
            .collect();

        // One ambiguous change the old policy treated as destructive and the
        // new one waves through.
        //
        // The first version of this test tried to loosen the *irreversible*
        // shadow threshold and could not: `effective_irreversible_shadow_threshold`
        // caps what a project may ask for, so raising it has no effect
        // (`specs/07` §4). The cap was doing its job and the test was trying to
        // construct something the design forbids — which is a good way to find
        // out that it holds.
        let mut loosened = RecordedDecision {
            change_id: "chg_rename".into(),
            change: SchemaChange::RenameColumn {
                table: "orders".into(),
                from: "total".into(),
                to: "amount".into(),
            },
            impact: Impact::new(5, 10),
            protected: true,
            branch_id: 0,
            gate: Gate::Confirm,
            at_ms: 3_000,
        };
        loosened.gate = classify(&loosened.change, loosened.impact, &was, true, 0).gate;
        assert_eq!(
            loosened.gate,
            Gate::Confirm,
            "ambiguous is destructive until a project says otherwise"
        );
        recorded.push(loosened);

        let report = replay(&recorded, &proposed);
        assert_eq!(report.stricter, 50);
        assert_eq!(
            report.looser, 1,
            "the one loosening must be counted on its own"
        );
        assert_eq!(report.loosened().next().unwrap().change_id, "chg_rename");
    }

    #[test]
    fn the_impact_is_replayed_as_recorded_rather_than_re_measured() {
        // A forensic replay asks what the server decided *then*. Re-measuring
        // against today's data would answer a different question and would
        // silently exonerate a decision that was wrong at the time.
        let policy = SafetyPolicy::protected();
        let mut decision = drop_table(50_000);
        decision.gate = Gate::ShadowValidate;

        let report = replay(std::slice::from_ref(&decision), &policy);
        assert_eq!(report.agreed, 1);
        assert!(report.decisions[0].reason.contains("50000"));
    }

    #[test]
    fn a_replayed_decision_carries_the_reason_for_the_gate_it_got() {
        // The reason is what somebody reading a forensic replay actually needs.
        // A report of gate names alone answers "what" and never "why", which is
        // the question that started the investigation.
        let policy = SafetyPolicy::protected();
        let decision = drop_table(200);
        let report = replay(std::slice::from_ref(&decision), &policy);
        assert!(!report.decisions[0].reason.is_empty());
        assert!(
            report.decisions[0].reason.contains("irreversible"),
            "a 200-row drop on a protected branch is over the shadow threshold, and the reason should say which rule that was: {}",
            report.decisions[0].reason
        );
    }

    #[test]
    fn replaying_nothing_says_so_rather_than_reporting_perfect_agreement() {
        // Zero disagreements over zero decisions is a true statement that reads
        // like a passing result, which is exactly the shape that gets quoted.
        let report = replay(&[], &SafetyPolicy::protected());
        assert_eq!(report.agreed, 0);
        assert!(!report.has_loosened());
        assert!(report.summary().contains("nothing to replay"));
    }

    #[test]
    fn replay_and_the_live_classifier_are_the_same_function() {
        // A separate "what-if" path would be a second implementation of the
        // decision, free to disagree with the first. This pins that replay goes
        // through `classify` rather than through a copy of its rules.
        let policy = SafetyPolicy::development();
        let decision = drop_table(700);
        let report = replay(std::slice::from_ref(&decision), &policy);

        let live = classify(&decision.change, decision.impact, &policy, true, 0);
        assert_eq!(report.decisions[0].replayed_gate, live.gate);
        assert_eq!(report.decisions[0].reason, live.reason);
    }
}
