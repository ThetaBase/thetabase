//! The review queue and decision replay, through a real engine.
//!
//! Both modules had passing unit tests over hand-built inputs. Replay in
//! particular was written against a `RecordedDecision` that nothing in the
//! system produced — the audit trail held the resulting diff and none of the
//! classifier's inputs — so it could not have been run against a real decision
//! at all. That is the failure this file exists to make impossible.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use theta_safety::SafetyPolicy;
use thetad::engine::Engine;
use thetad::Config;

/// A policy that gates less than `protected`, for testing the loosening
/// direction.
///
/// Built by relaxing the thresholds rather than by naming a preset, because the
/// property under test is "weaker than what was recorded" and a preset that
/// later changed would stop being weaker without any test noticing.
/// The pair of policies these replays compare.
///
/// **Both are stricter than `protected()`, and that is required rather than
/// stylistic.** These decisions are recorded on `main`, and a protected branch
/// floors its thresholds at the protected preset's — so a policy looser than
/// `protected()` is clamped to it, and replaying one against the other shows no
/// divergence at all. The first version of these tests compared a wide-open
/// policy against `protected()` and started reporting "nothing loosened", which
/// was *true* and useless: on a protected branch nothing can.
///
/// Below the floor the policy is still the thing deciding, so the comparison
/// means something. It also matches the realistic case — a project that tightened
/// past the default and is considering relaxing back toward it.
fn stricter() -> SafetyPolicy {
    SafetyPolicy {
        irreversible_shadow_threshold: 10,
        ..SafetyPolicy::protected()
    }
}

fn weaker() -> SafetyPolicy {
    SafetyPolicy {
        irreversible_shadow_threshold: 60,
        ..SafetyPolicy::protected()
    }
}

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine_with(policy: SafetyPolicy) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("replayed");
    config.data_dir = dir.path().to_path_buf();
    config.safety = policy;
    (Engine::open(config).expect("open"), dir)
}

fn field(name: &str) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: ValueType::Int,
        nullable: true,
        crdt: None,
        declared_at: None,
    }
}

/// A table with `columns` real columns and `rows` rows.
///
/// The columns have to exist. Dropping a column that was never there measures
/// zero rows, and a zero-row drop lands on the same gate under every policy —
/// so a fixture built that way cannot show a threshold change in either
/// direction, which is how the first version of the two policy-direction tests
/// below managed to prove nothing.
fn seed(engine: &mut Engine, table: &str, rows: u64) {
    let mut fields = BTreeMap::from([("total".to_string(), field("total"))]);
    for i in 0..10u64 {
        fields.insert(format!("col_{i}"), field(&format!("col_{i}")));
    }
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddTable {
            table: TableDef {
                name: table.into(),
                fields,
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
        engine
            .put(
                BranchId::MAIN,
                &format!("{table}:{i}"),
                Value::Map(
                    (0..10u64)
                        .map(|c| (format!("col_{c}"), Value::Int(i as i64)))
                        .chain([("total".to_string(), Value::Int(i as i64))])
                        .collect::<BTreeMap<_, _>>(),
                ),
                agent("setup"),
                2_000 + i as i64,
            )
            .expect("put");
    }
}

fn drop_column(table: &str, n: u64) -> SchemaChange {
    SchemaChange::DropColumn {
        table: table.into(),
        column: format!("col_{n}"),
    }
}

// ---- replay ---------------------------------------------------------------

#[test]
fn a_decision_the_engine_actually_took_can_be_replayed() {
    // The gap this closes. `replay` took a `RecordedDecision` carrying the
    // change, the measured impact, branch protection and the branch — and the
    // audit trail stored a `ChangeDiff`, which has none of them. The module was
    // unreachable from real data and its tests could not tell.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    engine.propose_schema_change(BranchId::MAIN, drop_column("orders", 1), agent("a"), 3_000);

    let (decisions, opaque) = engine.recorded_decisions();
    assert!(
        !decisions.is_empty(),
        "no decision in the trail could be replayed, so replay has no input"
    );
    assert_eq!(opaque, 0, "a decision was recorded without its inputs");
}

#[test]
fn replaying_the_same_policy_agrees_with_itself() {
    // The baseline that makes every other replay result meaningful. If the
    // engine's own policy replayed differently from what the engine decided,
    // every divergence would be noise.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    for i in 0..6 {
        engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
    }

    let replay = engine.replay_decisions(&SafetyPolicy::protected());
    assert!(replay.is_complete(), "the replay did not cover the trail");
    assert_eq!(
        replay.report.looser, 0,
        "replaying the live policy claimed it would loosen its own decisions"
    );
    assert_eq!(
        replay.report.stricter, 0,
        "replaying the live policy claimed it would tighten its own decisions"
    );
    assert!(replay.report.agreed > 0, "nothing was replayed at all");
}

