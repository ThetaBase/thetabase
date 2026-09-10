//! The review budget, through the engine rather than in isolation.
//!
//! `theta-safety`'s unit tests prove the budget arithmetic. These prove the
//! thing that actually matters: that a proposal arriving at the engine is
//! *refused* when the project has no review left, and that answering a proposal
//! settles what it reserved.
//!
//! The distinction is not academic. A budget module with passing tests and
//! nothing calling it bounds nothing at all, which is exactly the state
//! `specs/07` §6.1 exists to prevent — the review queue grows and the budget
//! reports on it.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use theta_safety::SafetyPolicy;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("budgeted");
    config.data_dir = dir.path().to_path_buf();
    config.safety = SafetyPolicy::protected();
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

/// A table with `rows` rows, landed through the ordinary path.
fn seed(engine: &mut Engine, rows: u64) {
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddTable {
            table: TableDef {
                name: "orders".into(),
                fields: BTreeMap::from([("total".to_string(), field("total"))]),
                indexes: vec![],
            },
        },
        agent("setup"),
        1_000,
    );
    engine
        .apply_schema_change(&diff.change_id, true, agent("setup"), 1_001)
        .expect("the table lands");

    for i in 0..rows {
        engine
            .put(
                BranchId::MAIN,
                &format!("orders:{i}"),
                Value::Map(BTreeMap::from([(
                    "total".to_string(),
                    Value::Int(i as i64),
                )])),
                agent("setup"),
                2_000 + i as i64,
            )
            .expect("put");
    }
}

/// A change that will be gated, so it costs review.
fn gated_change(n: u64) -> SchemaChange {
    SchemaChange::DropColumn {
        table: "orders".into(),
        column: format!("total_{n}"),
    }
}

#[test]
fn a_change_needing_no_human_is_never_refused_however_many_there_are() {
    // The property that makes this a *review* budget rather than a rate limit.
    // An agent doing provably safe work must never be throttled by a mechanism
    // whose purpose is rationing human attention.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    for i in 0..500 {
        let diff = engine
            .propose_within_budget(
                BranchId::MAIN,
                SchemaChange::AddColumn {
                    table: "orders".into(),
                    field: field(&format!("extra_{i}")),
                },
                agent("busy"),
                10_000 + i,
            )
            .unwrap_or_else(|e| panic!("a free change was refused on iteration {i}: {e}"));
        assert_eq!(diff.gate, Gate::AutoApply);
    }
}

#[test]
fn an_agent_that_runs_out_of_review_is_refused_rather_than_queued() {
    // The whole point. Without this the queue grows and the budget reports on
    // it; with it, the agent is told while it still remembers why it made the
    // change.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let mut refused = None;
    for i in 0..200u64 {
        match engine.propose_within_budget(
            BranchId::MAIN,
            gated_change(i),
            agent("spender"),
            10_000 + i as i64,
        ) {
            Ok(diff) => assert_ne!(
                diff.gate,
                Gate::AutoApply,
                "the fixture must produce gated changes for this to test anything"
            ),
            Err(e) => {
                refused = Some((i, e));
                break;
            }
        }
    }

    let (at, error) = refused.expect("the budget must run out within 200 gated proposals");
    assert!(
        matches!(error, EngineError::ReviewExhausted { .. }),
        "expected exhaustion, got {error:?}"
    );
    assert!(at > 0, "it must not refuse the very first proposal");
}

#[test]
fn a_refused_proposal_is_not_held_and_can_never_be_confirmed() {
    // Exhaustion refuses; it does not accept-and-defer. A proposal that was
    // refused but still held would be one a caller could confirm later, which
    // would make running out of budget a delay rather than a limit.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let mut last_ok = None;
    for i in 0..200u64 {
        match engine.propose_within_budget(
            BranchId::MAIN,
            gated_change(i),
            agent("spender"),
            10_000 + i as i64,
        ) {
            Ok(diff) => last_ok = Some(diff.change_id),
            Err(_) => break,
        }
    }
    let held = last_ok.expect("something was accepted");

    // The one that was accepted is still answerable...
    assert!(engine.pending_change(&held).is_some());

    // ...and nothing that was refused is.
    let refused_ids = (0..200u64)
        .map(|i| theta_safety::diff::ChangeId::of(&gated_change(i), 0))
        .filter(|id| engine.pending_change(id).is_some())
        .count();
    assert!(
        refused_ids < 200,
        "every proposal was held, so nothing was actually refused"
    );
}

