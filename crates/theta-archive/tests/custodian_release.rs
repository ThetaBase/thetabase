//! M10: sweeps run on a schedule, and a proved segment is let go.
//!
//! The property is unglamorous and load-bearing: **local disk goes down**.
//! Everything needed to archive and prove a segment already existed and nothing
//! deleted one, so a database that ran for a month kept a month of segments.
//!
//! Most tests here drive a real `DurableLogStore` rather than a stub of one.
//! The interesting question is not whether the custodian calls `release` — it
//! is whether the storage engine agrees to it, and a stub would agree to
//! anything.
//!
//! The last two use a fake source, because they need failures a real store
//! cannot be asked for on demand: a data directory that cannot be listed, and a
//! manifest with a hole in it.

use std::path::PathBuf;

use theta_archive::custodian::{Custodian, SegmentSource};
use theta_archive::{ArchiveBackend, ArchiveError, Digest, Manifest, StoredRef, Sweep};
use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::durable::{BranchViews, DurableLogStore};
use theta_storage::wal::WalConfig;

const MAIN: BranchId = BranchId::MAIN;

/// A log with several rolled segments and a checkpoint well past them.
struct Log {
    store: DurableLogStore,
    dir: tempfile::TempDir,
}

impl Log {
    /// Small segments, so a handful of writes rolls several files.
    fn with_segments(count: u64) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = WalConfig {
            segment_bytes: 512,
            ..WalConfig::new(dir.path())
        };
        let opened = DurableLogStore::open(config).expect("open");
        let mut store = opened.store;
        let mut views: BranchViews = opened.views;

        let mut head = ContentHash::ZERO;
        for n in 0..count * 8 {
            let entry = LogEntry {
                prev_hash: head,
                commit_id: CommitId(n),
                branch_id: MAIN,
                op: OpType::Put {
                    key: format!("k{n}"),
                    value: Value::Text("x".repeat(48)),
                },
                author: Author::System,
                timestamp_ms: n as i64,
            };
            head = store.append_and_apply(entry, &mut views).expect("append");
        }
        // Checkpoint at the end, so everything before the active segment is
        // archivable.
        store.checkpoint(&views).expect("checkpoint");
        Self { store, dir }
    }

    fn segments_dir(&self) -> PathBuf {
        self.dir.path().join("segments")
    }

    fn segment_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.segments_dir())
            .expect("read segments")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        files.sort();
        files
    }

    fn bytes_on_disk(&self) -> u64 {
        self.segment_files()
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }
}

impl SegmentSource for Log {
    fn archivable(&self) -> Result<Vec<PathBuf>, String> {
        self.store.archivable_segments().map_err(|e| e.to_string())
    }

    fn release(&mut self, sequence: u64) -> Result<u64, String> {
        self.store
            .release_segment(sequence)
            .map_err(|e| e.to_string())
    }
}

/// An archive that stores faithfully, or misbehaves on demand.
#[derive(Default)]
struct Backend {
    root: Option<tempfile::TempDir>,
    unavailable: bool,
    /// Return altered bytes, so the proof fails the way a silently corrupting
    /// archive would.
    corrupt: bool,
}

impl Backend {
    fn working() -> Self {
        Self {
            root: Some(tempfile::tempdir().expect("tempdir")),
            ..Default::default()
        }
    }
    fn unreachable() -> Self {
        Self {
            unavailable: true,
            ..Self::working()
        }
    }
    fn corrupting() -> Self {
        Self {
            corrupt: true,
            ..Self::working()
        }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.root
            .as_ref()
            .expect("root")
            .path()
            .join(key.replace('/', "_"))
    }
}

impl ArchiveBackend for Backend {
    fn store(&mut self, key: &str, source: &std::path::Path) -> Result<StoredRef, ArchiveError> {
        if self.unavailable {
            return Err(ArchiveError::Unavailable("the archive is down".into()));
        }
        let bytes = read(source)?;
        write(&self.path(key), &bytes)?;
        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes: bytes.len() as u64,
            container_checksum: Some(Digest::of(&bytes).to_hex()),
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &std::path::Path) -> Result<(), ArchiveError> {
        if self.unavailable {
            return Err(ArchiveError::Unavailable("the archive is down".into()));
        }
        let mut bytes = read(&self.path(&stored.key))?;
        // A silently corrupting archive: it accepted the write, reports success,
        // and returns something else. The failure the round-trip proof is for.
        if self.corrupt && !bytes.is_empty() {
            bytes[0] ^= 0xff;
        }
        write(dest, &bytes)
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        if self.unavailable {
            return Err(ArchiveError::Unavailable("the archive is down".into()));
        }
        // Deliberately says yes even while `fetch` corrupts. That is the point
        // of the container check being weaker than the round trip: it verifies
        // the container, not that the container holds what we meant to store.
        Ok(self.path(&stored.key).exists())
    }
}

