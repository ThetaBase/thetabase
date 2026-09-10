//! How far back the log is kept (M10.5).
//!
//! `specs/03` §5 says time-travel is "bounded by log retention policy
//! (configurable per tier)". It was not configurable, because there was no
//! policy — depth was whatever happened to still be on disk, which M10 then
//! made *shorter* by releasing archived segments. Stating a bound is a
//! correctness matter once something starts enforcing one.
//!
//! # Retention removes a prefix, never a hole
//!
//! The single property this module exists to guarantee. The log is a fold, so
//! applying segment 5 after a missing 4 produces a state that never existed and
//! looks entirely normal (`docs/INVARIANTS.md` invariant 6). An expiry policy that
//! deleted "everything older than N days" segment by segment would eventually
//! do exactly that — the moment one segment aged out while an older one was
//! still held back for any reason.
//!
//! So [`Manifest::expired`] returns the longest *contiguous prefix* that has
//! aged out, and stops at the first segment that has not. The archive can only
//! ever get shorter from the front.
//!
//! # What retention actually bounds
//!
//! Not "how long data is kept" — a row written once and never changed lives in
//! the materialized view forever. What ages out is the *history*: how far back
//! a point-in-time restore can reach. After expiry the earliest restorable
//! point moves forward, and [`Manifest::horizon`] is what to report to a caller
//! rather than letting them discover it during a restore.

use serde::{Deserialize, Serialize};

use crate::Manifest;

/// How long to keep log history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Retention {
    /// Never expire anything.
    ///
    /// The default, and a supportable one rather than a placeholder: the
    /// archive compresses log-shaped segments about 53× with a verified
    /// restore, so keeping everything is cheap enough to offer as a tier
    /// instead of disclaiming.
    ///
    /// Default because losing history should take a deliberate act. A window
    /// that arrived by forgetting to configure one would quietly shorten what a
    /// restore can reach, and nobody would find out until they needed it.
    #[default]
    Forever,
    /// Keep at least this many milliseconds of history.
    ///
    /// "At least", not "exactly". A segment is only expired once *every* older
    /// one has aged out too, so the retained window is often longer than asked
    /// for — which is the safe direction to be wrong in.
    For { ms: i64 },
}

impl Retention {
    /// Whether a segment verified at `verified_at_ms` has aged out by `now_ms`.
    ///
    /// Measured from when the round trip was *proved*, not from when the write
    /// happened. Those differ by however long the archive was unreachable, and
    /// using the write time would let an outage silently shorten the window a
    /// customer is paying for.
    pub fn expired_by(&self, verified_at_ms: i64, now_ms: i64) -> bool {
        match self {
            Retention::Forever => false,
            Retention::For { ms } => now_ms.saturating_sub(verified_at_ms) >= *ms,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Retention::Forever => "kept indefinitely".to_string(),
            Retention::For { ms } => format!("kept for {} days", ms / 86_400_000),
        }
    }
}

impl Manifest {
    /// The oldest segment still archived — the earliest point a restore can
    /// reach.
    ///
    /// Reported rather than left to be discovered. A caller asking for a
    /// restore to a point below this should be told so before the restore runs,
    /// not after it fails.
    pub fn horizon(&self) -> Option<u64> {
        self.segments.keys().next().copied()
    }

    /// Segments that have aged out, oldest first.
    ///
    /// The longest contiguous prefix whose every member has expired. Stops at
    /// the first segment that has not, even if later ones have — expiring past
    /// a retained segment would leave a hole, and a hole makes every restore
    /// through it wrong rather than merely shorter.
    pub fn expired(&self, retention: Retention, now_ms: i64) -> Vec<u64> {
        let mut out = Vec::new();
        // `BTreeMap` iterates in key order, so this is the prefix by
        // construction rather than by sorting afterwards.
        for (sequence, segment) in &self.segments {
            if !retention.expired_by(segment.verified_at_ms, now_ms) {
                break;
            }
            out.push(*sequence);
        }

        // Never everything. An archive with no segments cannot be restored from
        // at all, and a retention window shorter than the time since the last
        // write would otherwise empty it — turning a policy about history into
        // the deletion of the whole backup.
        if out.len() == self.segments.len() {
            out.pop();
        }
        out
    }

