//! Restore drills: what a drill proves, and what it must refuse to claim.
//!
//! The per-segment round-trip proof is already the strongest thing this crate
//! does. These are about the questions it does not answer — whether the manifest
//! is complete, whether the segments are still fetchable *today*, and whether a
//! partial drill can be mistaken for a full one.

use std::collections::BTreeMap;
use std::path::Path;

use theta_archive::drill::{run, Depth, DrillLog, DrillResult, Outcome};
use theta_archive::manifest::{ArchivedSegment, Manifest};
use theta_archive::{ArchiveBackend, ArchiveError, Digest, StoredRef};

/// A backend that stores bytes and can be told to lose or corrupt them.
///
/// The failure modes a real archive cannot be asked to produce on demand, which
/// is the same reason `round_trip.rs` keeps its own fake.
#[derive(Default)]
struct Archive {
    objects: BTreeMap<String, Vec<u8>>,
    /// Keys that will fail to fetch, as if the object had been deleted.
    lost: Vec<String>,
    /// Keys that fetch successfully and return the wrong number of bytes.
    /// Bit rot: the store succeeded, the fetch succeeds, the contents are wrong.
    truncated: Vec<String>,
}

impl Archive {
    fn lose(&mut self, key: &str) {
        self.lost.push(key.to_string());
    }

    fn rot(&mut self, key: &str) {
        self.truncated.push(key.to_string());
    }
}

impl ArchiveBackend for Archive {
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError> {
        let bytes = std::fs::read(source).map_err(|e| ArchiveError::Io {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })?;
        let stored_bytes = bytes.len() as u64;
        self.objects.insert(key.to_string(), bytes);
        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes,
            container_checksum: None,
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError> {
        if self.lost.contains(&stored.key) {
            return Err(ArchiveError::Unavailable(format!(
                "{} is not there any more",
                stored.key
            )));
        }
        let bytes = self
            .objects
            .get(&stored.key)
            .ok_or_else(|| ArchiveError::Unavailable(stored.key.clone()))?;
        let out = if self.truncated.contains(&stored.key) {
            &bytes[..bytes.len() / 2]
        } else {
            &bytes[..]
        };
        std::fs::write(dest, out).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: e.to_string(),
        })
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        Ok(self.objects.contains_key(&stored.key))
    }
}

/// An archive holding `sequences`, each a segment of `size` bytes.
fn archive_with(sequences: &[u64], size: usize) -> (Archive, Manifest, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut backend = Archive::default();
    let mut manifest = Manifest::new("drill-test");

    for sequence in sequences {
        let path = dir.path().join(format!("{sequence}.seg"));
        let bytes = vec![(*sequence % 251) as u8; size];
        std::fs::write(&path, &bytes).expect("write segment");
        let key = format!("seg-{sequence}");
        let stored = backend.store(&key, &path).expect("store");
        manifest.record(ArchivedSegment {
            sequence: *sequence,
            digest: Digest::of(&bytes),
            original_bytes: size as u64,
            stored,
            verified_at_ms: 1_000,
        });
    }

    (backend, manifest, dir)
}

#[test]
fn a_complete_archive_passes_and_says_it_covered_everything() {
    let (backend, manifest, dir) = archive_with(&[1, 2, 3, 4], 64);
    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");

    assert!(result.passed(), "{}", result.summary());
    assert!(result.covered_everything());
    assert_eq!(result.segments_restored, 4);
    assert_eq!(result.bytes_restored, 256);
}

#[test]
fn a_gap_in_the_manifest_fails_before_anything_is_fetched() {
    // The failure that matters most, because the restore *succeeds*. Every
    // segment round-trips individually while segment 3 is simply absent, and
    // applying 4 after 2 produces a state that never existed and looks entirely
    // normal.
    let (backend, manifest, dir) = archive_with(&[1, 2, 4, 5], 64);
    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");

    assert!(!result.passed());
    assert_eq!(result.outcome, Outcome::GapsFound);
    assert_eq!(result.gaps, vec![3]);
    assert_eq!(
        result.segments_restored, 0,
        "a drill on a holed manifest must not report partial success"
    );
    assert!(result.summary().contains("never existed"));
}

#[test]
fn a_segment_that_has_gone_missing_is_a_failure_naming_the_segment() {
    let (mut backend, manifest, dir) = archive_with(&[1, 2, 3], 64);
    backend.lose("seg-2");

    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");
    match result.outcome {
        Outcome::Failed { segment, .. } => assert_eq!(segment, 2),
        other => panic!("expected a failure on segment 2, got {other:?}"),
    }
    assert_eq!(
        result.segments_restored, 1,
        "the one that restored before the failure is reported honestly"
    );
}

