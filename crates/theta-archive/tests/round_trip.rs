//! The archive's failure modes, in the only place they can be exercised.
//!
//! A real archive cannot be asked to corrupt a segment, lie about success, or
//! go unreachable on the third of five uploads. So the backend is a trait and
//! these tests supply one that does all three on demand.
//!
//! Every test here is about the same claim, which is the only claim a backup
//! system gets to make: **the bytes come back**. The interesting cases are the
//! ones where something says they will and they do not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use theta_archive::{
    ArchiveBackend, ArchiveError, ArchiveOutcome, Digest, Manifest, StoredRef, Sweep,
};

/// A backend that can be told to misbehave.
#[derive(Debug, Default)]
struct FakeArchive {
    objects: BTreeMap<String, Vec<u8>>,
    /// Bytes returned instead of what was stored, by key. Silent corruption:
    /// the store succeeds, the fetch succeeds, and the contents are wrong.
    corrupt: BTreeMap<String, Vec<u8>>,
    /// Keys the archive will refuse to accept, as if it were unreachable.
    unreachable: Vec<String>,
    /// Keys whose container reports itself intact when it is not.
    integrity_lies: Vec<String>,
    /// Keys the store claims to have written without writing anything.
    phantom_writes: Vec<String>,
}

impl FakeArchive {
    fn corrupt_on_fetch(&mut self, key: &str, bytes: &[u8]) {
        self.corrupt.insert(key.to_string(), bytes.to_vec());
    }

    fn make_unreachable(&mut self, key: &str) {
        self.unreachable.push(key.to_string());
    }

    fn lie_about_integrity(&mut self, key: &str) {
        self.integrity_lies.push(key.to_string());
    }

    fn accept_without_storing(&mut self, key: &str) {
        self.phantom_writes.push(key.to_string());
    }
}

impl ArchiveBackend for FakeArchive {
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError> {
        if self.unreachable.iter().any(|k| k == key) {
            return Err(ArchiveError::Unavailable("the archive is down".into()));
        }

        let bytes = std::fs::read(source).map_err(|e| ArchiveError::Io {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })?;
        let stored_bytes = bytes.len() as u64;

        if !self.phantom_writes.iter().any(|k| k == key) {
            self.objects.insert(key.to_string(), bytes);
        }

        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes,
            container_checksum: Some("sha256:fake".into()),
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError> {
        let bytes = self
            .corrupt
            .get(&stored.key)
            .or(self.objects.get(&stored.key))
            .ok_or(ArchiveError::NotArchived(0))?;

        std::fs::write(dest, bytes).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: e.to_string(),
        })
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        if self.integrity_lies.contains(&stored.key) {
            return Ok(true);
        }
        Ok(self.objects.contains_key(&stored.key))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    /// A segment file with realistic, compressible contents.
    fn segment(&self, sequence: u64) -> PathBuf {
        let path = self.dir.path().join(format!("{sequence:012}.seg"));
        let mut bytes = Vec::new();
        for i in 0..500 {
            bytes.extend_from_slice(
                format!("{{\"seq\":{sequence},\"key\":\"users:{i}\",\"op\":\"put\"}}\n").as_bytes(),
            );
        }
        std::fs::write(&path, bytes).expect("write segment");
        path
    }

    fn scratch(&self) -> PathBuf {
        self.dir.path().join("scratch")
    }

    fn restore_dir(&self) -> PathBuf {
        self.dir.path().join("restore")
    }
}

fn sweep(fixture: &Fixture) -> Sweep<FakeArchive> {
    Sweep::new(
        FakeArchive::default(),
        Manifest::new("test-project"),
        fixture.scratch(),
    )
}

// ---- the invariant ---------------------------------------------------------

#[test]
fn a_segment_that_round_trips_is_releasable() {
    let f = Fixture::new();
    let mut s = sweep(&f);

    let outcome = s.archive_segment(0, &f.segment(0), 1_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Archived { .. }),
        "{outcome:?}"
    );
    assert!(outcome.releasable());
    assert_eq!(s.manifest().len(), 1);
}

