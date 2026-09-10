//! Verifying the log continuously rather than on demand (ROADMAP-V3 M20).
//!
//! # "We could detect tampering" against "we would have detected it by Tuesday"
//!
//! `SegmentWal::verify_chain` is a full replay. It proves the chain holds and it
//! runs when somebody asks, which in practice is during an incident — the moment
//! at which the answer is least useful, because the question has already been
//! raised by something else.
//!
//! A background verifier changes what can be said. Given a bounded rate and a
//! recorded position, "when would we have noticed" becomes arithmetic rather
//! than a hope.
//!
//! # It must never claim to have verified what it has not
//!
//! This is the whole design constraint. A verifier that reports "chain OK" while
//! having covered a third of the log is worse than none, because it converts a
//! known unknown into a false certainty.
//!
//! So [`Verifier::progress`] reports **coverage**, not a verdict, and
//! [`Coverage::fully_verified_since`] is `None` until a pass has actually
//! completed. The word "verified" is reserved for what a pass finished.
//!
//! # Restarting is not progress
//!
//! A verifier that restarts from genesis whenever the log grows never finishes
//! on a busy instance, and reports increasing coverage the whole time. The
//! cursor advances monotonically and a new pass begins only when the previous
//! one ended — so the number this reports is elapsed *passes*, which is the
//! thing an operator actually wants to know.

use serde::{Deserialize, Serialize};
use theta_core::hash::ContentHash;
use theta_core::log::LogEntry;

/// How fast to verify, and therefore how long a full pass takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierPolicy {
    /// Entries checked per tick.
    ///
    /// Bounded because verification competes with serving. An unbounded
    /// verifier is one an operator turns off during the first busy hour, and a
    /// verifier that is off does not verify anything.
    pub entries_per_tick: usize,
    /// The longest a full pass may take before it is a finding.
    ///
    /// The number that turns this from a background job into a claim: a pass
    /// that has not finished within it means the verifier is not keeping up
    /// with the log, and nobody would have noticed tampering by Tuesday after
    /// all.
    pub max_pass_ms: i64,
}

impl Default for VerifierPolicy {
    fn default() -> Self {
        Self {
            entries_per_tick: 1_000,
            max_pass_ms: 24 * 60 * 60 * 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error(
        "entry {commit_id} on branch {branch} claims a predecessor ({prev_hash}) \
         that is not in the log"
    )]
    BrokenChain {
        commit_id: u64,
        branch: u64,
        prev_hash: String,
    },

    #[error(
        "the verifier has not completed a pass in {elapsed_ms}ms and the policy \
         allows {allowed_ms}ms: it is not keeping up with the log, so nothing \
         here should be read as 'the chain is intact'"
    )]
    NotKeepingUp { elapsed_ms: i64, allowed_ms: i64 },
}

/// How much has actually been checked.
///
/// Deliberately not a boolean. A verdict would have to be either optimistic
/// about the part not yet reached or pessimistic about the part already done,
/// and both are worse than saying how far it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    /// Entries checked in the pass currently running.
    pub checked_this_pass: usize,
    /// Entries in the log when this pass started.
    pub pass_target: usize,
    /// Completed passes.
    pub passes_completed: u64,
    /// When the last pass *finished*. `None` until one has.
    ///
    /// The field that stops this being read as a verdict: until a pass
    /// completes, no part of this structure says the chain is intact.
    pub fully_verified_since: Option<i64>,
    /// When the current pass began.
    pub pass_started_ms: i64,
}

impl Coverage {
    /// Whether a full pass has ever finished.
    pub fn has_completed_a_pass(&self) -> bool {
        self.fully_verified_since.is_some()
    }

    /// Fraction of the current pass done, in basis points.
    pub fn progress_basis_points(&self) -> u32 {
        if self.pass_target == 0 {
            return 10_000;
        }
        ((self.checked_this_pass.min(self.pass_target) as f64 / self.pass_target as f64) * 10_000.0)
            .round() as u32
    }
}

/// Walks the log at a bounded rate, forever.
#[derive(Debug, Clone)]
pub struct Verifier {
    policy: VerifierPolicy,
    cursor: usize,
    pass_target: usize,
    passes_completed: u64,
    fully_verified_since: Option<i64>,
    pass_started_ms: i64,
    /// Hashes seen in this pass, so a predecessor can be checked without
    /// re-walking.
    seen: std::collections::HashSet<ContentHash>,
}

