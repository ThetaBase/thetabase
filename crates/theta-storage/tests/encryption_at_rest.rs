//! SEC-2: encryption at rest, verified against the bytes on disk.
//!
//! `specs/04` §3 claimed encryption at rest before any existed, which is the
//! failure this suite is built to prevent recurring. So these tests do not
//! assert that a cipher was *called* — they open the files ThetaBase wrote and
//! search them for the rows. A test that trusted the code it was testing would
//! have passed against the specification that was wrong.

use std::fs;
use std::path::Path;

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_storage::durable::{BranchViews, DurableLogStore};
use theta_storage::log::LogStore;
use theta_storage::wal::WalConfig;
use theta_storage::DataKey;

const MAIN: BranchId = BranchId::MAIN;

/// Distinctive enough that finding it anywhere under the data directory is
/// unambiguous evidence it was stored in the clear.
const SECRET: &str = "patient-4417-diagnosis-confidential";

/// The first segment's path. Named explicitly because two tests assert the file
/// is byte-identical after a failed open.
fn first_segment(dir: &Path) -> std::path::PathBuf {
    dir.join("segments").join(format!("{:012}.seg", 0))
}

fn key() -> DataKey {
    DataKey::from_hex(&"3f".repeat(32)).expect("a valid key")
}

fn entry(n: u64, prev: ContentHash) -> LogEntry {
    LogEntry {
        prev_hash: prev,
        commit_id: CommitId(n),
        branch_id: MAIN,
        op: OpType::Put {
            key: format!("patients:{n}"),
            value: Value::Text(SECRET.to_string()),
        },
        author: Author::System,
        timestamp_ms: n as i64,
    }
}

/// Append `count` rows starting at `from`, then checkpoint so a view snapshot
/// exists on disk as well as segments.
fn write_rows(config: WalConfig, from: u64, count: u64) {
    let opened = DurableLogStore::open(config).expect("open");
    let mut store = opened.store;
    let mut views: BranchViews = opened.views;

    let mut head = store.head(MAIN).unwrap_or(ContentHash::ZERO);
    for n in from..from + count {
        head = store
            .append_and_apply(entry(n, head), &mut views)
            .expect("append");
    }
    store.checkpoint(&views).expect("checkpoint");
}

/// How many of the rows this suite writes are visible on `main`.
fn row_count(views: &BranchViews) -> usize {
    let view = views.get(&MAIN).expect("main view");
    (0..u64::MAX)
        .take_while(|n| view.get(&format!("patients:{n}")).is_some())
        .count()
}

/// Every byte ThetaBase wrote under `dir`, concatenated.
///
/// Deliberately every file rather than just the segments: encrypting the log
/// and leaving the view snapshot in the clear would satisfy a narrower test
/// while leaving the plaintext sitting on the same volume.
fn all_bytes_on_disk(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(&path).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.extend_from_slice(&fs::read(&path).expect("read file"));
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_plaintext_store_leaves_rows_readable_on_disk() {
    // The control. Without it the search below proves nothing — it could be
    // finding no plaintext because it cannot find plaintext at all.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()), 0, 8);

    assert!(
        contains(&all_bytes_on_disk(dir.path()), SECRET),
        "the control failed: an unencrypted store did not leave the row on disk, \
         so this suite cannot detect plaintext at all"
    );
}

#[test]
fn an_encrypted_store_leaves_no_row_readable_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 8);

    let bytes = all_bytes_on_disk(dir.path());
    assert!(
        !contains(&bytes, SECRET),
        "a row value was readable on disk; encryption at rest is not in effect"
    );
    // Keys too: which patients exist is not less sensitive than their records.
    assert!(
        !contains(&bytes, "patients:3"),
        "a row key was readable on disk"
    );
}

#[test]
fn an_encrypted_store_reopens_and_returns_every_row() {
    // Encryption that loses the data is not a security control, it is an outage.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 8);

    let opened = DurableLogStore::open(WalConfig::new(dir.path()).with_data_key(key()))
        .expect("reopen with the right key");
    assert_eq!(row_count(&opened.views), 8, "rows did not survive sealing");
    assert_eq!(
        opened.view(MAIN).get("patients:3"),
        Some(&Value::Text(SECRET.to_string())),
        "a row came back altered"
    );
}

#[test]
fn reopening_without_the_key_refuses_rather_than_truncating_the_log() {
    // The dangerous failure mode. Recovery answers a torn record by truncating
    // the log, so if an undecryptable record were reported as corruption,
    // starting the server without the key would silently delete the database.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 8);
    let before = fs::read(first_segment(dir.path())).expect("segment");

    let err = DurableLogStore::open(WalConfig::new(dir.path()))
        .expect_err("opening an encrypted store with no key must fail");
    let message = err.to_string();
    assert!(
        message.contains("encrypted") && message.contains("THETA_DATA_KEY"),
        "the error must name the problem and the variable that fixes it; got: {message}"
    );

    let after = fs::read(first_segment(dir.path())).expect("segment");
    assert_eq!(before, after, "the failed open truncated the log");
}

