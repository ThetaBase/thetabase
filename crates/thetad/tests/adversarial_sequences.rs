//! Adversarial cases that need real rows and a real branch.
//!
//! Two of the three categories `07-agent-safety-layer.md` §10 requires cannot be
//! expressed against the classifier alone, because the thing under test is what
//! happens *across* operations:
//!
//!   * a sustained agent loop that never spikes, only accumulates;
//!   * a migration whose steps are each benign and whose sequence is not.
//!
//! Both are the same shape of threat, and the reason the breaker is independent
//! of destructive/non-destructive classification (§6): every individual write
//! here is one the rules would allow.
//!
//! Corpus entries are never deleted to make this suite pass.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use theta_safety::SafetyPolicy;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent() -> Author {
    Author::agent("sess_runaway", "agent")
}

/// An engine on the production-strict policy — the branch the central claim is
/// about.
fn protected_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("adversarial");
    config.data_dir = dir.path().to_path_buf();
    config.safety = SafetyPolicy::protected();
    (Engine::open(config).expect("open"), dir)
}

fn row(i: usize) -> Value {
    Value::Map(BTreeMap::from([
        (
            "email".to_string(),
            Value::Text(format!("u{i}@example.com")),
        ),
        ("name".to_string(), Value::Text(format!("User {i}"))),
    ]))
}

fn seed(engine: &mut Engine, count: usize) {
    for i in 0..count {
        engine
            .put(BranchId::MAIN, &format!("users:{i}"), row(i), agent(), 0)
            .expect("seed");
    }
}

// ---- sustained loops straddling the threshold ------------------------------

