//! Merge queues, through the engine.
//!
//! The module proves speculation over a store it is handed. What only the engine
//! can answer is whether the speculation is against the right thing: the target
//! plus everything already queued, and not the target as it stands.

use std::collections::BTreeMap;

use theta_core::{Author, BranchId, Value};
use theta_storage::merge::MergeOutcome;
use theta_storage::mergequeue::EnqueueError;
use thetad::engine::Engine;
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("queued");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn put(engine: &mut Engine, branch: BranchId, key: &str, n: i64, at: i64) {
    engine
        .put(
            branch,
            key,
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(n))])),
            agent("writer"),
            at,
        )
        .expect("put");
}

fn branch(engine: &mut Engine, name: &str, at: i64) -> BranchId {
    engine
        .create_branch(name, BranchId::MAIN, Author::System, at)
        .expect("branch")
}

#[test]
fn a_merge_that_will_land_joins_the_queue() {
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    put(&mut engine, a, "from_a", 1, 3_000);

    let ticket = engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("a clean merge was refused");
    assert_eq!(engine.merge_queue(BranchId::MAIN).len(), 1);
    assert_eq!(engine.merge_queue(BranchId::MAIN)[0].ticket, ticket);
}

#[test]
fn the_second_merge_is_speculated_against_the_first_and_not_against_the_target() {
    // The property the whole module exists for. Two branches both writing `k`
    // do not conflict with `main` — they conflict with *each other*. A queue
    // that checked each against the target as it stands would accept both and
    // then fail the second at the front of the queue, hours later.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    let b = branch(&mut engine, "b", 2_001);

    put(&mut engine, a, "contested", 1, 3_000);
    put(&mut engine, b, "contested", 2, 3_001);

    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("the first merge is clean against main");

    let second = engine.enqueue_merge(b, BranchId::MAIN, 4_001);
    assert!(
        matches!(second, Err(EnqueueError::WouldConflict { .. })),
        "the second merge was queued despite conflicting with the first: {second:?}"
    );
    assert_eq!(
        engine.merge_queue(BranchId::MAIN).len(),
        1,
        "a conflicting merge was queued anyway"
    );
}

#[test]
fn a_conflicting_merge_is_told_which_branch_is_ahead_of_it() {
    // "You conflict" is not actionable. The agent's next question is always with
    // whom, and answering it is the difference between fixing the change and
    // filing a ticket.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    let b = branch(&mut engine, "b", 2_001);
    put(&mut engine, a, "contested", 1, 3_000);
    put(&mut engine, b, "contested", 2, 3_001);

    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("first");
    let Err(EnqueueError::WouldConflict { conflicts, behind }) =
        engine.enqueue_merge(b, BranchId::MAIN, 4_001)
    else {
        panic!("expected a conflict");
    };

    assert!(
        !conflicts.is_empty(),
        "a conflict was reported with no detail"
    );
    assert!(
        behind.is_some(),
        "the agent was told it conflicts and not with what"
    );
}

#[test]
fn the_same_branch_cannot_join_twice() {
    // Two tickets for one branch would land it twice, and the second landing
    // would be a merge of a branch that is already in.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    put(&mut engine, a, "from_a", 1, 3_000);

    let first = engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("first");
    match engine.enqueue_merge(a, BranchId::MAIN, 4_001) {
        Err(EnqueueError::AlreadyQueued { ticket }) => assert_eq!(ticket, first),
        other => panic!("a branch joined the queue twice: {other:?}"),
    }
}

#[test]
fn a_branch_with_nothing_to_merge_is_told_so_rather_than_queued() {
    // A no-op sitting in a queue makes the queue longer without making anything
    // land, and the agent watching it learns nothing.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);

    // Found by wiring: a freshly forked branch is not its parent's ancestor —
    // forking appends a `BranchCreate` entry — so every untouched branch used to
    // produce a `Merged` outcome carrying four empty collections, and this
    // error was unreachable.
    assert!(
        matches!(
            engine.enqueue_merge(a, BranchId::MAIN, 4_000),
            Err(EnqueueError::NothingToMerge)
        ),
        "a branch with no changes was queued"
    );
    assert!(engine.merge_queue(BranchId::MAIN).is_empty());
}

#[test]
fn withdrawing_a_merge_lets_what_it_blocked_back_in() {
    // Speculation is a prediction and predictions expire. A merge refused
    // because of one ahead of it must become acceptable once that one is gone —
    // otherwise a withdrawal permanently poisons the branch it displaced.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    let b = branch(&mut engine, "b", 2_001);
    put(&mut engine, a, "contested", 1, 3_000);
    put(&mut engine, b, "contested", 2, 3_001);

    let ticket = engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("first");
    assert!(engine.enqueue_merge(b, BranchId::MAIN, 4_001).is_err());

    engine
        .withdraw_merge(BranchId::MAIN, ticket)
        .expect("the ticket is in the queue");

    engine
        .enqueue_merge(b, BranchId::MAIN, 5_000)
        .expect("a merge blocked only by a withdrawn one was still refused");
}

