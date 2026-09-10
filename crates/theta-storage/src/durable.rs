//! The durable log store: [`LogStore`] backed by the segment WAL.
//!
//! This is what `thetad` runs on. `MemLogStore` remains for tests and for the
//! embeddable core; both implement the same trait, and the durability tests in
//! `tests/durability.rs` plus the store tests here are what keep them
//! behaviourally identical.
//!
//! # Views are per branch
//!
//! One log holds every branch's entries interleaved, so a fold over the whole
//! stream is not any branch's state. Views are therefore kept per branch, and a
//! `BranchCreate` entry seeds the new branch's view from its parent's — which is
//! what makes branching O(1) in the log while still giving the new branch the
//! state it forked from.
//!
//! # The view snapshot
//!
//! A checkpoint says "everything up to here is folded in". That is only useful
//! if the folded state itself survives the restart, so [`DurableLogStore::checkpoint`]
//! writes a snapshot of the materialized view next to the checkpoint. If the
//! snapshot is missing or disagrees with the checkpoint, the store replays from
//! genesis instead of failing — the log is the source of truth, so a lost
//! snapshot costs time, never correctness.
//!
//! # What is held in memory
//!
//! The entries themselves live on disk, but an index of `hash -> position` and
//! the per-branch heads are kept in memory so `get` and `head` do not touch the
//! filesystem. That index is a fold over the log, so it is rebuilt on recovery
//! rather than persisted — persisting it would create a second source of truth
//! that could disagree with the log (`03-data-model-consistency.md` §2.1).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use theta_core::{BranchId, ContentHash, LogEntry, OpType};

use crate::encryption::DataKey;
use crate::error::{Result, StorageError};
use crate::log::LogStore;
use crate::view::MaterializedView;
use crate::wal::{LogPosition, Recovery, SegmentWal, Wal, WalConfig};

/// Snapshot of the materialized view as of the last checkpoint.
const VIEW_SNAPSHOT_FILE: &str = "VIEW";

/// Marks a sealed snapshot. Present so a reader can tell a sealed file from a
/// plaintext one without being told which to expect - the same reason the
/// segment header carries a flag, and what lets encryption be enabled on a
/// store that already has a snapshot on disk.
const SNAPSHOT_SEALED_MAGIC: &[u8] = b"THETASNAPSEALED1";

#[derive(Debug)]
pub struct DurableLogStore {
    wal: SegmentWal,
    /// Every entry, keyed by content hash. Rebuilt on recovery.
    entries: HashMap<ContentHash, LogEntry>,
    heads: HashMap<BranchId, ContentHash>,
    /// Entries folded into the view since the last checkpoint.
    since_checkpoint: u64,
    /// Fold every this many entries. A larger value means faster steady-state
    /// writes and a longer replay after a crash.
    checkpoint_interval: u64,
    entries_applied: u64,
}

/// Materialized state per branch.
pub type BranchViews = BTreeMap<BranchId, MaterializedView>;

/// What opening a durable store produced: the store, each branch's folded view,
/// and what recovery had to say.
#[derive(Debug)]
pub struct Opened {
    pub store: DurableLogStore,
    pub views: BranchViews,
    pub recovery: Recovery,
}

impl Opened {
    /// The view for `branch`, or an empty one if the branch has no entries yet.
    pub fn view(&self, branch: BranchId) -> MaterializedView {
        self.views.get(&branch).cloned().unwrap_or_default()
    }
}

/// A view snapshot, and the log position it is a fold of.
///
/// The position is carried rather than derived, because it cannot be derived.
/// It used to be `views.values().map(|v| v.applied).sum()`, which is correct
/// with one branch and wrong with two: a fork inherits its parent's `applied`
/// count along with its rows, so main having folded N entries and a fork of
/// main having folded the same N summed to 2N when the log held N + 1.
///
/// The consequence was not a wrong number in a log line. `open_with_views`
/// compares that sum against the checkpoint and returns `ViewDrift` when they
/// disagree, so **a store that branched, checkpointed and restarted refused to
/// open** — a correct snapshot rejected by an incorrect check. The data was
/// never at risk (a full replay rebuilds it) but the fast path was unusable.
/// Found by `bench/comparative/branch_compare.py` forking fifty deep;
/// `a_branched_store_reopens_from_its_checkpoint` is the regression test.
#[derive(serde::Serialize, serde::Deserialize)]
struct ViewSnapshot {
    /// Entries the log had applied when this was written.
    applied: u64,
    views: BranchViews,
}

impl DurableLogStore {
    pub const DEFAULT_CHECKPOINT_INTERVAL: u64 = 1_000;

