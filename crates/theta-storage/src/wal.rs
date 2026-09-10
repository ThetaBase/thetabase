//! Write-ahead log durability.
//!
//! Durability contract, which the rest of the engine is written against:
//!
//! * An [`Wal::append`] that returned `Ok` survives process death. The bytes are
//!   written *and* fsynced before the call returns — there is no configuration
//!   in which an acknowledged write is still in a kernel buffer.
//! * [`Wal::recover`] replays from the last checkpoint and is idempotent:
//!   replaying the same segments twice yields identical state.
//! * A torn final record is detected by checksum and truncated, never
//!   interpreted (`01-system-architecture.md` §7, "Storage node crash").
//!
//! # On group commit
//!
//! [`SyncPolicy::GroupCommit`] amortizes one fsync across a batch — that is what
//! [`Wal::append_batch`] does. It cannot make a *single* append cheaper: a lone
//! commit has nothing to group with, and deferring its fsync would mean
//! acknowledging a write that is not yet durable. So a single append fsyncs
//! under both policies, and the batch window in the policy is the interval the
//! server loop accumulates requests over before handing them here as one batch
//! (ROADMAP M2, where concurrent in-flight requests first exist to group).

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use theta_core::LogEntry;

use crate::encryption::DataKey;
use crate::error::{Result, StorageError};
use crate::segment::{self, TornRecord, HEADER_LEN};

#[derive(Debug, Clone)]
pub struct WalConfig {
    pub dir: PathBuf,
    /// Bytes per segment before rolling to a new file.
    pub segment_bytes: u64,
    pub sync_policy: SyncPolicy,
    /// The project's data key, if this log is encrypted at rest (SEC-2).
    ///
    /// `None` is not a fallback — it is a deployment that has not configured a
    /// key, and the server says so at startup. What it must never be is a
    /// silent default, which is how `specs/04` §3 came to describe encryption
    /// that did not exist.
    ///
    /// Turning it on for an existing log is safe and needs no migration: each
    /// segment records whether it is sealed, so old plaintext segments keep
    /// replaying while new ones are written encrypted.
    pub data_key: Option<DataKey>,
}

impl WalConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            segment_bytes: 64 * 1024 * 1024,
            sync_policy: SyncPolicy::EveryCommit,
            data_key: None,
        }
    }

    /// Encrypt records written from here on.
    pub fn with_data_key(mut self, key: DataKey) -> Self {
        self.data_key = Some(key);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Fsync before acknowledging every append.
    EveryCommit,
    /// Share one fsync across a batch. An acknowledged write is still durable —
    /// this trades latency for throughput, never safety. `max_batch_window_ms`
    /// is how long the server loop accumulates requests before flushing.
    GroupCommit { max_batch_window_ms: u64 },
}

/// Position in the log: which segment, and how far into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LogPosition {
    pub segment: u64,
    pub offset: u64,
}

impl LogPosition {
    pub const START: LogPosition = LogPosition {
        segment: 0,
        offset: HEADER_LEN,
    };
}

impl Default for LogPosition {
    /// The start of the log — segment 0, just past its header.
    fn default() -> Self {
        Self::START
    }
}

/// Everything durable up to `position` has been folded into the materialized
/// view, so recovery can start here instead of at genesis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub format_version: u32,
    pub position: LogPosition,
    /// Entries folded in as of this checkpoint. Used to detect a view that has
    /// drifted from the log rather than trusting it blindly.
    pub entries_applied: u64,
}

impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            format_version: segment::FORMAT_VERSION,
            position: LogPosition::START,
            entries_applied: 0,
        }
    }
}

/// What recovery found. Reported rather than logged-and-forgotten, because a
/// truncation is a data-loss event the operator has to be able to see.
#[derive(Debug, Default)]
pub struct Recovery {
    pub entries: Vec<LogEntry>,
    pub from: LogPosition,
    pub to: LogPosition,
    /// The torn record that ended recovery, if any, and the offset truncated to.
    pub truncated: Option<TornRecord>,
}

pub trait Wal {
    /// Durably record `entry`. Returns only once it is fsynced.
    fn append(&mut self, entry: &LogEntry) -> Result<LogPosition>;

    /// Durably record every entry, sharing one fsync across the batch under
    /// [`SyncPolicy::GroupCommit`]. All-or-nothing is *not* implied at this
    /// layer: atomicity across keys is expressed as a single
    /// `OpType::Transaction` entry, which is one record here.
    fn append_batch(&mut self, entries: &[LogEntry]) -> Result<LogPosition>;

    /// Mark everything up to `position` as folded, allowing older segments to be
    /// released to cold archive.
    fn checkpoint(&mut self, position: LogPosition, entries_applied: u64) -> Result<()>;

