//! What is in the archive, and what it is supposed to be.
//!
//! The manifest is the index a restore reads. It is deliberately a plain,
//! human-readable file rather than something clever: the moment it is needed is
//! the moment everything else has gone wrong, and a format that requires
//! working tooling to read is a format that fails exactly then.
//!
//! It records, for every archived segment, the digest of the *original* bytes.
//! That is what makes a restore verifiable — not the archive's own checksum,
//! which only says the container is the one that was stored, but a digest taken
//! before the archive ever saw the data.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ArchiveError, Digest, StoredRef};

/// One archived segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedSegment {
    /// The segment's sequence number, which is also its order in the log.
    pub sequence: u64,
    /// Digest of the original segment, taken before archiving.
    pub digest: Digest,
    /// Size of the original, so a restore knows what it is expecting.
    pub original_bytes: u64,
    /// Where it went.
    pub stored: StoredRef,
    /// When the round trip was proved, in milliseconds.
    ///
    /// Not when the upload finished — when the bytes were read back and
    /// checked. The distinction is the point of this whole crate.
    pub verified_at_ms: i64,
}

impl ArchivedSegment {
    /// What compression bought, as a ratio of stored to original.
    pub fn ratio(&self) -> f64 {
        match self.original_bytes {
            0 => 1.0,
            n => self.stored.stored_bytes as f64 / n as f64,
        }
    }
}

/// The archive's index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub project_id: String,
    /// Archived segments by sequence. A map rather than a list, because the one
    /// thing a restore must never do is silently skip a gap.
    pub segments: BTreeMap<u64, ArchivedSegment>,
}

