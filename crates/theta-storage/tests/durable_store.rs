//! `DurableLogStore` tests: the log and the materialized view staying in step
//! across restarts.
//!
//! The property that matters here is the one `03-data-model-consistency.md`
//! §2.1 rests on: **the view is always exactly the fold of the log.** Not
//! approximately, not eventually. A restart that produces a different view than
//! a full replay would means some state is not derived from the log, which
//! would make every guarantee downstream unprovable.

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::view::MaterializedView;
use theta_storage::wal::WalConfig;
use theta_storage::{BranchViews, DurableLogStore, LogStore};

/// Views for a single-branch test, which is every test in this file.
fn main_view(views: &BranchViews) -> MaterializedView {
    views.get(&BranchId::MAIN).cloned().unwrap_or_default()
}

fn put(prev: ContentHash, n: u64, key: &str, value: i64) -> LogEntry {
    LogEntry {
        prev_hash: prev,
        commit_id: CommitId(n),
        branch_id: BranchId::MAIN,
        op: OpType::Put {
            key: key.into(),
            value: Value::Int(value),
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

fn config(dir: &std::path::Path) -> WalConfig {
    WalConfig::new(dir)
}

#[test]
fn state_after_a_restart_equals_a_full_replay() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut prev = ContentHash::ZERO;
    for i in 0..50 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{}", i % 7), i as i64), &mut views)
            .expect("append");
    }
    let before = main_view(&views);
    drop(store);

    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(
        reopened.view(BranchId::MAIN),
        before,
        "restart produced a different view than the fold"
    );
    assert_eq!(reopened.store.head(BranchId::MAIN), Some(prev));
}

#[test]
fn a_checkpoint_shortens_replay_without_changing_the_result() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut prev = ContentHash::ZERO;
    for i in 0..30 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    let checkpointed = views.clone();

    for i in 30..40 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    let full = main_view(&views);
    drop(store);

    // Resume from the checkpointed view: only the 10 post-checkpoint entries
    // should replay, and the result must match the uninterrupted state.
    // The position is passed explicitly. It cannot be derived from the views —
    // a forked branch inherits its parent's count — so the caller states it and
    // the store checks it.
    let resumed = DurableLogStore::open_with_views(config(dir.path()), 30, checkpointed)
        .expect("reopen with view");
    assert_eq!(
        resumed.recovery.entries.len(),
        10,
        "replayed more than the tail"
    );
    assert_eq!(main_view(&resumed.views), full);
    assert_eq!(resumed.store.head(BranchId::MAIN), Some(prev));
}

#[test]
fn resuming_from_a_view_at_the_wrong_position_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);
    let mut prev = ContentHash::ZERO;
    for i in 0..10 {
        prev = store
            .append_and_apply(put(prev, i, "k", i as i64), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    drop(store);

    // Views from some other point in history are not a fold of this log, and
    // saying so is what the position argument is for.
    let stale = BranchViews::new();
    let err = DurableLogStore::open_with_views(config(dir.path()), 0, stale)
        .expect_err("must refuse a mismatched view");
    assert!(
        err.to_string().contains("not a fold of this log"),
        "got: {err}"
    );
}

#[test]
fn a_failed_append_leaves_the_view_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let first = store
        .append_and_apply(put(ContentHash::ZERO, 0, "a", 1), &mut views)
        .expect("append");
    let after_first = main_view(&views);

    // A second writer that still believes the head is genesis.
    let stale = put(ContentHash::ZERO, 1, "b", 2);
    assert!(store.append_and_apply(stale, &mut views).is_err());

    assert_eq!(
        main_view(&views),
        after_first,
        "a rejected append still mutated the view"
    );
    assert_eq!(store.head(BranchId::MAIN), Some(first));
}

#[test]
fn automatic_checkpointing_keeps_replay_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (store, mut views) = (opened.store, opened.views);
    let mut store = store.with_checkpoint_interval(10);

    let mut prev = ContentHash::ZERO;
    for i in 0..35 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    let full = main_view(&views);
    drop(store);

    let fresh = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(
        fresh.view(BranchId::MAIN),
        full,
        "restart did not reproduce the folded state"
    );
    assert!(
        fresh.recovery.entries.len() < 35,
        "automatic checkpointing did not shorten replay: {} entries replayed",
        fresh.recovery.entries.len()
    );
}

