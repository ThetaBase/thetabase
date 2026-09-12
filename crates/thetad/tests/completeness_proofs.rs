//! Completeness proofs, against a real engine holding real rows.
//!
//! `theta-storage`'s unit tests prove the tree is a correct Merkle structure.
//! They build maps out of synthetic keys, so they cannot tell you the engine
//! hands the map the right rows, or that a proof taken from one branch fails
//! against another. This does.
//!
//! The property under test is the one `inclusion.rs` names and cannot provide:
//! **a server cannot answer a range query with less than it holds.** An
//! inclusion proof shows every row it returned is real, and says nothing about
//! the row it left out.

use theta_core::{Author, BranchId, Value};
use theta_storage::completeness::verify_range;
use thetad::{Config, Engine};

/// An engine on `main` holding exactly these rows.
///
/// The `TempDir` is returned and held by the caller: dropping it deletes the
/// data directory out from under the engine, and a test that let it go would
/// fail somewhere unrelated to what it is testing.
fn engine_with_rows(rows: &[(String, Value)]) -> (Engine, BranchId, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("proofs");
    config.data_dir = dir.path().to_path_buf();
    let mut engine = Engine::open(config).expect("open");

    for (i, (key, value)) in rows.iter().enumerate() {
        engine
            .put(
                BranchId::MAIN,
                key,
                value.clone(),
                Author::System,
                1_700_000_000_000 + i as i64,
            )
            .expect("put");
    }

    (engine, BranchId::MAIN, dir)
}

/// Ten rows under one prefix, so a range means something.
fn rows() -> Vec<(String, Value)> {
    (0..10)
        .map(|i| {
            (
                format!("customers/{i:02}"),
                Value::Text(format!("customer {i}")),
            )
        })
        .collect()
}

#[test]
fn a_range_over_real_rows_proves_complete() {
    let (engine, branch, _dir) = engine_with_rows(&rows());

    let proof = engine
        .prove_range_complete(branch, "customers/02", "customers/05")
        .expect("proves");
    let root = engine.map_root(branch).expect("a root");

    assert_eq!(
        proof.matched.len(),
        3,
        "02, 03 and 04 are in [02, 05): {:?}",
        proof
            .matched
            .iter()
            .map(|p| &p.entry.key)
            .collect::<Vec<_>>()
    );
    verify_range(&proof, &root).expect("verifies against the engine's own root");
}

/// The whole point.
#[test]
fn a_server_cannot_quietly_omit_a_row() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let root = engine.map_root(branch).expect("a root");

    let mut proof = engine
        .prove_range_complete(branch, "customers/00", "customers/10")
        .expect("proves");
    assert_eq!(proof.matched.len(), 10);

    // A server that would rather you did not see customer 7.
    let dropped = proof
        .matched
        .iter()
        .position(|p| p.entry.key == "customers/07")
        .expect("07 is in the result");
    proof.matched.remove(dropped);

    assert!(
        verify_range(&proof, &root).is_err(),
        "a row was removed from the result and the proof still verified, which \
         is exactly the lie completeness proofs exist to catch"
    );
}

/// Every row that *is* returned still verifies — the control.
///
/// Without this the test above passes against a verifier that rejects
/// everything, which catches omissions the way a broken clock tells the time.
#[test]
fn an_untampered_result_is_not_rejected() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let root = engine.map_root(branch).expect("a root");

    for (start, end) in [
        ("customers/00", "customers/10"),
        ("customers/03", "customers/04"),
        ("customers/09", "customers/99"),
        ("a", "z"),
    ] {
        let proof = engine
            .prove_range_complete(branch, start, end)
            .expect("proves");
        verify_range(&proof, &root)
            .unwrap_or_else(|e| panic!("[{start}, {end}) was rejected: {e}"));
    }
}

#[test]
fn absence_is_provable_for_a_key_the_engine_does_not_hold() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let root = engine.map_root(branch).expect("a root");

    let proof = engine.prove_absent(branch, "customers/99").expect("proves");
    assert!(proof.matched.is_empty());
    verify_range(&proof, &root).expect("verifies");
}

/// A key that exists cannot be proved absent.
#[test]
fn a_present_key_cannot_be_proved_absent() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let root = engine.map_root(branch).expect("a root");

    let proof = engine.prove_absent(branch, "customers/04").expect("builds");

    // `prove_absent` on a present key produces a proof of the empty range at
    // that key, whose flanking entries are 03 and 04 -- and 04 is not outside
    // the range, so the boundary check refuses it.
    assert!(
        verify_range(&proof, &root).is_err(),
        "a key the engine holds was proved absent"
    );
}

/// A root from one branch must not verify another branch's proof.
#[test]
fn a_proof_does_not_verify_against_a_different_branch() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let (other, other_branch, _other_dir) = engine_with_rows(&[(
        "customers/00".to_string(),
        Value::Text("somebody else".into()),
    )]);

    let proof = engine
        .prove_range_complete(branch, "customers/00", "customers/10")
        .expect("proves");

    assert!(
        verify_range(&proof, &other.map_root(other_branch).expect("a root")).is_err(),
        "one database's proof verified against another's root"
    );
}

/// The root has to move when the data does.
///
/// A root that did not change after a write would let a server keep serving
/// proofs of yesterday's state that verify perfectly today.
#[test]
fn writing_a_row_changes_the_root() {
    let (engine, branch, _dir) = engine_with_rows(&rows());
    let before = engine.map_root(branch).expect("a root");

    let (after_engine, after_branch, _after_dir) = engine_with_rows(
        &rows()
            .into_iter()
            .chain(std::iter::once((
                "customers/10".to_string(),
                Value::Text("customer 10".into()),
            )))
            .collect::<Vec<_>>(),
    );
    let after = after_engine.map_root(after_branch).expect("a root");

    assert_ne!(before, after, "adding a row left the root unchanged");
}
