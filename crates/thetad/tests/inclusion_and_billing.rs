//! Inclusion proofs and storage attribution, through the engine.
//!
//! Both modules are correct in isolation. What only the engine can answer is
//! *which root* a proof is against — and a proof against the head the server is
//! holding right now establishes almost nothing.

use std::collections::BTreeMap;

use theta_core::{Author, BranchId, ContentHash, Value};
use theta_storage::anchor::InMemorySink;
use theta_storage::inclusion::{self, ProofError};
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("proofs");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn put(engine: &mut Engine, branch: BranchId, key: &str, n: i64, at: i64) -> ContentHash {
    engine
        .put(
            branch,
            key,
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(n))])),
            agent("writer"),
            at,
        )
        .expect("put")
}

// ---- inclusion ------------------------------------------------------------

#[test]
fn an_entry_in_the_log_can_be_proved_to_be_in_it() {
    let (mut engine, _dir) = engine();
    let target = put(&mut engine, BranchId::MAIN, "k1", 1, 1_000);
    for i in 2..8 {
        put(&mut engine, BranchId::MAIN, &format!("k{i}"), i, 1_000 + i);
    }

    let proof = engine
        .prove_inclusion(BranchId::MAIN, &target)
        .expect("no error")
        .expect("the entry is in the log and was not proved");

    let head = engine.head(BranchId::MAIN).expect("a head");
    inclusion::verify(&proof, &head).expect("a proof of a real entry did not verify");
}

#[test]
fn an_entry_that_is_not_in_the_log_yields_no_proof() {
    // Not an unverifiable proof — no proof. A structure that could be built for
    // an absent entry and then failed verification would put the check in the
    // wrong place.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "k", 1, 1_000);

    let absent = ContentHash::of(b"an entry nobody ever wrote");
    assert!(
        engine
            .prove_inclusion(BranchId::MAIN, &absent)
            .expect("no error")
            .is_none(),
        "a proof was produced for an entry that is not in the log"
    );
}

#[test]
fn a_proof_does_not_verify_against_a_root_it_is_not_for() {
    // The whole reason `verify` takes the trusted root as a parameter rather
    // than reading it off the proof. Verifying against the root the proof
    // carries would establish only that the proof is self-consistent.
    let (mut engine, _dir) = engine();
    let target = put(&mut engine, BranchId::MAIN, "k1", 1, 1_000);
    put(&mut engine, BranchId::MAIN, "k2", 2, 1_001);

    let proof = engine
        .prove_inclusion(BranchId::MAIN, &target)
        .expect("no error")
        .expect("proof");

    let err = inclusion::verify(&proof, &ContentHash::of(b"some other root"))
        .expect_err("a proof verified against a root it was not built for");
    assert!(matches!(err, ProofError::WrongRoot { .. }), "{err}");
}

#[test]
fn a_proof_under_an_anchor_is_against_a_root_the_server_did_not_choose() {
    // The composition that makes an inclusion proof worth having. The anchored
    // head was published to a sink that can produce it independently, so a
    // counterparty verifying against it is checking against something this
    // server cannot quietly revise.
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));

    let target = put(&mut engine, BranchId::MAIN, "k1", 1, 1_000);
    put(&mut engine, BranchId::MAIN, "k2", 2, 1_001);

    let report = engine.maintain(10_000);
    assert!(!report.anchored.is_empty(), "nothing was anchored");
    let anchored_root = engine
        .anchor_log()
        .latest(BranchId::MAIN)
        .expect("an anchor")
        .head;

    // Writes after the anchor. The proof must still be against the anchored
    // root, not against what the log has grown into since.
    for i in 3..9 {
        put(&mut engine, BranchId::MAIN, &format!("k{i}"), i, 2_000 + i);
    }
    assert_ne!(
        engine.head(BranchId::MAIN),
        Some(anchored_root),
        "the fixture did not move the head past the anchor"
    );

    let proof = engine
        .prove_inclusion_under_anchor(BranchId::MAIN, &target)
        .expect("no error")
        .expect("proof");

    assert_eq!(
        proof.root, anchored_root,
        "the proof is against the current head rather than the anchored one"
    );
    inclusion::verify(&proof, &anchored_root)
        .expect("a proof under an anchor did not verify against the anchored root");
}