    /// Segments fully covered by the checkpoint, and therefore safe to archive.
    fn archivable_segments(&self) -> Result<Vec<PathBuf>>;

    /// Delete a segment that cold archive has proved it can return.
    ///
    /// The caller decides *whether* — that is the archive's proof to make, and
    /// this layer has no way to check it. This layer decides *whether it is
    /// allowed*, which is a different question and one it can answer: a segment
    /// at or above the checkpoint is still needed for recovery, and no proof
    /// about a backup makes deleting it safe.
    ///
    /// Both checks, because either alone is a way to lose data. Trusting the
    /// caller's sequence number would let a bug in the archiver delete the
    /// active segment; skipping the archive's proof would delete data that is
    /// nowhere else.
    fn release_segment(&mut self, sequence: u64) -> Result<u64>;

    /// Bytes this log occupies on disk, across every segment.
    ///
    /// Measured rather than estimated from entry counts: the figure a customer
    /// is billed on has to be the one their disk actually holds
    /// (`06-provisioning-identity-flow.md` §5).
    fn storage_bytes(&self) -> Result<u64>;
}

const SEGMENTS_DIR: &str = "segments";
const CHECKPOINT_FILE: &str = "CHECKPOINT";

#[derive(Debug)]
pub struct SegmentWal {
    config: WalConfig,
    active: File,
    position: LogPosition,
    checkpoint: Checkpoint,
}

impl SegmentWal {
    /// Open the log for recovery.
    ///
    /// Returns a [`RecoveringWal`], whose only operation is
    /// [`RecoveringWal::recover`]. A log cannot be written to before it has been
    /// replayed — appending past a torn record would resurrect bytes from a
    /// write that was never acknowledged — and making that a type-level
    /// requirement means no caller can forget.
    pub fn open(config: WalConfig) -> Result<RecoveringWal> {
        fs::create_dir_all(config.dir.join(SEGMENTS_DIR))?;
        let checkpoint = Self::read_checkpoint(&config.dir)?;
        Ok(RecoveringWal { config, checkpoint })
    }

    pub fn position(&self) -> LogPosition {
        self.position
    }

    /// Root directory of this log.
    pub fn dir(&self) -> &Path {
        &self.config.dir
    }

    pub fn checkpoint_state(&self) -> &Checkpoint {
        &self.checkpoint
    }

    /// The data key this log is sealed with, if any.
    pub fn data_key(&self) -> Option<&DataKey> {
        self.config.data_key.as_ref()
    }

    /// Create a segment, write its header, and make both the contents and the
    /// directory entry durable.
    fn create_segment(dir: &Path, sequence: u64, encrypted: bool) -> Result<(File, LogPosition)> {
        let path = Self::segment_path(dir, sequence);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        segment::write_header(&mut file, sequence, encrypted)?;
        file.sync_all()?;
        Self::sync_dir(dir)?;
        Ok((
            file,
            LogPosition {
                segment: sequence,
                offset: HEADER_LEN,
            },
        ))
    }

    fn segments_dir(&self) -> PathBuf {
        self.config.dir.join(SEGMENTS_DIR)
    }

    fn segment_path(dir: &Path, sequence: u64) -> PathBuf {
        dir.join(format!("{sequence:012}.seg"))
    }

    /// Segment sequence numbers present on disk, ascending.
    fn segment_sequences(dir: &Path) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "seg") {
                if let Some(seq) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    out.push(seq);
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Make a newly created segment's *name* durable, not just its contents.
    /// Without this a crash can leave a segment whose data survived but whose
    /// directory entry did not. How that is achieved differs per platform; see
    /// [`crate::fsync`].
    fn sync_dir(dir: &Path) -> Result<()> {
        crate::fsync::sync_dir(dir)
    }

    fn roll_if_needed(&mut self) -> Result<()> {
        if self.position.offset < self.config.segment_bytes {
            return Ok(());
        }
        let (file, position) = Self::create_segment(
            &self.segments_dir(),
            self.position.segment + 1,
            self.config.data_key.is_some(),
        )?;
        self.active = file;
        self.position = position;
        Ok(())
    }

    fn read_checkpoint(dir: &Path) -> Result<Checkpoint> {
        let path = dir.join(CHECKPOINT_FILE);
        if !path.exists() {
            return Ok(Checkpoint::default());
        }
        let bytes = fs::read(&path)?;
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes).map_err(|e| {
            // A corrupt checkpoint is recoverable — replay from genesis — but it
            // must be surfaced, never silently reset.
            StorageError::BadCheckpoint {
                detail: e.to_string(),
            }
        })?;
        if checkpoint.format_version != segment::FORMAT_VERSION {
            return Err(StorageError::UnsupportedFormat {
                version: checkpoint.format_version,
            });
        }
        Ok(checkpoint)
    }
}

