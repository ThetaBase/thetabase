//! Durability and crash-recovery tests for the segment WAL.
//!
//! These are the M1 gate's crash half (`docs/specs/08-test-validation-plan.md`
//! §2). The property under test throughout: **an append that returned `Ok`
//! survives, and anything that did not is never resurrected.** A recovery that
//! invents an unacknowledged write is as bad as one that loses an acknowledged
//! one — arguably worse, because the caller was told it failed.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::segment::TornRecord;
use theta_storage::wal::{LogPosition, Recovery, SegmentWal, SyncPolicy, Wal, WalConfig};

fn entry(n: u64) -> LogEntry {
    LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(n),
        branch_id: BranchId::MAIN,
        op: OpType::Put {
            key: format!("k{n}"),
            value: Value::Int(n as i64),
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

/// Open and replay, the only way to reach a writable log.
fn recover(dir: &std::path::Path) -> (SegmentWal, Recovery) {
    SegmentWal::open(WalConfig::new(dir))
        .expect("open wal")
        .recover()
        .expect("recover")
}

fn wal(dir: &std::path::Path) -> SegmentWal {
    recover(dir).0
}

/// Simulate a process that died partway through writing a record: append real
/// bytes, then lose the tail of the last one.
fn tear_last_record(dir: &std::path::Path, lost_bytes: u64) {
    let segments = dir.join("segments");
    let mut newest: Option<std::path::PathBuf> = None;
    for e in std::fs::read_dir(&segments).expect("read segments") {
        let p = e.expect("entry").path();
        if p.extension().is_some_and(|x| x == "seg") {
            newest = Some(match newest {
                Some(cur) if cur > p => cur,
                _ => p,
            });
        }
    }
    let path = newest.expect("at least one segment");
    let file = OpenOptions::new().write(true).open(&path).expect("open");
    let len = file.metadata().expect("meta").len();
    file.set_len(len - lost_bytes).expect("truncate");
    file.sync_all().expect("sync");
}

#[test]
fn an_acknowledged_append_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..10 {
        w.append(&entry(i)).expect("append");
    }
    drop(w); // process dies here, with no clean shutdown

    let (_reopened, recovery) = recover(dir.path());
    assert_eq!(recovery.entries.len(), 10);
    assert!(recovery.truncated.is_none());
    assert_eq!(recovery.entries, (0..10).map(entry).collect::<Vec<_>>());
}

#[test]
fn a_torn_final_record_is_dropped_and_never_resurrected() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..5 {
        w.append(&entry(i)).expect("append");
    }
    drop(w);

    // The 6th write died mid-flight.
    tear_last_record(dir.path(), 20);

    let (_reopened, recovery) = recover(dir.path());
    assert_eq!(
        recovery.entries.len(),
        4,
        "the torn record must not be replayed"
    );
    assert!(matches!(
        recovery.truncated,
        Some(TornRecord::Truncated { .. })
    ));
}

#[test]
fn appending_after_a_torn_recovery_lands_on_clean_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..5 {
        w.append(&entry(i)).expect("append");
    }
    drop(w);
    tear_last_record(dir.path(), 15);

    let (mut reopened, first_recovery) = recover(dir.path());
    let survived = first_recovery.entries.len();
    reopened.append(&entry(100)).expect("append after recovery");
    drop(reopened);

    // The new record must be readable, which it only is if the torn bytes were
    // truncated rather than appended past.
    let (_third, recovery) = recover(dir.path());
    assert!(
        recovery.truncated.is_none(),
        "torn bytes were not cleaned up"
    );
    assert_eq!(recovery.entries.len(), survived + 1);
    assert_eq!(
        recovery.entries.last().expect("entry").commit_id,
        CommitId(100)
    );
}

#[test]
fn recovery_is_idempotent_under_repeated_replay() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..8 {
        w.append(&entry(i)).expect("append");
    }
    drop(w);

    let (first, a) = recover(dir.path());
    drop(first);
    let (_second, b) = recover(dir.path());
    assert_eq!(
        a.entries, b.entries,
        "a second replay must produce the same entries"
    );
}

#[test]
fn a_checkpoint_lets_recovery_skip_what_is_already_folded() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..6 {
        w.append(&entry(i)).expect("append");
    }
    let mid = w.position();
    for i in 6..10 {
        w.append(&entry(i)).expect("append");
    }
    w.checkpoint(mid, 6).expect("checkpoint");
    drop(w);

    let (_reopened, recovery) = recover(dir.path());
    assert_eq!(
        recovery.entries.len(),
        4,
        "only post-checkpoint entries replay"
    );
    assert_eq!(recovery.entries[0].commit_id, CommitId(6));
}

#[test]
fn a_crash_midway_through_a_checkpoint_leaves_the_previous_one_intact() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..6 {
        w.append(&entry(i)).expect("append");
    }
    let good = w.position();
    w.checkpoint(good, 6).expect("checkpoint");
    for i in 6..10 {
        w.append(&entry(i)).expect("append");
    }
    drop(w);

    // A half-written temp file is what a crash mid-checkpoint leaves behind. The
    // rename is atomic, so it must not be picked up as the checkpoint.
    std::fs::write(dir.path().join("CHECKPOINT.tmp"), b"{\"format_ver").expect("write temp");

    let (_reopened, recovery) = recover(dir.path());
    assert_eq!(
        recovery.from, good,
        "fell back to something other than the good checkpoint"
    );
    assert_eq!(recovery.entries.len(), 4);
}

