//! Shadow branches are ephemeral, and something has to make that true.
//!
//! A shadow branch exists to answer one question. Once it has — promoted,
//! rejected, or left unanswered past its deadline — it should be gone. Nothing
//! forces a proposer to come back, so an agent proposing a thousand drops would
//! otherwise leave a thousand branches behind
//! (`01-system-architecture.md` §2.2).
//!
//! Reclaiming is safe in the only direction that matters: a reclaimed branch is
//! a change that did *not* land. There is no way for collecting too eagerly to
//! let an unreviewed change through, which is why it can run unattended.

use std::collections::BTreeMap;

use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, Value};
use theta_safety::policy::DEFAULT_SHADOW_TTL_MS;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent() -> Author {
    Author::agent("session", "agent")
}

fn human() -> Author {
    Author::Human {
        user_id: "reviewer".into(),
    }
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("project");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

/// Enough rows on `users` that a drop lands at the shadow-validation gate.
fn seed(engine: &mut Engine, count: usize) {
    for i in 0..count {
        let row = Value::Map(BTreeMap::from([
            (
                "email".to_string(),
                Value::Text(format!("u{i}@example.com")),
            ),
            ("name".to_string(), Value::Text(format!("User {i}"))),
        ]));
        engine
            .put(BranchId::MAIN, &format!("users:{i}"), row, agent(), 0)
            .expect("put");
    }
}

fn drop_email() -> SchemaChange {
    SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    }
}

/// Propose a drop and open its shadow branch at `now_ms`.
fn open_shadow(engine: &mut Engine, now_ms: i64) -> (theta_safety::diff::ChangeId, BranchId) {
    let change = drop_email();
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), now_ms);
    let shadow = engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), now_ms)
        .expect("shadow");
    (diff.change_id, shadow)
}

#[test]
fn a_promoted_shadow_branch_is_reclaimed_once_it_has_landed() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (change_id, shadow) = open_shadow(&mut engine, 1_000);

    engine
        .validate_shadow(&change_id, agent(), 1_001)
        .expect("validate");
    engine
        .promote_shadow(&change_id, human(), 1_002)
        .expect("promote");

    assert!(
        engine.branches().get(shadow).is_none(),
        "a promoted shadow branch outlived its purpose"
    );
    assert!(engine.open_shadows().is_empty());
}

#[test]
fn a_rejected_proposal_takes_its_shadow_branch_with_it() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (change_id, shadow) = open_shadow(&mut engine, 1_000);

    engine
        .reject_shadow(&change_id, "we still need that column", human(), 1_001)
        .expect("reject");

    assert!(engine.branches().get(shadow).is_none());
    // And the change is not promotable afterwards by any route.
    assert!(matches!(
        engine.promote_shadow(&change_id, human(), 1_002),
        Err(EngineError::UnknownChange(_))
    ));
    assert!(matches!(
        engine.apply_schema_change(&change_id, true, agent(), 1_003),
        Err(EngineError::UnknownChange(_))
    ));
}

#[test]
fn a_proposal_can_be_rejected_before_anyone_validates_it() {
    // Most rejections happen here: a reviewer reads the diff and says no
    // without waiting for checks to run.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (change_id, shadow) = open_shadow(&mut engine, 0);

    engine
        .reject_shadow(&change_id, "not this quarter", human(), 1)
        .expect("reject");
    assert!(engine.branches().get(shadow).is_none());
}

#[test]
fn an_abandoned_shadow_branch_is_reclaimed_once_its_deadline_passes() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (change_id, shadow) = open_shadow(&mut engine, 0);

    let ttl = DEFAULT_SHADOW_TTL_MS as i64;
    assert!(
        engine.collect_expired(ttl - 1).is_empty(),
        "collected a branch that was still inside its deadline"
    );
    assert!(engine.branches().get(shadow).is_some());

    let reclaimed = engine.collect_expired(ttl);
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].change_id, change_id);
    assert!(!reclaimed[0].was_validated);
    assert!(engine.branches().get(shadow).is_none());
}