#[test]
fn a_weaker_policy_shows_up_as_loosening_rather_than_as_a_difference_count() {
    // The direction that matters. A single difference number lets a hundred
    // harmless tightenings hide the one decision that would now sail through.
    let (mut engine, _dir) = engine_with(stricter());
    // Thirty rows: above `stricter()`'s threshold of 10 and below
    // `weaker()`'s of 60. The two policies only disagree in that gap, and a
    // fixture outside it makes them agree — which is what the first version
    // did, at 250 rows, where both say ShadowValidate.
    seed(&mut engine, "orders", 30);

    for i in 0..5 {
        engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
    }

    let replay = engine.replay_decisions(&weaker());
    assert!(
        replay.report.has_loosened(),
        "a permissive policy replayed against protected decisions loosened nothing: {}",
        replay.report.summary()
    );
    assert!(
        replay.report.loosened().count() > 0,
        "`looser` was counted but the decisions themselves are not listed"
    );
}

#[test]
fn a_stronger_policy_is_reported_as_stricter_and_never_as_loosening() {
    let (mut engine, _dir) = engine_with(weaker());
    // Thirty rows: above `stricter()`'s threshold of 10 and below
    // `weaker()`'s of 60. The two policies only disagree in that gap, and a
    // fixture outside it makes them agree — which is what the first version
    // did, at 250 rows, where both say ShadowValidate.
    seed(&mut engine, "orders", 30);

    for i in 0..5 {
        engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
    }

    let replay = engine.replay_decisions(&stricter());
    assert_eq!(
        replay.report.looser, 0,
        "tightening the policy was reported as a loosening"
    );
    assert!(
        replay.report.stricter > 0,
        "a strictly stronger policy changed nothing, so the fixture proves nothing"
    );
}

#[test]
fn replay_uses_the_impact_measured_then_not_the_data_now() {
    // A replay that re-measured row counts against today's table would answer a
    // different question: it would tell you how the policy grades the database
    // as it is, not what it would have done to the decisions that were taken.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);
    engine.propose_schema_change(BranchId::MAIN, drop_column("orders", 1), agent("a"), 3_000);

    let (before, _) = engine.recorded_decisions();
    let recorded_impact = before[0].impact;

    // The table grows by a lot after the decision was taken.
    for i in 100..900 {
        engine
            .put(
                BranchId::MAIN,
                &format!("orders:{i}"),
                Value::Map(BTreeMap::from([("total".to_string(), Value::Int(1))])),
                agent("later"),
                5_000 + i,
            )
            .expect("put");
    }

    let (after, _) = engine.recorded_decisions();
    assert_eq!(
        after[0].impact, recorded_impact,
        "the recorded impact moved with the data, so replay answers a question \
         about today rather than about the decision"
    );
}

#[test]
fn a_replay_that_could_not_read_the_whole_trail_says_so() {
    // Coverage, not a verdict — the same rule the chain verifier follows. A
    // replay over part of the trail that reports no loosening has established
    // nothing about the part it could not read.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);
    engine.propose_schema_change(BranchId::MAIN, drop_column("orders", 1), agent("a"), 3_000);

    let replay = engine.replay_decisions(&SafetyPolicy::protected());
    assert!(replay.is_complete());
    assert_eq!(replay.not_replayable, 0);

    // And `is_complete` is not simply always true: an entry with a diff but no
    // decision inputs is the shape of a trail written before they were captured.
    assert_eq!(
        replay.report.decisions.len() + replay.not_replayable,
        replay.report.agreed
            + replay.report.stricter
            + replay.report.looser
            + replay.not_replayable,
        "the report does not account for every decision it was given"
    );
}

// ---- the review queue -----------------------------------------------------

