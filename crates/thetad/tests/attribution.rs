//! "Who has been in here", asked of a real engine with real branches.
//!
//! `theta-storage` proves the attribution functions over a list of entries. The
//! question this file exists for is which list — a project's history is not one
//! branch, and branches share their ancestors.

use std::collections::BTreeMap;

use theta_core::log::AgentProvenance;
use theta_core::{Author, BranchId, ContentHash, Value};
use theta_storage::attribution::Attribution;
use thetad::engine::Engine;
use thetad::Config;

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("attributed");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

/// An agent that says what it is and what prompt it is acting on.
fn described(session: &str, agent: &str, prompt: &str) -> Author {
    Author::Agent {
        session_id: session.into(),
        user_id: "alice".into(),
        provenance: Some(AgentProvenance {
            agent: agent.to_string(),
            prompt_hash: ContentHash::of(prompt.as_bytes()),
            task_id: None,
        }),
    }
}

fn put(engine: &mut Engine, branch: BranchId, key: &str, author: Author, at: i64) {
    engine
        .put(
            branch,
            key,
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(1))])),
            author,
            at,
        )
        .expect("put");
}

#[test]
fn an_ancestor_entry_is_counted_once_however_many_branches_descend_from_it() {
    // The defect this test exists for. Branches share history up to their fork
    // point, so walking each branch and concatenating counts every shared
    // ancestor once per descendant — reporting a session that wrote three
    // entries before anyone branched as having written nine.
    //
    // An attribution tool that inflates counts is worse than one that is
    // missing, because the number looks like evidence.
    let (mut engine, _dir) = engine();

    for i in 0..3 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("shared{i}"),
            Author::agent("early", "alice"),
            1_000 + i,
        );
    }

    let a = engine
        .create_branch("a", BranchId::MAIN, Author::System, 2_000)
        .expect("branch a");
    let b = engine
        .create_branch("b", BranchId::MAIN, Author::System, 2_001)
        .expect("branch b");
    put(
        &mut engine,
        a,
        "on_a",
        Author::agent("early", "alice"),
        3_000,
    );
    put(
        &mut engine,
        b,
        "on_b",
        Author::agent("early", "alice"),
        3_001,
    );

    let activity = engine.activity(&Attribution::Session("early"));

    // Three shared puts plus one on each branch. Branch creation may add its own
    // entries, so the put count is what is asserted rather than the total.
    assert_eq!(
        activity.by_op.get("put").copied().unwrap_or(0),
        5,
        "shared ancestors were counted once per descendant branch: {:?}",
        activity.by_op
    );
}

#[test]
fn work_done_on_a_branch_still_shows_up_in_who_has_been_in_here() {
    // Attribution scoped to main would answer "who touched main", which is a
    // different question and the wrong one for an investigation. An agent that
    // did everything on a branch has still been in here.
    let (mut engine, _dir) = engine();
    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 1_000)
        .expect("branch");
    put(
        &mut engine,
        side,
        "k",
        Author::agent("hidden", "alice"),
        2_000,
    );

    assert!(
        engine.sessions().iter().any(|s| s == "hidden"),
        "a session that worked only on a branch was invisible: {:?}",
        engine.sessions()
    );
}

#[test]
fn a_breakdown_by_operation_rather_than_a_total() {
    // "Wrote 4,000 entries" and "made four schema changes" answer different
    // questions, and the first hides the second.
    let (mut engine, _dir) = engine();
    for i in 0..6 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("k{i}"),
            Author::agent("mixed", "alice"),
            1_000 + i,
        );
    }
    engine
        .delete(BranchId::MAIN, "k0", Author::agent("mixed", "alice"), 2_000)
        .expect("delete");

    let activity = engine.activity(&Attribution::Session("mixed"));
    assert_eq!(activity.by_op.get("put").copied(), Some(6));
    assert_eq!(activity.by_op.get("delete").copied(), Some(1));
    assert_eq!(activity.entries, 7);
}

#[test]
fn one_prompt_across_many_writes_is_visibly_different_from_many_prompts() {
    // A loop and a session thinking about each write look identical by entry
    // count and completely different by distinct prompt.
    let (mut engine, _dir) = engine();

    for i in 0..10 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("loop{i}"),
            described("looper", "batch-writer", "the same prompt every time"),
            1_000 + i,
        );
    }
    for i in 0..10 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("think{i}"),
            described("thinker", "assistant", &format!("prompt number {i}")),
            2_000 + i,
        );
    }

    let looper = engine.activity(&Attribution::Session("looper"));
    let thinker = engine.activity(&Attribution::Session("thinker"));

    assert_eq!(looper.entries, thinker.entries, "the fixture is unbalanced");
    assert_eq!(looper.distinct_prompts, 1);
    assert_eq!(thinker.distinct_prompts, 10);
}