#[test]
fn bit_rot_is_caught_because_the_drill_re_proves_the_round_trip_now() {
    // The archive proved a round trip when it stored this. That was then. A
    // drill re-proves it today, and rot is the failure only the second catches.
    let (mut backend, manifest, dir) = archive_with(&[1, 2, 3], 64);
    backend.rot("seg-3");

    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");
    match &result.outcome {
        Outcome::Failed { segment, detail } => {
            assert_eq!(*segment, 3);
            assert!(detail.contains("32 bytes"), "{detail}");
        }
        other => panic!("expected a size mismatch on segment 3, got {other:?}"),
    }
}

#[test]
fn a_shallow_drill_does_not_claim_to_have_covered_the_archive() {
    // The archive equivalent of a percentile over eleven samples. "The last two
    // segments restore" is true and useful; it is not "the archive restores".
    let (backend, manifest, dir) = archive_with(&[1, 2, 3, 4, 5, 6], 32);
    let result = run(&backend, &manifest, Depth::Newest(2), dir.path(), 5_000).expect("drill");

    assert!(result.passed());
    assert_eq!(result.segments_restored, 2);
    assert_eq!(result.segments_in_manifest, 6);
    assert!(
        !result.covered_everything(),
        "two of six segments is not the archive"
    );
    assert!(result.summary().contains("2 of 6 segments"));
}

#[test]
fn a_shallow_drill_checks_the_newest_segments_rather_than_the_oldest() {
    // The recent archive is what an incident needs. A drill that verified the
    // oldest segments would pass while everything written this week was
    // unreadable.
    let (mut backend, manifest, dir) = archive_with(&[1, 2, 3, 4, 5], 32);
    backend.lose("seg-5");

    let result = run(&backend, &manifest, Depth::Newest(2), dir.path(), 5_000).expect("drill");
    assert!(
        !result.passed(),
        "a shallow drill must look at the newest segments: {}",
        result.summary()
    );
}

#[test]
fn the_log_answers_when_a_restore_last_worked() {
    // The question an operator has mid-incident, and the reason passes are
    // recorded rather than only failures.
    let (backend, manifest, dir) = archive_with(&[1, 2], 32);
    let mut log = DrillLog::new();

    assert_eq!(log.last_pass_ms(), None);
    assert!(
        log.is_stale(1_000, 0),
        "no drill has ever run, which is not the same as a recent pass"
    );

    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");
    log.record(result);

    assert_eq!(log.last_pass_ms(), Some(5_000));
    assert_eq!(log.last_full_pass_ms(), Some(5_000));
    assert!(!log.is_stale(1_000, 5_500));
    assert!(log.is_stale(1_000, 7_000));
}

#[test]
fn a_shallow_pass_does_not_answer_can_i_restore() {
    // `last_full_pass_ms` ignores partial drills on purpose: "the newest ten
    // segments restored on Tuesday" is not an answer to "can I restore".
    let (backend, manifest, dir) = archive_with(&[1, 2, 3, 4], 32);
    let mut log = DrillLog::new();
    log.record(run(&backend, &manifest, Depth::Newest(1), dir.path(), 5_000).expect("drill"));

    assert_eq!(log.last_pass_ms(), Some(5_000));
    assert_eq!(
        log.last_full_pass_ms(),
        None,
        "a shallow pass must not be mistaken for a full one"
    );
}

#[test]
fn a_failed_drill_is_recorded_too() {
    // Otherwise the log reads as a history of successes, and "when did this
    // last work" is answerable while "how often does it fail" is not.
    let (mut backend, manifest, dir) = archive_with(&[1, 2], 32);
    backend.lose("seg-1");

    let mut log = DrillLog::new();
    let result = run(&backend, &manifest, Depth::Everything, dir.path(), 5_000).expect("drill");
    assert!(!result.passed());
    log.record(result);

    assert_eq!(log.all().len(), 1);
    assert_eq!(log.last_pass_ms(), None, "a failure is not a pass");
}

#[test]
fn a_drill_leaves_nothing_behind() {
    // A drill that restored over the live data directory would be a restore,
    // not a drill. It writes only into the scratch it was given, and cleans up.
    let (backend, manifest, dir) = archive_with(&[1, 2, 3], 32);
    let scratch = tempfile::tempdir().expect("scratch");

    run(
        &backend,
        &manifest,
        Depth::Everything,
        scratch.path(),
        5_000,
    )
    .expect("drill");

    let left: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("read scratch")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "the drill left files behind: {left:?}");
    // ...and the source directory is untouched.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn an_empty_archive_passes_vacuously_and_says_so() {
    // Reported rather than refused: a project that has archived nothing has
    // nothing broken. `covered_everything` is true over zero segments, and
    // `segments_in_manifest` is what stops that reading as reassurance.
    let backend = Archive::default();
    let manifest = Manifest::new("empty");
    let scratch = tempfile::tempdir().expect("scratch");

    let result: DrillResult =
        run(&backend, &manifest, Depth::Everything, scratch.path(), 1).expect("drill");
    assert!(result.passed());
    assert_eq!(result.segments_in_manifest, 0);
    assert_eq!(result.bytes_restored, 0);
}