impl Verifier {
    pub fn new(policy: VerifierPolicy, now_ms: i64) -> Self {
        Self {
            policy,
            cursor: 0,
            pass_target: 0,
            passes_completed: 0,
            fully_verified_since: None,
            pass_started_ms: now_ms,
            seen: Default::default(),
        }
    }

    pub fn progress(&self) -> Coverage {
        Coverage {
            checked_this_pass: self.cursor,
            pass_target: self.pass_target,
            passes_completed: self.passes_completed,
            fully_verified_since: self.fully_verified_since,
            pass_started_ms: self.pass_started_ms,
        }
    }

    /// Verify the next slice.
    ///
    /// `entries` is the whole log, oldest first. The verifier keeps its own
    /// cursor into it rather than being handed a slice, because deciding where
    /// to resume is the part that has to be got right: a caller that passed the
    /// wrong window would produce a verifier that reported progress over
    /// entries it never saw.
    pub fn tick(&mut self, entries: &[LogEntry], now_ms: i64) -> Result<Coverage, VerifyError> {
        // A pass covers the log as it was when the pass began. Entries appended
        // during a pass belong to the next one — otherwise a busy instance moves
        // the target faster than the verifier moves the cursor, and no pass ever
        // completes while coverage climbs reassuringly.
        if self.cursor == 0 {
            self.pass_target = entries.len();
            self.pass_started_ms = now_ms;
            self.seen.clear();
        }

        let end = (self.cursor + self.policy.entries_per_tick).min(self.pass_target);
        for entry in &entries[self.cursor..end] {
            if entry.prev_hash != ContentHash::ZERO && !self.seen.contains(&entry.prev_hash) {
                return Err(VerifyError::BrokenChain {
                    commit_id: entry.commit_id.0,
                    branch: entry.branch_id.0,
                    prev_hash: entry.prev_hash.to_hex(),
                });
            }
            self.seen.insert(entry.hash());
        }
        self.cursor = end;

        if self.cursor >= self.pass_target {
            self.passes_completed += 1;
            self.fully_verified_since = Some(now_ms);
            self.cursor = 0;
        }

        Ok(self.progress())
    }