fn read(path: &std::path::Path) -> Result<Vec<u8>, ArchiveError> {
    std::fs::read(path).map_err(|e| ArchiveError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

fn write(path: &std::path::Path, bytes: &[u8]) -> Result<(), ArchiveError> {
    std::fs::write(path, bytes).map_err(|e| ArchiveError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

fn custodian(backend: Backend) -> Custodian<Backend> {
    let scratch = tempfile::tempdir().expect("scratch");
    let path = scratch.path().to_path_buf();
    // Leaked deliberately: the sweep writes verification files here for the
    // life of the custodian, and dropping the handle would remove the directory
    // underneath it.
    std::mem::forget(scratch);
    Custodian::new(Sweep::new(backend, Manifest::new("org_a/checkout"), path))
}

#[test]
fn a_tick_archives_every_archivable_segment_and_frees_the_disk() {
    // The whole point. Not "release was called" — the bytes are gone.
    let mut log = Log::with_segments(6);
    let before = log.bytes_on_disk();
    let archivable = log.archivable().expect("archivable").len();
    assert!(archivable >= 3, "the fixture did not roll enough segments");

    let mut custodian = custodian(Backend::working());
    let report = custodian.tick(&mut log, 1_000);

    assert_eq!(report.archived.len(), archivable, "{report:?}");
    assert_eq!(report.released.len(), archivable, "{report:?}");
    assert!(report.freed_bytes > 0);
    assert!(!report.needs_attention(), "{report:?}");

    let after = log.bytes_on_disk();
    assert!(
        after < before,
        "local disk did not shrink: {before} -> {after}"
    );
    assert_eq!(
        before - after,
        report.freed_bytes,
        "the report and the disk disagree"
    );
}

#[test]
fn the_active_and_checkpoint_segments_are_never_released() {
    // The safety half. Releasing these would delete what recovery reads.
    let mut log = Log::with_segments(6);
    let mut custodian = custodian(Backend::working());
    custodian.tick(&mut log, 1_000);

    let left = log.segment_files();
    assert!(
        !left.is_empty(),
        "every segment was released, including the one being written to"
    );

    // And the log still opens and still holds its rows.
    let reopened = DurableLogStore::open(WalConfig {
        segment_bytes: 512,
        ..WalConfig::new(log.dir.path())
    })
    .expect("the log must still open after a release sweep");
    assert!(reopened.view(MAIN).get("k0").is_some(), "history was lost");
}

#[test]
fn an_unreachable_archive_releases_nothing_and_is_not_an_incident() {
    // `specs/01` §7: writes continue locally, snapshots queue and retry. A
    // database whose backup being down deletes nothing and pages nobody.
    let mut log = Log::with_segments(6);
    let before = log.bytes_on_disk();

    let mut custodian = custodian(Backend::unreachable());
    let report = custodian.tick(&mut log, 1_000);

    assert!(report.released.is_empty(), "released without an archive");
    assert!(!report.deferred.is_empty(), "{report:?}");
    assert!(
        !report.needs_attention(),
        "an unreachable archive paged somebody: {report:?}"
    );
    assert_eq!(log.bytes_on_disk(), before, "disk changed anyway");
}

#[test]
fn a_failed_proof_keeps_the_segment_and_asks_for_a_human() {
    // The archive accepted the write and returned different bytes. That is the
    // failure the round-trip proof exists to catch, and the one case where
    // keeping the local copy is the whole value of the check.
    let mut log = Log::with_segments(6);
    let before = log.bytes_on_disk();

    let mut custodian = custodian(Backend::corrupting());
    let report = custodian.tick(&mut log, 1_000);

    assert!(!report.failed.is_empty(), "{report:?}");
    assert!(
        report.released.is_empty(),
        "a segment was released on a failed proof"
    );
    assert!(
        report.needs_attention(),
        "a corrupting archive did not raise anything"
    );
    assert_eq!(log.bytes_on_disk(), before);
}

#[test]
fn a_second_tick_has_nothing_left_to_do() {
    // Idempotence. A scheduler runs this every few minutes forever, so a tick
    // that re-archived everything would multiply the bill by the number of
    // ticks.
    let mut log = Log::with_segments(6);
    let mut custodian = custodian(Backend::working());

    let first = custodian.tick(&mut log, 1_000);
    assert!(!first.archived.is_empty());

    let second = custodian.tick(&mut log, 2_000);
    assert!(second.archived.is_empty(), "{second:?}");
    assert!(second.released.is_empty(), "{second:?}");
    assert_eq!(second.freed_bytes, 0);
    assert!(!second.needs_attention());
}

#[test]
fn without_releasing_archives_everything_and_deletes_nothing() {
    // The stance an operator building confidence wants, supported explicitly
    // rather than by commenting out a line.
    let mut log = Log::with_segments(6);
    let before = log.bytes_on_disk();

    let mut custodian = custodian(Backend::working()).without_releasing();
    let report = custodian.tick(&mut log, 1_000);

    assert!(!report.archived.is_empty(), "{report:?}");
    assert!(report.released.is_empty());
    assert_eq!(log.bytes_on_disk(), before, "a segment was deleted anyway");
}

#[test]
fn releasing_a_segment_the_log_still_needs_is_refused() {
    // Straight at the storage layer, because this is its decision and not the
    // archiver's. A custodian bug must be a wasted sweep, never a lost segment.
    let mut log = Log::with_segments(6);
    let archivable = log.archivable().expect("archivable");
    let highest: u64 = archivable
        .iter()
        .filter_map(|p| p.file_stem()?.to_str()?.parse::<u64>().ok())
        .max()
        .expect("some archivable segment");

    // The next one up is at or above the checkpoint.
    let err = log
        .store
        .release_segment(highest + 1)
        .expect_err("the log must refuse a segment it still needs");
    assert!(
        err.to_string().contains("cannot be released"),
        "the refusal must say why: {err}"
    );
}

#[test]
fn releasing_twice_is_not_an_error() {
    // A sweep interrupted between deleting and recording runs again, and the
    // second run must not report a failure for work the first one finished.
    let mut log = Log::with_segments(6);
    let sequence: u64 = log
        .archivable()
        .expect("archivable")
        .first()
        .and_then(|p| p.file_stem()?.to_str()?.parse().ok())
        .expect("a segment");

    let freed = log.store.release_segment(sequence).expect("release");
    assert!(freed > 0);
    assert_eq!(
        log.store.release_segment(sequence).expect("second release"),
        0,
        "releasing an already-released segment must be a no-op, not a failure"
    );
}

#[test]
fn a_gap_in_the_manifest_is_reported_every_tick() {
    // An archive with a hole is broken from the moment the hole appears.
    // Finding out at restore time means finding out at the worst moment.
    let mut manifest = Manifest::new("org_a/checkout");
    for sequence in [0u64, 1, 3] {
        manifest.record(theta_archive::ArchivedSegment {
            sequence,
            digest: Digest::of(b"x"),
            original_bytes: 1,
            stored: StoredRef {
                key: format!("segments/{sequence:012}.at1"),
                stored_bytes: 1,
                container_checksum: None,
            },
            verified_at_ms: 0,
        });
    }

    let scratch = tempfile::tempdir().expect("scratch");
    let mut custodian = Custodian::new(Sweep::new(
        Backend::working(),
        manifest,
        scratch.path().to_path_buf(),
    ));

    // Nothing to archive; the gap check still runs.
    struct Empty;
    impl SegmentSource for Empty {
        fn archivable(&self) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }
        fn release(&mut self, _sequence: u64) -> Result<u64, String> {
            unreachable!("nothing to release")
        }
    }

    let report = custodian.tick(&mut Empty, 1_000);
    assert_eq!(report.gaps, vec![2], "{report:?}");
    assert!(report.needs_attention());
}

#[test]
fn a_source_that_cannot_be_listed_is_a_failure_not_a_quiet_no_op() {
    // A tick that archived nothing because it could not look must not read the
    // same as a tick with nothing to do — that is how an archive silently stops
    // running.
    struct Broken;
    impl SegmentSource for Broken {
        fn archivable(&self) -> Result<Vec<PathBuf>, String> {
            Err("the data directory is unreadable".into())
        }
        fn release(&mut self, _sequence: u64) -> Result<u64, String> {
            unreachable!()
        }
    }

    let mut custodian = custodian(Backend::working());
    let report = custodian.tick(&mut Broken, 1_000);
    assert!(report.needs_attention(), "{report:?}");
    assert!(
        report.summary().contains("failed 1"),
        "{}",
        report.summary()
    );
}
