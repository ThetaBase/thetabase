//! SEC-8: the log is hash-chained, so tampering with history is detectable.
//!
//! Recorded in `docs/SECURITY-REVIEW.md` as a property worth defending rather
//! than rediscovering. This suite is the audit of that claim: it edits records
//! inside a segment file, fixes up the checksum the way an attacker with write
//! access would, and asserts that recovery notices.
//!
//! The checksum alone proves nothing here. It exists to catch a torn write, and
//! it is a CRC — recomputing it over altered bytes is arithmetic, not an
//! attack. Only the chain distinguishes "these bytes changed" from "these bytes
//! were always this way".

use std::fs;
use std::path::{Path, PathBuf};

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::durable::{BranchViews, DurableLogStore};
use theta_storage::wal::WalConfig;

const MAIN: BranchId = BranchId::MAIN;

fn segment_path(dir: &Path) -> PathBuf {
    dir.join("segments").join(format!("{:012}.seg", 0))
}

fn entry(n: u64, prev: ContentHash, value: &str) -> LogEntry {
    LogEntry {
        prev_hash: prev,
        commit_id: CommitId(n),
        branch_id: MAIN,
        op: OpType::Put {
            key: format!("accounts:{n}"),
            value: Value::Text(value.to_string()),
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

/// Write `count` rows and return the directory holding them.
///
/// No checkpoint, deliberately: a checkpoint would let recovery resume from the
/// view snapshot and skip the records this suite is about to edit, and a test
/// that never replays the tampered bytes is a test of the cache.
fn log_with(dir: &Path, count: u64) {
    let opened = DurableLogStore::open(WalConfig::new(dir)).expect("open");
    let mut store = opened.store;
    let mut views: BranchViews = opened.views;
    let mut head = ContentHash::ZERO;
    for n in 0..count {
        head = store
            .append_and_apply(entry(n, head, "1000.00"), &mut views)
            .expect("append");
    }
}

/// Replace `find` with `replace` inside the segment, repairing the record's
/// checksum so the edit is indistinguishable from an honest write at that
/// level. Both must be the same length, so the framing does not shift.
///
/// This is the attacker's job, done faithfully. An edit that left a bad
/// checksum would be caught by machinery that has nothing to do with the
/// property under test, and would make this suite pass for the wrong reason.
fn tamper(path: &Path, find: &str, replace: &str) {
    assert_eq!(
        find.len(),
        replace.len(),
        "the edit must not move the frames"
    );
    let mut bytes = fs::read(path).expect("segment");

    let at = bytes
        .windows(find.len())
        .position(|w| w == find.as_bytes())
        .unwrap_or_else(|| panic!("`{find}` is not in the segment"));
    bytes[at..at + find.len()].copy_from_slice(replace.as_bytes());

    // Walk the records to find the one containing `at`, and rewrite its CRC.
    const HEADER_LEN: usize = 32;
    const PREFIX: usize = 8;
    let mut offset = HEADER_LEN;
    while offset + PREFIX <= bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4")) as usize;
        let body = offset + PREFIX;
        if body + len > bytes.len() {
            break;
        }
        if (body..body + len).contains(&at) {
            let crc = crc32fast::hash(&bytes[body..body + len]);
            bytes[offset + 4..offset + PREFIX].copy_from_slice(&crc.to_le_bytes());
            fs::write(path, &bytes).expect("write back");
            return;
        }
        offset = body + len;
    }
    panic!("no record covers the edited bytes");
}

#[test]
fn the_tampering_this_suite_does_is_invisible_to_the_checksum() {
    // The control. Every assertion below is only meaningful if the edit really
    // does survive the layer that is *not* under test — otherwise this suite
    // would be reporting the CRC's success as the chain's.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 6);
    tamper(&segment_path(dir.path()), "1000.00", "9999.99");

    let opened = DurableLogStore::open(WalConfig::new(dir.path()));
    if let Ok(opened) = &opened {
        assert!(
            opened.recovery.truncated.is_none(),
            "the edit was caught as corruption, so it is not testing the chain"
        );
    }
}

#[test]
fn an_altered_record_is_detected_when_the_log_replays() {
    // The claim in SECURITY-REVIEW.md, asserted rather than assumed.
    //
    // An altered record hashes differently, so the entry that came after it
    // now points at a hash the log does not contain. That break is what makes
    // the chain worth having: without checking it, `prev_hash` is decoration.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 6);
    tamper(&segment_path(dir.path()), "1000.00", "9999.99");

    let err = DurableLogStore::open(WalConfig::new(dir.path()))
        .expect_err("a tampered log must not replay as though it were intact");
    let message = err.to_string();
    assert!(
        message.contains("chain") || message.contains("prev_hash"),
        "the error must say the history does not link up; got: {message}"
    );
}

#[test]
fn an_altered_row_key_is_detected_too() {
    // Not only values. Moving a write from one row to another is the edit with
    // the clearest motive, and it changes the entry's hash exactly the same way.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 6);
    tamper(&segment_path(dir.path()), "accounts:2", "accounts:5");

    assert!(
        DurableLogStore::open(WalConfig::new(dir.path())).is_err(),
        "a redirected write replayed as though it had always been there"
    );
}

#[test]
fn an_untampered_log_still_replays() {
    // The check has to be worth having, which means it must not refuse honest
    // logs. A tamper-detector that fires on ordinary recovery would be turned
    // off within a week.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 24);

    let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("an honest log opens");
    assert_eq!(opened.recovery.entries.len(), 24);
    assert_eq!(
        opened.view(MAIN).get("accounts:5"),
        Some(&Value::Text("1000.00".to_string()))
    );
}

#[test]
fn a_branching_log_still_replays() {
    // Branches fork, so an entry's `prev_hash` is not always the previous
    // entry in file order — it is the fork point. A check that assumed a single
    // chain would refuse every branched log, which is every real one.
    const FEATURE: BranchId = BranchId(7);

    let dir = tempfile::tempdir().expect("tempdir");
    let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
    let mut store = opened.store;
    let mut views: BranchViews = opened.views;

    let mut head = ContentHash::ZERO;
    for n in 0..4 {
        head = store
            .append_and_apply(entry(n, head, "1000.00"), &mut views)
            .expect("append");
    }

    // Fork: the feature branch's first entry points at main's head.
    let fork_point = head;
    store.set_head(FEATURE, fork_point);
    let mut feature_head = fork_point;
    for n in 4..7 {
        let mut e = entry(n, feature_head, "2000.00");
        e.branch_id = FEATURE;
        feature_head = store.append_and_apply(e, &mut views).expect("append");
    }
    // And main keeps moving after the fork.
    for n in 7..10 {
        head = store
            .append_and_apply(entry(n, head, "1000.00"), &mut views)
            .expect("append");
    }
    drop(store);

    let reopened =
        DurableLogStore::open(WalConfig::new(dir.path())).expect("a branched log must replay");
    assert_eq!(reopened.recovery.entries.len(), 10);
}

#[test]
fn verify_chain_reads_the_whole_log_and_says_how_much_it_checked() {
    // `open` resumes from a checkpoint and so can only speak for what it
    // replayed. This is the full pass, and it reports a count so that "it
    // passed" is distinguishable from "it found nothing to check" — a
    // verifier that silently verifies zero entries is worse than none.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 12);

    let checked = DurableLogStore::verify_chain(WalConfig::new(dir.path())).expect("intact");
    assert_eq!(checked, 12, "the full pass did not read every entry");
}

#[test]
fn verify_chain_finds_an_edit_that_a_checkpointed_open_would_skip() {
    // The reason `verify_chain` exists at all. A checkpoint lets recovery start
    // mid-log, so an edit below that point is never replayed and never checked.
    // Routine startup cannot catch it; a deliberate audit must.
    let dir = tempfile::tempdir().expect("tempdir");

    let opened = DurableLogStore::open(WalConfig::new(dir.path())).expect("open");
    let mut store = opened.store.with_checkpoint_interval(1);
    let mut views: BranchViews = opened.views;
    let mut head = ContentHash::ZERO;
    for n in 0..8 {
        head = store
            .append_and_apply(entry(n, head, "1000.00"), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
    drop(store);

    tamper(&segment_path(dir.path()), "1000.00", "9999.99");

    // Startup resumes from the snapshot, so it neither replays nor notices.
    // Asserted rather than glossed over: this is the limit of the control, and
    // an unstated limit is how SEC-2 happened.
    assert!(
        DurableLogStore::open(WalConfig::new(dir.path())).is_ok(),
        "this test is built on startup skipping the edit; if that changed, \
         the assertion below no longer demonstrates anything"
    );

    assert!(
        DurableLogStore::verify_chain(WalConfig::new(dir.path())).is_err(),
        "the full audit missed an edit below the checkpoint"
    );
}

#[test]
fn editing_the_last_entry_is_not_detectable_from_inside_the_log() {
    // The honest boundary of a hash chain, pinned so nobody has to discover it
    // by trusting the claim too far. Nothing follows the last entry, so nothing
    // points at its hash, so changing it breaks no link.
    //
    // Closing this needs an anchor outside the log — a head signed and
    // published where whoever holds the disk cannot reach it. That is v2 work,
    // and this test is here so the gap stays visible until then rather than
    // being quietly assumed away.
    let dir = tempfile::tempdir().expect("tempdir");
    log_with(dir.path(), 4);
    tamper(&segment_path(dir.path()), "accounts:3", "accounts:9");

    assert!(
        DurableLogStore::verify_chain(WalConfig::new(dir.path())).is_ok(),
        "if this now fails, the chain gained an external anchor and this test \
         should be replaced by one asserting the stronger property"
    );
}
