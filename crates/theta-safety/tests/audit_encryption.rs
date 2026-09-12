//! The audit log, sealed at rest.
//!
//! `specs/04` §3 named this as the gap left open by SEC-2: segments and the
//! view snapshot are sealed, and `audit.jsonl` sat beside them in clear,
//! holding change summaries, table and column names, risk classifications and
//! who proposed what. A stolen volume gave up the shape of the schema and the
//! history of every gated decision even though it gave up no rows.
//!
//! The property that matters most here is not that entries are unreadable
//! without the key -- that is the easy half. It is that **a wrong key is an
//! error rather than an empty log**. The reader stops at the first unparseable
//! line, because a crash mid-append leaves half a line and losing the last
//! event beats refusing to open the file. If a sealed line took that path, a
//! server started with the wrong key would present a complete-looking audit
//! trail with nothing in it, which is worse than no audit trail at all.

use std::path::Path;

use theta_core::Author;
use theta_core::DataKey;
use theta_safety::audit::{AuditEntry, RiskLevel};
use theta_safety::store::AuditStore;

fn key() -> DataKey {
    let (_, key) = DataKey::generate();
    key
}

fn entry(id: &str) -> AuditEntry {
    AuditEntry {
        risk: RiskLevel::High,
        summary: format!("{id}: drop column on customers.legacy_ref"),
        author: Author::System,
        timestamp_ms: 1_700_000_000_000,
        detail: serde_json::json!({ "table": "customers", "column": "legacy_ref" }),
    }
}

fn write_three(dir: &Path, key: Option<DataKey>) {
    let mut store = AuditStore::open_with_key(dir, "proj", key).expect("opens");
    for id in ["chg_1", "chg_2", "chg_3"] {
        store.append(entry(id)).expect("appends");
    }
}

#[test]
fn sealed_entries_are_not_readable_in_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_three(dir.path(), Some(key()));

    let raw = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("readable file");

    // The header stays clear: it carries the format version and the project id,
    // which is also the name of the directory the file sits in. Sealing it
    // would mean needing the key to find out whether you have the right key.
    assert!(raw.contains("\"projectId\":\"proj\"") || raw.contains("\"project_id\":\"proj\""));

    for leaked in ["legacy_ref", "customers", "chg_1", "high"] {
        assert!(
            !raw.contains(leaked),
            "{leaked:?} is in the file in clear, so sealing achieved nothing"
        );
    }
}

#[test]
fn what_was_sealed_reads_back_under_the_same_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (bytes, _) = DataKey::generate();
    write_three(dir.path(), Some(DataKey::from_bytes(&bytes)));

    let store = AuditStore::open_with_key(dir.path(), "proj", Some(DataKey::from_bytes(&bytes)))
        .expect("reopens under the same key");

    assert_eq!(store.len(), 3, "entries went missing across a reopen");
    let summaries: Vec<_> = store.recent().map(|e| e.summary.clone()).collect();
    assert!(
        summaries.iter().any(|s| s.starts_with("chg_3")),
        "the last entry is missing: {summaries:?}"
    );
}

/// The one that matters.
#[test]
fn the_wrong_key_is_an_error_and_never_an_empty_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_three(dir.path(), Some(key()));

    let result = AuditStore::open_with_key(dir.path(), "proj", Some(key()));

    match result {
        Err(e) => {
            let said = e.to_string();
            assert!(
                said.contains("will not open") || said.to_lowercase().contains("key"),
                "the refusal should name the key as the problem: {said}"
            );
        }
        Ok(store) => panic!(
            "opening under the wrong key succeeded with {} entries — a server \
             started with the wrong key would show an audit trail that looks \
             complete and holds nothing",
            store.len()
        ),
    }
}

/// The same, with no key at all.
#[test]
fn a_sealed_log_opened_without_a_key_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_three(dir.path(), Some(key()));

    assert!(
        AuditStore::open_with_key(dir.path(), "proj", None).is_err(),
        "a sealed log opened with no key reported success, so removing \
         THETA_DATA_KEY would silently empty the trail"
    );
}

/// Encryption can be switched on for a project that has been running.
#[test]
fn clear_entries_written_before_the_key_stay_readable_after_it() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Two entries from before, in clear.
    {
        let mut store = AuditStore::open(dir.path(), "proj").expect("opens");
        store.append(entry("old_1")).expect("appends");
        store.append(entry("old_2")).expect("appends");
    }

    // The key arrives; new entries are sealed, old ones are left alone.
    let (bytes, _) = DataKey::generate();
    {
        let mut store =
            AuditStore::open_with_key(dir.path(), "proj", Some(DataKey::from_bytes(&bytes)))
                .expect("opens the mixed log");
        store.append(entry("new_1")).expect("appends");
    }

    let store = AuditStore::open_with_key(dir.path(), "proj", Some(DataKey::from_bytes(&bytes)))
        .expect("reopens");

    assert_eq!(
        store.len(),
        3,
        "turning encryption on lost the entries written before it"
    );

    let raw = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("readable");
    assert!(
        raw.contains("old_1"),
        "an existing clear entry was rewritten, which an append-only log must never do"
    );
    assert!(
        !raw.contains("new_1"),
        "the entry written after the key arrived was not sealed"
    );
}

/// A crash mid-append still costs only the last event.
#[test]
fn a_half_written_final_line_loses_one_entry_and_not_the_trail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (bytes, _) = DataKey::generate();
    write_three(dir.path(), Some(DataKey::from_bytes(&bytes)));

    // Truncate the file mid-way through its last line, which is what a crash
    // between `write` and `sync_all` leaves behind.
    let path = dir.path().join("audit.jsonl");
    let raw = std::fs::read_to_string(&path).expect("readable");
    let cut = raw.len() - (raw.lines().last().expect("a last line").len() / 2);
    std::fs::write(&path, &raw[..cut]).expect("truncates");

    let store = AuditStore::open_with_key(dir.path(), "proj", Some(DataKey::from_bytes(&bytes)))
        .expect("a truncated tail must not stop the log opening");

    assert_eq!(
        store.len(),
        2,
        "a half-written final line should cost exactly the entry it was writing"
    );
}

/// Nothing in the data directory is left in clear except the header.
///
/// The file-level tests above look for known strings. This sweeps every byte
/// under the directory, which is what an attacker with a copied volume actually
/// has — and it is the check that would notice a second file appearing beside
/// `audit.jsonl` holding the same content unsealed.
#[test]
fn a_copied_directory_gives_up_nothing_but_the_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_three(dir.path(), Some(key()));

    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir.path()).expect("readable dir") {
        let path = entry.expect("entry").path();
        if path.is_file() {
            bytes.extend(std::fs::read(&path).expect("readable"));
        }
    }
    let raw = String::from_utf8_lossy(&bytes);

    for leaked in [
        "legacy_ref",
        "customers",
        "chg_1",
        "chg_2",
        "chg_3",
        "System",
    ] {
        assert!(
            !raw.contains(leaked),
            "{leaked:?} survives in the data directory in clear"
        );
    }

    // The positive control. Without it this passes against an empty directory,
    // which gives up nothing for the least useful reason.
    assert!(
        raw.contains("proj"),
        "the header is missing, so this swept nothing"
    );
}
