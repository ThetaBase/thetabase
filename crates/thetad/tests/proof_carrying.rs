//! Proof-carrying migrations, checked against a real branch.
//!
//! The module's own tests run the checker against a hand-built `RowSource`. What
//! they cannot see is whether the engine hands it *all* the rows: CRDT-typed
//! fields live in their own map, and a checker shown only half a table would
//! report that a claim holds over the half it saw. That is the direction that
//! lowers a gate it should not.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use theta_safety::proof::{Claim, Verdict};
use theta_safety::SafetyPolicy;
use thetad::engine::Engine;
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("proven");
    config.data_dir = dir.path().to_path_buf();
    config.safety = SafetyPolicy::protected();
    (Engine::open(config).expect("open"), dir)
}

fn field(name: &str, ty: ValueType) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty,
        nullable: true,
        crdt: None,
        declared_at: None,
    }
}

/// A `measurements` table whose `reading` is a `Float`.
///
/// Float-to-Int is the narrowing this claim is actually for: a whole float fits
/// an int and a fractional one does not, so the same column can hold rows that
/// satisfy the claim and rows that break it.
///
/// The first version stored a `Text` in an `Int` column to make a violating row.
/// `put` refused it — the engine will not create data that contradicts the
/// declared type, so a fixture cannot manufacture one that way. Worth recording:
/// the only rows that can violate a *narrowing* are ones that were legal under
/// the wider type, which is exactly what this now builds.
fn seed(engine: &mut Engine, rows: u64, oversized_at: Option<u64>) {
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddTable {
            table: TableDef {
                name: "measurements".into(),
                fields: BTreeMap::from([
                    ("reading".to_string(), field("reading", ValueType::Float)),
                    ("label".to_string(), field("label", ValueType::Text)),
                ]),
                indexes: vec![],
            },
        },
        agent("setup"),
        1_000,
    );
    engine
        .apply_schema_change(&diff.change_id, true, agent("setup"), 1_001)
        .expect("table lands");

    for i in 0..rows {
        let reading = match oversized_at {
            // Fractional: legal as a Float, and lost by a narrowing to Int.
            Some(n) if n == i => Value::Float(i as f64 + 0.5),
            _ => Value::Float(i as f64),
        };
        engine
            .put(
                BranchId::MAIN,
                &format!("measurements:{i}"),
                Value::Map(BTreeMap::from([
                    ("reading".to_string(), reading),
                    ("label".to_string(), Value::Text(format!("m{i}"))),
                ])),
                agent("setup"),
                2_000 + i as i64,
            )
            .expect("put");
    }
}

fn narrow_reading() -> SchemaChange {
    SchemaChange::AlterColumnType {
        table: "measurements".into(),
        column: "reading".into(),
        from: ValueType::Float,
        to: ValueType::Int,
    }
}

#[test]
fn a_true_claim_lowers_the_gate_a_large_migration_earned_for_its_size() {
    // The whole point. A large migration that provably loses nothing should stop
    // needing a human merely because it is large.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 400, None);

    let ungated =
        engine.propose_schema_change(BranchId::MAIN, narrow_reading(), agent("baseline"), 3_000);
    assert_ne!(
        ungated.gate,
        Gate::AutoApply,
        "the fixture is not large enough to be gated, so this proves nothing"
    );

    let (diff, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("prover"),
        4_000,
    );

    assert!(matches!(verdict, Verdict::Holds { .. }), "{verdict:?}");
    assert_eq!(diff.gate, Gate::AutoApply);
}

#[test]
fn a_false_claim_leaves_the_gate_exactly_where_the_classifier_put_it() {
    // A failed proof is not a reason to gate harder and never a reason to gate
    // less. It simply buys nothing.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 400, Some(377));

    let baseline = engine
        .propose_schema_change(BranchId::MAIN, narrow_reading(), agent("baseline"), 3_000)
        .gate;

    let (diff, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("liar"),
        4_000,
    );

    assert!(matches!(verdict, Verdict::Fails { .. }), "{verdict:?}");
    assert_eq!(diff.gate, baseline, "a false claim moved the gate");
}

#[test]
fn a_refusal_names_a_key_and_never_a_value() {
    // A refusal ends up in logs and in an agent's context. The caller already
    // knows which rows exist; putting row *contents* there is a disclosure with
    // no upside.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200, Some(150));

    let (_, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("liar"),
        4_000,
    );

    let Verdict::Fails {
        first_violating_key,
        ..
    } = &verdict
    else {
        panic!("expected a failure, got {verdict:?}");
    };
    assert!(first_violating_key.starts_with("measurements:"));
    assert!(
        !first_violating_key.contains("150.5") && !first_violating_key.contains('.'),
        "the refusal carried the row's contents: {first_violating_key}"
    );
}

#[test]
fn every_row_of_the_table_is_examined_not_the_ones_the_caller_pointed_at() {
    // A checker that examined only rows the claim mentioned, or only a prefix,
    // would report that a claim holds over the part it looked at — the direction
    // that lowers a gate on the strength of rows nobody checked.
    //
    // The violating row is the *last* one, so a check that stopped early passes
    // the claim it should have failed.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 300, Some(299));

    let (_, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("prover"),
        4_000,
    );

    let Verdict::Fails { rows_examined, .. } = verdict else {
        panic!("a violating row in the last position was not found: {verdict:?}");
    };
    assert_eq!(
        rows_examined, 300,
        "the checker reported on fewer rows than the table has"
    );
}

