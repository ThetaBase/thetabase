//! Chain verification and anchoring, driven through the engine.
//!
//! `theta-storage` proves the verifier and the anchor log in isolation. What
//! those tests cannot see is the seam: whether the engine hands the verifier the
//! log in the order it expects, whether an unconfigured deployment is told it is
//! unprotected, and whether anchoring happens at all. Every defect found while
//! integrating the review budget lived in exactly that region.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use theta_core::{Author, BranchId, Value};
use theta_storage::anchor::{Anchor, AnchorError, AnchorSink, InMemorySink};
use theta_storage::verifier::VerifyError;
use thetad::engine::Engine;
use thetad::Config;

/// A sink whose contents the test can still reach after the engine owns it.
///
/// The obvious version of the "sink lost the anchor" test called `forget` on a
/// sink the engine had never used, so the receipt was absent because it was
/// never there — the test passed without exercising anything. Sharing the
/// storage is what makes the counterparty in the test the same counterparty the
/// engine published to.
#[derive(Clone, Default)]
struct SharedSink {
    held: Arc<Mutex<BTreeMap<String, Anchor>>>,
}

impl AnchorSink for SharedSink {
    fn publish(&mut self, anchor: &Anchor) -> Result<String, AnchorError> {
        let receipt = format!("shared-{}", anchor.head.to_hex());
        self.held
            .lock()
            .expect("lock")
            .insert(receipt.clone(), anchor.clone());
        Ok(receipt)
    }

    fn recall(&self, receipt: &str) -> Result<Option<Anchor>, AnchorError> {
        Ok(self.held.lock().expect("lock").get(receipt).cloned())
    }

    fn describe(&self) -> String {
        "shared test sink".into()
    }
}

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("maintained");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

/// `rows` writes on main, through the ordinary path.
fn write(engine: &mut Engine, rows: u64) {
    for i in 0..rows {
        engine
            .put(
                BranchId::MAIN,
                &format!("k{i}"),
                Value::Map(BTreeMap::from([("n".to_string(), Value::Int(i as i64))])),
                agent("writer"),
                2_000 + i as i64,
            )
            .expect("put");
    }
}

#[test]
fn a_multi_entry_log_verifies_rather_than_reporting_its_own_order_as_tampering() {
    // The seam this test exists for. `history` walks backwards from the head, so
    // it arrives newest first; the verifier checks that each entry's predecessor
    // has already been seen. Handing it the store's order would fail on the
    // second entry of any log — and it would fail as `BrokenChain`, which is
    // indistinguishable from the tampering this is meant to detect.
    //
    // An integration bug that manufactures the exact alarm it was built to raise
    // is worse than one that stays quiet, so this is pinned before anything else.
    let (mut engine, _dir) = engine();
    write(&mut engine, 20);

    let report = engine.maintain(10_000);

    assert!(
        report.chain.is_none(),
        "an untampered log reported as broken: {:?}",
        report.chain
    );
    assert!(
        report.coverage.has_completed_a_pass(),
        "20 entries is under one tick, so a pass should have completed"
    );
}

#[test]
fn nothing_reads_as_verified_until_a_pass_has_completed() {
    // §7.3. A verifier reporting progress over a third of the log converts a
    // known unknown into a false certainty, so coverage is what it reports and
    // `has_completed_a_pass` is the only thing that means anything.
    let (mut engine, _dir) = engine();

    // A tick budget small enough that one pass takes several ticks.
    engine.set_verifier_entries_per_tick(4);
    write(&mut engine, 30);

    let first = engine.maintain(10_000);
    assert!(
        !first.coverage.has_completed_a_pass(),
        "a pass completed after one tick over 30 entries with a budget of 4"
    );
    assert!(
        first.coverage.progress_basis_points() > 0,
        "the tick covered nothing"
    );
    assert!(
        first.coverage.progress_basis_points() < 10_000,
        "one tick reported the whole log"
    );

    let mut ticks = 1;
    while !engine
        .maintain(10_000 + ticks)
        .coverage
        .has_completed_a_pass()
    {
        ticks += 1;
        assert!(ticks < 50, "the pass never completed");
    }
}