#[test]
fn a_segment_the_archive_returns_wrong_is_never_releasable() {
    // The failure this crate exists for. The store succeeds, the fetch
    // succeeds, and the bytes are not the bytes — which is exactly the shape of
    // the bug that is invisible until a restore.
    let f = Fixture::new();
    let mut backend = FakeArchive::default();
    backend.corrupt_on_fetch("segments/000000000000.at1", b"not what went in");
    let mut s = Sweep::new(backend, Manifest::new("p"), f.scratch());

    let outcome = s.archive_segment(0, &f.segment(0), 1_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert!(
        !outcome.releasable(),
        "a segment whose round trip failed was marked safe to delete"
    );
}

#[test]
fn a_failed_round_trip_leaves_nothing_in_the_manifest() {
    // A manifest entry is a claim that the segment is safely archived. Writing
    // one for a segment that just failed its proof would make the archive lie
    // in the one place a restore trusts.
    let f = Fixture::new();
    let mut backend = FakeArchive::default();
    backend.corrupt_on_fetch("segments/000000000000.at1", b"wrong");
    let mut s = Sweep::new(backend, Manifest::new("p"), f.scratch());

    s.archive_segment(0, &f.segment(0), 1_000);

    assert!(s.manifest().is_empty());
}

#[test]
fn a_store_that_claims_success_without_storing_is_caught() {
    // "The upload returned 200" is not evidence. Only reading it back is.
    let f = Fixture::new();
    let mut backend = FakeArchive::default();
    backend.accept_without_storing("segments/000000000000.at1");
    let mut s = Sweep::new(backend, Manifest::new("p"), f.scratch());

    let outcome = s.archive_segment(0, &f.segment(0), 1_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert!(!outcome.releasable());
}

#[test]
fn an_unreachable_archive_defers_rather_than_failing() {
    // An archive that is down is a normal condition: writes continue locally
    // and the snapshot queues (ROADMAP M10). Treating it as corruption would
    // page someone for a network blip.
    let f = Fixture::new();
    let mut backend = FakeArchive::default();
    backend.make_unreachable("segments/000000000000.at1");
    let mut s = Sweep::new(backend, Manifest::new("p"), f.scratch());

    let outcome = s.archive_segment(0, &f.segment(0), 1_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Deferred { .. }),
        "{outcome:?}"
    );
    assert!(
        !outcome.releasable(),
        "a deferred segment must stay on local disk"
    );
    assert!(s.manifest().is_empty());
}

#[test]
fn a_deferred_segment_is_archived_on_a_later_sweep() {
    // The other half of "queue and retry": the queue has to drain.
    let f = Fixture::new();
    let mut backend = FakeArchive::default();
    backend.make_unreachable("segments/000000000000.at1");
    let mut s = Sweep::new(backend, Manifest::new("p"), f.scratch());
    let path = f.segment(0);

    assert!(matches!(
        s.archive_segment(0, &path, 1_000),
        ArchiveOutcome::Deferred { .. }
    ));

    // The next sweep, with the archive back up: a new backend, carrying forward
    // the manifest the deferred sweep left behind.
    let mut fresh = Sweep::new(FakeArchive::default(), s.into_manifest(), f.scratch());
    let outcome = fresh.archive_segment(0, &path, 2_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Archived { .. }),
        "{outcome:?}"
    );
    assert!(outcome.releasable());
}

#[test]
fn archiving_the_same_segment_twice_does_not_store_it_twice() {
    let f = Fixture::new();
    let mut s = sweep(&f);
    let path = f.segment(0);

    s.archive_segment(0, &path, 1_000);
    let second = s.archive_segment(0, &path, 2_000);

    assert!(matches!(second, ArchiveOutcome::AlreadyArchived { .. }));
    assert!(
        second.releasable(),
        "an already-proved segment is releasable"
    );
    assert_eq!(s.manifest().len(), 1);
}

// ---- restore ---------------------------------------------------------------

#[test]
fn a_restore_returns_every_segment_byte_for_byte() {
    let f = Fixture::new();
    let mut s = sweep(&f);

    let mut originals = Vec::new();
    for sequence in 0..4 {
        let path = f.segment(sequence);
        originals.push(std::fs::read(&path).expect("read"));
        assert!(s.archive_segment(sequence, &path, 1_000).releasable());
    }

    let restored = s.restore(3, &f.restore_dir()).expect("restores");

    assert_eq!(restored.len(), 4);
    for (path, original) in restored.iter().zip(&originals) {
        assert_eq!(&std::fs::read(path).expect("read"), original);
    }
}

#[test]
fn a_restore_stops_at_a_segment_that_rotted_after_it_was_archived() {
    // Object storage can corrupt an object long after it was written and
    // proved. A restore that trusted the archive would be trusting the thing it
    // exists to recover from.
    let f = Fixture::new();
    let mut s = Sweep::new(FakeArchive::default(), Manifest::new("p"), f.scratch());

    for sequence in 0..3 {
        assert!(s
            .archive_segment(sequence, &f.segment(sequence), 1_000)
            .releasable());
    }

    // Rot sets in on segment 1, after it was archived and proved.
    let mut backend = FakeArchive::default();
    for sequence in 0..3 {
        backend
            .store(
                &format!("segments/{sequence:012}.at1"),
                &f.segment(sequence),
            )
            .expect("store");
    }
    backend.corrupt_on_fetch("segments/000000000001.at1", b"rotted");

    let rotted = Sweep::new(backend, s.into_manifest(), f.scratch());
    let err = rotted
        .restore(2, &f.restore_dir())
        .expect_err("a rotted segment must stop the restore");

    assert!(
        matches!(err, ArchiveError::RoundTripFailed { sequence: 1, .. }),
        "{err:?}"
    );
}

