//! End-to-end gate tests: the Safety Layer as the engine actually enforces it.
//!
//! The unit tests in `theta-safety` prove the classifier is correct. These prove
//! the engine cannot be driven *around* it — which is the claim that matters.

use std::collections::BTreeMap;
use theta_core::schema::{FieldDef, SchemaChange};

use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use theta_safety::diff::ChangeId;
use thetad::config::Environment;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent() -> Author {
    Author::agent("sess_test", "u_test")
}

/// A production-strictness engine over a throwaway data directory.
///
/// The `TempDir` is returned alongside so the caller keeps it alive; dropping it
/// deletes the log out from under the engine.
fn prod_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("test-project");
    config.environment = Environment::Prod;
    config.safety = Environment::Prod.default_policy();
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open engine"), dir)
}

/// Write `count` rows of `table`, each holding `column`.
///
/// Real rows, because the engine measures a proposal's impact against its own
/// view. These tests used to pass a made-up `Impact` alongside the change, so
/// what they proved held for a number the caller invented rather than for the
/// data on the branch.
fn seed_rows(engine: &mut Engine, table: &str, column: &str, count: u64) {
    for i in 0..count {
        // Two columns, so dropping one leaves a row behind. A single-column row
        // *is* its column, and dropping it removes the row — correct, but not
        // the case these tests are about.
        let row = Value::Map(BTreeMap::from([
            (column.to_string(), Value::Text(format!("row-{i}"))),
            ("name".to_string(), Value::Text(format!("name-{i}"))),
        ]));
        engine
            .put(BranchId::MAIN, &format!("{table}:{i}"), row, agent(), 0)
            .expect("seed write");
    }
}

#[test]
fn a_drop_on_main_cannot_be_applied_by_confirmation_alone() {
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };

    seed_rows(&mut engine, "users", "email", 200);

    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 1_000);
    assert_eq!(
        diff.rows_affected, 200,
        "the engine counted, not the caller"
    );
    assert!(diff.destructive);
    assert!(diff.requires_confirm);

    // Even with confirm = true, the strongest gate holds.
    let result = engine.apply_schema_change(&diff.change_id, true, agent(), 1_001);
    assert!(
        matches!(result, Err(EngineError::Gated { .. })),
        "an irreversible high-impact drop was applied with only a confirmation"
    );
}

#[test]
fn the_same_drop_lands_after_shadow_validation_and_promotion() {
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };

    seed_rows(&mut engine, "users", "email", 200);

    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 1_000);

    let shadow = engine
        .open_shadow_branch(
            BranchId::MAIN,
            &diff.change_id,
            change.clone(),
            agent(),
            1_001,
        )
        .unwrap();
    assert_ne!(
        shadow,
        BranchId::MAIN,
        "validation must not run against the target"
    );

    // The change ran on the shadow branch and nowhere else.
    assert!(
        engine.get(shadow, "users:0").is_some(),
        "the row survives; only its dropped column should be gone"
    );
    assert!(
        engine.get(BranchId::MAIN, "users:0").is_some(),
        "the target must be untouched until promotion"
    );

    let validation = engine
        .validate_shadow(&diff.change_id, agent(), 1_002)
        .expect("validate");
    assert!(validation.passed, "{}", validation.summary());

    // Confirmation is still not a path to applying this change. Promotion is.
    let confirmed = engine.apply_schema_change(&diff.change_id, true, agent(), 1_003);
    assert!(
        matches!(confirmed, Err(EngineError::Gated { .. })),
        "a validated change must still not be clearable by confirmation: {confirmed:?}"
    );

    let promoted = engine.promote_shadow(&diff.change_id, agent(), 1_004);
    assert!(promoted.is_ok(), "promotion should succeed: {promoted:?}");

    // And it landed: the column is gone from the target.
    let Some(theta_core::Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("row missing from the target after promotion");
    };
    assert!(!fields.contains_key("email"), "the drop did not reach main");
}

