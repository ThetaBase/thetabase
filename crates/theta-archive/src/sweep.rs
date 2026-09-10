//! Archiving a segment, and proving it before letting go of the original.
//!
//! This module is the invariant:
//!
//! > **A local segment is never released until the archive has been proved to
//! > return it byte for byte.**
//!
//! The proof is a round trip — store, fetch back, compare against a digest
//! taken before the compressor saw the data. Not the exit code, not the
//! container checksum, not the compressor's own assurance of losslessness.
//! Those all answer "did the thing I asked for appear to work", and the
//! question a backup has to answer is "can I get the bytes back".
//!
//! It costs one decompression per segment, once, against the alternative of
//! finding out during a restore. That is not a close trade.

use std::path::{Path, PathBuf};

use crate::manifest::ArchivedSegment;
use crate::{ArchiveBackend, ArchiveError, Digest, Manifest, StoredRef};

/// What happened to one segment.
#[derive(Debug, Clone, PartialEq)]
pub enum ArchiveOutcome {
    /// Stored and proved. The local segment may now be released.
    Archived {
        sequence: u64,
        original_bytes: u64,
        stored_bytes: u64,
    },
    /// Already in the manifest, proved on an earlier sweep. Nothing to do.
    AlreadyArchived { sequence: u64 },
    /// The archive could not be reached. The segment stays where it is and the
    /// next sweep will try again — an unreachable archive is a normal
    /// condition, not an incident (ROADMAP M10: writes continue locally,
    /// snapshots queue and retry).
    Deferred { sequence: u64, reason: String },
    /// Stored, and what came back was not what went in. Loud, and the local
    /// segment is kept.
    Failed { sequence: u64, reason: String },
}

impl ArchiveOutcome {
    /// Whether the local segment may now be deleted.
    ///
    /// The only place that decision is made, and it is true in exactly one
    /// case: the round trip was proved, on this sweep or an earlier one.
    pub fn releasable(&self) -> bool {
        matches!(
            self,
            ArchiveOutcome::Archived { .. } | ArchiveOutcome::AlreadyArchived { .. }
        )
    }

    pub fn sequence(&self) -> u64 {
        match self {
            ArchiveOutcome::Archived { sequence, .. }
            | ArchiveOutcome::AlreadyArchived { sequence }
            | ArchiveOutcome::Deferred { sequence, .. }
            | ArchiveOutcome::Failed { sequence, .. } => *sequence,
        }
    }
}

/// Archives segments and proves each one.
pub struct Sweep<B: ArchiveBackend> {
    backend: B,
    manifest: Manifest,
    /// Somewhere to decompress into while checking. Cleaned up per segment: a
    /// verification scratch file left behind is a second copy of customer data
    /// that nothing is tracking.
    scratch: PathBuf,
}

impl<B: ArchiveBackend> Sweep<B> {
    pub fn new(backend: B, manifest: Manifest, scratch: impl Into<PathBuf>) -> Self {
        Self {
            backend,
            manifest,
            scratch: scratch.into(),
        }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn into_manifest(self) -> Manifest {
        self.manifest
    }

    /// Archive one segment and prove it.
    pub fn archive_segment(&mut self, sequence: u64, source: &Path, now_ms: i64) -> ArchiveOutcome {
        if self.manifest.get(sequence).is_some() {
            return ArchiveOutcome::AlreadyArchived { sequence };
        }

        // The digest comes from the original, before the archive has touched
        // anything. Taken after compression it would only prove the archive is
        // self-consistent, which is not the claim being made.
        let (digest, original_bytes) = match read_digest(source) {
            Ok(pair) => pair,
            Err(e) => {
                return ArchiveOutcome::Failed {
                    sequence,
                    reason: e.to_string(),
                }
            }
        };

        let key = format!("segments/{sequence:012}.at1");
        let stored = match self.backend.store(&key, source) {
            Ok(stored) => stored,
            Err(ArchiveError::Unavailable(reason)) => {
                return ArchiveOutcome::Deferred { sequence, reason }
            }
            Err(e) => {
                return ArchiveOutcome::Failed {
                    sequence,
                    reason: e.to_string(),
                }
            }
        };

        // The proof.
        let check = self.scratch.join(format!("verify-{sequence:012}"));
        let proof = self.prove(&stored, &check, digest, sequence);
        let _ = std::fs::remove_file(&check);

        match proof {
            Ok(()) => {
                let stored_bytes = stored.stored_bytes;
                self.manifest.record(ArchivedSegment {
                    sequence,
                    digest,
                    original_bytes,
                    stored,
                    verified_at_ms: now_ms,
                });
                ArchiveOutcome::Archived {
                    sequence,
                    original_bytes,
                    stored_bytes,
                }
            }
            // Deliberately not recorded. A manifest entry says the segment is
            // safely archived, which is the one thing just shown to be false.
            Err(e) => ArchiveOutcome::Failed {
                sequence,
                reason: e.to_string(),
            },
        }
    }

    /// Read back what was stored and compare it to what went in.
    fn prove(
        &self,
        stored: &StoredRef,
        scratch: &Path,
        expected: Digest,
        sequence: u64,
    ) -> Result<(), ArchiveError> {
        if let Some(parent) = scratch.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ArchiveError::Io {
                path: parent.to_path_buf(),
                detail: e.to_string(),
            })?;
        }

        self.backend.fetch(stored, scratch)?;
        let (returned, _) = read_digest(scratch)?;

        match returned == expected {
            true => Ok(()),
            false => Err(ArchiveError::RoundTripFailed {
                sequence,
                stored: expected,
                returned,
            }),
        }
    }

    /// Restore segments up to `through` into `dest`, verifying each.
    ///
    /// Every segment is checked against the digest recorded when it was
    /// archived. A restore that trusted the archive would be trusting the thing
    /// it exists to recover from.
    pub fn restore(&self, through: u64, dest: &Path) -> Result<Vec<PathBuf>, ArchiveError> {
        let plan = self
            .manifest
            .restore_plan(through)
            .map_err(|gap| ArchiveError::Backend(gap.to_string()))?;

        std::fs::create_dir_all(dest).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: e.to_string(),
        })?;

        let mut restored = Vec::new();
        for segment in plan {
            let path = dest.join(format!("{:012}.seg", segment.sequence));
            self.backend.fetch(&segment.stored, &path)?;

            let (returned, _) = read_digest(&path)?;
            if returned != segment.digest {
                // Removed rather than left in place: a restore directory
                // holding a segment that failed its check is a directory
                // someone will later mistake for a good restore.
                let _ = std::fs::remove_file(&path);
                return Err(ArchiveError::RoundTripFailed {
                    sequence: segment.sequence,
                    stored: segment.digest,
                    returned,
                });
            }
            restored.push(path);
        }
        Ok(restored)
    }

    /// Re-check every archived container, without a full restore.
    ///
    /// Weaker than a restore and far cheaper, so it can run often: it catches a
    /// container that has rotted in object storage, which is the failure that
    /// otherwise stays invisible until the day it matters.
    pub fn audit(&self) -> Vec<(u64, bool)> {
        self.manifest
            .segments
            .values()
            .map(|s| {
                (
                    s.sequence,
                    self.backend.check_integrity(&s.stored).unwrap_or(false),
                )
            })
            .collect()
    }
}

fn read_digest(path: &Path) -> Result<(Digest, u64), ArchiveError> {
    let bytes = std::fs::read(path).map_err(|e| ArchiveError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    Ok((Digest::of(&bytes), bytes.len() as u64))
}
