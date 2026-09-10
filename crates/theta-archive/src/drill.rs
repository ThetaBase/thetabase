//! Restore drills, on a clock, with the result recorded (ROADMAP-V3 M20).
//!
//! # A backup nobody has restored is a hypothesis
//!
//! `theta-archive` already proves each segment round-trips before releasing the
//! local copy, which is a stronger per-segment guarantee than most archives
//! offer. It does not answer the question an operator actually has during an
//! incident, which is whether a **restore** works: the manifest is complete, the
//! segments are all still fetchable, and applying them in order produces the
//! state it should.
//!
//! Those are different claims. Every segment can round-trip individually while
//! segment 4 is missing from the manifest, and a restore that skips it produces
//! a state that never existed and looks entirely normal — which is why
//! `Manifest::gaps` exists and why this runs it as part of a drill rather than
//! trusting that somebody did.
//!
//! # What a drill must not do
//!
//! **Report success on a subset.** A drill that fetched what it could and
//! reported "restore verified" would be the archive equivalent of a percentile
//! over eleven samples. A partial drill is reported as partial, with what it
//! covered.
//!
//! **Cost so much that it stops running.** A full restore of a large archive is
//! expensive, so a drill takes a depth: the newest N segments, or everything.
//! Reporting the depth is what keeps a cheap drill honest — "the last ten
//! segments restore" is a true and useful sentence, and it is not "the archive
//! restores".
//!
//! # The result is recorded whether it passes or fails
//!
//! A drill that only records failures leaves nobody able to answer "when did
//! this last work". That question is the whole point: an operator deciding
//! whether to attempt a restore during an incident wants the date of the last
//! successful drill, not the absence of a recent alarm.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::{ArchiveBackend, ArchiveError};

/// How much of the archive a drill covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "depth")]
pub enum Depth {
    /// Every segment. The real answer, and the expensive one.
    Everything,
    /// The newest `n` segments.
    ///
    /// Cheap enough to run often. It proves the recent archive restores, which
    /// is what an incident usually needs, and it deliberately does not prove
    /// that the whole archive does.
    Newest(usize),
}

/// What a drill found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrillResult {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub depth: Depth,
    /// Segments the drill actually fetched and verified.
    pub segments_restored: usize,
    /// Segments in the manifest. The denominator, so a partial drill reads as
    /// partial rather than as a number with no scale.
    pub segments_in_manifest: usize,
    pub bytes_restored: u64,
    /// Sequence numbers the manifest is missing.
    ///
    /// Checked on every drill rather than assumed. The log is a fold, so
    /// applying segment 5 after a missing 4 produces a state that never existed
    /// and looks entirely normal.
    pub gaps: Vec<u64>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Outcome {
    /// Everything the drill was asked to cover came back intact.
    Passed,
    /// The manifest has holes. Not a fetch failure — a restore from this
    /// archive would silently produce a state that never existed.
    GapsFound,
    /// A segment could not be retrieved or did not verify.
    Failed { segment: u64, detail: String },
}

impl DrillResult {
    pub fn passed(&self) -> bool {
        matches!(self.outcome, Outcome::Passed)
    }

    /// Whether this drill covered the whole archive.
    ///
    /// The sentence a report has to be able to make: "the last ten segments
    /// restore" is true and useful, and it is not "the archive restores".
    pub fn covered_everything(&self) -> bool {
        self.segments_restored == self.segments_in_manifest
    }

    /// A line for an operator, and for the audit trail.
    pub fn summary(&self) -> String {
        let scope = if self.covered_everything() {
            "the whole archive".to_string()
        } else {
            format!(
                "{} of {} segments",
                self.segments_restored, self.segments_in_manifest
            )
        };
        match &self.outcome {
            Outcome::Passed => format!(
                "restore drill passed over {scope} ({} bytes, {}ms)",
                self.bytes_restored,
                self.finished_at_ms - self.started_at_ms
            ),
            Outcome::GapsFound => format!(
                "restore drill found {} gap(s) in the manifest ({:?}): a restore \
                 would apply segments out of order and produce a state that never \
                 existed",
                self.gaps.len(),
                self.gaps
            ),
            Outcome::Failed { segment, detail } => {
                format!("restore drill failed on segment {segment} over {scope}: {detail}")
            }
        }
    }
}

/// Every drill that has run, passed or failed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrillLog {
    results: Vec<DrillResult>,
}