#[test]
fn opening_a_shadow_branch_is_not_by_itself_a_validation() {
    // The hole this flow was built to close: the engine used to accept a
    // confirmation as soon as a shadow branch existed, so opening one and
    // confirming cleared the strongest gate in the product.
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    seed_rows(&mut engine, "users", "email", 200);
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);

    engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change.clone(), agent(), 1)
        .expect("shadow");

    // Nothing has been validated, so there is nothing to promote.
    assert!(
        matches!(
            engine.promote_shadow(&diff.change_id, agent(), 2),
            Err(EngineError::Gated { .. })
        ),
        "an unvalidated shadow branch was promoted"
    );
    // And confirmation is not an alternative route.
    assert!(matches!(
        engine.apply_schema_change(&diff.change_id, true, agent(), 3),
        Err(EngineError::Gated { .. })
    ));

    // The target still holds everything.
    let Some(theta_core::Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("row missing");
    };
    assert!(fields.contains_key("email"));
}

#[test]
fn a_validation_that_failed_cannot_be_promoted() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 200);

    // A change that does nothing: the column is not there to drop. Validation
    // must not report a pass for a change with no effect, because the record it
    // leaves says the change was checked.
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "not-a-column".into(),
    };
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);
    engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), 1)
        .expect("shadow");

    let validation = engine
        .validate_shadow(&diff.change_id, agent(), 2)
        .expect("validate");
    assert!(!validation.passed, "a no-op change validated clean");
    assert!(validation
        .checks
        .iter()
        .any(|c| c.name == "change_took_effect" && !c.passed));

    assert!(matches!(
        engine.promote_shadow(&diff.change_id, agent(), 3),
        Err(EngineError::Gated { .. })
    ));
}

#[test]
fn a_validation_goes_stale_when_the_shadow_branch_moves_under_it() {
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    seed_rows(&mut engine, "users", "email", 200);
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);

    let shadow = engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), 1)
        .expect("shadow");
    assert!(
        engine
            .validate_shadow(&diff.change_id, agent(), 2)
            .expect("validate")
            .passed
    );

    // Someone writes to the shadow branch after the checks ran. Promoting now
    // would merge content nothing verified.
    engine
        .put(shadow, "users:extra", Value::Int(1), agent(), 3)
        .expect("write");

    assert!(
        matches!(
            engine.promote_shadow(&diff.change_id, agent(), 4),
            Err(EngineError::Gated { .. })
        ),
        "a stale pass was accepted as a current one"
    );
}

#[test]
fn validation_catches_a_change_that_touches_a_table_it_did_not_name() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 10);
    seed_rows(&mut engine, "orders", "total", 10);

    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);
    let shadow = engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), 1)
        .expect("shadow");

    // Stand in for a migration that quietly reaches beyond its own table — the
    // failure a reviewer cannot spot by reading the proposal.
    engine
        .delete(shadow, "orders:3", agent(), 2)
        .expect("delete");

    let validation = engine
        .validate_shadow(&diff.change_id, agent(), 3)
        .expect("validate");
    assert!(!validation.passed);

    let stray = validation
        .checks
        .iter()
        .find(|c| c.name == "only_the_named_table_changed")
        .expect("the check ran");
    assert!(!stray.passed);
    assert!(
        stray.samples.iter().any(|s| s.key == "orders:3"),
        "the offending row is named, not just counted"
    );
}

#[test]
fn the_same_change_is_gated_by_the_data_on_the_branch_it_targets() {
    // The classification follows the branch's own rows. Nothing the proposer
    // sends is read — there is no longer a parameter for it — so this is what
    // "the caller cannot argue its way past the gate" means concretely.
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };

    let empty = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);
    assert_eq!(empty.rows_affected, 0);
    assert_eq!(
        empty.gate,
        Gate::Confirm,
        "a drop that destroys no data is confirmable, not shadow-gated"
    );

    seed_rows(&mut engine, "users", "email", 200);

    let populated = engine.propose_schema_change(BranchId::MAIN, change, agent(), 1);
    assert_eq!(populated.rows_affected, 200);
    assert_eq!(
        populated.gate,
        Gate::ShadowValidate,
        "the same change over real data must reach the strongest gate: {}",
        populated.reason
    );
}

#[test]
fn a_proposal_never_mutates_anything_by_itself() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "orders", "total", 50);

    engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::DropTable {
            table: "orders".into(),
        },
        agent(),
        1_000,
    );
    // Proposing changed nothing: the rows are all still there.
    assert!(engine.get(BranchId::MAIN, "orders:0").is_some());
    assert!(engine.get(BranchId::MAIN, "orders:49").is_some());
}