    /// Open a store, replay the log, and restore the materialized view.
    ///
    /// Uses the view snapshot written at the last checkpoint when one is
    /// available and consistent with it, so only entries after the checkpoint
    /// replay. Falls back to a full replay otherwise.
    pub fn open(config: WalConfig) -> Result<Opened> {
        let dir = config.dir.clone();
        let data_key = config.data_key.clone();
        let recovering = SegmentWal::open(config)?;
        let checkpoint_applied = recovering.checkpoint_state().entries_applied;

        let snapshot = Self::read_view_snapshot(&dir, data_key.as_ref())?;
        let resumable = snapshot.filter(|(applied, _)| *applied == checkpoint_applied);

        let (views, from_applied, (wal, recovery)) = match resumable {
            Some((_, views)) => (views, checkpoint_applied, recovering.recover()?),
            None => {
                if checkpoint_applied > 0 {
                    tracing::warn!(
                        checkpoint_applied,
                        "view snapshot missing or inconsistent with the checkpoint; \
                         replaying the log from genesis"
                    );
                }
                (BranchViews::new(), 0, recovering.recover_from_genesis()?)
            }
        };

        Self::assemble(wal, recovery, views, from_applied)
    }

    /// Read the view snapshot, if there is a readable one.
    ///
    /// An unreadable snapshot is not an error: it is discarded and the log is
    /// replayed instead. The snapshot is a cache, never a second source of truth.
    ///
    /// That is also why a snapshot that will not decrypt is discarded rather
    /// than raised. If the key is genuinely wrong the log will refuse next, with
    /// an error that says so — and it is the log's refusal that should reach the
    /// operator, because the log is the thing that cannot be rebuilt.
    fn read_view_snapshot(dir: &Path, key: Option<&DataKey>) -> Result<Option<(u64, BranchViews)>> {
        let path = dir.join(VIEW_SNAPSHOT_FILE);
        if !path.exists() {
            return Ok(None);
        }

        let raw = fs::read(&path)?;
        let plaintext = match raw.strip_prefix(SNAPSHOT_SEALED_MAGIC) {
            Some(sealed) => match key.map(|k| k.open(sealed)) {
                Some(Ok(plaintext)) => plaintext,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "view snapshot did not decrypt; discarding it");
                    return Ok(None);
                }
                None => {
                    tracing::warn!(
                        "view snapshot is encrypted and no data key is configured; \
                         discarding it"
                    );
                    return Ok(None);
                }
            },
            // No magic: written before encryption was enabled. Still valid, and
            // replaced by a sealed one at the next checkpoint.
            None => raw,
        };

        match serde_json::from_slice::<ViewSnapshot>(&plaintext) {
            Ok(ViewSnapshot { applied, mut views }) => {
                // A snapshot carries rows and schema but not indexes, which are
                // a fold over both (invariant 6). Rebuilt here so a process that
                // started from a snapshot answers indexed queries the same way
                // as one that replayed the log.
                for view in views.values_mut() {
                    view.rebuild_indexes();
                }
                Ok(Some((applied, views)))
            }
            Err(e) => {
                tracing::warn!(error = %e, "view snapshot is unreadable; discarding it");
                Ok(None)
            }
        }
    }

    /// Write the view snapshot, sealed if the store is encrypted.
    ///
    /// Sealed for the same reason the log is (SEC-2): the snapshot holds every
    /// row of every branch, so encrypting the log and leaving this in the clear
    /// would encrypt nothing in practice — the plaintext would simply be in a
    /// different file on the same volume.
    fn write_view_snapshot(
        dir: &Path,
        applied: u64,
        views: &BranchViews,
        key: Option<&DataKey>,
    ) -> Result<()> {
        // Temp file then rename, same as the checkpoint: a crash mid-write must
        // leave the previous snapshot intact rather than a half-written one.
        let final_path = dir.join(VIEW_SNAPSHOT_FILE);
        let temp_path = dir.join(format!("{VIEW_SNAPSHOT_FILE}.tmp"));

        // The position travels with the views, because it cannot be recovered
        // from them — see `ViewSnapshot`.
        let body = serde_json::to_vec(&ViewSnapshot {
            applied,
            views: views.clone(),
        })?;
        let body = match key {
            Some(key) => {
                let mut sealed = SNAPSHOT_SEALED_MAGIC.to_vec();
                sealed.extend_from_slice(&key.seal(&body)?);
                sealed
            }
            None => body,
        };

        let mut file = fs::File::create(&temp_path)?;
        file.write_all(&body)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temp_path, &final_path)?;
        crate::fsync::sync_dir(dir)?;
        Ok(())
    }

    /// Build the store and catch the view up over the replayed entries.
    ///
    /// Verifies the hash chain as it goes (SEC-8). `SECURITY-REVIEW.md` records
    /// "the log is hash-chained, so tampering with history is detectable" as a
    /// property worth defending — and it was only half true: the chain was
    /// written and never read. `prev_hash` that nothing checks is decoration.
    ///
    /// # What this catches, and what it cannot
    ///
    /// Every entry's hash covers its contents, so altering an entry changes its
    /// hash and orphans its successor. Any edit to an entry that *has* a
    /// successor in the replayed range is therefore detected.
    ///
    /// It cannot detect an edit to the last entry of a branch, and it cannot
    /// detect entries removed from the end. Nothing internal can: that needs an
    /// anchor outside the log — a signed head, published somewhere the holder of
    /// the disk does not control. That is the v2 item the review points at, and
    /// it is deliberately not claimed here.
    ///
    /// It also only checks what it replays. A recovery that resumes from a
    /// checkpoint never reads the entries the snapshot already covers, so an
    /// edit below the checkpoint is invisible until a full replay. See
    /// [`DurableLogStore::verify_chain`], which is that full replay.
    fn assemble(
        wal: SegmentWal,
        recovery: Recovery,
        mut views: BranchViews,
        from_applied: u64,
    ) -> Result<Opened> {
        let mut store = Self {
            wal,
            entries: HashMap::new(),
            heads: HashMap::new(),
            since_checkpoint: recovery.entries.len() as u64,
            checkpoint_interval: Self::DEFAULT_CHECKPOINT_INTERVAL,
            entries_applied: from_applied,
        };
        // A replay that started at genesis has seen every entry there is, so
        // the only parent it may legitimately not recognise is the empty one.
        // A resumed replay begins mid-log, so the first entry it meets on each
        // branch necessarily points below the window — that one is a boundary,
        // not a break, and every entry after it on that branch must link.
        let from_genesis = from_applied == 0;
        let mut branch_started: HashSet<BranchId> = HashSet::new();

        for entry in &recovery.entries {
            let first_on_branch = branch_started.insert(entry.branch_id);
            let links = entry.prev_hash == ContentHash::ZERO
                || store.entries.contains_key(&entry.prev_hash);
            // The one entry per branch a resumed replay is allowed not to
            // recognise: its parent is below the window rather than missing.
            let window_boundary = first_on_branch && !from_genesis;
            if !(links || window_boundary) {
                return Err(StorageError::BrokenChain {
                    commit_id: entry.commit_id.0,
                    prev_hash: entry.prev_hash.to_hex(),
                    branch: entry.branch_id.0,
                });
            }

            store.index(entry.clone());

            // A new branch inherits its parent's state as of the fork point. The
            // parent is whichever branch owns the commit forked from, which is
            // already indexed because the log replays in order.
            if let OpType::BranchCreate { from, .. } = &entry.op {
                let inherited = store
                    .entries
                    .get(from)
                    .map(|parent| parent.branch_id)
                    .and_then(|parent| views.get(&parent).cloned())
                    .unwrap_or_default();
                views.insert(entry.branch_id, inherited);
            }

            views.entry(entry.branch_id).or_default().apply(entry);
            store.entries_applied += 1;
        }

        Ok(Opened {
            store,
            views,
            recovery,
        })
    }

    /// Replay the whole log from genesis and verify every link in the chain.
    ///
    /// Separate from `open` because `open` resumes from a checkpoint, and a
    /// resumed recovery never reads the entries the snapshot already covers —
    /// so it cannot speak for them. This reads all of them.
    ///
    /// The cost is a full replay, which is why it is not what starting the
    /// server does. It is what an operator runs when they have reason to
    /// suspect the disk, and what an audit runs on a schedule.
    ///
    /// Returns the number of entries verified, so "it passed" is distinguishable
    /// from "it found nothing to check".
    pub fn verify_chain(config: WalConfig) -> Result<u64> {
        let recovering = SegmentWal::open(config)?;
        let (_, recovery) = recovering.recover_from_genesis()?;

        let mut seen: HashSet<ContentHash> = HashSet::new();
        for entry in &recovery.entries {
            if entry.prev_hash != ContentHash::ZERO && !seen.contains(&entry.prev_hash) {
                return Err(StorageError::BrokenChain {
                    commit_id: entry.commit_id.0,
                    prev_hash: entry.prev_hash.to_hex(),
                    branch: entry.branch_id.0,
                });
            }
            seen.insert(entry.hash());
        }
        Ok(recovery.entries.len() as u64)
    }

    /// Open, replaying only what the checkpoint does not already cover, and
    /// catch `views` up incrementally.
    ///
    /// `views` must be the state as of the WAL's checkpoint. Passing views from
    /// a different position produces a state that is not a fold over the log,
    /// which is exactly the thing the design forbids — so the mismatch is
    /// checked, not trusted.
    /// The caller states which log position the views are a fold of, and the
    /// store refuses them if it disagrees.
    ///
    /// `applied` is a parameter rather than something derived from `views`
    /// because it cannot be derived: a forked branch inherits its parent's
    /// count, so summing over branches double-counts shared history — see
    /// [`ViewSnapshot`]. It is *required* rather than optional deliberately.
    /// An `Option` here would have made "I do not know" a valid answer, and
    /// the whole point of this function is that passing views from the wrong
    /// position is caught rather than trusted.
    pub fn open_with_views(config: WalConfig, applied: u64, views: BranchViews) -> Result<Opened> {
        let recovering = SegmentWal::open(config)?;
        let expected = recovering.checkpoint_state().entries_applied;

        if applied != expected {
            return Err(StorageError::ViewDrift {
                view_applied: applied,
                checkpoint_applied: expected,
            });
        }

        let (wal, recovery) = recovering.recover()?;
        Self::assemble(wal, recovery, views, expected)
    }

    pub fn with_checkpoint_interval(mut self, interval: u64) -> Self {
        self.checkpoint_interval = interval.max(1);
        self
    }

    pub fn position(&self) -> LogPosition {
        self.wal.position()
    }

    pub fn entries_applied(&self) -> u64 {
        self.entries_applied
    }

    /// A branch's entries, oldest first.
    ///
    /// Walked back from the head through `prev_hash` and reversed, rather than
    /// filtered out of the index by branch id. The chain is the authority on
    /// what a branch contains: a fork inherits its parent's history, and those
    /// entries carry the *parent's* branch id, so filtering on the id would
    /// return only what happened after the fork and silently lose everything
    /// the branch inherited.
    ///
    /// **Cost is linear in the chain**, so this is for orientation calls like
    /// `describe`, not for the hot path. It reads only the in-memory index and
    /// touches no disk.
    pub fn chain(&self, branch: BranchId) -> Vec<LogEntry> {
        let mut walked = Vec::new();
        let mut cursor = self.heads.get(&branch).copied();

        while let Some(hash) = cursor {
            if hash.is_zero() {
                break;
            }
            let Some(entry) = self.entries.get(&hash) else {
                // A gap means the segment holding it has been released to the
                // archive. Stopping here is the honest answer: everything older
                // is outside what this process can see, and callers report that
                // as an absence rather than guessing (`specs/03`, retention
                // bounds time travel).
                break;
            };
            cursor = Some(entry.prev_hash);
            walked.push(entry.clone());
        }

        walked.reverse();
        walked
    }

    /// A branch's schema entries only, oldest first.
    ///
    /// What provenance needs. Filtering here rather than at the caller keeps
    /// the whole-chain clone out of the common path: a branch with a million
    /// writes and four schema changes should cost four entries to describe.
    pub fn schema_chain(&self, branch: BranchId) -> Vec<LogEntry> {
        let mut walked = Vec::new();
        let mut cursor = self.heads.get(&branch).copied();

        while let Some(hash) = cursor {
            if hash.is_zero() {
                break;
            }
            let Some(entry) = self.entries.get(&hash) else {
                break;
            };
            cursor = Some(entry.prev_hash);
            if matches!(entry.op, OpType::Schema { .. }) {
                walked.push(entry.clone());
            }
        }

        walked.reverse();
        walked
    }

    /// Fold `entry` into the in-memory index and advance its branch head.
    fn index(&mut self, entry: LogEntry) {
        let hash = entry.hash();
        let branch = entry.branch_id;
        self.entries.insert(hash, entry);
        self.heads.insert(branch, hash);
    }

    /// Append durably and fold into the owning branch's view, keeping the two
    /// in lockstep.
    ///
    /// Taking the views here rather than letting callers apply separately is
    /// deliberate: it makes it impossible to acknowledge a write that is durable
    /// but not visible, or visible but not durable.
    pub fn append_and_apply(
        &mut self,
        entry: LogEntry,
        views: &mut BranchViews,
    ) -> Result<ContentHash> {
        let expected = self
            .heads
            .get(&entry.branch_id)
            .copied()
            .unwrap_or(ContentHash::ZERO);
        if entry.prev_hash != expected {
            return Err(StorageError::HeadMoved {
                expected: entry.prev_hash.to_hex(),
                actual: expected.to_hex(),
            });
        }

        // Durable before visible. If the fsync fails the caller is told the
        // write failed, and nothing has been folded into any view.
        self.wal.append(&entry)?;

        let hash = entry.hash();
        self.seed_branch_view(&entry, views);
        views.entry(entry.branch_id).or_default().apply(&entry);
        self.index(entry);
        self.entries_applied += 1;
        self.since_checkpoint += 1;

        if self.since_checkpoint >= self.checkpoint_interval {
            self.checkpoint(views)?;
        }

        Ok(hash)
    }

    /// Seed a new branch's view from its parent, so the live path folds a
    /// `BranchCreate` exactly the way recovery does.
    fn seed_branch_view(&self, entry: &LogEntry, views: &mut BranchViews) {
        let OpType::BranchCreate { from, .. } = &entry.op else {
            return;
        };
        let inherited = self
            .entries
            .get(from)
            .map(|parent| parent.branch_id)
            .and_then(|parent| views.get(&parent).cloned())
            .unwrap_or_default();
        views.insert(entry.branch_id, inherited);
    }

    /// Record that everything durable so far is folded into `views`.
    pub fn checkpoint(&mut self, views: &BranchViews) -> Result<()> {
        // The store's own counter, not a sum over the views. Summing was wrong
        // as soon as a branch existed — see `ViewSnapshot`.
        let applied = self.entries_applied;
        // Snapshot first, checkpoint second. A crash between the two leaves a
        // snapshot ahead of the checkpoint, which `open` detects and resolves by
        // replaying — the reverse order would leave a checkpoint pointing at a
        // snapshot that does not exist.
        Self::write_view_snapshot(self.wal.dir(), applied, views, self.wal.data_key())?;
        self.wal.checkpoint(self.wal.position(), applied)?;
        self.since_checkpoint = 0;
        Ok(())
    }

    /// Segments fully behind the checkpoint, safe to ship to cold archive.
    /// Delete a segment cold archive has proved. Returns the bytes freed.
    ///
    /// See [`Wal::release_segment`] for why this re-checks the caller rather
    /// than trusting the sequence it was handed.
    pub fn release_segment(&mut self, sequence: u64) -> Result<u64> {
        self.wal.release_segment(sequence)
    }

    pub fn archivable_segments(&self) -> Result<Vec<std::path::PathBuf>> {
        self.wal.archivable_segments()
    }

    /// Bytes this project's log occupies on disk.
    pub fn storage_bytes(&self) -> Result<u64> {
        self.wal.storage_bytes()
    }

    /// Point a branch at an existing commit. O(1).
    pub fn set_head(&mut self, branch: BranchId, head: ContentHash) {
        self.heads.insert(branch, head);
    }
}