#[test]
fn history_walks_the_chain_and_stops_at_a_fork_point() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let h1 = store
        .append_and_apply(put(ContentHash::ZERO, 0, "a", 1), &mut views)
        .expect("a");
    let h2 = store
        .append_and_apply(put(h1, 1, "b", 2), &mut views)
        .expect("b");
    store
        .append_and_apply(put(h2, 2, "c", 3), &mut views)
        .expect("c");

    assert_eq!(
        store.history(BranchId::MAIN, None).expect("history").len(),
        3
    );
    assert_eq!(
        store
            .history(BranchId::MAIN, Some(h1))
            .expect("history")
            .len(),
        2
    );
}

#[test]
fn transactions_survive_a_restart_as_one_entry() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let txn = LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(0),
        branch_id: BranchId::MAIN,
        op: OpType::Transaction {
            ops: vec![
                OpType::Put {
                    key: "debit".into(),
                    value: Value::Int(-100),
                },
                OpType::Put {
                    key: "credit".into(),
                    value: Value::Int(100),
                },
            ],
        },
        author: Author::System,
        timestamp_ms: 0,
    };
    store.append_and_apply(txn, &mut views).expect("append txn");
    drop(store);

    // All-or-nothing is structural: the batch is a single record, so a crash
    // either loses the whole transaction or none of it.
    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(
        reopened.recovery.entries.len(),
        1,
        "a transaction is one log record"
    );
    assert_eq!(
        main_view(&reopened.views).get("debit"),
        Some(&Value::Int(-100))
    );
    assert_eq!(
        main_view(&reopened.views).get("credit"),
        Some(&Value::Int(100))
    );
}

#[test]
fn a_lost_view_snapshot_costs_replay_time_not_correctness() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut prev = ContentHash::ZERO;
    for i in 0..20 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    let expected = main_view(&views);
    drop(store);

    // The snapshot is a cache over the log, not a second source of truth.
    std::fs::remove_file(dir.path().join("VIEW")).expect("remove snapshot");

    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(
        reopened.view(BranchId::MAIN),
        expected,
        "full replay produced a different view"
    );
    assert_eq!(
        reopened.recovery.entries.len(),
        20,
        "expected a genesis replay"
    );
}

#[test]
fn a_corrupt_view_snapshot_is_discarded_rather_than_trusted() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut prev = ContentHash::ZERO;
    for i in 0..12 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    let expected = main_view(&views);
    drop(store);

    std::fs::write(dir.path().join("VIEW"), b"{ not a view").expect("corrupt");

    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(main_view(&reopened.views), expected);
}