#[test]
fn a_segment_that_failed_its_check_is_not_left_in_the_restore_directory() {
    // Otherwise someone finds a directory of segments, one of them wrong, and
    // has no way to tell which.
    let f = Fixture::new();
    let mut s = Sweep::new(FakeArchive::default(), Manifest::new("p"), f.scratch());
    s.archive_segment(0, &f.segment(0), 1_000);

    let mut backend = FakeArchive::default();
    backend
        .store("segments/000000000000.at1", &f.segment(0))
        .expect("store");
    backend.corrupt_on_fetch("segments/000000000000.at1", b"wrong");

    let rotted = Sweep::new(backend, s.into_manifest(), f.scratch());
    let dest = f.restore_dir();
    assert!(rotted.restore(0, &dest).is_err());

    assert!(
        !dest.join("000000000000.seg").exists(),
        "a segment that failed verification was left behind"
    );
}

#[test]
fn a_restore_through_a_gap_is_refused() {
    // The log is a fold: applying what comes after a gap produces a state that
    // never existed, and it looks entirely normal.
    let f = Fixture::new();
    let mut s = sweep(&f);

    s.archive_segment(0, &f.segment(0), 1_000);
    s.archive_segment(2, &f.segment(2), 1_000);

    let err = s
        .restore(2, &f.restore_dir())
        .expect_err("segment 1 is missing");
    assert!(err.to_string().contains("hole"), "{err}");
}

#[test]
fn a_point_in_time_restore_stops_where_it_was_asked_to() {
    let f = Fixture::new();
    let mut s = sweep(&f);
    for sequence in 0..5 {
        s.archive_segment(sequence, &f.segment(sequence), 1_000);
    }

    let restored = s.restore(2, &f.restore_dir()).expect("restores");
    assert_eq!(restored.len(), 3, "restored past the requested point");
}

// ---- auditing --------------------------------------------------------------

#[test]
fn an_audit_finds_a_container_that_has_gone_missing() {
    let f = Fixture::new();
    let mut s = sweep(&f);
    for sequence in 0..3 {
        s.archive_segment(sequence, &f.segment(sequence), 1_000);
    }

    // The archive loses one object.
    let mut backend = FakeArchive::default();
    for sequence in [0u64, 2] {
        backend
            .store(
                &format!("segments/{sequence:012}.at1"),
                &f.segment(sequence),
            )
            .expect("store");
    }

    let audited = Sweep::new(backend, s.into_manifest(), f.scratch()).audit();
    assert_eq!(audited, vec![(0, true), (1, false), (2, true)]);
}

#[test]
fn an_audit_is_weaker_than_a_restore_and_the_tests_say_so() {
    // `check_integrity` asks the container whether it is intact. A container
    // that lies passes. This is why the audit is a cheap frequent check and
    // never a substitute for the round-trip proof — stated here so nobody
    // later mistakes a green audit for a verified backup.
    let f = Fixture::new();
    let mut s = sweep(&f);
    s.archive_segment(0, &f.segment(0), 1_000);

    let mut backend = FakeArchive::default();
    backend.lie_about_integrity("segments/000000000000.at1");
    backend.corrupt_on_fetch("segments/000000000000.at1", b"wrong");

    let lying = Sweep::new(backend, s.into_manifest(), f.scratch());

    assert_eq!(
        lying.audit(),
        vec![(0, true)],
        "the audit believed the container"
    );
    assert!(
        lying.restore(0, &f.restore_dir()).is_err(),
        "and the restore did not"
    );
}

// ---- the manifest ----------------------------------------------------------

#[test]
fn the_manifest_records_the_digest_of_the_original_not_of_the_container() {
    // The digest has to be taken before the archive touches anything. Taken
    // after, it would only prove the archive is self-consistent — which is true
    // of an archive that lost everything and is consistent about it.
    let f = Fixture::new();
    let path = f.segment(0);
    let expected = Digest::of(&std::fs::read(&path).expect("read"));

    let mut s = sweep(&f);
    s.archive_segment(0, &path, 1_000);

    assert_eq!(s.manifest().get(0).expect("archived").digest, expected);
}

#[test]
fn the_manifest_survives_a_restart_and_still_restores() {
    let f = Fixture::new();
    let manifest_path = f.dir.path().join("manifest.json");

    let mut s = sweep(&f);
    for sequence in 0..3 {
        s.archive_segment(sequence, &f.segment(sequence), 1_000);
    }
    s.manifest().save(&manifest_path).expect("saves");

    // A new process, the same archive.
    let mut backend = FakeArchive::default();
    for sequence in 0..3 {
        backend
            .store(
                &format!("segments/{sequence:012}.at1"),
                &f.segment(sequence),
            )
            .expect("store");
    }

    let reloaded = Manifest::load(&manifest_path).expect("loads");
    let after_restart = Sweep::new(backend, reloaded, f.scratch());

    assert_eq!(
        after_restart
            .restore(2, &f.restore_dir())
            .expect("restores")
            .len(),
        3
    );
}