    /// Whether the verifier is keeping up.
    ///
    /// Separate from [`Verifier::tick`] because it is a different question with
    /// a different answer: a tick that finds no broken link says nothing about
    /// whether the pass will ever finish.
    pub fn check_pace(&self, now_ms: i64) -> Result<(), VerifyError> {
        let reference = self.fully_verified_since.unwrap_or(self.pass_started_ms);
        let elapsed = now_ms.saturating_sub(reference);
        if elapsed > self.policy.max_pass_ms {
            return Err(VerifyError::NotKeepingUp {
                elapsed_ms: elapsed,
                allowed_ms: self.policy.max_pass_ms,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::{Author, CommitId, OpType};
    use theta_core::{BranchId, Value};

    fn chain(n: usize) -> Vec<LogEntry> {
        let mut entries: Vec<LogEntry> = Vec::new();
        let mut prev = ContentHash::ZERO;
        for i in 1..=n {
            let entry = LogEntry {
                prev_hash: prev,
                commit_id: CommitId(i as u64),
                branch_id: BranchId(0),
                op: OpType::Put {
                    key: format!("a:{i}"),
                    value: Value::Int(i as i64),
                },
                author: Author::System,
                timestamp_ms: i as i64,
            };
            prev = entry.hash();
            entries.push(entry);
        }
        entries
    }

    #[test]
    fn nothing_reads_as_verified_until_a_pass_has_finished() {
        // The design constraint. A verifier that reports "chain OK" having
        // covered a third of the log converts a known unknown into a false
        // certainty, which is worse than not running at all.
        let policy = VerifierPolicy {
            entries_per_tick: 10,
            ..VerifierPolicy::default()
        };
        let mut verifier = Verifier::new(policy, 0);
        let log = chain(100);

        let coverage = verifier.tick(&log, 1).unwrap();
        assert_eq!(coverage.checked_this_pass, 10);
        assert!(
            !coverage.has_completed_a_pass(),
            "ten of a hundred entries is not a verified log"
        );
        assert_eq!(coverage.progress_basis_points(), 1_000);
    }

    #[test]
    fn a_completed_pass_is_recorded_and_a_new_one_starts() {
        let policy = VerifierPolicy {
            entries_per_tick: 50,
            ..VerifierPolicy::default()
        };
        let mut verifier = Verifier::new(policy, 0);
        let log = chain(100);

        verifier.tick(&log, 1).unwrap();
        let coverage = verifier.tick(&log, 2).unwrap();

        assert_eq!(coverage.passes_completed, 1);
        assert_eq!(coverage.fully_verified_since, Some(2));
        assert_eq!(
            coverage.checked_this_pass, 0,
            "the cursor resets so the next pass starts from the beginning"
        );
    }

    #[test]
    fn a_busy_log_does_not_stop_a_pass_from_ever_completing() {
        // The failure a naive verifier has: if the target moves with the log, a
        // busy instance outruns the cursor forever while coverage climbs
        // reassuringly. Entries appended mid-pass belong to the next pass.
        let policy = VerifierPolicy {
            entries_per_tick: 10,
            ..VerifierPolicy::default()
        };
        let mut verifier = Verifier::new(policy, 0);
        let mut log = chain(20);

        verifier.tick(&log, 1).unwrap();
        // The log doubles mid-pass.
        log = chain(40);
        let coverage = verifier.tick(&log, 2).unwrap();

        assert_eq!(
            coverage.passes_completed, 1,
            "the pass covered the log as it was when the pass began"
        );
    }

    #[test]
    fn a_broken_link_is_found_and_named() {
        let policy = VerifierPolicy {
            entries_per_tick: 100,
            ..VerifierPolicy::default()
        };
        let mut verifier = Verifier::new(policy, 0);
        let mut log = chain(20);

        // Rewrite an entry in the middle: its successor's `prev_hash` now points
        // at something no longer in the log.
        log[9].timestamp_ms = 9_999;

        let err = verifier.tick(&log, 1).unwrap_err();
        match err {
            VerifyError::BrokenChain { commit_id, .. } => {
                assert_eq!(commit_id, 11, "the successor of the edited entry");
            }
            other => panic!("expected a broken chain, got {other:?}"),
        }
    }

    #[test]
    fn a_verifier_that_is_not_keeping_up_is_a_finding() {
        // The number that turns a background job into a claim. Without it,
        // "we would have detected it by Tuesday" is a hope.
        let policy = VerifierPolicy {
            entries_per_tick: 1,
            max_pass_ms: 1_000,
        };
        let mut verifier = Verifier::new(policy, 0);
        let log = chain(10_000);

        verifier.tick(&log, 1).unwrap();
        assert!(verifier.check_pace(500).is_ok());

        let err = verifier.check_pace(2_000).unwrap_err();
        assert!(matches!(err, VerifyError::NotKeepingUp { .. }), "{err:?}");
    }

    #[test]
    fn pace_is_a_separate_question_from_integrity() {
        // A tick that finds no broken link says nothing about whether the pass
        // will ever finish, and collapsing the two would let a verifier that is
        // hopelessly behind report success on every tick.
        let policy = VerifierPolicy {
            entries_per_tick: 1,
            max_pass_ms: 10,
        };
        let mut verifier = Verifier::new(policy, 0);
        let log = chain(1_000);

        // The tick starts the pass, so time has to pass before being behind is
        // a fact rather than a prediction. The first version of this test
        // checked the pace at the same instant the pass began and expected a
        // failure; `check_pace` was right to disagree.
        assert!(
            verifier.tick(&log, 0).is_ok(),
            "the slice it checked was intact"
        );
        assert!(
            verifier.check_pace(100).is_err(),
            "and a hundred milliseconds later it is nowhere near finishing"
        );
    }

    #[test]
    fn an_empty_log_completes_a_pass_rather_than_stalling() {
        // Otherwise a new instance never reports a completed pass, and
        // `check_pace` starts failing on a database nobody has written to.
        let mut verifier = Verifier::new(VerifierPolicy::default(), 0);
        let coverage = verifier.tick(&[], 1).unwrap();
        assert_eq!(coverage.passes_completed, 1);
        assert!(coverage.has_completed_a_pass());
        assert_eq!(coverage.progress_basis_points(), 10_000);
    }

    #[test]
    fn progress_is_reported_against_the_pass_and_not_against_the_log() {
        // A pass covers a fixed target. Reporting progress against a log that
        // grew during the pass would show coverage falling while the verifier
        // did nothing but work.
        let policy = VerifierPolicy {
            entries_per_tick: 5,
            ..VerifierPolicy::default()
        };
        let mut verifier = Verifier::new(policy, 0);
        let mut log = chain(10);

        let first = verifier.tick(&log, 1).unwrap();
        assert_eq!(first.progress_basis_points(), 5_000);

        log = chain(1_000);
        let second = verifier.tick(&log, 2).unwrap();
        assert_eq!(
            second.passes_completed, 1,
            "the pass finished against its own target"
        );
    }
}
