//! Publishing the head somewhere we do not control (ROADMAP-V3 M20).
//!
//! # The gap this closes, and the one it does not
//!
//! The hash chain detects an edit anywhere except the newest entry: nothing
//! inside the log commits to the tail, so truncating it is indistinguishable
//! from those writes never having happened. `claims.toml` carries that as the
//! non-claim `the-newest-entry-is-not-tamper-evident`, and closing it needs an
//! anchor the disk does not control.
//!
//! An anchor is the head hash, published outside. Afterwards, rewriting history
//! below that point produces a log whose head no longer matches what was
//! published, and we cannot un-publish it.
//!
//! **It closes the gap up to the last anchor and not one entry further.**
//! Everything written since is exactly as unprotected as before. So the
//! interval between anchors *is* the size of the window, and this module treats
//! that as the number that matters rather than as an implementation detail.
//!
//! # The attack that makes a missing anchor an alarm
//!
//! We cannot retract a published anchor. We can decline to publish the next one.
//!
//! An operator rewriting history would therefore stop anchoring first, and a
//! verifier that only checked the anchors it *had* would find them all
//! consistent and report success. The absence is the evidence, so
//! [`AnchorLog::verify`] fails on a gap longer than the policy allows — silence
//! is a finding, not an absence of findings.
//!
//! This is the whole reason the schedule is part of the design rather than
//! something an operator configures in a cron file nobody reads.
//!
//! # What an anchor is worth depends on where it goes
//!
//! [`AnchorSink`] is a trait because the destinations differ in what they
//! actually guarantee, and the difference is not ours to paper over:
//!
//! - A **counterparty** who keeps their own copy is the strongest ordinary
//!   option: they can produce the receipt independently, and colluding requires
//!   two organisations rather than one.
//! - A **public transparency log** is checkable by anyone, and depends on the
//!   log operator not rewriting.
//! - A **public chain** is the most expensive and the hardest to rewrite.
//!
//! What none of them do is make an anchor meaningful if nobody ever checks it.
//! Publishing is the cheap half.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use theta_core::branch::BranchId;
use theta_core::hash::ContentHash;

/// One published head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    /// The branch this anchors. Anchoring `main` says nothing about a preview
    /// branch, and pretending otherwise would be the more dangerous error.
    pub branch: BranchId,
    /// The head at the moment of publication.
    pub head: ContentHash,
    /// How many entries that head covers, so a later anchor can be checked for
    /// going backwards.
    pub entries_covered: u64,
    pub published_at_ms: i64,
    /// Whatever the sink returned as proof it accepted this: a transaction id,
    /// a signed timestamp, a log index.
    ///
    /// Opaque here on purpose. Interpreting it is the sink's job, and a receipt
    /// this module could parse would be one this module could also fabricate.
    pub receipt: String,
}

/// Where anchors go.
/// Implementors are used behind `dyn` — where anchors go is a deployment
/// decision, and a generic parameter on the engine would make every caller name
/// it. The methods on [`AnchorLog`] therefore take `S: AnchorSink + ?Sized`.
/// A sink is a *destination*, and what it stores is compared on the facts it
/// was given — never on the receipt.
///
/// The receipt is the lookup key. Comparing it to itself proves nothing, and
/// requiring it to match set a trap that a correct-looking sink walked straight
/// into: `publish` receives an anchor whose `receipt` field is still empty,
/// because the receipt is the thing `publish` is in the middle of producing. A
/// sink that stored what it was handed therefore failed verification as
/// `ReceiptNotHonoured` — a message accusing the counterparty of dishonesty when
/// the only fault was an implementor taking the argument at face value.
///
/// Found by an integration test that had first been written the easy way, where
/// it passed while exercising nothing.
pub trait AnchorSink {
    /// Publish `head` and return a receipt.
    fn publish(&mut self, anchor: &Anchor) -> Result<String, AnchorError>;

    /// Ask the sink to produce what it holds for a receipt.
    ///
    /// Used by verification. A sink that cannot answer this is a sink that only
    /// ever proves we *sent* something, which is a weaker thing than it looks:
    /// it is our own record of our own action.
    fn recall(&self, receipt: &str) -> Result<Option<Anchor>, AnchorError>;