#[test]
fn entries_written_during_a_pass_belong_to_the_next_one() {
    // Otherwise a busy instance moves the target faster than the verifier moves
    // the cursor, coverage climbs reassuringly, and no pass ever finishes.
    let (mut engine, _dir) = engine();
    engine.set_verifier_entries_per_tick(4);
    write(&mut engine, 20);

    // Start a pass, then keep writing at the rate it verifies.
    let mut ticks = 0;
    loop {
        let report = engine.maintain(10_000 + ticks);
        if report.coverage.has_completed_a_pass() {
            break;
        }
        write(&mut engine, 4);
        ticks += 1;
        assert!(
            ticks < 40,
            "the verifier never finished a pass while writes kept arriving at \
             the rate it verifies — the target is following the log"
        );
    }
}

#[test]
fn a_deployment_with_nowhere_to_anchor_is_told_so_rather_than_told_it_is_fine() {
    // The default is no sink, and that must read as a finding. The tempting
    // default is an in-memory sink, which would let every branch report
    // "anchored" while proving nothing — an anchor held in the same process as
    // the log is our own record of our own action (§7.1).
    let (mut engine, _dir) = engine();
    write(&mut engine, 5);

    let report = engine.maintain(10_000);

    assert!(!report.is_clean(), "an unanchored log reported as clean");
    assert!(
        report
            .unanchored
            .iter()
            .any(|(b, e)| *b == BranchId::MAIN && matches!(e, AnchorError::NeverAnchored { .. })),
        "main was not reported unanchored: {:?}",
        report.unanchored
    );
    assert!(report.anchored.is_empty());
}

#[test]
fn a_configured_sink_gets_the_head_and_the_receipt_comes_back() {
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    write(&mut engine, 5);

    let report = engine.maintain(10_000);

    assert!(
        report.is_clean(),
        "a verified, anchored log was not clean: {report:?}"
    );
    let (branch, receipt) = report
        .anchored
        .iter()
        .find(|(b, _)| *b == BranchId::MAIN)
        .expect("main was not anchored");
    assert_eq!(*branch, BranchId::MAIN);
    assert!(!receipt.is_empty(), "the sink returned an empty receipt");

    let anchored_head = engine
        .anchor_log()
        .latest(BranchId::MAIN)
        .expect("an anchor")
        .head;
    assert_eq!(
        Some(anchored_head),
        engine.head(BranchId::MAIN),
        "the anchor covers something other than the branch head"
    );
}

#[test]
fn anchoring_does_not_happen_again_until_the_gap_has_elapsed() {
    // The gap is the window in which the tail is unprotected, and it is a
    // deliberate number. Anchoring on every maintenance pass would make the
    // interval meaningless and the cost unbounded.
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    write(&mut engine, 5);

    let gap = engine.anchor_policy().max_gap_ms;
    assert_eq!(engine.maintain(10_000).anchored.len(), 1);
    assert!(
        engine.maintain(10_000 + gap - 1).anchored.is_empty(),
        "anchored again before the gap elapsed"
    );

    write(&mut engine, 3);
    assert_eq!(
        engine.maintain(10_000 + gap).anchored.len(),
        1,
        "did not anchor once the gap had elapsed"
    );
}

#[test]
fn verification_reports_what_is_protected_and_therefore_what_is_not() {
    // `entries_protected` is the sentence this exists to make unavoidable:
    // everything written after the newest anchor is protected by nothing.
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    write(&mut engine, 5);
    engine.maintain(10_000);

    let before = engine
        .verify_anchors(BranchId::MAIN, 10_001)
        .expect("anchors verify");
    assert!(before.anchors_checked > 0);
    let protected = before.entries_protected;

    // Writes after the anchor are not covered by it, and verification must not
    // start claiming they are.
    write(&mut engine, 7);
    let after = engine
        .verify_anchors(BranchId::MAIN, 10_002)
        .expect("anchors still verify");
    assert_eq!(
        after.entries_protected, protected,
        "entries written after the last anchor were reported as protected by it"
    );
}