#[test]
fn revalidation_evicts_a_merge_that_stopped_being_clean_rather_than_forcing_it() {
    // A merge validated against a future that did not happen has not been
    // validated at all. Landing it anyway would be precisely the failure the
    // queue exists to prevent.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    put(&mut engine, a, "contested", 1, 3_000);

    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("queued");

    // The target moves underneath the queued merge, in a way that conflicts.
    put(&mut engine, BranchId::MAIN, "contested", 99, 5_000);

    let evicted = engine
        .revalidate_merges(BranchId::MAIN)
        .expect("revalidation");
    assert_eq!(
        evicted.len(),
        1,
        "a merge that stopped landing was kept in the queue"
    );
    assert_eq!(evicted[0].source, a);
    assert!(
        !evicted[0].reason.is_empty(),
        "a merge was evicted without saying why"
    );
    assert!(engine.merge_queue(BranchId::MAIN).is_empty());
}

#[test]
fn revalidation_keeps_a_merge_that_is_still_clean() {
    // Otherwise revalidation is a queue-clearing operation with a reassuring
    // name, and nothing would ever land.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    put(&mut engine, a, "from_a", 1, 3_000);
    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("queued");

    // The target moves, but not in a way that touches the queued merge.
    put(&mut engine, BranchId::MAIN, "unrelated", 7, 5_000);

    let evicted = engine
        .revalidate_merges(BranchId::MAIN)
        .expect("revalidation");
    assert!(
        evicted.is_empty(),
        "a merge that still lands was evicted: {evicted:?}"
    );
    assert_eq!(engine.merge_queue(BranchId::MAIN).len(), 1);
}

#[test]
fn landing_the_head_actually_merges_and_removes_it_from_the_queue() {
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let a = branch(&mut engine, "a", 2_000);
    put(&mut engine, a, "from_a", 1, 3_000);
    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("queued");

    let outcome = engine
        .land_next_merge(BranchId::MAIN, agent("lander"), 5_000)
        .expect("landing")
        .expect("something was queued");

    assert!(
        matches!(outcome, MergeOutcome::Merged { .. }),
        "the head of the queue did not land: {outcome:?}"
    );
    assert!(engine.merge_queue(BranchId::MAIN).is_empty());
    assert!(
        engine.get(BranchId::MAIN, "from_a").is_some(),
        "the merge landed nothing"
    );
}

#[test]
fn landing_from_an_empty_queue_is_nothing_rather_than_an_error() {
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);

    assert!(engine
        .land_next_merge(BranchId::MAIN, agent("lander"), 5_000)
        .expect("no error")
        .is_none());
}

#[test]
fn two_targets_have_two_queues_and_do_not_block_each_other() {
    // One global queue would make an agent merging into `staging` wait behind
    // one merging into `main`, which are unrelated events.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let staging = branch(&mut engine, "staging", 1_500);
    let a = branch(&mut engine, "a", 2_000);
    let b = branch(&mut engine, "b", 2_001);

    // Both write the same key, so they would conflict *with each other* if they
    // shared a queue.
    put(&mut engine, a, "contested", 1, 3_000);
    put(&mut engine, b, "contested", 2, 3_001);

    engine
        .enqueue_merge(a, BranchId::MAIN, 4_000)
        .expect("into main");
    engine
        .enqueue_merge(b, staging, 4_001)
        .expect("a merge into a different target was blocked by one into main");

    assert_eq!(engine.merge_queue(BranchId::MAIN).len(), 1);
    assert_eq!(engine.merge_queue(staging).len(), 1);
}

#[test]
fn a_merge_is_speculated_against_its_own_target_and_not_against_main() {
    // Found by planting. The queues are keyed by target, so a queue built with
    // the wrong target still gets *found* correctly — and then speculates its
    // merges against a branch nobody asked about. Every test that only checked
    // which queue a ticket landed in passed.
    //
    // Here the two targets genuinely disagree: `staging` holds a value that
    // conflicts with the source, and `main` does not. A merge into `staging`
    // must conflict. Validated against `main`, it sails through — and would then
    // land into a branch it was never checked against, which is the failure the
    // whole module exists to prevent.
    let (mut engine, _dir) = engine();
    put(&mut engine, BranchId::MAIN, "base", 0, 1_000);
    let staging = branch(&mut engine, "staging", 1_500);
    let a = branch(&mut engine, "a", 2_000);

    put(&mut engine, staging, "contested", 1, 3_000);
    put(&mut engine, a, "contested", 2, 3_001);

    let result = engine.enqueue_merge(a, staging, 4_000);
    assert!(
        matches!(result, Err(EnqueueError::WouldConflict { .. })),
        "a merge into `staging` was validated against some other branch: {result:?}"
    );
}