#[test]
fn an_entry_written_after_the_anchor_cannot_be_proved_under_it() {
    // It genuinely is not covered. An anchor closes the gap up to itself and not
    // one entry further, and a proof suggesting otherwise would be the exact
    // overreach the anchoring design refuses.
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    put(&mut engine, BranchId::MAIN, "before", 1, 1_000);
    engine.maintain(10_000);

    let after = put(&mut engine, BranchId::MAIN, "after", 2, 20_000);

    assert!(
        engine
            .prove_inclusion_under_anchor(BranchId::MAIN, &after)
            .expect("no error")
            .is_none(),
        "an entry written after the anchor was proved to be under it"
    );
}

#[test]
fn a_branch_that_was_never_anchored_refuses_rather_than_falling_back_to_its_own_head() {
    // The dangerous convenience. A silent fallback returns a proof that looks
    // identical to a strong one and establishes strictly less — and the caller
    // asking for a proof under an anchor is precisely the caller who cannot tell
    // the difference by inspection.
    let (mut engine, _dir) = engine();
    let target = put(&mut engine, BranchId::MAIN, "k", 1, 1_000);

    let result = engine.prove_inclusion_under_anchor(BranchId::MAIN, &target);
    assert!(
        matches!(result, Err(EngineError::Anchor(_))),
        "an unanchored branch produced a proof anyway"
    );
}

// ---- storage attribution --------------------------------------------------

#[test]
fn exclusive_plus_shared_equals_the_real_total() {
    // The property that makes this defensible as a bill. Anything else is a
    // number a customer can dispute and nobody can reconstruct.
    let (mut engine, _dir) = engine();
    for i in 0..20 {
        put(&mut engine, BranchId::MAIN, &format!("k{i}"), i, 1_000 + i);
    }
    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 2_000)
        .expect("branch");
    for i in 0..5 {
        put(&mut engine, side, &format!("s{i}"), i, 3_000 + i);
    }

    let attribution = engine.storage_attribution();
    let summed: u64 = attribution
        .branches
        .iter()
        .map(|b| b.exclusive_bytes)
        .sum::<u64>()
        + attribution.shared_bytes;

    assert_eq!(
        attribution.total_bytes(),
        summed,
        "the parts do not add up to the whole"
    );
    assert!(attribution.total_bytes() > 0, "nothing was attributed");
}

#[test]
fn a_row_two_branches_share_is_charged_to_neither_alone() {
    // Splitting shared rows across branches would make one branch's bill change
    // when an unrelated branch was deleted, which is impossible to explain to
    // the person paying it.
    let (mut engine, _dir) = engine();
    for i in 0..10 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("shared{i}"),
            i,
            1_000 + i,
        );
    }
    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 2_000)
        .expect("branch");

    let attribution = engine.storage_attribution();
    assert!(
        attribution.shared_bytes > 0,
        "rows visible from two branches were charged to one of them"
    );

    let side_share = attribution.branch(side).expect("the branch is missing");
    assert_eq!(
        side_share.exclusive_rows, 0,
        "a branch that has written nothing was charged for rows it merely inherited"
    );
}

#[test]
fn a_row_only_one_branch_can_see_is_charged_to_that_branch() {
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "common", 1, 1_000);
    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 2_000)
        .expect("branch");
    for i in 0..7 {
        put(&mut engine, side, &format!("only_here{i}"), i, 3_000 + i);
    }

    let attribution = engine.storage_attribution();
    let side_share = attribution.branch(side).expect("the branch is missing");
    assert_eq!(
        side_share.exclusive_rows, 7,
        "rows only this branch can see were not charged to it"
    );
}

#[test]
fn every_branch_appears_even_when_it_owns_nothing() {
    // A bill that omits a branch reads as "this branch is free". It is not free;
    // it is sharing, and the number that says so is zero rather than absent.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "k", 1, 1_000);
    let empty = engine
        .create_branch("untouched", BranchId::MAIN, Author::System, 2_000)
        .expect("branch");

    let attribution = engine.storage_attribution();
    let share = attribution
        .branch(empty)
        .expect("a branch that owns nothing was left out of the bill entirely");
    assert_eq!(share.exclusive_bytes, 0);
}