// NOTE — CRDT rows.
//
// `Engine::rows_of` reads both the plain and the CRDT map, because CRDT-typed
// fields are just as much rows of the table and a checker shown only `keys`
// would examine a subset. There is deliberately **no test here** for that half,
// and the reason is worth recording rather than leaving as an apparent gap:
//
// there is currently no way to write CRDT state through the engine at all. The
// wire has no request for it, the engine has no method for it, and `get_crdt`
// can only ever return state that arrived through `merge` or `sync` — neither of
// which is wired either. A test was written for it, and a planted violation
// deleting the CRDT scan passed, because the fixture could not create a single
// CRDT row to be blind to.
//
// The scan stays. `view.rs` folds `OpType::Crdt`, `merge.rs` converges it and
// `sync.rs` declines to call it a conflict, so the state is real in the formats
// and in every layer that would handle it — only its creation is missing. A
// checker that silently skipped those rows once the write path lands would be a
// gate lowered on unexamined data, and it would be lowered quietly. Guarding a
// case the tests cannot yet reach is a different thing from guarding nothing,
// and saying which is the point of this note.
#[test]
fn a_verdict_says_how_much_it_looked_at() {
    // A claim that held over zero rows and one that held over a million are
    // different facts, and the first is usually a table name somebody spelled
    // wrong.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 250, None);

    let (_, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("prover"),
        4_000,
    );

    let Verdict::Holds { rows_examined } = verdict else {
        panic!("expected the claim to hold, got {verdict:?}");
    };
    assert_eq!(
        rows_examined, 250,
        "the verdict reports a row count that is not the table's"
    );
}

#[test]
fn no_proof_lowers_the_gate_on_a_drop() {
    // The case somebody will ask for, and the answer is no. Nothing true about
    // what a column contains today makes removing it reversible.
    //
    // The claim has to genuinely *hold*, or this proves nothing: `apply` returns
    // early on a failed verdict, so a test using a false claim never reaches the
    // rule it is aiming at. The first version did exactly that, and a planted
    // violation lowering drops to auto-apply passed.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 0, None);

    // An empty table, so `TableIsEmpty` is true — the strongest claim available,
    // and still not enough.
    let (_, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        SchemaChange::DropTable {
            table: "measurements".into(),
        },
        Claim::TableIsEmpty {
            table: "measurements".into(),
        },
        agent("optimist"),
        4_000,
    );
    assert!(
        verdict.holds(),
        "the claim must hold for this to reach the rule under test: {verdict:?}"
    );

    let classified = engine
        .propose_schema_change(
            BranchId::MAIN,
            SchemaChange::DropTable {
                table: "measurements".into(),
            },
            agent("baseline"),
            5_000,
        )
        .gate;

    let (diff, _) = engine.propose_with_claim(
        BranchId::MAIN,
        SchemaChange::DropTable {
            table: "measurements".into(),
        },
        Claim::TableIsEmpty {
            table: "measurements".into(),
        },
        agent("optimist"),
        6_000,
    );
    assert_eq!(
        diff.gate, classified,
        "a proved claim lowered the gate on a drop, which no proof may do"
    );
}
#[test]
fn a_claim_about_something_else_is_irrelevant_rather_than_false() {
    // A caller who attached the wrong claim made a different mistake from one
    // whose claim is false, and telling them apart is the difference between
    // fixing a typo and fixing their data.
    //
    // The claim has to be about a different *table*. `TableIsEmpty` on the table
    // being altered is not a mismatch — an empty table makes any change to it
    // safe — which the first version of this test got wrong and the checker got
    // right.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 300, None);

    let (_, verdict) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::TableIsEmpty {
            table: "some_other_table".into(),
        },
        agent("confused"),
        4_000,
    );

    assert!(
        matches!(verdict, Verdict::Irrelevant { .. }),
        "a mismatched claim was judged on its truth rather than its relevance: {verdict:?}"
    );
}

#[test]
fn a_lowered_gate_is_explained_in_the_audit_trail() {
    // Otherwise the trail shows a large destructive migration applied
    // automatically with nothing accounting for it, which is indistinguishable
    // from a classifier bug — and is the entry a reviewer most needs to be able
    // to reconstruct.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 400, None);

    let before = engine.audit_log().len();
    let (diff, _) = engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("prover"),
        4_000,
    );
    assert_eq!(
        diff.gate,
        Gate::AutoApply,
        "the fixture did not lower a gate"
    );
    assert!(engine.audit_log().len() > before);

    let log = engine.audit_log();
    let entry = log.last().expect("the proposal left an entry");
    assert!(
        entry.proof_lowered_the_gate(),
        "the trail does not record that a proof lowered this gate"
    );
    let proof = entry
        .detail
        .get("proof")
        .expect("the claim and verdict are not in the trail");
    assert!(proof.get("claim").is_some(), "the claim was not recorded");
    assert!(
        proof.get("verdict").is_some(),
        "the verdict was not recorded"
    );
}

#[test]
fn a_proof_lowered_gate_does_not_replay_as_a_policy_divergence() {
    // Replay asks what a *policy* would have decided. A proof is not part of the
    // policy, so recording the lowered gate as the decision would make every
    // proof-carrying migration replay as a spurious tightening — noise in the
    // one report whose value is that its numbers mean something.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 400, None);

    engine.propose_with_claim(
        BranchId::MAIN,
        narrow_reading(),
        Claim::NoValueViolatesType {
            table: "measurements".into(),
            column: "reading".into(),
        },
        agent("prover"),
        4_000,
    );

    let replay = engine.replay_decisions(&SafetyPolicy::protected());
    assert!(replay.is_complete(), "the replay did not cover the trail");
    assert_eq!(
        replay.report.looser + replay.report.stricter,
        0,
        "replaying the live policy diverged from itself because a proof changed \
         the recorded gate: {}",
        replay.report.summary()
    );
}