#[test]
fn the_queue_holds_everything_waiting_and_hides_nothing() {
    // A queue view that filters has made a decision about what it dropped. When
    // the queue is too long the answer is the review budget, which refuses new
    // work at the door — not a view that conceals work already accepted.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    let mut gated = 0;
    for i in 0..9 {
        let diff = engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
        if diff.gate != Gate::AutoApply {
            gated += 1;
        }
    }
    assert!(gated > 0, "the fixture produced nothing gated");

    let queued: usize = engine.review_queue().iter().map(|b| b.changes.len()).sum();
    assert_eq!(
        queued, gated,
        "the queue does not show every change that is waiting"
    );
}

#[test]
fn two_engines_given_the_same_proposals_in_different_orders_show_the_same_queue() {
    // Proposals are held in a HashMap, whose iteration order is a property of
    // the process rather than of the data. A queue that reshuffles is one where
    // two operators looking at "the top item" are looking at different changes.
    //
    // The first version of this test compared the queue to itself within one
    // process, where HashMap order is perfectly stable — so a planted violation
    // removing the sort entirely passed. Insertion order has to actually differ
    // for the property to be under test.
    let columns: Vec<u64> = (0..8).collect();
    let mut reversed = columns.clone();
    reversed.reverse();

    let shape = |order: &[u64]| -> Vec<(String, Vec<String>)> {
        let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
        seed(&mut engine, "orders", 250);
        seed(&mut engine, "customers", 250);
        for (n, i) in order.iter().enumerate() {
            engine.propose_schema_change(
                BranchId::MAIN,
                drop_column("orders", *i),
                agent("a"),
                3_000 + n as i64,
            );
            engine.propose_schema_change(
                BranchId::MAIN,
                drop_column("customers", *i),
                agent("a"),
                4_000 + n as i64,
            );
        }
        engine
            .review_queue()
            .iter()
            .map(|b| {
                (
                    b.key.0.clone(),
                    b.changes.iter().map(|c| c.change_id.0.clone()).collect(),
                )
            })
            .collect()
    };

    let forwards = shape(&columns);
    assert!(!forwards.is_empty(), "the fixture produced an empty queue");
    assert_eq!(
        forwards,
        shape(&reversed),
        "the queue order follows the order proposals arrived rather than the changes themselves"
    );
}
#[test]
fn a_batch_carries_the_gate_of_its_strongest_member() {
    // Five safe changes can be a large one. A batch gated at its average, or at
    // whichever member happened to sort first, would let a reviewer approve
    // something under a weaker gate than it earned.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    for i in 0..6 {
        engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
    }

    for batch in engine.review_queue() {
        let strongest = batch
            .changes
            .iter()
            .map(|c| theta_safety::triage::strictness(c.gate))
            .max()
            .expect("a batch is never empty");
        assert_eq!(
            theta_safety::triage::strictness(batch.gate),
            strongest,
            "batch `{}` is gated below its strongest member",
            batch.key.0
        );
    }
}

#[test]
fn a_batch_names_the_change_driving_its_gate() {
    // "This batch needs shadow validation" is not actionable on its own. The
    // reviewer's next question is always which one, and a queue that cannot
    // answer it produces a reviewer who splits the batch by hand.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    for i in 0..5 {
        engine.propose_schema_change(
            BranchId::MAIN,
            drop_column("orders", i),
            agent("a"),
            3_000 + i as i64,
        );
    }

    for batch in engine.review_queue() {
        let driver = batch.driving_change().expect("a batch is never empty");
        assert_eq!(
            theta_safety::triage::strictness(driver.gate),
            theta_safety::triage::strictness(batch.gate),
            "the named driving change does not carry the batch's gate"
        );
    }
}

#[test]
fn an_auto_applied_change_never_reaches_the_review_queue() {
    // The queue rations human attention. A change that needs no human in it is
    // noise that makes the queue look longer than the work it represents.
    let (mut engine, _dir) = engine_with(SafetyPolicy::protected());
    seed(&mut engine, "orders", 250);

    for i in 0..20 {
        engine.propose_schema_change(
            BranchId::MAIN,
            SchemaChange::AddColumn {
                table: "orders".into(),
                field: field(&format!("extra_{i}")),
            },
            agent("a"),
            3_000 + i,
        );
    }

    assert!(
        engine.review_queue().is_empty(),
        "auto-applied changes are queued for a human who has nothing to decide"
    );
}