#[test]
fn a_loop_that_never_exceeds_the_per_write_threshold_is_still_stopped() {
    // The case the breaker exists for, and the one classification cannot see:
    // every write is small enough to be waved through on its own, and the
    // aggregate is ruinous. Nothing here is destructive by type.
    let (mut engine, _dir) = protected_engine();
    let ceiling = engine.policy().breaker_row_ceiling;

    let mut written = 0u64;
    let mut stopped_at = None;
    for i in 0..(ceiling * 2) {
        // One row at a time — the smallest possible write, forever.
        match engine.put(
            BranchId::MAIN,
            &format!("k:{i}"),
            Value::Int(i as i64),
            agent(),
            1_000,
        ) {
            Ok(_) => written += 1,
            Err(EngineError::BreakerOpen { .. }) => {
                stopped_at = Some(written);
                break;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    let stopped = stopped_at.expect("a runaway loop ran to completion");
    assert!(
        stopped <= ceiling,
        "the loop wrote {stopped} rows before stopping, past the {ceiling}-row ceiling"
    );
}

#[test]
fn a_loop_cannot_wait_out_its_own_window_and_resume() {
    // The obvious evasion: pause until the rolling window clears, then carry on.
    // A tripped breaker stays tripped until an operator resets it, so the pause
    // buys nothing.
    let (mut engine, _dir) = protected_engine();
    let ceiling = engine.policy().breaker_row_ceiling;

    for i in 0..(ceiling + 10) {
        if engine
            .put(
                BranchId::MAIN,
                &format!("k:{i}"),
                Value::Int(1),
                agent(),
                1_000,
            )
            .is_err()
        {
            break;
        }
    }

    // An hour later.
    let after_the_window = 1_000 + 3_600_000;
    assert!(
        matches!(
            engine.put(
                BranchId::MAIN,
                "k:resumed",
                Value::Int(1),
                agent(),
                after_the_window
            ),
            Err(EngineError::BreakerOpen { .. })
        ),
        "the loop resumed by waiting out its own window"
    );
}

#[test]
fn batched_writes_are_counted_by_rows_and_not_by_calls() {
    // The other way to straddle a threshold: make fewer, larger calls. The
    // breaker accumulates row impact, so a batch of 10,000 counts as 10,000.
    let (mut engine, _dir) = protected_engine();
    let ceiling = engine.policy().breaker_row_ceiling;

    let batch = 10_000usize;
    let mut rows = 0u64;
    for round in 0..((ceiling as usize / batch) + 2) {
        let ops: Vec<theta_core::OpType> = (0..batch)
            .map(|i| theta_core::OpType::Put {
                key: format!("b{round}:{i}"),
                value: Value::Int(1),
            })
            .collect();

        match engine.write_batch(BranchId::MAIN, ops, agent(), 1_000) {
            Ok(_) => rows += batch as u64,
            Err(EngineError::BreakerOpen { .. }) => {
                assert!(
                    rows <= ceiling,
                    "{rows} rows landed in batches before the breaker noticed, \
                     past the {ceiling}-row ceiling"
                );
                return;
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    panic!("batched writes bypassed the breaker entirely");
}

// ---- benign in isolation, destructive in sequence --------------------------

#[test]
fn emptying_a_table_first_does_not_make_dropping_it_a_small_change() {
    // The sequence attack worth worrying about. Impact is measured against the
    // branch's rows, so an agent that deletes the rows first can propose a drop
    // that measures as trivial — the gate reads a nearly empty table.
    //
    // What stops it is that the deletions are themselves the destructive act,
    // and they go through the breaker. The drop being cheap afterwards is
    // correct: by then there is genuinely almost nothing left to lose.
    let (mut engine, _dir) = protected_engine();
    let ceiling = engine.policy().breaker_row_ceiling as usize;

    // More rows than the breaker will let anyone delete in one window.
    seed(&mut engine, 2_000);

    let mut deleted = 0;
    for i in 0..(ceiling + 2_000) {
        match engine.delete(BranchId::MAIN, &format!("users:{i}"), agent(), 1_000) {
            Ok(_) => deleted += 1,
            Err(EngineError::BreakerOpen { .. }) => break,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    assert!(
        deleted <= ceiling,
        "an agent deleted {deleted} rows unchecked while preparing a 'cheap' drop"
    );
}

#[test]
fn a_rename_then_drop_is_gated_at_the_step_that_destroys() {
    // Benign step: rename `email` to `email_v2` — reversible, and the classifier
    // says so. Destructive step: drop `email_v2`. The sequence launders nothing,
    // because the drop is classified on the rows it will actually destroy.
    let (mut engine, _dir) = protected_engine();
    seed(&mut engine, 500);

    let rename = SchemaChange::RenameColumn {
        table: "users".into(),
        from: "email".into(),
        to: "email_v2".into(),
    };
    let renamed = engine.propose_schema_change(BranchId::MAIN, rename, agent(), 0);
    engine
        .apply_schema_change(&renamed.change_id, true, agent(), 1)
        .expect("a rename is confirmable");

    let drop = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email_v2".into(),
    };
    let dropped = engine.propose_schema_change(BranchId::MAIN, drop, agent(), 2);

    assert_eq!(
        dropped.rows_affected, 500,
        "the drop was measured against the renamed column's real rows"
    );
    assert_eq!(
        dropped.gate,
        Gate::ShadowValidate,
        "a drop reached by way of a rename must be gated like any other drop"
    );
}

#[test]
fn adding_a_column_then_dropping_the_original_does_not_launder_the_drop() {
    // The "safe migration" shape: add the new column, backfill, drop the old
    // one. Only the last step destroys anything, and only the last step is
    // gated — which is correct, and worth pinning so a future optimisation
    // cannot decide the sequence as a whole is "a migration" and wave it past.
    let (mut engine, _dir) = protected_engine();
    seed(&mut engine, 500);

    let add = SchemaChange::AddColumn {
        table: "users".into(),
        field: FieldDef {
            name: "email_v2".into(),
            ty: ValueType::Text,
            nullable: true,
            crdt: None,
            declared_at: None,
        },
    };
    let added = engine.propose_schema_change(BranchId::MAIN, add, agent(), 0);
    assert_eq!(
        added.gate,
        Gate::AutoApply,
        "adding a nullable column is safe"
    );

    let drop = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let dropped = engine.propose_schema_change(BranchId::MAIN, drop, agent(), 1);
    assert_eq!(dropped.gate, Gate::ShadowValidate);
}

#[test]
fn a_sequence_of_gated_proposals_never_lands_without_an_answer() {
    // The central claim, stated over a sequence rather than one change: no
    // ordering of proposals puts a destructive change on a protected branch
    // without someone answering for it.
    let (mut engine, _dir) = protected_engine();
    seed(&mut engine, 500);

    let changes = [
        SchemaChange::DropColumn {
            table: "users".into(),
            column: "email".into(),
        },
        SchemaChange::DropColumn {
            table: "users".into(),
            column: "name".into(),
        },
        SchemaChange::DropTable {
            table: "users".into(),
        },
    ];

    for change in changes {
        let diff = engine.propose_schema_change(BranchId::MAIN, change, agent(), 0);
        assert_ne!(diff.gate, Gate::AutoApply, "{}", diff.reason);

        // And confirmation does not clear any of them.
        assert!(
            matches!(
                engine.apply_schema_change(&diff.change_id, true, agent(), 1),
                Err(EngineError::Gated { .. })
            ),
            "`{}` was cleared by confirmation alone",
            diff.affected_schema.change_type
        );
    }

    // Nothing landed.
    let Some(Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("the table is gone");
    };
    assert!(fields.contains_key("email") && fields.contains_key("name"));
}