impl Wal for SegmentWal {
    fn append(&mut self, entry: &LogEntry) -> Result<LogPosition> {
        self.append_batch(std::slice::from_ref(entry))
    }

    fn append_batch(&mut self, entries: &[LogEntry]) -> Result<LogPosition> {
        if entries.is_empty() {
            return Ok(self.position);
        }
        self.roll_if_needed()?;

        // Build the whole batch before touching the file: an encoding failure
        // must not leave a partial record on disk.
        let mut buffer = Vec::new();
        for entry in entries {
            buffer.extend_from_slice(&segment::encode_record(
                entry,
                self.config.data_key.as_ref(),
            )?);
        }

        self.active.write_all(&buffer)?;
        // Durability before acknowledgement, under both policies. See the module
        // docs on why group commit cannot skip this for a lone append.
        self.active.sync_all()?;

        self.position.offset += buffer.len() as u64;
        Ok(self.position)
    }

    fn checkpoint(&mut self, position: LogPosition, entries_applied: u64) -> Result<()> {
        let checkpoint = Checkpoint {
            format_version: segment::FORMAT_VERSION,
            position,
            entries_applied,
        };

        // Write to a temp file and rename: a crash mid-checkpoint must leave the
        // *previous* checkpoint intact, never a half-written one. Replaying from
        // an older checkpoint is free (recovery is idempotent); replaying from a
        // corrupt one is not.
        let final_path = self.config.dir.join(CHECKPOINT_FILE);
        let temp_path = self.config.dir.join(format!("{CHECKPOINT_FILE}.tmp"));

        let mut file = File::create(&temp_path)?;
        file.write_all(&serde_json::to_vec(&checkpoint)?)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temp_path, &final_path)?;
        Self::sync_dir(&self.config.dir)?;

        self.checkpoint = checkpoint;
        Ok(())
    }

    fn storage_bytes(&self) -> Result<u64> {
        let dir = self.segments_dir();
        let mut total = 0;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "seg") {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    fn archivable_segments(&self) -> Result<Vec<PathBuf>> {
        let dir = self.segments_dir();
        Ok(Self::segment_sequences(&dir)?
            .into_iter()
            // Strictly less than: the checkpoint's own segment is still being
            // read from, and the active segment is still being written to.
            .filter(|seq| *seq < self.checkpoint.position.segment)
            .map(|seq| Self::segment_path(&dir, seq))
            .collect())
    }

    fn release_segment(&mut self, sequence: u64) -> Result<u64> {
        // The same rule `archivable_segments` applies, re-checked here rather
        // than assumed from the fact that this sequence came from that list.
        // Between the two calls a checkpoint could not have moved *backwards* —
        // but the caller is a different process, and "it must have come from my
        // list" is exactly the reasoning that makes a bug in one program delete
        // another's data.
        if sequence >= self.checkpoint.position.segment {
            return Err(StorageError::SegmentInUse {
                sequence,
                checkpoint: self.checkpoint.position.segment,
            });
        }

        let path = Self::segment_path(&self.segments_dir(), sequence);
        let freed = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            // Already gone. Not an error: a sweep that was interrupted between
            // deleting and recording will run again, and the second run must
            // not report a failure for work the first one finished.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        std::fs::remove_file(&path)?;
        // The directory entry, not just the file. Without this the deletion can
        // be lost to a crash, and the segment reappears — which recovery would
        // then read, having already archived and released it.
        crate::fsync::sync_dir(&self.segments_dir())?;

        tracing::info!(sequence, freed, "segment released after archive proof");
        Ok(freed)
    }
}

/// A log that has been opened but not yet replayed.
///
/// It has no append path at all: the only way to reach a writable
/// [`SegmentWal`] is through [`RecoveringWal::recover`]. That is what makes
/// "never append past a torn record" a property of the API rather than a rule
/// someone has to remember.
#[derive(Debug)]
pub struct RecoveringWal {
    config: WalConfig,
    checkpoint: Checkpoint,
}

impl RecoveringWal {
    pub fn checkpoint_state(&self) -> &Checkpoint {
        &self.checkpoint
    }

    /// Replay from the last checkpoint, truncating a torn tail, and return the
    /// writable log positioned at the end of the valid region.
    ///
    /// Recovery is idempotent: running it twice over the same segments yields
    /// the same entries, because truncation only ever removes bytes that were
    /// already unreadable.
    pub fn recover(self) -> Result<(SegmentWal, Recovery)> {
        let from = self.checkpoint.position;
        self.recover_at(from)
    }

    /// Replay the entire log, ignoring the checkpoint.
    ///
    /// Used when the view snapshot the checkpoint refers to is missing or does
    /// not match it. Replaying from genesis is always correct — the log is the
    /// source of truth — it is just slower, so it is the safe fallback rather
    /// than an error.
    pub fn recover_from_genesis(self) -> Result<(SegmentWal, Recovery)> {
        self.recover_at(LogPosition::START)
    }