impl LogStore for DurableLogStore {
    /// Append without a view to fold into.
    ///
    /// Prefer [`DurableLogStore::append_and_apply`]: this exists to satisfy the
    /// trait, and leaves the caller responsible for keeping the view in step.
    fn append(&mut self, entry: LogEntry) -> Result<ContentHash> {
        let expected = self
            .heads
            .get(&entry.branch_id)
            .copied()
            .unwrap_or(ContentHash::ZERO);
        if entry.prev_hash != expected {
            return Err(StorageError::HeadMoved {
                expected: entry.prev_hash.to_hex(),
                actual: expected.to_hex(),
            });
        }
        self.wal.append(&entry)?;
        let hash = entry.hash();
        self.index(entry);
        self.entries_applied += 1;
        self.since_checkpoint += 1;
        Ok(hash)
    }

    fn get(&self, hash: &ContentHash) -> Option<&LogEntry> {
        self.entries.get(hash)
    }

    fn head(&self, branch: BranchId) -> Option<ContentHash> {
        self.heads.get(&branch).copied()
    }

    fn history(&self, branch: BranchId, until: Option<ContentHash>) -> Result<Vec<LogEntry>> {
        let mut out = Vec::new();
        let mut cursor = match self.heads.get(&branch) {
            Some(head) => *head,
            None => return Ok(out),
        };
        while !cursor.is_zero() && Some(cursor) != until {
            let entry = self
                .entries
                .get(&cursor)
                .ok_or_else(|| StorageError::UnknownCommit(cursor.to_hex()))?;
            cursor = entry.prev_hash;
            out.push(entry.clone());
        }
        Ok(out)
    }
}
