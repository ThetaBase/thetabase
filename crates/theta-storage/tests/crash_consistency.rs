//! Fault injection: crash the process at an arbitrary byte and check what
//! survives.
//!
//! This is the M1 gate's adversarial half (`docs/specs/08-test-validation-plan.md`
//! §2). The hand-written crash tests in `durability.rs` cover the failure points
//! we thought of; this covers every byte offset, which is where the ones we did
//! not think of live.
//!
//! One property covers most of what durability means:
//!
//! > **Recovery yields a prefix of what was written.** Never a suffix, never a
//! > gap, never a record that was only half-written, and never a reordering.
//!
//! A database that loses the tail after a crash is doing its job. One that loses
//! the middle, or resurrects a partial write, is corrupting data.

use std::fs::OpenOptions;

use proptest::prelude::*;
use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::view::MaterializedView;
use theta_storage::wal::{SegmentWal, Wal, WalConfig};
use theta_storage::DurableLogStore;

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

fn segment_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("segments").join("000000000000.seg")
}

/// Kill the process mid-write: keep the first `keep_bytes` of the segment.
fn crash_at(dir: &std::path::Path, keep_bytes: u64) {
    let path = segment_path(dir);
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open segment");
    let len = file.metadata().expect("meta").len();
    file.set_len(keep_bytes.min(len)).expect("truncate");
    file.sync_all().expect("sync");
}

proptest! {
    // Persistence is off: proptest cannot find a source root from an
    // integration test to write its regression file into.
    #![proptest_config(ProptestConfig {
        cases: 400,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Crash at any byte; whatever comes back is a prefix of what went in.
    #[test]
    fn recovery_always_yields_a_prefix_of_what_was_written(
        count in 1usize..25,
        crash_fraction in 0.0f64..1.0,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let written: Vec<LogEntry> = (0..count as u64).map(entry).collect();

        let mut w = SegmentWal::open(WalConfig::new(dir.path()))
            .expect("open")
            .recover()
            .expect("recover")
            .0;
        for e in &written {
            w.append(e).expect("append");
        }
        drop(w);

        let full_len = std::fs::metadata(segment_path(dir.path())).expect("meta").len();
        crash_at(dir.path(), (full_len as f64 * crash_fraction) as u64);

        let recovered = SegmentWal::open(WalConfig::new(dir.path()))
            .expect("reopen")
            .recover()
            .expect("recover")
            .1
            .entries;

        prop_assert!(
            recovered.len() <= written.len(),
            "recovery invented entries that were never written"
        );
        prop_assert_eq!(
            &recovered[..],
            &written[..recovered.len()],
            "recovered entries are not a prefix of what was written"
        );
    }

    /// After any crash, the restored view is exactly the fold of the entries
    /// that survived. There is no state that is not derived from the log
    /// (`03-data-model-consistency.md` §2.1).
    #[test]
    fn the_restored_view_is_always_the_fold_of_the_surviving_log(
        count in 1usize..20,
        crash_fraction in 0.0f64..1.0,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");

        let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
        let (mut store, mut views) = (opened.store, opened.views);
        let mut prev = ContentHash::ZERO;
        for i in 0..count as u64 {
            let e = LogEntry { prev_hash: prev, ..entry(i) };
            prev = store.append_and_apply(e, &mut views).expect("append");
        }
        drop(store);

        let full_len = std::fs::metadata(segment_path(dir.path())).expect("meta").len();
        crash_at(dir.path(), (full_len as f64 * crash_fraction) as u64);

        let reopened = DurableLogStore::open(WalConfig::new(dir.path())).expect("reopen");
        // Every entry in these tests is on `main`, so the whole-log fold and
        // main's per-branch view must agree exactly.
        let expected = MaterializedView::replay(&reopened.recovery.entries);
        prop_assert_eq!(reopened.view(BranchId::MAIN), expected);
    }

    /// Re-running recovery over an already-recovered log changes nothing.
    /// Truncation only ever removes bytes that were already unreadable, so it
    /// must not be able to eat into valid records on a second pass.
    #[test]
    fn repeated_recovery_after_a_crash_is_stable(
        count in 1usize..20,
        crash_fraction in 0.0f64..1.0,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut w = SegmentWal::open(WalConfig::new(dir.path()))
            .expect("open")
            .recover()
            .expect("recover")
            .0;
        for i in 0..count as u64 {
            w.append(&entry(i)).expect("append");
        }
        drop(w);

        let full_len = std::fs::metadata(segment_path(dir.path())).expect("meta").len();
        crash_at(dir.path(), (full_len as f64 * crash_fraction) as u64);

        let first = SegmentWal::open(WalConfig::new(dir.path()))
            .expect("open")
            .recover()
            .expect("recover")
            .1
            .entries;
        let second = SegmentWal::open(WalConfig::new(dir.path()))
            .expect("open")
            .recover()
            .expect("recover")
            .1
            .entries;

        prop_assert_eq!(first, second, "a second recovery pass changed the log");
    }

    /// Read-your-writes: a session that wrote a value reads that value back,
    /// for every key, at every point in the sequence
    /// (`03-data-model-consistency.md` §3.1).
    #[test]
    fn a_session_always_reads_its_own_writes(
        writes in prop::collection::vec((0u8..8, -500i64..500), 1..40),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
        let (mut store, mut views) = (opened.store, opened.views);

        let mut prev = ContentHash::ZERO;
        for (i, (key, value)) in writes.iter().enumerate() {
            let key = format!("k{key}");
            let e = LogEntry {
                prev_hash: prev,
                commit_id: CommitId(i as u64),
                branch_id: BranchId::MAIN,
                op: OpType::Put { key: key.clone(), value: Value::Int(*value) },
                author: Author::agent("s", "u"),
                timestamp_ms: i as i64,
            };
            prev = store.append_and_apply(e, &mut views).expect("append");

            // The write is acknowledged, so it must be visible to this session
            // immediately — not eventually.
            prop_assert_eq!(
                views[&BranchId::MAIN].get(&key),
                Some(&Value::Int(*value)),
                "session did not read its own write of {}",
                key
            );
        }
    }

    /// Read-your-writes survives a restart: everything acknowledged before the
    /// crash is still readable after it.
    #[test]
    fn acknowledged_writes_are_still_readable_after_a_restart(
        writes in prop::collection::vec((0u8..6, -200i64..200), 1..25),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
        let (mut store, mut views) = (opened.store, opened.views);

        let mut prev = ContentHash::ZERO;
        for (i, (key, value)) in writes.iter().enumerate() {
            let e = LogEntry {
                prev_hash: prev,
                commit_id: CommitId(i as u64),
                branch_id: BranchId::MAIN,
                op: OpType::Put { key: format!("k{key}"), value: Value::Int(*value) },
                author: Author::System,
                timestamp_ms: i as i64,
            };
            prev = store.append_and_apply(e, &mut views).expect("append");
        }
        let before = views[&BranchId::MAIN].clone();
        drop(store); // crash with no clean shutdown

        let reopened = DurableLogStore::open(WalConfig::new(dir.path())).expect("reopen");
        prop_assert_eq!(reopened.view(BranchId::MAIN), before, "an acknowledged write was lost");
    }
}