#[test]
fn reopening_with_the_wrong_key_refuses_rather_than_truncating_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 8);
    let before = fs::read(first_segment(dir.path())).expect("segment");

    let wrong = DataKey::from_hex(&"a1".repeat(32)).expect("a valid key");
    let err = DurableLogStore::open(WalConfig::new(dir.path()).with_data_key(wrong))
        .expect_err("the wrong key must fail");
    assert!(
        err.to_string().contains("wrong data key"),
        "a wrong key must be reported as a wrong key, not as corruption; got: {err}"
    );

    let after = fs::read(first_segment(dir.path())).expect("segment");
    assert_eq!(before, after, "the failed open truncated the log");
}

#[test]
fn encryption_can_be_enabled_on_a_store_that_already_holds_plaintext() {
    // The migration story. If turning encryption on meant abandoning the log,
    // nobody holding data would ever turn it on, and `specs/04` §3 would stay
    // aspirational for exactly the databases that matter.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()), 0, 4);
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 4, 4);

    // Delete the snapshot before reopening, so the log has to prove itself.
    //
    // This is not incidental. The first version of this test passed while the
    // implementation was writing sealed records into a segment whose header
    // said plaintext — a state that destroys the log on the next real recovery.
    // It passed because the snapshot matched the checkpoint, so those records
    // were never replayed. Removing the cache is what turns this into a test of
    // the log rather than a test of the cache.
    fs::remove_file(dir.path().join("VIEW")).expect("remove the snapshot");

    let reopened = DurableLogStore::open(WalConfig::new(dir.path()).with_data_key(key()))
        .expect("reopen a log holding both kinds of segment");
    assert_eq!(
        row_count(&reopened.views),
        8,
        "a log holding both plaintext and sealed records must replay in full"
    );
    assert!(
        reopened.recovery.truncated.is_none(),
        "enabling encryption truncated the log: {:?}",
        reopened.recovery.truncated
    );
}

#[test]
fn enabling_encryption_does_not_seal_records_into_a_plaintext_segment() {
    // The specific failure the test above was blind to, asserted directly: the
    // segment that was written before encryption was enabled must still contain
    // only plaintext records, and the sealed ones must have gone somewhere else.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()), 0, 4);
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 4, 4);

    let segments: Vec<_> = fs::read_dir(dir.path().join("segments"))
        .expect("segments dir")
        .map(|e| e.expect("entry").path())
        .collect();
    assert!(
        segments.len() > 1,
        "enabling encryption must roll to a new segment, but only one exists"
    );

    let first = fs::read(first_segment(dir.path())).expect("the original segment");
    assert!(
        contains(&first, "patients:1"),
        "the pre-encryption segment lost its plaintext records"
    );
    assert!(
        !contains(&first, "patients:5"),
        "a sealed record was appended to a segment whose header says plaintext"
    );
}

#[test]
fn removing_the_key_refuses_rather_than_starting_a_fresh_plaintext_log() {
    // The other direction, and the likelier mistake in practice: a deployment
    // whose key configuration goes missing and that restarts anyway.
    //
    // There is no rolling to be done here, because there is no opening to be
    // done: the sealed segments are still in the log and cannot be read, so the
    // store refuses. That refusal is the whole point. Silently beginning a new
    // plaintext log would leave the process healthy, serving an empty database,
    // while the real one sat unreadable on the same disk — and the first thing
    // that would notice is a checkpoint overwriting the snapshot.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 4);

    assert!(
        segment_is_encrypted(&first_segment(dir.path())),
        "the segment should declare itself sealed"
    );
    let err = DurableLogStore::open(WalConfig::new(dir.path()))
        .expect_err("a store with sealed segments must not open without the key");
    assert!(
        err.to_string().contains("THETA_DATA_KEY"),
        "the refusal must name the variable that fixes it; got: {err}"
    );
}

/// Read a segment header's encryption flag straight from the file.
fn segment_is_encrypted(path: &Path) -> bool {
    let header = fs::read(path).expect("segment");
    // Bytes 20..24 are the flags; bit 0 means sealed.
    u32::from_le_bytes(header[20..24].try_into().expect("4 bytes")) & 1 != 0
}

#[test]
fn one_projects_key_cannot_open_another_projects_store() {
    // `specs/04` §3: no key shared across projects. This is the property that
    // makes per-project keys worth their cost — a compromise stops at one
    // tenant instead of reaching every tenant on the host.
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let key_a = DataKey::from_hex(&"11".repeat(32)).expect("key");
    let key_b = DataKey::from_hex(&"22".repeat(32)).expect("key");

    write_rows(WalConfig::new(a.path()).with_data_key(key_a.clone()), 0, 4);
    write_rows(WalConfig::new(b.path()).with_data_key(key_b), 0, 4);

    assert!(
        DurableLogStore::open(WalConfig::new(b.path()).with_data_key(key_a)).is_err(),
        "one project's key opened another project's store"
    );
}

#[test]
fn the_view_snapshot_is_sealed_and_not_only_the_log() {
    // Called out separately because it is the easy thing to miss: the snapshot
    // holds every row of every branch, so a plaintext snapshot beside a sealed
    // log leaves the whole database in the clear in a different file.
    let dir = tempfile::tempdir().expect("tempdir");
    write_rows(WalConfig::new(dir.path()).with_data_key(key()), 0, 8);

    let snapshot = fs::read(dir.path().join("VIEW")).expect("a snapshot must exist");
    assert!(
        !contains(&snapshot, SECRET),
        "the view snapshot holds rows in the clear"
    );
    assert!(
        snapshot.starts_with(b"THETASNAPSEALED1"),
        "the snapshot is not marked as sealed, so a reader cannot tell which it is"
    );
}
