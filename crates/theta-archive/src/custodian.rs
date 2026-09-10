//! Driving archive sweeps on a schedule, and letting go of what is proved
//! (ROADMAP M10).
//!
//! Everything this needs already existed — archiving, proving, the manifest,
//! the gap check — and nothing called any of it periodically. So a database
//! that ran for a month had a month of segments on local disk, every one of
//! them archived by hand or not at all. This is the loop.
//!
//! # Why releasing is a separate decision from archiving
//!
//! [`ArchiveOutcome::releasable`] answers "has the archive proved it can return
//! these bytes". [`Wal::release_segment`] answers "can this process still
//! recover without them". Neither answers the other's question, and deleting
//! needs both to be yes.
//!
//! Keeping them apart is what makes a bug in the archiver a wasted sweep
//! rather than a lost segment: the storage layer re-checks the sequence against
//! its own checkpoint and refuses anything it still needs, whatever the
//! archiver believes.
//!
//! [`Wal::release_segment`]: theta_storage::wal::Wal::release_segment
//!
//! # A tick is not a transaction
//!
//! It archives what it can, releases what it may, and reports the rest. An
//! unreachable archive defers and the next tick tries again — `specs/01` §7
//! calls that a normal condition, not an incident, because a database whose
//! writes stop when a *backup* is unreachable has made its backup a dependency
//! of being up.
//!
//! What is *not* tolerated silently is a proof that failed. That means the
//! archive returned something other than what went in, and the local segment is
//! kept and the outcome is loud.

use std::path::{Path, PathBuf};

use crate::sweep::{ArchiveOutcome, Sweep};
use crate::{ArchiveBackend, Manifest};

/// What one tick did.
///
/// Returned rather than only logged, so the caller — a scheduler, a test, or an
/// operator running it by hand — can act on it. A sweep whose only output is a
/// log line is a sweep nobody can build an alert on.
#[derive(Debug, Default, PartialEq)]
pub struct TickReport {
    /// Segments archived and proved on this tick.
    pub archived: Vec<u64>,
    /// Segments already proved on an earlier tick, still present locally.
    pub already: Vec<u64>,
    /// Segments deleted, and the bytes each freed.
    pub released: Vec<(u64, u64)>,
    /// The archive could not be reached. Normal; the next tick retries.
    pub deferred: Vec<(u64, String)>,
    /// Stored, and what came back was not what went in. Not normal.
    pub failed: Vec<(u64, String)>,
    /// Sequences the manifest is missing below its head.
    ///
    /// Checked every tick rather than at restore time, because an archive with
    /// a hole is broken from the moment the hole appears — and finding out
    /// during a restore means finding out at the worst possible moment.
    pub gaps: Vec<u64>,
    /// Bytes reclaimed from local disk.
    pub freed_bytes: u64,
}

impl TickReport {
    /// Whether anything on this tick needs a human.
    ///
    /// Deliberately narrow. A deferred segment is not a problem, and treating
    /// it as one would page somebody every time object storage blinked — which
    /// is how alerts get muted, and how the *real* failure then goes unseen.
    pub fn needs_attention(&self) -> bool {
        !self.failed.is_empty() || !self.gaps.is_empty()
    }

    /// A one-line summary for an operator.
    pub fn summary(&self) -> String {
        format!(
            "archived {}, released {} ({} bytes), deferred {}, failed {}, gaps {}",
            self.archived.len(),
            self.released.len(),
            self.freed_bytes,
            self.deferred.len(),
            self.failed.len(),
            self.gaps.len(),
        )
    }
}

/// What the custodian needs from the log it is archiving.
///
/// A trait rather than `DurableLogStore` directly, for a narrower reason than
/// dependency hygiene — this crate already depends on `theta-storage`. It is so
/// the failure paths can be exercised: a source that cannot be listed, and one
/// that refuses a release. Neither can be asked of a real store on demand, and
/// both are what a tick has to survive.
///
/// The tests then use the real store for everything else, because the
/// interesting question about releasing a segment is whether the engine agrees
/// to it, and a stub would agree to anything.
pub trait SegmentSource {
    /// Segments fully covered by the checkpoint, and safe to archive.
    fn archivable(&self) -> Result<Vec<PathBuf>, String>;