    /// A name for the destination, for the audit trail and for a human deciding
    /// how much the anchor is worth.
    fn describe(&self) -> String;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnchorError {
    #[error("the anchor sink is unreachable: {detail}")]
    Unreachable { detail: String },

    #[error("the sink rejected the anchor: {detail}")]
    Rejected { detail: String },

    #[error(
        "anchor for branch {branch} covers {covered} entries, fewer than the \
         {previous} the last anchor covered: history has been rewritten, or an \
         anchor is being replayed"
    )]
    WentBackwards {
        branch: u64,
        covered: u64,
        previous: u64,
    },

    #[error(
        "the head anchored at {published_at_ms} is not in the log any more; \
         history below it has been rewritten"
    )]
    AnchoredHeadMissing { head: String, published_at_ms: i64 },

    #[error(
        "the sink does not have the anchor we recorded under receipt `{receipt}`; \
         either it was never published or the sink has lost or altered it"
    )]
    ReceiptNotHonoured { receipt: String },

    #[error(
        "no anchor for branch {branch} in {gap_ms}ms, and the policy allows \
         {allowed_ms}ms. An operator rewriting history would stop anchoring \
         first, so silence is the finding."
    )]
    Stale {
        branch: u64,
        gap_ms: i64,
        allowed_ms: i64,
    },

    #[error("nothing has ever been anchored for branch {branch}")]
    NeverAnchored { branch: u64 },
}

/// How often anchoring must happen, and therefore how large the unprotected
/// window is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorPolicy {
    /// The longest tolerable gap between anchors.
    ///
    /// **This is the window in which the tail is unprotected**, stated as such.
    /// An hour means an attacker with disk access has an hour of writes they can
    /// rewrite undetectably, and no amount of anchoring changes that — only a
    /// shorter interval does.
    pub max_gap_ms: i64,
    /// Whether a branch that has never been anchored is a failure.
    ///
    /// True by default. A verifier that treated "never anchored" as "nothing to
    /// check" would report success for a log nobody has ever protected, which is
    /// the most confident wrong answer available here.
    pub require_first_anchor: bool,
}

impl Default for AnchorPolicy {
    /// An hour. Short enough that an undetectable rewrite covers minutes of work
    /// rather than a day of it, long enough that a sink outage does not page
    /// anyone immediately.
    fn default() -> Self {
        Self {
            max_gap_ms: 60 * 60 * 1_000,
            require_first_anchor: true,
        }
    }
}

/// Anchors we have published, per branch.
#[derive(Debug, Clone, Default)]
pub struct AnchorLog {
    by_branch: BTreeMap<BranchId, Vec<Anchor>>,
}

impl AnchorLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn anchors(&self, branch: BranchId) -> &[Anchor] {
        self.by_branch
            .get(&branch)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn latest(&self, branch: BranchId) -> Option<&Anchor> {
        self.anchors(branch).last()
    }

    /// Publish a head and record the receipt.
    ///
    /// Refuses an anchor that covers fewer entries than the last one for the
    /// same branch. That is either history being rewritten or an old anchor
    /// being replayed, and both are exactly what this exists to catch — so it is
    /// refused *before* publication rather than published and noticed later.
    pub fn publish<S: AnchorSink + ?Sized>(
        &mut self,
        sink: &mut S,
        branch: BranchId,
        head: ContentHash,
        entries_covered: u64,
        now_ms: i64,
    ) -> Result<&Anchor, AnchorError> {
        if let Some(previous) = self.latest(branch) {
            if entries_covered < previous.entries_covered {
                return Err(AnchorError::WentBackwards {
                    branch: branch.0,
                    covered: entries_covered,
                    previous: previous.entries_covered,
                });
            }
        }

        let mut anchor = Anchor {
            branch,
            head,
            entries_covered,
            published_at_ms: now_ms,
            receipt: String::new(),
        };
        anchor.receipt = sink.publish(&anchor)?;

        let anchors = self.by_branch.entry(branch).or_default();
        anchors.push(anchor);
        Ok(anchors.last().expect("just pushed"))
    }

