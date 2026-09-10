#![allow(dead_code)]

//! The contract every `ArchiveBackend` must satisfy (ROADMAP M10).
//!
//! A shared module rather than a file, because two suites run it: one against
//! a directory backend on every CI run, and one against a real S3-compatible
//! store when there is a bucket. A contract with two copies is two contracts.
//!
//! Adding a backend is adding a place a customer's only copy of their history
//! can go. So it is not enough that a backend passes tests written for it —
//! the failure this prevents is precisely a backend that passes its own suite
//! and not the shared one, which is how "we support S3 too" comes to mean
//! something weaker for S3 than it does for AT-1.
//!
//! So the invariants live here, once, generic over the trait. Every backend
//! runs all of them.
//!
//! # What is *not* here
//!
//! The adversarial cases — an archive that corrupts silently, lies about its
//! own integrity, accepts a write without storing it, or goes unreachable
//! mid-sweep. Those need a backend that misbehaves on demand, which no real
//! service can be asked to be, and they live in `round_trip.rs` against a fake
//! built to do exactly that. This file is the other half: the honest-path
//! contract, run against everything real.

use std::path::PathBuf;

use theta_archive::sweep::ArchiveOutcome;
use theta_archive::{ArchiveBackend, Manifest, Sweep};

/// A segment shaped like a real one: line-delimited JSON records.
///
/// Shape matters. A backend that compresses will behave differently on
/// log-shaped data than on random bytes, and log-shaped is what it will
/// actually be given.
fn segment_bytes(records: usize) -> Vec<u8> {
    (0..records)
        .flat_map(|n| {
            format!(
                "{{\"commitId\":{n},\"branchId\":0,\"op\":{{\"put\":{{\"key\":\"users:{n}\",\
                 \"value\":\"a fairly ordinary row value\"}}}}}}\n"
            )
            .into_bytes()
        })
        .collect()
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

    fn segment(&self, sequence: u64, records: usize) -> PathBuf {
        let path = self.dir.path().join(format!("{sequence:012}.seg"));
        std::fs::write(&path, segment_bytes(records)).expect("write segment");
        path
    }

    fn scratch(&self) -> PathBuf {
        let path = self.dir.path().join("scratch");
        std::fs::create_dir_all(&path).expect("scratch");
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// Run the whole contract against one backend.
///
/// Takes a closure rather than a value so each case gets a fresh backend — a
/// contract that only holds on a backend nothing has touched yet is not a
/// contract.
pub fn contract<B: ArchiveBackend>(name: &str, mut make: impl FnMut() -> B) {
    a_stored_segment_comes_back_byte_for_byte(name, &mut make);
    a_segment_that_round_trips_is_releasable(name, &mut make);
    two_segments_do_not_collide(name, &mut make);
    an_empty_segment_is_handled_rather_than_special_cased(name, &mut make);
    a_large_segment_survives(name, &mut make);
    integrity_never_claims_more_than_it_checked(name, &mut make);
}

fn a_stored_segment_comes_back_byte_for_byte<B: ArchiveBackend>(
    name: &str,
    make: &mut impl FnMut() -> B,
) {
    // The one that matters. Everything else in this crate is scaffolding
    // around this sentence.
    let fixture = Fixture::new();
    let mut backend = make();
    let source = fixture.segment(0, 500);
    let original = std::fs::read(&source).expect("read");

    let stored = backend
        .store("segments/000000000000", &source)
        .expect("store");
    let restored = fixture.path("restored");
    backend.fetch(&stored, &restored).expect("fetch");

    assert_eq!(
        std::fs::read(&restored).expect("read restored"),
        original,
        "[{name}] what came out of the archive is not what went in"
    );
}

fn a_segment_that_round_trips_is_releasable<B: ArchiveBackend>(
    name: &str,
    make: &mut impl FnMut() -> B,
) {
    // Through the sweep rather than the backend directly, because releasing is
    // the decision the proof gates and the sweep is what makes it.
    let fixture = Fixture::new();
    let mut sweep = Sweep::new(make(), Manifest::new("org_a/checkout"), fixture.scratch());
    let source = fixture.segment(0, 300);

    let outcome = sweep.archive_segment(0, &source, 1_000);
    assert!(
        outcome.releasable(),
        "[{name}] a segment that stored and proved is not releasable: {outcome:?}"
    );
    assert!(
        matches!(outcome, ArchiveOutcome::Archived { .. }),
        "[{name}] expected Archived, got {outcome:?}"
    );
    assert_eq!(
        sweep.manifest().len(),
        1,
        "[{name}] a proved segment is missing from the manifest"
    );
}

fn two_segments_do_not_collide<B: ArchiveBackend>(name: &str, make: &mut impl FnMut() -> B) {
    // Distinct keys must stay distinct. A backend that flattened them would
    // still round-trip each one — it would return the *last* one for both, and
    // the proof would pass for the second while the first was gone.
    let fixture = Fixture::new();
    let mut backend = make();

    let first = fixture.segment(0, 100);
    let second = fixture.segment(1, 400);
    let first_bytes = std::fs::read(&first).expect("read");
    let second_bytes = std::fs::read(&second).expect("read");
    assert_ne!(
        first_bytes, second_bytes,
        "the fixture made two identical segments"
    );

    let a = backend
        .store("segments/000000000000", &first)
        .expect("store");
    let b = backend
        .store("segments/000000000001", &second)
        .expect("store");

    let out_a = fixture.path("a");
    let out_b = fixture.path("b");
    backend.fetch(&a, &out_a).expect("fetch a");
    backend.fetch(&b, &out_b).expect("fetch b");

    assert_eq!(
        std::fs::read(&out_a).expect("read a"),
        first_bytes,
        "[{name}] first segment"
    );
    assert_eq!(
        std::fs::read(&out_b).expect("read b"),
        second_bytes,
        "[{name}] second segment"
    );
}

fn an_empty_segment_is_handled_rather_than_special_cased<B: ArchiveBackend>(
    name: &str,
    make: &mut impl FnMut() -> B,
) {
    // A segment with a header and no records is ordinary — a crash during
    // creation produces one. A backend that chokes on zero bytes turns a normal
    // artifact into a stuck sweep.
    let fixture = Fixture::new();
    let mut backend = make();
    let source = fixture.path("empty.seg");
    std::fs::write(&source, b"").expect("write");

    let stored = backend.store("segments/empty", &source).expect("store");
    let restored = fixture.path("restored-empty");
    backend.fetch(&stored, &restored).expect("fetch");

    assert!(
        std::fs::read(&restored).expect("read").is_empty(),
        "[{name}] an empty segment did not come back empty"
    );
}

fn a_large_segment_survives<B: ArchiveBackend>(name: &str, make: &mut impl FnMut() -> B) {
    // Large enough to cross a buffer boundary in anything that streams. The
    // failure this catches is a backend that works on a test-sized segment and
    // truncates a real one.
    let fixture = Fixture::new();
    let mut backend = make();
    let source = fixture.segment(0, 20_000);
    let original = std::fs::read(&source).expect("read");
    assert!(
        original.len() > 1_000_000,
        "the fixture is not large enough to be a test"
    );

    let stored = backend.store("segments/large", &source).expect("store");
    let restored = fixture.path("restored-large");
    backend.fetch(&stored, &restored).expect("fetch");

    let back = std::fs::read(&restored).expect("read");
    assert_eq!(
        back.len(),
        original.len(),
        "[{name}] a large segment changed length"
    );
    assert_eq!(back, original, "[{name}] a large segment came back altered");
}

fn integrity_never_claims_more_than_it_checked<B: ArchiveBackend>(
    name: &str,
    make: &mut impl FnMut() -> B,
) {
    // `check_integrity` is allowed to be weak. It is not allowed to be wrong
    // about an object that is fine, because a check that reports a healthy
    // archive as broken is one somebody switches off — taking the real
    // detection with it.
    let fixture = Fixture::new();
    let mut backend = make();
    let source = fixture.segment(0, 200);
    let stored = backend.store("segments/integrity", &source).expect("store");

    assert!(
        backend.check_integrity(&stored).expect("check"),
        "[{name}] a freshly stored object was reported as not intact"
    );
}