#[test]
fn a_safe_additive_change_applies_without_ceremony() {
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::AddColumn {
        table: "users".into(),
        field: FieldDef {
            name: "nickname".into(),
            ty: ValueType::Text,
            nullable: true,
            crdt: None,
            declared_at: None,
        },
    };
    // A populated table, so "additive changes are cheap" is being asserted
    // about a real table rather than an empty one.
    seed_rows(&mut engine, "users", "email", 5_000);

    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 1_000);
    assert_eq!(
        diff.rows_affected, 0,
        "adding a column touches no existing row, however big the table"
    );
    assert!(!diff.requires_confirm, "the safe path must stay fast");
    assert!(engine
        .apply_schema_change(&diff.change_id, false, agent(), 1_001)
        .is_ok());
}

#[test]
fn a_runaway_agent_loop_is_stopped_by_the_breaker() {
    let (mut engine, _dir) = prod_engine();
    let ceiling = engine.config().safety.breaker_row_ceiling;

    let mut writes_before_trip = 0u64;
    let mut tripped = false;
    for i in 0..10_000u64 {
        let op = theta_core::OpType::Put {
            key: format!("k{i}"),
            value: Value::Int(i as i64),
        };
        match engine.write_batch(BranchId::MAIN, vec![op; 100], agent(), i as i64) {
            Ok(_) => writes_before_trip += 100,
            Err(EngineError::BreakerOpen { .. }) => {
                tripped = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    assert!(
        tripped,
        "a runaway loop ran to completion without tripping the breaker"
    );
    assert!(
        writes_before_trip <= ceiling,
        "breaker let {writes_before_trip} rows through"
    );
    assert!(engine.breaker().is_tripped());

    // The trip is legible, not a silent 503.
    let trail = engine.audit_log();
    let summary = trail.last().expect("breaker trip is audited");
    assert!(
        summary.summary.contains("breaker tripped"),
        "got: {}",
        summary.summary
    );
}

#[test]
fn a_write_contradicting_a_declared_type_is_rejected_never_coerced() {
    let (mut engine, _dir) = prod_engine();
    let change = SchemaChange::AddColumn {
        table: "users".into(),
        field: FieldDef {
            name: "age".into(),
            ty: ValueType::Int,
            nullable: true,
            crdt: None,
            declared_at: None,
        },
    };
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);
    engine
        .apply_schema_change(&diff.change_id, false, agent(), 1)
        .unwrap();

    // `users:1` addresses row 1 of table `users`; the value is the row, so
    // `age` is checked against its declared Int.
    let bad_row = Value::Map(BTreeMap::from([(
        "age".to_string(),
        Value::Text("42".into()),
    )]));
    let result = engine.put(BranchId::MAIN, "users:1", bad_row, agent(), 2);
    assert!(
        matches!(result, Err(EngineError::TypeMismatch { .. })),
        "a text value was accepted into an int column, got {result:?}"
    );
    assert!(
        engine.get(BranchId::MAIN, "users:1").is_none(),
        "nothing was written"
    );

    // The same row with the right type is accepted, so the rejection above was
    // about the type and not about the write path being broken.
    let good_row = Value::Map(BTreeMap::from([("age".to_string(), Value::Int(42))]));
    assert!(engine
        .put(BranchId::MAIN, "users:1", good_row, agent(), 3)
        .is_ok());
}

#[test]
fn an_undeclared_column_is_allowed_because_dynamic_writes_are_legal() {
    let (mut engine, _dir) = prod_engine();
    // Prototyping writes have no schema to contradict. What the engine must
    // never do is *reinterpret* a value it already accepted.
    let row = Value::Map(BTreeMap::from([(
        "anything".to_string(),
        Value::Bool(true),
    )]));
    assert!(engine
        .put(BranchId::MAIN, "scratch:1", row, agent(), 0)
        .is_ok());
}

#[test]
fn every_gated_proposal_produces_a_human_legible_audit_entry() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 200);

    engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::DropColumn {
            table: "users".into(),
            column: "email".into(),
        },
        agent(),
        1_000,
    );

    let trail = engine.audit_log();
    let entry = trail.last().expect("proposal is audited");
    assert!(entry.summary.contains("agent session sess_test"));
    assert!(entry.summary.contains("users.email"));
    assert!(entry.summary.contains("main"));
    assert!(entry.summary.contains("blocked"));
}