#[test]
fn a_corrupt_checkpoint_is_surfaced_rather_than_silently_reset() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    w.append(&entry(0)).expect("append");
    let pos = w.position();
    w.checkpoint(pos, 1).expect("checkpoint");
    drop(w);

    std::fs::write(dir.path().join("CHECKPOINT"), b"not json at all").expect("corrupt it");

    // Replaying from genesis would be *correct* but silent. An operator needs to
    // know the checkpoint was lost, so opening errors instead.
    let err = SegmentWal::open(WalConfig::new(dir.path())).expect_err("must surface");
    assert!(
        err.to_string().contains("checkpoint"),
        "error should name the checkpoint, got: {err}"
    );
}

#[test]
fn segments_roll_at_the_configured_size_and_all_of_them_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = WalConfig {
        segment_bytes: 512,
        ..WalConfig::new(dir.path())
    };

    let mut w = SegmentWal::open(config.clone())
        .expect("open")
        .recover()
        .expect("recover")
        .0;
    for i in 0..60 {
        w.append(&entry(i)).expect("append");
    }
    let final_segment = w.position().segment;
    drop(w);

    assert!(
        final_segment > 0,
        "expected the log to roll past one segment"
    );

    let recovery = SegmentWal::open(config)
        .expect("reopen")
        .recover()
        .expect("recover")
        .1;
    assert_eq!(
        recovery.entries.len(),
        60,
        "entries were lost across a segment boundary"
    );
    assert_eq!(recovery.entries, (0..60).map(entry).collect::<Vec<_>>());
}

#[test]
fn only_segments_fully_behind_the_checkpoint_are_archivable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = WalConfig {
        segment_bytes: 512,
        ..WalConfig::new(dir.path())
    };

    let mut w = SegmentWal::open(config)
        .expect("open")
        .recover()
        .expect("recover")
        .0;
    for i in 0..60 {
        w.append(&entry(i)).expect("append");
    }
    let position = w.position();
    assert!(
        w.archivable_segments().expect("list").is_empty(),
        "nothing checkpointed yet"
    );

    w.checkpoint(position, 60).expect("checkpoint");
    let archivable = w.archivable_segments().expect("list");

    assert_eq!(
        archivable.len() as u64,
        position.segment,
        "all but the live segment"
    );
    for path in &archivable {
        let stem = path.file_stem().and_then(|s| s.to_str()).expect("stem");
        assert!(stem.parse::<u64>().expect("seq") < position.segment);
    }
}

#[test]
fn a_batch_shares_one_fsync_and_still_replays_completely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = WalConfig {
        sync_policy: SyncPolicy::GroupCommit {
            max_batch_window_ms: 5,
        },
        ..WalConfig::new(dir.path())
    };

    let batch: Vec<_> = (0..25).map(entry).collect();
    let mut w = SegmentWal::open(config.clone())
        .expect("open")
        .recover()
        .expect("recover")
        .0;
    w.append_batch(&batch).expect("append batch");
    drop(w);

    let recovery = SegmentWal::open(config)
        .expect("reopen")
        .recover()
        .expect("recover")
        .1;
    assert_eq!(recovery.entries, batch);
}

#[test]
fn an_empty_batch_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut w = wal(dir.path());
    let before = w.position();
    assert_eq!(w.append_batch(&[]).expect("append empty"), before);
    assert_eq!(w.position(), before);
}

#[test]
fn a_fresh_log_recovers_to_an_empty_state_rather_than_erroring() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_w, recovery) = recover(dir.path());
    assert!(recovery.entries.is_empty());
    assert_eq!(recovery.from, LogPosition::START);
}

#[test]
fn bit_rot_inside_an_older_record_stops_replay_at_the_damage() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut w = wal(dir.path());
    for i in 0..10 {
        w.append(&entry(i)).expect("append");
    }
    drop(w);

    // Corrupt a byte in the middle of the segment, not at the tail. Unlike a
    // torn write this is real corruption, and replaying past it would fold
    // damaged state into the view.
    let path = dir.path().join("segments").join("000000000000.seg");
    let mut file = OpenOptions::new().write(true).open(&path).expect("open");
    file.seek(SeekFrom::Start(80)).expect("seek");
    file.write_all(b"\xff\xff\xff\xff").expect("write");
    file.sync_all().expect("sync");

    let (_reopened, recovery) = recover(dir.path());
    assert!(recovery.truncated.is_some(), "corruption was not detected");
    assert!(
        recovery.entries.len() < 10,
        "replay continued past corrupted bytes"
    );
}

#[test]
fn damage_with_later_segments_behind_it_stops_for_an_operator_decision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = WalConfig {
        segment_bytes: 512,
        ..WalConfig::new(dir.path())
    };

    let mut w = SegmentWal::open(config.clone())
        .expect("open")
        .recover()
        .expect("recover")
        .0;
    for i in 0..60 {
        w.append(&entry(i)).expect("append");
    }
    assert!(w.position().segment > 0, "need more than one segment");
    drop(w);

    // Corrupt the *first* segment, which has whole segments after it. Truncating
    // would discard them; replaying past would silently reorder the log.
    let path = dir.path().join("segments").join("000000000000.seg");
    let mut file = OpenOptions::new().write(true).open(&path).expect("open");
    file.seek(SeekFrom::Start(60)).expect("seek");
    file.write_all(b"\xff\xff\xff\xff").expect("write");
    file.sync_all().expect("sync");

    let err = SegmentWal::open(config)
        .expect("open")
        .recover()
        .expect_err("recovery must refuse to choose");
    assert!(
        err.to_string().contains("operator decision required"),
        "got: {err}"
    );
}