#[test]
fn a_session_that_changed_its_story_shows_more_than_one_agent_name() {
    // The agent name is self-reported. Its value here is not that it is true but
    // that a session claiming to be two different things is visible.
    let (mut engine, _dir) = engine();
    put(
        &mut engine,
        BranchId::MAIN,
        "a",
        described("shifty", "migration-bot", "p"),
        1_000,
    );
    put(
        &mut engine,
        BranchId::MAIN,
        "b",
        described("shifty", "analytics-reader", "p"),
        1_001,
    );

    let activity = engine.activity(&Attribution::Session("shifty"));
    assert_eq!(
        activity.agents.len(),
        2,
        "a session that changed its story looked consistent: {:?}",
        activity.agents
    );
}

#[test]
fn attributing_by_agent_name_finds_every_session_that_claimed_it() {
    // Deliberately a filter across sessions, not a session lookup: two sessions
    // claiming the same agent name is exactly what somebody investigating an
    // impersonation needs to see.
    let (mut engine, _dir) = engine();
    put(
        &mut engine,
        BranchId::MAIN,
        "a",
        described("s1", "migration-bot", "p"),
        1_000,
    );
    put(
        &mut engine,
        BranchId::MAIN,
        "b",
        described("s2", "migration-bot", "p"),
        1_001,
    );

    let activity = engine.activity(&Attribution::Agent("migration-bot"));
    assert_eq!(
        activity.entries, 2,
        "two sessions claimed the same agent and only one was found"
    );
}

#[test]
fn asking_about_a_session_that_never_wrote_returns_nothing_rather_than_everything() {
    // A predicate that fell through to "match all" would make every
    // investigation return the whole log, which reads as a finding.
    let (mut engine, _dir) = engine();
    put(
        &mut engine,
        BranchId::MAIN,
        "k",
        Author::agent("real", "alice"),
        1_000,
    );

    let activity = engine.activity(&Attribution::Session("never-existed"));
    assert_eq!(activity.entries, 0);
    assert!(activity.by_op.is_empty());
    assert!(activity.first_commit.is_none());
}

#[test]
fn the_entries_themselves_are_available_and_in_the_order_they_happened() {
    // A summary is where an investigation starts; the entries are where it goes.
    let (mut engine, _dir) = engine();
    for i in 0..5 {
        put(
            &mut engine,
            BranchId::MAIN,
            &format!("k{i}"),
            Author::agent("reader", "alice"),
            1_000 + i,
        );
    }

    let entries = engine.entries_for(&Attribution::Session("reader"));
    assert_eq!(entries.len(), 5);
    let commits: Vec<u64> = entries.iter().map(|e| e.commit_id.0).collect();
    let mut sorted = commits.clone();
    sorted.sort_unstable();
    assert_eq!(commits, sorted, "entries came back out of order");
}

#[test]
fn entries_from_different_branches_at_the_same_height_come_back_in_a_stable_order() {
    // Commit ids are per-branch, so two branches hold a commit 4. Sorting by
    // commit id alone leaves those tied, and a stable sort then preserves
    // whatever order the caller happened to build — which made the ordering a
    // property of how branches were walked rather than a decision.
    //
    // Found by planting: reversing the assembled list changed nothing any test
    // could see, because the ascending-by-commit assertion still held. A
    // reviewer comparing two runs of the same investigation would have seen a
    // diff with no cause.
    let (mut engine, _dir) = engine();
    let a = engine
        .create_branch("a", BranchId::MAIN, Author::System, 1_000)
        .expect("branch a");
    let b = engine
        .create_branch("b", BranchId::MAIN, Author::System, 1_001)
        .expect("branch b");

    // Interleaved, so insertion order is not branch order.
    for i in 0..4 {
        put(
            &mut engine,
            b,
            &format!("b{i}"),
            Author::agent("twin", "alice"),
            2_000 + i,
        );
        put(
            &mut engine,
            a,
            &format!("a{i}"),
            Author::agent("twin", "alice"),
            2_100 + i,
        );
    }

    let first = engine.entries_for(&Attribution::Session("twin"));
    let again = engine.entries_for(&Attribution::Session("twin"));
    assert_eq!(
        first.iter().map(|e| e.hash()).collect::<Vec<_>>(),
        again.iter().map(|e| e.hash()).collect::<Vec<_>>(),
        "the same question answered twice gave two different orders"
    );

    // Within one commit height, lower branch id first — a decision, not an
    // accident of traversal.
    let ordering: Vec<(u64, u64)> = first
        .iter()
        .map(|e| (e.commit_id.0, e.branch_id.0))
        .collect();
    let mut expected = ordering.clone();
    expected.sort_unstable();
    assert_eq!(
        ordering, expected,
        "entries at equal commit height were not ordered by branch"
    );
}
