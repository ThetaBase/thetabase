//! The archive against the real AT-1 CLI.
//!
//! Everything in `round_trip.rs` runs against a fake backend, which is the only
//! way to exercise corruption, phantom writes and an unreachable archive. That
//! leaves one thing untested: whether the commands this crate actually issues do
//! what the CLI's documentation says they do.
//!
//! These tests close that gap. They are the ones that would catch a flag being
//! renamed, an exit code changing meaning, or a codec that is not as lossless
//! for our data as it is in general.
//!
//! # Why they skip rather than fail
//!
//! Encoding requires a connected AT-1 account; decoding and verification do
//! not. A contributor without a key should be able to run the suite, and CI
//! without a key should not go red for a reason that has nothing to do with the
//! change under review. So these skip when the CLI is absent or unauthenticated
//! — and say so on stdout, because a test that skips silently is a test that
//! stops being run.
//!
//! `make archive-live` runs them, and that is what has to pass before the cold
//! archive can be called done.

use std::path::PathBuf;

use theta_archive::at1::At1Archive;
use theta_archive::{ArchiveBackend, ArchiveOutcome, Digest, Manifest, Sweep};

/// Whether a real, logged-in `at1` is available.
///
/// Skipping is right for a contributor without a key and wrong for a release:
/// a suite that skips reports the same green as one that passed, and the whole
/// point of `make full` is that nothing gets tagged on a path nobody exercised.
/// So `THETA_REQUIRE_LIVE=1` turns a skip into a failure, and `make full`
/// sets it.
fn live() -> bool {
    let archive = At1Archive::new(std::env::temp_dir());

    let reason = if !archive.available() {
        Some("the `at1` CLI is not installed (npm install -g @tinyfiles/cli)")
    } else if archive.account().is_none() {
        Some("`at1` has no connected account — run `at1 login --key <key>`")
    } else {
        None
    };

    match reason {
        None => true,
        Some(reason) => {
            assert!(
                std::env::var("THETA_REQUIRE_LIVE").is_err(),
                "the live AT-1 suite cannot run: {reason}.\n\nTHETA_REQUIRE_LIVE is set, \
                 so this is a failure rather than a skip: a suite that skipped reports the \
                 same green as one that passed, and nothing is tagged on a path nobody ran."
            );
            println!("skipping: {reason}");
            false
        }
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

    /// A segment shaped like a real one: framed, line-delimited JSON records,
    /// repetitive in the way a log actually is.
    fn segment(&self, sequence: u64, rows: usize) -> PathBuf {
        let path = self.dir.path().join(format!("{sequence:012}.seg"));
        let mut bytes = Vec::new();
        for i in 0..rows {
            bytes.extend_from_slice(
                format!(
                    "{{\"seq\":{sequence},\"op\":\"put\",\"key\":\"users:{i}\",\
                     \"value\":{{\"email\":\"u{i}@example.com\",\"name\":\"User {i}\"}},\
                     \"ts\":{}}}\n",
                    1_700_000_000 + i
                )
                .as_bytes(),
            );
        }
        std::fs::write(&path, bytes).expect("write segment");
        path
    }

    fn archive_root(&self) -> PathBuf {
        self.dir.path().join("archive")
    }

    fn scratch(&self) -> PathBuf {
        self.dir.path().join("scratch")
    }

    fn restore_dir(&self) -> PathBuf {
        self.dir.path().join("restore")
    }
}

fn sweep(f: &Fixture) -> Sweep<At1Archive> {
    Sweep::new(
        At1Archive::new(f.archive_root()),
        Manifest::new("live-test"),
        f.scratch(),
    )
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn a_real_segment_survives_a_real_round_trip() {
    // The claim the whole crate rests on, against the real compressor rather
    // than a fake that returns what it was given.
    if !live() {
        return;
    }

    let f = Fixture::new();
    let path = f.segment(0, 2_000);
    let original = std::fs::read(&path).expect("read");

    let mut s = sweep(&f);
    let outcome = s.archive_segment(0, &path, 1_000);

    assert!(
        matches!(outcome, ArchiveOutcome::Archived { .. }),
        "AT-1 could not archive a segment: {outcome:?}"
    );
    assert!(outcome.releasable());

    // And the restore returns exactly what went in.
    let restored = s.restore(0, &f.restore_dir()).expect("restores");
    assert_eq!(restored.len(), 1);
    assert_eq!(
        std::fs::read(&restored[0]).expect("read"),
        original,
        "AT-1 did not return the segment byte for byte"
    );
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn compression_actually_buys_something_on_log_shaped_data() {
    // Not a benchmark — a sanity check that the codec is doing its job on the
    // data we actually have. A ratio near 1.0 would mean we are paying for a
    // compressor and getting a copy, which is worth knowing early rather than
    // from a storage bill.
    if !live() {
        return;
    }

    let f = Fixture::new();
    let mut s = sweep(&f);
    let path = f.segment(0, 5_000);
    let original_bytes = std::fs::metadata(&path).expect("stat").len();

    let outcome = s.archive_segment(0, &path, 1_000);
    let ArchiveOutcome::Archived { stored_bytes, .. } = outcome else {
        panic!("expected an archive, got {outcome:?}");
    };

    let ratio = stored_bytes as f64 / original_bytes as f64;
    println!("live AT-1: {original_bytes} -> {stored_bytes} bytes (ratio {ratio:.3})");
    assert!(
        ratio < 0.9,
        "AT-1 compressed a log segment to {ratio:.3} of its size, which is close \
         enough to a copy to be worth investigating"
    );
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn the_container_reports_its_own_integrity() {
    if !live() {
        return;
    }

    let f = Fixture::new();
    let mut s = sweep(&f);
    s.archive_segment(0, &f.segment(0, 500), 1_000);

    assert_eq!(s.audit(), vec![(0, true)]);
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn a_damaged_container_fails_both_the_audit_and_the_restore() {
    // The failure that actually happens to archives: bytes rot in object
    // storage long after they were written. Simulated by corrupting the
    // container on disk, which is what rot looks like from here.
    if !live() {
        return;
    }

    let f = Fixture::new();
    let mut s = sweep(&f);
    let path = f.segment(0, 500);
    assert!(s.archive_segment(0, &path, 1_000).releasable());

    // Damage the stored container in place.
    let container = f.archive_root().join("segments/000000000000.at1");
    let mut bytes = std::fs::read(&container).expect("read container");
    let midpoint = bytes.len() / 2;
    bytes[midpoint] ^= 0xff;
    std::fs::write(&container, &bytes).expect("damage it");

    let damaged = Sweep::new(
        At1Archive::new(f.archive_root()),
        s.into_manifest(),
        f.scratch(),
    );

    assert_eq!(
        damaged.audit(),
        vec![(0, false)],
        "a damaged container passed its integrity check"
    );
    assert!(
        damaged.restore(0, &f.restore_dir()).is_err(),
        "a damaged container was restored as if it were sound"
    );
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn several_segments_restore_in_log_order() {
    if !live() {
        return;
    }

    let f = Fixture::new();
    let mut s = sweep(&f);

    let mut originals = Vec::new();
    for sequence in 0..3 {
        let path = f.segment(sequence, 400);
        originals.push(std::fs::read(&path).expect("read"));
        assert!(
            s.archive_segment(sequence, &path, 1_000).releasable(),
            "segment {sequence} did not archive"
        );
    }

    let restored = s.restore(2, &f.restore_dir()).expect("restores");
    assert_eq!(restored.len(), 3);
    for (path, original) in restored.iter().zip(&originals) {
        assert_eq!(&std::fs::read(path).expect("read"), original);
    }
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn an_empty_segment_is_handled_rather_than_special_cased() {
    // A segment can legitimately be empty — a branch created and never written
    // to. Worth knowing whether the compressor copes, because the alternative
    // is discovering it on the one branch nobody touched.
    if !live() {
        return;
    }

    let f = Fixture::new();
    let path = f.dir.path().join("empty.seg");
    std::fs::write(&path, b"").expect("write");

    let mut s = sweep(&f);
    let outcome = s.archive_segment(0, &path, 1_000);

    match outcome {
        ArchiveOutcome::Archived { .. } => {
            let restored = s.restore(0, &f.restore_dir()).expect("restores");
            assert!(std::fs::read(&restored[0]).expect("read").is_empty());
        }
        // A refusal is acceptable and a silent success is not: if AT-1 will not
        // take an empty file, the caller must be told so it can skip it rather
        // than record a segment it cannot get back.
        ArchiveOutcome::Failed { .. } | ArchiveOutcome::Deferred { .. } => {
            assert!(
                s.manifest().is_empty(),
                "an unarchivable segment was recorded as archived"
            );
        }
        ArchiveOutcome::AlreadyArchived { .. } => panic!("nothing was archived yet"),
    }
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn binary_content_survives_as_well_as_text_does() {
    // Segments carry framed records with binary length prefixes, not just JSON.
    // `auto` picks a codec by structural fingerprint, so it is worth asserting
    // that the pick stays lossless on the shape we actually write.
    if !live() {
        return;
    }

    let f = Fixture::new();
    let path = f.dir.path().join("framed.seg");
    let mut bytes = Vec::new();
    for i in 0..2_000u32 {
        let record = format!("{{\"key\":\"users:{i}\",\"op\":\"put\"}}");
        bytes.extend_from_slice(&(record.len() as u32).to_le_bytes());
        bytes.extend_from_slice(record.as_bytes());
    }
    std::fs::write(&path, &bytes).expect("write");

    let mut s = sweep(&f);
    let outcome = s.archive_segment(0, &path, 1_000);
    assert!(
        matches!(outcome, ArchiveOutcome::Archived { .. }),
        "framed binary content did not archive: {outcome:?}"
    );

    let restored = s.restore(0, &f.restore_dir()).expect("restores");
    assert_eq!(
        Digest::of(&std::fs::read(&restored[0]).expect("read")),
        Digest::of(&bytes),
        "framed binary content did not survive the round trip"
    );
}

#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn the_backend_reports_a_missing_object_rather_than_inventing_one() {
    // No account needed: this is a read path.
    let f = Fixture::new();
    let archive = At1Archive::new(f.archive_root());
    let dest = f.restore_dir().join("out");

    let err = archive
        .fetch(
            &theta_archive::StoredRef {
                key: "segments/000000000099.at1".into(),
                stored_bytes: 0,
                container_checksum: None,
            },
            &dest,
        )
        .expect_err("nothing was ever stored under that key");

    println!("missing object reported as: {err}");
    assert!(
        !dest.exists(),
        "a fetch that failed still left a file behind"
    );
}

/// Named so a reader can see at a glance whether the live suite actually ran.
#[test]
#[ignore = "live: hits the real AT-1 service; run with `make archive-live`"]
fn report_whether_the_live_suite_is_exercising_anything() {
    match live() {
        true => println!("live AT-1 suite: running against a connected account"),
        false => println!(
            "live AT-1 suite: SKIPPED. The fake-backend suite still ran, but nothing \
             here has been checked against the real CLI."
        ),
    }
}