    fn recover_at(self, from: LogPosition) -> Result<(SegmentWal, Recovery)> {
        let dir = self.config.dir.join(SEGMENTS_DIR);
        // Cloned up front because `self.config` is moved into the returned wal
        // partway through, and recovery still needs the key after that point.
        let data_key = self.config.data_key.clone();
        let sequences = SegmentWal::segment_sequences(&dir)?;

        if sequences.is_empty() {
            let (active, position) = SegmentWal::create_segment(&dir, 0, data_key.is_some())?;
            let wal = SegmentWal {
                config: self.config,
                active,
                position,
                checkpoint: self.checkpoint,
            };
            let recovery = Recovery {
                from: LogPosition::START,
                to: position,
                ..Default::default()
            };
            return Ok((wal, recovery));
        }

        let mut recovery = Recovery {
            from,
            to: from,
            ..Default::default()
        };
        let mut damaged_at: Option<u64> = None;

        for seq in &sequences {
            let seq = *seq;
            if seq < from.segment {
                continue; // fully covered by the checkpoint
            }
            let path = SegmentWal::segment_path(&dir, seq);
            let start = if seq == from.segment {
                from.offset
            } else {
                HEADER_LEN
            };

            let scan = segment::scan(&path, start, data_key.as_ref())?;
            recovery.entries.extend(scan.entries);
            recovery.to = LogPosition {
                segment: seq,
                offset: scan.valid_end,
            };

            if let Some(torn) = scan.torn {
                match torn {
                    // Truncating to the header length would zero-extend the file
                    // and leave an all-zero header behind, so it is rewritten.
                    // Safe: the header is fsynced at creation before any record
                    // can be appended, so a partial header proves the segment
                    // holds nothing that was ever acknowledged.
                    TornRecord::HeaderIncomplete { available } => {
                        segment::rewrite_header(&path, seq, data_key.is_some())?;
                        tracing::warn!(
                            segment = seq,
                            available,
                            "segment header was incomplete; rewritten as an empty segment"
                        );
                    }
                    // Everything after this point comes from a write that was
                    // never acknowledged, so it is truncated, not interpreted.
                    _ => {
                        let file = OpenOptions::new().write(true).open(&path)?;
                        file.set_len(scan.valid_end)?;
                        file.sync_all()?;
                        tracing::warn!(
                            segment = seq,
                            offset = torn.offset(),
                            truncated_to = scan.valid_end,
                            "torn record found during recovery; segment truncated"
                        );
                    }
                }
                recovery.truncated = Some(torn);
                damaged_at = Some(seq);
                break;
            }
        }

        // A segment after the damaged one holds records we can no longer chain
        // to. Deleting them would destroy data and continuing past them would
        // silently reorder the log, so neither is ours to choose: stop and make
        // it an operator decision.
        if let Some(damaged) = damaged_at {
            let orphaned: Vec<u64> = sequences.iter().copied().filter(|s| *s > damaged).collect();
            if !orphaned.is_empty() {
                return Err(StorageError::OrphanedSegments {
                    damaged,
                    orphaned: orphaned
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }

        // The segment to append to next — unless its encryption flag disagrees
        // with the key we are configured with.
        //
        // A segment's flag applies to the whole file, so appending a sealed
        // record to a segment that declares itself plaintext writes bytes that
        // recovery will later read as JSON, fail to parse, and respond to by
        // *truncating the log*. That is silent data loss produced by the act of
        // turning encryption on, which would make the feature unusable by
        // anyone who already has data.
        //
        // Rolling to a fresh segment costs one empty file and makes enabling or
        // disabling encryption a boundary between segments rather than a
        // corruption inside one.
        let mut path = SegmentWal::segment_path(&dir, recovery.to.segment);
        let mut position = recovery.to;

        let active_is_encrypted =
            segment::read_header(&mut BufReader::new(File::open(&path)?))?.encrypted;
        if active_is_encrypted != data_key.is_some() {
            let next = recovery.to.segment + 1;
            tracing::info!(
                from = recovery.to.segment,
                to = next,
                now_encrypted = data_key.is_some(),
                "encryption at rest changed since the last segment was written; \
                 rolling to a new segment"
            );
            let (_, rolled) = SegmentWal::create_segment(&dir, next, data_key.is_some())?;
            path = SegmentWal::segment_path(&dir, next);
            position = rolled;
        }

        let active = OpenOptions::new().append(true).open(&path)?;

        recovery.to = position;

        let wal = SegmentWal {
            config: self.config,
            active,
            position,
            checkpoint: self.checkpoint,
        };
        Ok((wal, recovery))
    }
}