// ---- what the gate classified is what runs ---------------------------------
//
// A gate is only worth anything if the change it classified is the change that
// executes, on the branch it was measured against. Both of these were live
// bypasses: the caller sent the change body and the branch alongside the
// confirmation, and the engine read the gate from the stored proposal while
// executing what the caller sent.

#[test]
fn a_confirmation_cannot_carry_a_different_change_than_the_one_proposed() {
    let (mut engine, _dir) = prod_engine();
    // Under the shadow threshold, so this change sits at the confirm gate —
    // which is the gate a confirmation is supposed to clear.
    seed_rows(&mut engine, "users", "email", 50);

    // Proposed and gated as a column drop: 50 rows, irreversible.
    let proposed = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let diff = engine.propose_schema_change(BranchId::MAIN, proposed, agent(), 0);
    assert!(diff.requires_confirm);

    // Confirming names the id and nothing else. There is no parameter for a
    // change body, which is what makes the substitution impossible rather than
    // merely detected: a `drop table` sent here used to drop the table under a
    // `drop column` proposal's gate.
    engine
        .apply_schema_change(&diff.change_id, true, agent(), 1)
        .expect("the proposed change applies");

    // The column is gone, and the table is not.
    let Some(theta_core::Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("the table was dropped; only its column should have been");
    };
    assert!(!fields.contains_key("email"));
    assert!(fields.contains_key("name"));
}

#[test]
fn a_proposal_lands_on_the_branch_it_was_measured_against() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 300);

    let dev = engine
        .create_branch("dev", BranchId::MAIN, agent(), 0)
        .expect("branch");
    // Empty the table on `dev`, so the same change measures as harmless there.
    for i in 0..300 {
        engine
            .delete(dev, &format!("users:{i}"), agent(), 1)
            .expect("delete");
    }

    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let diff = engine.propose_schema_change(dev, change, agent(), 2);
    assert_eq!(diff.rows_affected, 0, "the dev branch has no rows left");

    // Confirming applies it to `dev`, which is what was classified. There is no
    // branch parameter to point at `main`: doing so used to drop 300 rows from
    // `main` under a zero-row classification.
    engine
        .apply_schema_change(&diff.change_id, true, agent(), 3)
        .expect("applies to its own branch");

    let Some(theta_core::Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("row missing from main");
    };
    assert!(
        fields.contains_key("email"),
        "a change proposed against `dev` landed on `main`"
    );
}

#[test]
fn an_id_that_was_never_issued_confirms_nothing() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 500);

    assert!(matches!(
        engine.apply_schema_change(&ChangeId("chg_not_a_real_id".into()), true, agent(), 0),
        Err(EngineError::UnknownChange(_))
    ));
}

#[test]
fn a_change_cannot_be_applied_twice_from_one_proposal() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 50);

    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let diff = engine.propose_schema_change(BranchId::MAIN, change, agent(), 0);
    engine
        .apply_schema_change(&diff.change_id, true, agent(), 1)
        .expect("first apply");

    // One confirmation answers one proposal. Replaying it must not re-run the
    // change against whatever the branch looks like now.
    assert!(matches!(
        engine.apply_schema_change(&diff.change_id, true, agent(), 2),
        Err(EngineError::UnknownChange(_))
    ));
}

#[test]
fn unanswered_proposals_do_not_accumulate_for_the_life_of_the_process() {
    let (mut engine, _dir) = prod_engine();
    seed_rows(&mut engine, "users", "email", 500);

    let mut ids = Vec::new();
    for i in 0..50 {
        let change = SchemaChange::DropColumn {
            table: "users".into(),
            column: format!("col{i}"),
        };
        ids.push(
            engine
                .propose_schema_change(BranchId::MAIN, change, agent(), 0)
                .change_id,
        );
    }

    engine.collect_expired(theta_safety::policy::DEFAULT_SHADOW_TTL_MS as i64 + 1);

    for id in &ids {
        assert!(
            matches!(
                engine.apply_schema_change(id, true, agent(), 0),
                Err(EngineError::UnknownChange(_))
            ),
            "an expired proposal was still answerable"
        );
    }
}