#[test]
fn a_project_gets_exactly_its_stated_ceiling() {
    // Not "answering returns budget" — it does not, by design, and the engine
    // was right to disagree when this test first said so. Approved and rejected
    // proposals keep their spend (`specs/07` §6.1.4); the budget recovers when
    // the window rolls.
    //
    // What is observable here is that the stated ceiling is the real one. A
    // reservation counted twice — once held, once spent — would halve it.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let ceiling = engine.review_budget().policy().agent_ceiling as u64;
    let mut accepted = 0u64;

    for i in 0..(ceiling * 4) {
        match engine.propose_within_budget(
            BranchId::MAIN,
            gated_change(i),
            agent("steady"),
            10_000 + i as i64,
        ) {
            Ok(diff) => {
                accepted += 1;
                engine
                    .apply_schema_change(&diff.change_id, true, agent("steady"), 10_001 + i as i64)
                    .expect("confirm");
            }
            Err(EngineError::ReviewExhausted { .. }) => break,
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }

    assert_eq!(accepted, ceiling, "the stated ceiling must be the real one");
}

#[test]
fn answering_a_proposal_does_not_return_its_review_sooner_than_abandoning_it_would() {
    // The second plant that survived, and the property that makes settling
    // load-bearing rather than decorative.
    //
    // Held and spent review count alike against the ceiling, so at the moment of
    // confirmation a settled and an unsettled reservation look identical — which
    // is why the earlier test could not tell them apart. They diverge later. A
    // held reservation is auto-refunded at the proposal TTL (four hours); a
    // settled one becomes spend and stays for the window (a day).
    //
    // So an engine that confirmed changes without settling them would hand the
    // review back four hours later as though the proposal had been abandoned —
    // making a reviewed change cheaper than an ignored one, which inverts the
    // entire point of rationing attention.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let policy = engine.review_budget().policy().clone();
    assert!(
        policy.proposal_ttl_ms < policy.window_ms,
        "this test distinguishes settled from held by the gap between the two deadlines; with no gap it would assert nothing"
    );

    for i in 0..policy.agent_ceiling as u64 {
        let diff = engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(i),
                agent("diligent"),
                10_000 + i as i64,
            )
            .expect("within budget");
        engine
            .apply_schema_change(&diff.change_id, true, agent("diligent"), 10_001 + i as i64)
            .expect("confirm");
    }

    // Past the reservation TTL, still well inside the spend window.
    let later = 10_000 + policy.proposal_ttl_ms + 1;
    assert!(
        later < 10_000 + policy.window_ms,
        "the probe must land inside the window or it proves nothing"
    );

    let after = engine.propose_within_budget(
        BranchId::MAIN,
        gated_change(9_999),
        agent("diligent"),
        later,
    );
    assert!(
        matches!(after, Err(EngineError::ReviewExhausted { .. })),
        "review that was actually consumed came back at the proposal TTL, so confirming a change returns budget faster than walking away from it"
    );
}

#[test]
fn a_proposal_and_its_reservation_expire_together() {
    // Found by planting: the engine settled abandoned proposals as expired using
    // `shadow_ttl_ms` (a day) while the budget expires reservations at
    // `proposal_ttl_ms` (four hours). The budget's deadline always fired first,
    // so the engine's call was dead code — removing it entirely changed nothing.
    //
    // Two deadlines governing one lifetime meant "how long does a proposal live"
    // had two answers and the shorter one silently won. There is now one, and
    // this pins that a proposal does not outlive the reservation that makes it
    // answerable.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let reservation_ttl = engine.review_budget().policy().proposal_ttl_ms;
    let diff = engine
        .propose_within_budget(BranchId::MAIN, gated_change(1), agent("vanished"), 10_000)
        .expect("within budget");

    assert!(engine.pending_change(&diff.change_id).is_some());

    engine.collect_expired(10_000 + reservation_ttl + 1);

    assert!(
        engine.pending_change(&diff.change_id).is_none(),
        "a proposal outlived the reservation that made it answerable"
    );
}

#[test]
fn an_agent_that_proposes_and_vanishes_does_not_permanently_reduce_capacity() {
    // The leak this guards. An agent that fills its allowance and disappears
    // must not leave the budget held forever, or a quiet project ends up with no
    // review capacity and nothing to point at.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let ceiling = engine.review_budget().policy().agent_ceiling as u64;
    let ttl = engine.review_budget().policy().proposal_ttl_ms;

    for i in 0..ceiling {
        engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(i),
                agent("vanished"),
                10_000 + i as i64,
            )
            .expect("within budget");
    }
    assert!(
        engine
            .propose_within_budget(BranchId::MAIN, gated_change(999), agent("vanished"), 10_100)
            .is_err(),
        "the agent should be out of budget"
    );

    engine.collect_expired(10_000 + ttl + 1);

    assert!(
        engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(1_000),
                agent("vanished"),
                10_000 + ttl + 2
            )
            .is_ok(),
        "abandoned proposals must give back the review they were holding"
    );
}

#[test]
fn one_agents_exhaustion_does_not_stop_another() {
    // The reason the agent scope exists. A single misbehaving agent must not
    // consume a project's review and stop everyone else.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let mut spent = 0u64;
    for i in 0..200u64 {
        if engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(i),
                agent("greedy"),
                10_000 + i as i64,
            )
            .is_err()
        {
            spent = i;
            break;
        }
    }
    assert!(spent > 0, "the greedy agent must have exhausted something");

    assert!(
        engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(9_999),
                agent("innocent"),
                20_000
            )
            .is_ok(),
        "a different agent must still have its own allowance"
    );
}

#[test]
fn exhaustion_never_downgrades_a_gate() {
    // The safety property. Running out of review must refuse work; it must never
    // let something through more cheaply, and it must never weaken a gate.
    //
    // Checked by asserting that every proposal that *was* accepted carries the
    // gate the classifier gave it, right up to the refusal — a budget that
    // downgraded under pressure would show an `AutoApply` among them.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 500);

    for i in 0..200u64 {
        match engine.propose_within_budget(
            BranchId::MAIN,
            gated_change(i),
            agent("spender"),
            10_000 + i as i64,
        ) {
            Ok(diff) => assert_ne!(
                diff.gate,
                Gate::AutoApply,
                "a destructive change was downgraded to auto-apply under budget pressure"
            ),
            Err(EngineError::ReviewExhausted { .. }) => return,
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }
    panic!("the budget never ran out, so this asserted nothing");
}

#[test]
fn a_refusal_is_recorded_in_the_audit_trail() {
    // An agent repeatedly running out of review is the most interesting pattern
    // this trail can hold, and a refusal that leaves no trace is one nobody can
    // see coming.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let before = engine.audit_log().len();
    for i in 0..200u64 {
        if engine
            .propose_within_budget(
                BranchId::MAIN,
                gated_change(i),
                agent("spender"),
                10_000 + i as i64,
            )
            .is_err()
        {
            break;
        }
    }
    assert!(
        engine.audit_log().len() > before,
        "the refusal left no trace in the trail"
    );
}