    /// Check every anchor against the log and against the clock.
    ///
    /// `contains` answers whether a hash is still in the log. Passed as a
    /// closure rather than taking a store, so this can be checked against a
    /// restored archive as easily as against a live instance — a restore whose
    /// anchors do not verify is a restore of something other than what was
    /// backed up, which is worth knowing before it is put into service.
    pub fn verify<S: AnchorSink + ?Sized>(
        &self,
        sink: &S,
        branch: BranchId,
        policy: &AnchorPolicy,
        now_ms: i64,
        contains: impl Fn(&ContentHash) -> bool,
    ) -> Result<Verified, AnchorError> {
        let anchors = self.anchors(branch);

        if anchors.is_empty() {
            return if policy.require_first_anchor {
                Err(AnchorError::NeverAnchored { branch: branch.0 })
            } else {
                Ok(Verified {
                    anchors_checked: 0,
                    entries_protected: 0,
                    unprotected_since_ms: None,
                })
            };
        }

        let mut previous_covered = 0u64;
        for anchor in anchors {
            // The log must still contain what we published. This is the whole
            // point: a rewrite below the anchored head changes it, and the
            // published copy does not change with it.
            if !contains(&anchor.head) {
                return Err(AnchorError::AnchoredHeadMissing {
                    head: anchor.head.to_hex(),
                    published_at_ms: anchor.published_at_ms,
                });
            }

            // The sink must still have what it told us it took. Without this,
            // an anchor is our own record of our own action.
            match sink.recall(&anchor.receipt)? {
                Some(held) if held.attests_same_as(anchor) => {}
                _ => {
                    return Err(AnchorError::ReceiptNotHonoured {
                        receipt: anchor.receipt.clone(),
                    })
                }
            }

            if anchor.entries_covered < previous_covered {
                return Err(AnchorError::WentBackwards {
                    branch: branch.0,
                    covered: anchor.entries_covered,
                    previous: previous_covered,
                });
            }
            previous_covered = anchor.entries_covered;
        }

        let last = anchors.last().expect("non-empty");
        let gap = now_ms.saturating_sub(last.published_at_ms);
        if gap > policy.max_gap_ms {
            return Err(AnchorError::Stale {
                branch: branch.0,
                gap_ms: gap,
                allowed_ms: policy.max_gap_ms,
            });
        }

        Ok(Verified {
            anchors_checked: anchors.len(),
            entries_protected: last.entries_covered,
            unprotected_since_ms: Some(last.published_at_ms),
        })
    }
}

impl Anchor {
    /// Whether two anchors say the same thing about the log.
    ///
    /// Compares the claim — branch, head, coverage and when it was published —
    /// and not the receipt, which is the key the comparison was reached through.
    pub fn attests_same_as(&self, other: &Anchor) -> bool {
        self.branch == other.branch
            && self.head == other.head
            && self.entries_covered == other.entries_covered
            && self.published_at_ms == other.published_at_ms
    }
}

/// What verification established, stated so a caller cannot overread it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub anchors_checked: usize,
    /// Entries the newest anchor covers. **Everything after this is not
    /// protected by any anchor**, which is the sentence this field exists to
    /// make unavoidable.
    pub entries_protected: u64,
    /// When protection stops. `None` when nothing has ever been anchored.
    pub unprotected_since_ms: Option<i64>,
}

/// A sink that keeps anchors in memory.
///
/// For tests and for a single-process deployment that wants the *shape* without
/// the guarantee. It is deliberately named for what it is: an anchor written to
/// the same machine as the log protects against nothing, because an attacker
/// with the log has this too.
#[derive(Debug, Default)]
pub struct InMemorySink {
    held: BTreeMap<String, Anchor>,
    next: u64,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a sink that loses or rewrites what it was given.
    ///
    /// Exists so the "receipt not honoured" path is exercised. A sink that can
    /// only behave is a sink whose failure handling has never run.
    pub fn forget(&mut self, receipt: &str) {
        self.held.remove(receipt);
    }
}

impl AnchorSink for InMemorySink {
    fn publish(&mut self, anchor: &Anchor) -> Result<String, AnchorError> {
        self.next += 1;
        let receipt = format!("mem-{}", self.next);
        self.held.insert(
            receipt.clone(),
            Anchor {
                receipt: receipt.clone(),
                ..anchor.clone()
            },
        );
        Ok(receipt)
    }

    fn recall(&self, receipt: &str) -> Result<Option<Anchor>, AnchorError> {
        Ok(self.held.get(receipt).cloned())
    }