    /// Delete one, returning the bytes freed. Expected to refuse a sequence the
    /// log still needs, whatever the caller believes about it.
    fn release(&mut self, sequence: u64) -> Result<u64, String>;
}

/// Archives, proves, and releases — on demand or on a timer.
pub struct Custodian<B: ArchiveBackend> {
    sweep: Sweep<B>,
    /// Whether to delete a proved segment.
    ///
    /// On by default, because unbounded local growth is the bug this exists to
    /// fix. Off is for an operator who wants archiving without deletion while
    /// they build confidence — a stance worth supporting explicitly rather than
    /// leaving them to comment out a line.
    release: bool,
}

impl<B: ArchiveBackend> Custodian<B> {
    pub fn new(sweep: Sweep<B>) -> Self {
        Self {
            sweep,
            release: true,
        }
    }

    /// Archive and prove, but keep every local segment.
    pub fn without_releasing(mut self) -> Self {
        self.release = false;
        self
    }

    pub fn manifest(&self) -> &Manifest {
        self.sweep.manifest()
    }

    pub fn into_manifest(self) -> Manifest {
        self.sweep.into_manifest()
    }

    /// One pass: archive what is archivable, release what is proved.
    ///
    /// `now_ms` is passed rather than read, so a test can assert what a
    /// manifest entry records without sleeping, and so two segments archived in
    /// one tick carry the same verification time — which is true, and reads as
    /// obviously true.
    pub fn tick(&mut self, source: &mut impl SegmentSource, now_ms: i64) -> TickReport {
        let mut report = TickReport::default();

        let segments = match source.archivable() {
            Ok(segments) => segments,
            Err(reason) => {
                // Cannot even enumerate. Reported as a failure with no sequence
                // attached rather than swallowed: a tick that archived nothing
                // because it could not look must not read as a tick with
                // nothing to do.
                report.failed.push((u64::MAX, reason));
                return report;
            }
        };

        for path in segments {
            let Some(sequence) = sequence_of(&path) else {
                report.failed.push((
                    u64::MAX,
                    format!("cannot read a sequence number from {}", path.display()),
                ));
                continue;
            };

            match self.sweep.archive_segment(sequence, &path, now_ms) {
                ArchiveOutcome::Archived { .. } => report.archived.push(sequence),
                ArchiveOutcome::AlreadyArchived { .. } => report.already.push(sequence),
                ArchiveOutcome::Deferred { reason, .. } => {
                    report.deferred.push((sequence, reason));
                    // Nothing after this is releasable, and continuing to try
                    // is a round trip per segment against an archive that just
                    // said it is unreachable.
                    break;
                }
                ArchiveOutcome::Failed { reason, .. } => {
                    report.failed.push((sequence, reason));
                    continue;
                }
            }

            if !self.release {
                continue;
            }
            match source.release(sequence) {
                Ok(freed) => {
                    report.freed_bytes += freed;
                    report.released.push((sequence, freed));
                }
                // A refusal to release is not a failure of the archive. The
                // segment is proved and safe; the log simply still wants it, and
                // the next tick will ask again.
                Err(reason) => {
                    tracing::warn!(sequence, reason, "segment archived but not released");
                }
            }
        }

        report.gaps = self.sweep.manifest().gaps();
        report
    }
}

/// A segment's sequence, from its filename.
///
/// The filename is the only place the number is written down, so parsing it is
/// not a shortcut — it is where the number lives.
fn sequence_of(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_is_read_from_the_filename() {
        assert_eq!(
            sequence_of(Path::new("/data/segments/000000000007.seg")),
            Some(7)
        );
        assert_eq!(sequence_of(Path::new("/data/segments/VIEW")), None);
    }

    #[test]
    fn only_a_real_problem_asks_for_a_human() {
        // A deferred archive is a normal condition. Paging on it is how the
        // alert gets muted, and how the failure that matters then goes unseen.
        let quiet = TickReport {
            deferred: vec![(1, "unreachable".into())],
            ..Default::default()
        };
        assert!(!quiet.needs_attention());

        let loud = TickReport {
            failed: vec![(1, "digest mismatch".into())],
            ..Default::default()
        };
        assert!(loud.needs_attention());

        let holed = TickReport {
            gaps: vec![4],
            ..Default::default()
        };
        assert!(holed.needs_attention());
    }
}