impl Manifest {
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            segments: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, segment: ArchivedSegment) {
        self.segments.insert(segment.sequence, segment);
    }

    pub fn get(&self, sequence: u64) -> Option<&ArchivedSegment> {
        self.segments.get(&sequence)
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// The highest sequence archived, if any.
    pub fn head(&self) -> Option<u64> {
        self.segments.keys().next_back().copied()
    }

    /// Segments needed to restore to `through`, in log order.
    ///
    /// Refuses on a gap rather than returning what it has. A log is a fold: a
    /// restore that skipped segment 4 and applied 5 would not be a restore of
    /// an earlier state, it would be a state that never existed — and it would
    /// look completely normal (`docs/INVARIANTS.md` invariant 6).
    pub fn restore_plan(&self, through: u64) -> Result<Vec<&ArchivedSegment>, RestoreGap> {
        let mut plan = Vec::new();
        for sequence in 0..=through {
            match self.segments.get(&sequence) {
                Some(segment) => plan.push(segment),
                None => {
                    return Err(RestoreGap {
                        missing: sequence,
                        through,
                    })
                }
            }
        }
        Ok(plan)
    }

    /// Every sequence missing below the head.
    ///
    /// Run on a schedule, not only at restore time: an archive with a hole in
    /// it is broken from the moment the hole appears, and finding out during a
    /// restore is finding out too late.
    pub fn gaps(&self) -> Vec<u64> {
        let (Some(horizon), Some(head)) = (self.horizon(), self.head()) else {
            return Vec::new();
        };
        // From the horizon, not from zero (M10.5). Retention expires a prefix,
        // so an archive that legitimately starts at segment 40 has no hole —
        // and measuring from zero would report forty of them, on every tick,
        // forever. An alert that fires permanently after a routine expiry is an
        // alert somebody turns off, taking the real gap detection with it.
        //
        // This is safe precisely because expiry can only remove a prefix:
        // anything missing *above* the horizon is a genuine hole, and that is
        // still exactly what this returns.
        (horizon..=head)
            .filter(|s| !self.segments.contains_key(s))
            .collect()
    }

    pub fn load(path: &Path) -> Result<Self, ArchiveError> {
        let text = std::fs::read_to_string(path).map_err(|e| ArchiveError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|e| ArchiveError::Io {
            path: path.to_path_buf(),
            detail: format!("the manifest is unreadable: {e}"),
        })
    }

    /// Write the manifest, atomically.
    ///
    /// Through a temporary file and a rename, because a manifest half-written by
    /// a crash is worse than one that is briefly out of date: the stale one
    /// under-reports what is archived, and the torn one cannot be parsed at all.
    pub fn save(&self, path: &Path) -> Result<(), ArchiveError> {
        let json = serde_json::to_string_pretty(self).map_err(|e| ArchiveError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        let temp = path.with_extension("tmp");
        std::fs::write(&temp, json).map_err(|e| ArchiveError::Io {
            path: temp.clone(),
            detail: e.to_string(),
        })?;
        std::fs::rename(&temp, path).map_err(|e| ArchiveError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "the archive has a hole at segment {missing}, so it cannot be restored through \
     {through}. A log is a fold: applying what comes after a gap would produce a state \
     that never existed, and it would look entirely normal."
)]
pub struct RestoreGap {
    pub missing: u64,
    pub through: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(sequence: u64) -> ArchivedSegment {
        ArchivedSegment {
            sequence,
            digest: Digest::of(format!("segment {sequence}").as_bytes()),
            original_bytes: 1024,
            stored: StoredRef {
                key: format!("seg/{sequence:012}.at1"),
                stored_bytes: 256,
                container_checksum: Some("sha256:abc".into()),
            },
            verified_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_restore_plan_is_every_segment_in_log_order() {
        let mut m = Manifest::new("p");
        for s in 0..5 {
            m.record(segment(s));
        }

        let plan = m.restore_plan(4).expect("no gaps");
        let sequences: Vec<u64> = plan.iter().map(|s| s.sequence).collect();
        assert_eq!(sequences, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_gap_refuses_the_restore_rather_than_skipping_it() {
        // The failure that would otherwise look like a success: the state you
        // get by skipping a segment is not an earlier state, it is one that
        // never existed.
        let mut m = Manifest::new("p");
        for s in [0, 1, 3, 4] {
            m.record(segment(s));
        }

        assert_eq!(
            m.restore_plan(4),
            Err(RestoreGap {
                missing: 2,
                through: 4
            })
        );
    }

    #[test]
    fn gaps_are_findable_before_a_restore_needs_them() {
        // An archive with a hole is broken from the moment the hole appears.
        // Finding out during a restore is finding out too late.
        let mut m = Manifest::new("p");
        for s in [0, 2, 5] {
            m.record(segment(s));
        }
        assert_eq!(m.gaps(), vec![1, 3, 4]);
    }

    #[test]
    fn an_archive_with_no_holes_reports_none() {
        let mut m = Manifest::new("p");
        for s in 0..4 {
            m.record(segment(s));
        }
        assert!(m.gaps().is_empty());
    }

    #[test]
    fn a_partial_restore_is_allowed_when_the_prefix_is_whole() {
        // Point-in-time restore: stopping early is fine, skipping is not.
        let mut m = Manifest::new("p");
        for s in [0, 1, 2, 5] {
            m.record(segment(s));
        }
        assert!(m.restore_plan(2).is_ok());
        assert!(m.restore_plan(3).is_err());
    }

    #[test]
    fn a_manifest_survives_a_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");

        let mut m = Manifest::new("churn-dashboard");
        m.record(segment(0));
        m.record(segment(1));
        m.save(&path).expect("saves");

        assert_eq!(Manifest::load(&path).expect("loads"), m);
    }

    #[test]
    fn the_manifest_is_readable_without_the_tooling_that_wrote_it() {
        // It is needed exactly when everything else has gone wrong, so it has
        // to be legible to a person with a text editor.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.json");
        let mut m = Manifest::new("p");
        m.record(segment(0));
        m.save(&path).expect("saves");

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(text.contains("\"projectId\": \"p\""));
        assert!(text.contains("\"sequence\": 0"));
        // The digest is hex, not an opaque byte array.
        assert!(text.contains(&m.get(0).expect("segment").digest.to_hex()));
    }

    #[test]
    fn the_ratio_reports_what_compression_bought() {
        assert!((segment(0).ratio() - 0.25).abs() < f64::EPSILON);
    }
}