    fn describe(&self) -> String {
        "in-memory (protects against nothing; for tests and for shape)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: BranchId = BranchId::MAIN;
    const HOUR: i64 = 60 * 60 * 1_000;

    /// A distinct head per byte, so a test can name the one it means.
    fn head(byte: u8) -> ContentHash {
        ContentHash::of(&[byte; 32])
    }

    /// A log containing exactly these hashes and nothing else.
    fn log_with(hashes: Vec<ContentHash>) -> impl Fn(&ContentHash) -> bool {
        move |h| hashes.contains(h)
    }

    /// A sink that stores exactly what `publish` handed it.
    ///
    /// The naive-but-reasonable implementation. It must verify, or the trait
    /// requires implementors to divine a field the caller left empty.
    #[derive(Debug, Default)]
    struct LiteralSink {
        held: BTreeMap<String, Anchor>,
        next: u64,
    }

    impl AnchorSink for LiteralSink {
        fn publish(&mut self, anchor: &Anchor) -> Result<String, AnchorError> {
            self.next += 1;
            let receipt = format!("lit-{}", self.next);
            self.held.insert(receipt.clone(), anchor.clone());
            Ok(receipt)
        }
        fn recall(&self, receipt: &str) -> Result<Option<Anchor>, AnchorError> {
            Ok(self.held.get(receipt).cloned())
        }
        fn describe(&self) -> String {
            "literal".into()
        }
    }

    #[test]
    fn a_sink_that_stores_what_it_was_handed_verifies() {
        // Regression. Verification compared the whole anchor including its
        // receipt, but `publish` receives the anchor *before* the receipt exists
        // — so a sink storing its argument unchanged was accused of not
        // honouring a receipt it had honoured perfectly.
        let mut log = AnchorLog::new();
        let mut sink = LiteralSink::default();
        let head = ContentHash::of(b"head");

        log.publish(&mut sink, BranchId::MAIN, head, 10, 1_000)
            .expect("publish");

        log.verify(
            &sink,
            BranchId::MAIN,
            &AnchorPolicy::default(),
            1_001,
            |h| *h == head,
        )
        .expect("a sink that stored what it was given must verify");
    }

    #[test]
    fn a_sink_that_changes_any_part_of_the_claim_still_fails() {
        // The relaxation must not have made this vacuous: the receipt stopped
        // being compared, so what is left has to still catch a sink that altered
        // the claim.
        //
        // Every field is exercised, not one. Written the obvious way it altered
        // only `entries_covered`, and a planted violation deleting `head` from
        // the comparison went unnoticed — the worst possible field to miss.
        // `contains` checks that *our* copy's head is still in the log; nothing
        // checked that the sink's copy names the same head, so a counterparty
        // could attest to an entirely different log state and verify.
        #[derive(Debug, Default)]
        struct LyingSink {
            held: BTreeMap<String, Anchor>,
            alter: u8,
        }
        impl AnchorSink for LyingSink {
            fn publish(&mut self, anchor: &Anchor) -> Result<String, AnchorError> {
                let receipt = "lie-1".to_string();
                let mut held = anchor.clone();
                match self.alter {
                    0 => held.entries_covered += 1,
                    1 => held.head = head(9),
                    2 => held.published_at_ms += 1,
                    _ => held.branch = BranchId(99),
                }
                self.held.insert(receipt.clone(), held);
                Ok(receipt)
            }
            fn recall(&self, receipt: &str) -> Result<Option<Anchor>, AnchorError> {
                Ok(self.held.get(receipt).cloned())
            }
            fn describe(&self) -> String {
                "lying".into()
            }
        }

        for alter in 0..4u8 {
            let mut log = AnchorLog::new();
            let mut sink = LyingSink {
                alter,
                ..Default::default()
            };
            log.publish(&mut sink, MAIN, head(1), 10, 1_000)
                .expect("publish");

            match log.verify(
                &sink,
                MAIN,
                &AnchorPolicy::default(),
                1_001,
                log_with(vec![head(1)]),
            ) {
                Err(AnchorError::ReceiptNotHonoured { .. }) => {}
                Err(other) => {
                    panic!("field {alter} was altered but failed for another reason: {other}")
                }
                Ok(v) => panic!("a sink that altered field {alter} verified anyway: {v:?}"),
            }
        }
    }
    #[test]
    fn an_anchored_head_that_left_the_log_is_detected() {
        // The gap this module closes. A rewrite below the anchored head changes
        // it, and the published copy does not change with it.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();

        let err = anchors
            .verify(
                &sink,
                MAIN,
                &AnchorPolicy::default(),
                1_000,
                log_with(vec![]),
            )
            .unwrap_err();

        assert!(
            matches!(err, AnchorError::AnchoredHeadMissing { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn silence_is_a_finding_rather_than_an_absence_of_findings() {
        // The attack that makes this design work at all: we cannot retract a
        // published anchor, but we can decline to publish the next one. A
        // verifier that only checked the anchors it had would find them all
        // consistent and report success.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();

        let policy = AnchorPolicy::default();
        let live = log_with(vec![head(1)]);

        assert!(anchors.verify(&sink, MAIN, &policy, HOUR, &live).is_ok());

        let err = anchors
            .verify(&sink, MAIN, &policy, HOUR + 1, &live)
            .unwrap_err();
        match err {
            AnchorError::Stale {
                gap_ms, allowed_ms, ..
            } => {
                assert_eq!(allowed_ms, HOUR);
                assert!(gap_ms > allowed_ms);
            }
            other => panic!("expected staleness to be a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_branch_nobody_ever_anchored_fails_rather_than_passing_vacuously() {
        // "Nothing to check" is the most confident wrong answer available here.
        let sink = InMemorySink::new();
        let anchors = AnchorLog::new();
        let err = anchors
            .verify(&sink, MAIN, &AnchorPolicy::default(), 0, log_with(vec![]))
            .unwrap_err();
        assert!(matches!(err, AnchorError::NeverAnchored { .. }));
    }

    #[test]
    fn an_anchor_covering_fewer_entries_than_the_last_is_refused_before_publication() {
        // Either history is being rewritten or an old anchor is being replayed.
        // Refused before publishing, because publishing it would put a false
        // statement somewhere we cannot retract it from.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();

        let err = anchors
            .publish(&mut sink, MAIN, head(2), 40, 1_000)
            .unwrap_err();
        assert!(matches!(err, AnchorError::WentBackwards { .. }), "{err:?}");
        assert_eq!(
            anchors.anchors(MAIN).len(),
            1,
            "a refused anchor must not have been published"
        );
    }

    #[test]
    fn a_sink_that_lost_what_it_took_is_a_failure() {
        // Without this an anchor is our own record of our own action, which is
        // the thing an external anchor exists not to be.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        let receipt = anchors
            .publish(&mut sink, MAIN, head(1), 100, 0)
            .unwrap()
            .receipt
            .clone();

        sink.forget(&receipt);

        let err = anchors
            .verify(
                &sink,
                MAIN,
                &AnchorPolicy::default(),
                1_000,
                log_with(vec![head(1)]),
            )
            .unwrap_err();
        assert!(
            matches!(err, AnchorError::ReceiptNotHonoured { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn verification_says_where_protection_stops() {
        // The sentence this whole module has to keep saying: an anchor closes
        // the gap up to itself and not one entry further.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();
        anchors.publish(&mut sink, MAIN, head(2), 250, 500).unwrap();

        let verified = anchors
            .verify(
                &sink,
                MAIN,
                &AnchorPolicy::default(),
                1_000,
                log_with(vec![head(1), head(2)]),
            )
            .unwrap();

        assert_eq!(verified.anchors_checked, 2);
        assert_eq!(
            verified.entries_protected, 250,
            "entries after the newest anchor are not protected and the caller \
             must be able to see how many that is"
        );
        assert_eq!(verified.unprotected_since_ms, Some(500));
    }

    #[test]
    fn anchoring_one_branch_says_nothing_about_another() {
        // Pretending otherwise is the more dangerous error: it would report a
        // preview branch as protected because `main` was anchored.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();

        let other = BranchId(7);
        let err = anchors
            .verify(
                &sink,
                other,
                &AnchorPolicy::default(),
                0,
                log_with(vec![head(1)]),
            )
            .unwrap_err();
        assert!(matches!(err, AnchorError::NeverAnchored { branch: 7 }));
    }

    #[test]
    fn an_anchor_at_the_same_height_is_allowed_because_nothing_moved() {
        // Re-anchoring an unchanged head is how a quiet branch keeps proving it
        // is quiet. Refusing it would make silence unavoidable for any branch
        // nobody is writing to, and silence is what this module treats as an
        // alarm.
        let mut sink = InMemorySink::new();
        let mut anchors = AnchorLog::new();
        anchors.publish(&mut sink, MAIN, head(1), 100, 0).unwrap();
        assert!(anchors.publish(&mut sink, MAIN, head(1), 100, HOUR).is_ok());

        assert!(anchors
            .verify(
                &sink,
                MAIN,
                &AnchorPolicy::default(),
                HOUR + 1,
                log_with(vec![head(1)])
            )
            .is_ok());
    }

    #[test]
    fn a_policy_that_does_not_require_a_first_anchor_reports_zero_protection() {
        // The opt-out exists for a preview branch nobody anchors. It must still
        // report *how much* is protected, which is none, rather than reporting
        // success in a way that reads like protection.
        let sink = InMemorySink::new();
        let anchors = AnchorLog::new();
        let policy = AnchorPolicy {
            require_first_anchor: false,
            ..AnchorPolicy::default()
        };

        let verified = anchors
            .verify(&sink, MAIN, &policy, 0, log_with(vec![]))
            .unwrap();
        assert_eq!(verified.entries_protected, 0);
        assert_eq!(verified.unprotected_since_ms, None);
    }
}