    /// Drop the given segments from the index.
    ///
    /// Takes what [`Manifest::expired`] returned rather than recomputing, so
    /// the decision and the deletion cannot disagree — and refuses anything
    /// that is not currently the oldest, because removing from the middle is
    /// the one thing this must never do.
    pub fn forget(&mut self, sequences: &[u64]) -> Result<(), RetentionError> {
        for sequence in sequences {
            match self.segments.keys().next().copied() {
                Some(oldest) if oldest == *sequence => {
                    self.segments.remove(sequence);
                }
                Some(oldest) => {
                    return Err(RetentionError::NotTheOldest {
                        asked: *sequence,
                        oldest,
                    })
                }
                None => return Err(RetentionError::Empty),
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RetentionError {
    #[error("segment {asked} cannot be expired: {oldest} is older, and removing {asked} first would leave a hole every restore through it would read as normal")]
    NotTheOldest { asked: u64, oldest: u64 },

    #[error("the archive is empty; there is nothing to expire")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchivedSegment, Digest, StoredRef};

    const DAY: i64 = 86_400_000;

    fn manifest(verified_at: &[i64]) -> Manifest {
        let mut manifest = Manifest::new("org_a/checkout");
        for (index, at) in verified_at.iter().enumerate() {
            manifest.record(ArchivedSegment {
                sequence: index as u64,
                digest: Digest::of(b"x"),
                original_bytes: 1,
                stored: StoredRef {
                    key: format!("segments/{index:012}.at1"),
                    stored_bytes: 1,
                    container_checksum: None,
                },
                verified_at_ms: *at,
            });
        }
        manifest
    }

    #[test]
    fn forever_expires_nothing() {
        let manifest = manifest(&[0, DAY, 2 * DAY]);
        assert!(manifest.expired(Retention::Forever, 1_000 * DAY).is_empty());
    }

    #[test]
    fn a_window_expires_the_oldest_first() {
        let manifest = manifest(&[0, DAY, 2 * DAY, 3 * DAY]);
        // Asked on day four with a two-day window: segments verified on days
        // 0 and 1 are three and four days old, day 2 is exactly two and the
        // boundary is inclusive, and day 3 is one day old and stays.
        let expired = manifest.expired(Retention::For { ms: 2 * DAY }, 4 * DAY);
        assert_eq!(
            expired,
            vec![0, 1, 2],
            "expected everything past the window"
        );
    }

    #[test]
    fn the_boundary_is_inclusive() {
        // Asserted rather than left implied, because "older than" and "at least
        // as old as" differ by exactly one segment and the difference only
        // shows up on a boundary nobody constructs by accident.
        let at_the_edge = manifest(&[0, DAY]);
        assert_eq!(
            at_the_edge.expired(Retention::For { ms: DAY }, DAY),
            vec![0],
            "a segment exactly one window old should expire"
        );
    }

    #[test]
    fn expiry_stops_at_the_first_retained_segment() {
        // The property the module exists for. Segment 1 is young, so nothing
        // after it may go even though 2 and 3 are old enough — the log is a
        // fold, and a hole makes every restore through it wrong.
        let manifest = manifest(&[0, 10 * DAY, 0, 0]);
        let expired = manifest.expired(Retention::For { ms: DAY }, 5 * DAY);
        assert_eq!(
            expired,
            vec![0],
            "expiry jumped over a retained segment and left a hole"
        );
    }

    #[test]
    fn expiry_never_empties_the_archive() {
        // A window shorter than the time since the last write would otherwise
        // delete the whole backup — a policy about history quietly becoming the
        // deletion of everything.
        let manifest = manifest(&[0, 0, 0]);
        let expired = manifest.expired(Retention::For { ms: DAY }, 100 * DAY);
        assert_eq!(expired.len(), 2, "the last segment must survive");
        assert!(!expired.contains(&2));
    }

    #[test]
    fn expiring_leaves_no_gap() {
        let mut manifest = manifest(&[0, DAY, 2 * DAY, 3 * DAY]);
        let expired = manifest.expired(Retention::For { ms: 2 * DAY }, 4 * DAY);
        manifest.forget(&expired).expect("forget");

        assert!(
            manifest.gaps().is_empty(),
            "expiry created a hole: {:?}",
            manifest.gaps()
        );
        assert_eq!(manifest.horizon(), Some(3), "the horizon did not move");
    }

    #[test]
    fn an_expired_prefix_is_not_reported_as_a_gap() {
        // The interaction that made this worth finding: `gaps()` used to count
        // from zero, so a routine expiry looked like a hole and kept looking
        // like one on every tick, forever. An alert that fires permanently
        // after normal operation is one somebody turns off — taking the real
        // gap detection with it.
        let mut manifest = manifest(&[0, DAY, 2 * DAY, 3 * DAY]);
        manifest
            .forget(&manifest.expired(Retention::For { ms: 2 * DAY }, 4 * DAY))
            .expect("forget");

        assert!(
            manifest.gaps().is_empty(),
            "an expired prefix was reported as a hole: {:?}",
            manifest.gaps()
        );
    }

    #[test]
    fn a_hole_above_the_horizon_is_still_a_gap() {
        // The other half. Measuring from the horizon must not stop the check
        // catching a real hole — only stop it counting expiry as one.
        let mut manifest = manifest(&[0, DAY, 2 * DAY, 3 * DAY]);
        manifest.forget(&[0]).expect("forget");
        manifest.segments.remove(&2);

        assert_eq!(
            manifest.gaps(),
            vec![2],
            "a genuine hole above the horizon went unreported"
        );
    }

    #[test]
    fn forgetting_out_of_order_is_refused() {
        // Belt and braces: `expired` returns a prefix, and `forget` refuses
        // anything else regardless of who computed it.
        let mut manifest = manifest(&[0, DAY, 2 * DAY]);
        assert!(matches!(
            manifest.forget(&[2]),
            Err(RetentionError::NotTheOldest {
                asked: 2,
                oldest: 0
            })
        ));
        assert_eq!(manifest.len(), 3, "a refused expiry removed something");
    }

    #[test]
    fn the_horizon_is_the_earliest_restorable_point() {
        let mut manifest = manifest(&[0, DAY, 2 * DAY]);
        assert_eq!(manifest.horizon(), Some(0));

        manifest.forget(&[0]).expect("forget");
        assert_eq!(
            manifest.horizon(),
            Some(1),
            "a caller asking to restore below this must be told before it runs"
        );
    }

    #[test]
    fn age_is_measured_from_the_proof_not_the_write() {
        // Those differ by however long the archive was unreachable. Using the
        // write time would let an outage silently shorten the window a customer
        // is paying for.
        let retention = Retention::For { ms: 7 * DAY };
        assert!(!retention.expired_by(6 * DAY, 10 * DAY));
        assert!(retention.expired_by(3 * DAY, 10 * DAY));
    }

    #[test]
    fn a_retention_policy_describes_itself() {
        assert_eq!(Retention::Forever.describe(), "kept indefinitely");
        assert_eq!(
            Retention::For { ms: 30 * DAY }.describe(),
            "kept for 30 days"
        );
    }
}