#[test]
fn reclaiming_a_shadow_branch_leaves_the_target_untouched() {
    // The whole safety case for unattended collection: the change did not land.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (change_id, _) = open_shadow(&mut engine, 0);
    engine
        .validate_shadow(&change_id, agent(), 1)
        .expect("validate");

    engine.collect_expired(DEFAULT_SHADOW_TTL_MS as i64 * 2);

    let Some(Value::Map(fields)) = engine.get(BranchId::MAIN, "users:0") else {
        panic!("row missing from main");
    };
    assert!(
        fields.contains_key("email"),
        "garbage collection applied the change it was supposed to discard"
    );
}

#[test]
fn a_validated_proposal_left_unanswered_is_reclaimed_and_says_so() {
    // Distinguishable from an abandoned one: a human was asked and never
    // answered, which is worth being able to see.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let (_, _) = open_shadow(&mut engine, 0);
    let change_id = engine.open_shadows()[0].change_id.clone();
    engine
        .validate_shadow(&change_id, agent(), 1)
        .expect("validate");

    let reclaimed = engine.collect_expired(DEFAULT_SHADOW_TTL_MS as i64);
    assert_eq!(reclaimed.len(), 1);
    assert!(reclaimed[0].was_validated);
}

#[test]
fn garbage_collection_never_touches_a_branch_it_did_not_create() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let feature = engine
        .create_branch("feature", BranchId::MAIN, agent(), 0)
        .expect("branch");
    open_shadow(&mut engine, 0);

    engine.collect_expired(DEFAULT_SHADOW_TTL_MS as i64 * 100);

    assert!(
        engine.branches().get(feature).is_some(),
        "collected a feature branch"
    );
    assert!(
        engine.branches().get(BranchId::MAIN).is_some(),
        "collected main"
    );
}

#[test]
fn reclaiming_a_shadow_branch_drops_a_pointer_and_not_history() {
    // Branches are ephemeral. The log is not — nothing in this flow may destroy
    // history (`03-data-model-consistency.md` §2.1).
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let before = engine.commits_applied();
    open_shadow(&mut engine, 0);
    let with_shadow = engine.commits_applied();
    assert!(
        with_shadow > before,
        "the change should have been committed"
    );

    engine.collect_expired(DEFAULT_SHADOW_TTL_MS as i64);
    assert_eq!(
        engine.commits_applied(),
        with_shadow,
        "reclaiming a branch removed committed history"
    );
}

#[test]
fn many_abandoned_proposals_do_not_accumulate() {
    // The case this exists for: an agent proposing repeatedly and never
    // returning to promote or reject.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);

    for i in 0..25 {
        let change = SchemaChange::DropColumn {
            table: "users".into(),
            column: format!("email{i}"),
        };
        let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), i as i64);
        // Only changes that reach the shadow gate open a branch; the rest are
        // ordinary proposals.
        let _ =
            engine.open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), i as i64);
    }
    let opened = engine.open_shadows().len();
    assert_eq!(opened, 25, "the proposals should each have opened a branch");

    engine.collect_expired(DEFAULT_SHADOW_TTL_MS as i64 + 100);
    assert!(
        engine.open_shadows().is_empty(),
        "{opened} branches were left behind"
    );

    let shadows_left = engine
        .branches()
        .iter()
        .filter(|b| matches!(b.kind, theta_core::branch::BranchKind::Shadow))
        .count();
    assert_eq!(shadows_left, 0);
}

#[test]
fn opening_a_shadow_branch_twice_for_one_change_does_not_leave_a_second_behind() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 200);
    let change = drop_email();
    let diff = engine.propose_schema_change(BranchId::MAIN, change.clone(), agent(), 0);

    let first = engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change.clone(), agent(), 1)
        .expect("first");
    let second = engine
        .open_shadow_branch(BranchId::MAIN, &diff.change_id, change, agent(), 2)
        .expect("retry");

    assert_eq!(first, second, "a retry opened a second branch");
    assert_eq!(engine.open_shadows().len(), 1);
}