#[test]
fn a_snapshot_that_disagrees_with_the_checkpoint_is_not_used() {
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut prev = ContentHash::ZERO;
    for i in 0..15 {
        prev = store
            .append_and_apply(put(prev, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    let expected = main_view(&views);
    drop(store);

    // This is what a crash between writing the snapshot and writing the
    // checkpoint leaves behind: a snapshot from a different position.
    let mut ahead = expected.clone();
    ahead.applied += 5;
    std::fs::write(
        dir.path().join("VIEW"),
        serde_json::to_vec(&ahead).expect("encode"),
    )
    .expect("write");

    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(
        reopened.view(BranchId::MAIN),
        expected,
        "an inconsistent snapshot was trusted"
    );
}

#[test]
fn a_new_branch_inherits_its_parents_state_and_then_diverges() {
    const FEATURE: BranchId = BranchId(1);

    let dir = tempfile::tempdir().expect("tempdir");
    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let mut main_head = ContentHash::ZERO;
    for i in 0..3 {
        let e = put(main_head, i, &format!("k{i}"), i as i64);
        main_head = store.append_and_apply(e, &mut views).expect("append");
    }

    // Fork: a BranchCreate whose `from` is main's head.
    let fork = LogEntry {
        prev_hash: main_head,
        commit_id: CommitId(0),
        branch_id: FEATURE,
        op: OpType::BranchCreate {
            name: "feature".into(),
            from: main_head,
        },
        author: Author::System,
        timestamp_ms: 100,
    };
    store.set_head(FEATURE, main_head);
    let feature_head = store.append_and_apply(fork, &mut views).expect("fork");

    // The new branch starts from what main had, not from nothing.
    assert_eq!(views[&FEATURE].get("k0"), Some(&Value::Int(0)));
    assert_eq!(views[&FEATURE].get("k2"), Some(&Value::Int(2)));

    // Then the branches diverge without leaking into each other.
    store
        .append_and_apply(
            LogEntry {
                branch_id: FEATURE,
                ..put(feature_head, 9, "only-on-feature", 9)
            },
            &mut views,
        )
        .expect("append");
    store
        .append_and_apply(put(main_head, 4, "only-on-main", 4), &mut views)
        .expect("append");

    assert!(views[&FEATURE].get("only-on-main").is_none());
    assert!(views[&BranchId::MAIN].get("only-on-feature").is_none());
}

#[test]
fn per_branch_views_survive_a_restart() {
    const FEATURE: BranchId = BranchId(1);

    let dir = tempfile::tempdir().expect("tempdir");
    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store, opened.views);

    let main_head = store
        .append_and_apply(put(ContentHash::ZERO, 0, "shared", 1), &mut views)
        .expect("append");

    store.set_head(FEATURE, main_head);
    let feature_head = store
        .append_and_apply(
            LogEntry {
                prev_hash: main_head,
                commit_id: CommitId(0),
                branch_id: FEATURE,
                op: OpType::BranchCreate {
                    name: "feature".into(),
                    from: main_head,
                },
                author: Author::System,
                timestamp_ms: 10,
            },
            &mut views,
        )
        .expect("fork");
    store
        .append_and_apply(
            LogEntry {
                branch_id: FEATURE,
                ..put(feature_head, 1, "feature-only", 7)
            },
            &mut views,
        )
        .expect("append");

    let before = views.clone();
    drop(store);

    // A fold over the whole log would mix the branches together; per-branch
    // views must come back exactly as they were.
    let reopened = DurableLogStore::open(config(dir.path())).expect("reopen");
    assert_eq!(reopened.views, before);
    assert_eq!(reopened.view(FEATURE).get("shared"), Some(&Value::Int(1)));
    assert!(reopened.view(BranchId::MAIN).get("feature-only").is_none());
}

/// A forked branch inherits its parent's `applied` count, and summing across
/// branches then double-counts every entry the parent had already folded.
///
/// Found by `bench/comparative/branch_compare.py`, which forks fifty deep
/// against a twenty-thousand-row parent and tripped the `debug_assert` in
/// `checkpoint` — 32,031 against 2,021 on a run with five branches.
///
/// It is not only a debug assertion. `open_with_views` compares the same sum
/// against the checkpoint and returns `ViewDrift` in release, so **a store that
/// branches, checkpoints and restarts refuses to open**. The data is intact and
/// a full replay would rebuild it, but the fast path rejects a snapshot that is
/// perfectly correct.
///
/// The count cannot be re-derived by summing, because two branches legitimately
/// share history: main having folded N entries and a fork of main having folded
/// the same N is not 2N entries in the log. The number belongs to the snapshot,
/// not to the views inside it.
#[test]
fn a_branched_store_reopens_from_its_checkpoint() {
    const FEATURE: BranchId = BranchId(1);

    let dir = tempfile::tempdir().expect("tempdir");
    let opened = DurableLogStore::open(config(dir.path())).expect("open");
    let (mut store, mut views) = (opened.store.with_checkpoint_interval(10), opened.views);

    let mut head = ContentHash::ZERO;
    for i in 0..20 {
        head = store
            .append_and_apply(put(head, i, &format!("k{i}"), i as i64), &mut views)
            .expect("append");
    }

    // Fork, which copies the parent's `applied` into the child's view.
    store.set_head(FEATURE, head);
    let fork = LogEntry {
        prev_hash: head,
        commit_id: CommitId(100),
        branch_id: FEATURE,
        op: OpType::BranchCreate {
            name: "feature".into(),
            from: head,
        },
        author: Author::System,
        timestamp_ms: 100,
    };
    let mut feature_head = store.append_and_apply(fork, &mut views).expect("fork");

    // Enough writes after the fork to cross a checkpoint boundary.
    for i in 0..20 {
        feature_head = store
            .append_and_apply(
                LogEntry {
                    branch_id: FEATURE,
                    ..put(feature_head, 200 + i, &format!("f{i}"), i as i64)
                },
                &mut views,
            )
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    drop(store);

    // The store must come back. Before the fix this returned `ViewDrift`,
    // because the snapshot's per-branch counts summed to more than the log had
    // applied — a correct snapshot rejected by an incorrect check.
    let reopened = DurableLogStore::open(config(dir.path())).expect("a branched store reopens");
    assert_eq!(
        reopened.views[&BranchId::MAIN].get("k19"),
        Some(&Value::Int(19)),
        "main lost a row across the restart"
    );
    assert_eq!(
        reopened.views[&FEATURE].get("f19"),
        Some(&Value::Int(19)),
        "the branch lost a row across the restart"
    );
    assert_eq!(
        reopened.views[&FEATURE].get("k19"),
        Some(&Value::Int(19)),
        "the branch lost the history it inherited"
    );
}