#[test]
fn an_anchor_the_sink_cannot_reproduce_is_a_failure() {
    // An anchor the destination cannot produce is our own record of our own
    // action. This is the whole difference between an anchor and a log line.
    let (mut engine, _dir) = engine();
    let sink = SharedSink::default();
    engine.set_anchor_sink(Box::new(sink.clone()));
    write(&mut engine, 5);

    let report = engine.maintain(10_000);
    let receipt = report.anchored[0].1.clone();

    // The anchor is genuinely there first, or the removal below proves nothing.
    assert!(
        sink.recall(&receipt).expect("recall").is_some(),
        "the sink never held the anchor, so forgetting it tests nothing"
    );
    assert!(
        engine.verify_anchors(BranchId::MAIN, 10_001).is_ok(),
        "verification must pass before the anchor is removed, or the failure below cannot be attributed to the removal"
    );

    // Now the counterparty loses it — or denies it.
    sink.held.lock().expect("lock").remove(&receipt);

    let err = engine
        .verify_anchors(BranchId::MAIN, 10_002)
        .expect_err("an anchor no sink can reproduce verified anyway");
    assert!(
        matches!(err, thetad::engine::EngineError::Anchor(_)),
        "unexpected error: {err}"
    );
}
#[test]
fn a_branch_that_has_never_been_anchored_fails_rather_than_passing_vacuously() {
    // Silence is a finding. A branch with no anchors has nothing to check, and
    // "nothing to check" must not render as "checked".
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    write(&mut engine, 3);

    // Verify before ever anchoring.
    let err = engine
        .verify_anchors(BranchId::MAIN, 10_000)
        .expect_err("a never-anchored branch verified");
    assert!(
        matches!(
            err,
            thetad::engine::EngineError::Anchor(AnchorError::NeverAnchored { .. })
        ),
        "a never-anchored branch failed for some other reason: {err}"
    );
}

#[test]
fn the_pace_check_is_asked_separately_from_the_tick() {
    // A tick that found no broken link says nothing about whether the pass will
    // ever finish, so the two are different questions with different answers.
    let (mut engine, _dir) = engine();
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    engine.set_verifier_entries_per_tick(1);
    write(&mut engine, 50);

    // One tick, then a clock far past the allowed pass time.
    engine.maintain(10_000);
    let late = engine.maintain(10_000 + 48 * 60 * 60 * 1_000);

    assert!(
        matches!(late.pace, Some(VerifyError::NotKeepingUp { .. })),
        "a verifier a day behind reported no pace problem: {:?}",
        late.pace
    );
    assert!(
        late.chain.is_none(),
        "falling behind was reported as a broken chain"
    );
    assert!(
        !late.is_clean(),
        "a verifier that is not keeping up is clean"
    );
}

#[test]
fn maintenance_returns_its_findings_rather_than_only_logging_them() {
    // Every field on `Maintenance` is something an operator may need to act on.
    // A periodic task that only writes to a log is one whose findings are
    // discovered during the incident they predicted — so this pins that the
    // caller can actually see them.
    let (mut engine, _dir) = engine();
    write(&mut engine, 2);

    let report = engine.maintain(10_000);
    assert!(!report.is_clean());
    assert!(
        !format!("{report:?}").is_empty(),
        "the report is not inspectable"
    );

    // And that `is_clean` is not merely always false.
    engine.set_anchor_sink(Box::new(InMemorySink::new()));
    assert!(
        engine.maintain(20_000).is_clean(),
        "is_clean never returns true, so asserting it is worthless"
    );
}

#[test]
fn the_sink_describes_itself_so_an_operator_can_judge_what_an_anchor_is_worth() {
    // A counterparty, a transparency log and a copy in this process are worth
    // very different things, and only the deployment knows which it configured.
    let sink = InMemorySink::new();
    assert!(
        !sink.describe().is_empty(),
        "a sink that will not say what it is cannot be judged"
    );
}