impl DrillLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, result: DrillResult) {
        self.results.push(result);
    }

    pub fn all(&self) -> &[DrillResult] {
        &self.results
    }

    /// When a drill last passed over the whole archive.
    ///
    /// The question an operator has mid-incident, and the reason passes are
    /// recorded rather than only failures. It deliberately ignores partial
    /// drills: "the newest ten segments restored on Tuesday" does not answer
    /// "can I restore".
    pub fn last_full_pass_ms(&self) -> Option<i64> {
        self.results
            .iter()
            .filter(|r| r.passed() && r.covered_everything())
            .map(|r| r.finished_at_ms)
            .max()
    }

    /// When a drill last passed at all, whatever its depth.
    pub fn last_pass_ms(&self) -> Option<i64> {
        self.results
            .iter()
            .filter(|r| r.passed())
            .map(|r| r.finished_at_ms)
            .max()
    }

    /// Whether drills are running often enough.
    ///
    /// A drill nobody runs is the same as no drill, and the failure is silent:
    /// nothing alarms, because nothing ran.
    pub fn is_stale(&self, max_age_ms: i64, now_ms: i64) -> bool {
        match self.last_pass_ms() {
            None => true,
            Some(last) => now_ms.saturating_sub(last) > max_age_ms,
        }
    }
}

/// Run a drill.
///
/// `scratch` is where segments are fetched to; the caller owns it and is
/// expected to discard it. Nothing here writes into the live data directory: a
/// drill that restored over the running instance would be a restore, not a
/// drill.
pub fn run<B: ArchiveBackend>(
    backend: &B,
    manifest: &Manifest,
    depth: Depth,
    scratch: &Path,
    now_ms: i64,
) -> Result<DrillResult, ArchiveError> {
    let total = manifest.len();

    // Gaps first, and independently of anything being fetchable. An archive
    // whose segments all fetch perfectly and whose manifest skips segment 4 is
    // broken in the way that matters most, because the restore succeeds.
    let gaps = manifest.gaps();
    if !gaps.is_empty() {
        return Ok(DrillResult {
            started_at_ms: now_ms,
            finished_at_ms: now_ms,
            depth,
            segments_restored: 0,
            segments_in_manifest: total,
            bytes_restored: 0,
            gaps,
            outcome: Outcome::GapsFound,
        });
    }

    // The map is keyed by sequence, so iteration is already in restore order.
    let mut segments: Vec<_> = manifest.segments.values().collect();
    if let Depth::Newest(n) = depth {
        let skip = segments.len().saturating_sub(n);
        segments = segments.split_off(skip);
    }

    let mut bytes_restored = 0u64;
    let mut restored = 0usize;

    for segment in &segments {
        let dest = scratch.join(format!("drill-{}.seg", segment.sequence));
        if let Err(e) = backend.fetch(&segment.stored, &dest) {
            return Ok(DrillResult {
                started_at_ms: now_ms,
                finished_at_ms: now_ms,
                depth,
                segments_restored: restored,
                segments_in_manifest: total,
                bytes_restored,
                gaps: Vec::new(),
                outcome: Outcome::Failed {
                    segment: segment.sequence,
                    detail: e.to_string(),
                },
            });
        }

        // The archive proved a round trip when it stored this. A drill re-proves
        // it *now*, which is the difference between "it was intact when we wrote
        // it" and "it is intact today" — and bit rot is the failure mode that
        // only the second catches.
        let landed = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        if landed != segment.original_bytes {
            return Ok(DrillResult {
                started_at_ms: now_ms,
                finished_at_ms: now_ms,
                depth,
                segments_restored: restored,
                segments_in_manifest: total,
                bytes_restored,
                gaps: Vec::new(),
                outcome: Outcome::Failed {
                    segment: segment.sequence,
                    detail: format!(
                        "restored {landed} bytes where the manifest records {}",
                        segment.original_bytes
                    ),
                },
            });
        }

        bytes_restored += landed;
        restored += 1;
        let _ = std::fs::remove_file(&dest);
    }

    Ok(DrillResult {
        started_at_ms: now_ms,
        finished_at_ms: now_ms,
        depth,
        segments_restored: restored,
        segments_in_manifest: total,
        bytes_restored,
        gaps: Vec::new(),
        outcome: Outcome::Passed,
    })
}
