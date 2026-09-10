//! Reading a branch as it was, and following it as it changes.
//!
//! `theta-storage` proves the fold over a list of entries. What it cannot see is
//! which list, in which order, and with what horizon — and the horizon is the
//! part that decides whether a caller who asks for a point that no longer exists
//! is refused or quietly answered about a different one.

use std::collections::BTreeMap;

use theta_core::{Author, BranchId, Value};
use theta_storage::temporal::{AsOf, Cursor, Feed, RowChange};
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("temporal");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn put(engine: &mut Engine, key: &str, n: i64, at: i64) {
    engine
        .put(
            BranchId::MAIN,
            key,
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(n))])),
            agent("writer"),
            at,
        )
        .expect("put");
}

#[test]
fn a_branch_reads_as_it_was_rather_than_as_it_is() {
    let (mut engine, _dir) = engine();
    put(&mut engine, "orders:1", 1, 1_000);
    let early = engine
        .read_as_of(BranchId::MAIN, AsOf::Now)
        .expect("snapshot");

    put(&mut engine, "orders:1", 999, 2_000);
    put(&mut engine, "orders:2", 2, 2_001);

    let then = engine
        .read_as_of(BranchId::MAIN, AsOf::Commit(early.commit))
        .expect("historical snapshot");

    assert_eq!(
        then.view.get("orders:2"),
        None,
        "a row written after the snapshot appeared in it"
    );
    let now = engine
        .read_as_of(BranchId::MAIN, AsOf::Now)
        .expect("snapshot");
    assert!(
        now.view.get("orders:2").is_some(),
        "the present is missing a row that was written"
    );
}

#[test]
fn a_timestamp_query_says_which_commit_it_actually_resolved_to() {
    // Timestamps are advisory. A caller who asked by time and assumed they got
    // their exact instant is drawing conclusions about a different one, so the
    // resolved commit comes back with the answer and not on request.
    let (mut engine, _dir) = engine();
    put(&mut engine, "k", 1, 1_000);
    put(&mut engine, "k", 2, 5_000);
    put(&mut engine, "k", 3, 9_000);

    // A time no entry lands on exactly.
    let snapshot = engine
        .read_as_of(BranchId::MAIN, AsOf::Timestamp(7_000))
        .expect("snapshot");

    let at_commit = engine
        .read_as_of(BranchId::MAIN, AsOf::Commit(snapshot.commit))
        .expect("snapshot");
    assert_eq!(
        snapshot.view.get("k"),
        at_commit.view.get("k"),
        "the reported commit does not name the state that was returned"
    );
}

#[test]
fn a_point_past_the_end_reports_the_commit_it_actually_reached() {
    // A snapshot names the last commit it *included*, never the one that was
    // asked for. A caller who overshot — polling ahead, or holding a cursor from
    // another branch — gets the present state and can see that is what they got.
    // Echoing the asked-for number back would make the two indistinguishable.
    let (mut engine, _dir) = engine();
    for i in 0..5 {
        put(&mut engine, &format!("k{i}"), i, 1_000 + i);
    }
    let head = engine
        .read_as_of(BranchId::MAIN, AsOf::Now)
        .expect("snapshot")
        .commit;

    let overshot = engine
        .read_as_of(BranchId::MAIN, AsOf::Commit(head + 10_000))
        .expect("a point past the end is the present, not an error");

    assert_eq!(
        overshot.commit, head,
        "the snapshot echoed back a commit it never reached"
    );
}

// NOTE — the retention horizon.
//
// `Engine::horizon_of` derives the horizon from the log's earliest entry rather
// than passing `None`. There is deliberately no test that a point *below* it is
// refused, and the reason is that the case cannot be built today:
// `Engine::chain` walks a branch back to genesis, so the earliest commit is
// always 0 and the horizon is always `Some(0)` — behaviourally identical to
// `None`. A test was written for it and passed with the derivation replaced by
// `None`, which is what an untestable guard looks like.
//
// The derivation stays because `None` is a claim — "nothing has expired" — that
// is true only because nothing expires anything yet. On the day retention lands,
// `None` would answer a caller who asked for an expired point from the earliest
// state still available, handing them a different instant with no way to tell.
// The derived form refuses instead, and does so without anyone remembering to
// come back to this line. `theta-storage`'s own tests cover the refusal, over a
// horizon they can set.
#[test]
fn a_time_before_the_branch_began_is_an_error_and_not_an_empty_state() {
    // An empty snapshot is a plausible-looking answer to an impossible question,
    // and a caller diffing against it would see every row as an insert.
    let (mut engine, _dir) = engine();
    put(&mut engine, "k", 1, 50_000);

    let result = engine.read_as_of(BranchId::MAIN, AsOf::Timestamp(1));
    assert!(
        matches!(result, Err(EngineError::Temporal(_))),
        "a time before the branch existed returned a state instead of an error"
    );
}

#[test]
fn a_diff_between_two_points_names_what_changed() {
    let (mut engine, _dir) = engine();
    put(&mut engine, "orders:1", 1, 1_000);
    put(&mut engine, "orders:2", 2, 1_001);
    let before = engine
        .read_as_of(BranchId::MAIN, AsOf::Now)
        .expect("snapshot")
        .commit;

    put(&mut engine, "orders:2", 22, 2_000);
    put(&mut engine, "orders:3", 3, 2_001);
    engine
        .delete(BranchId::MAIN, "orders:1", agent("writer"), 2_002)
        .expect("delete");

    let changes = engine
        .diff_between(BranchId::MAIN, AsOf::Commit(before), AsOf::Now)
        .expect("diff");

    let keys: Vec<&str> = changes.iter().map(|c| c.key()).collect();
    assert!(keys.contains(&"orders:3"), "an inserted row is missing");
    assert!(keys.contains(&"orders:2"), "an updated row is missing");
    assert!(keys.contains(&"orders:1"), "a deleted row is missing");
    // A removal must carry the value that is gone. Reported as a bare key, the
    // value is unrecoverable from the diff — which is exactly when somebody
    // needs it.
    let removed = changes
        .iter()
        .find(|c| c.key() == "orders:1")
        .expect("the deleted row is missing");
    assert!(
        matches!(removed, RowChange::Removed { from, .. } if *from != Value::Null),
        "the deleted row came back without the value it lost: {removed:?}"
    );

    let changed = changes
        .iter()
        .find(|c| c.key() == "orders:2")
        .expect("the updated row is missing");
    assert!(
        matches!(changed, RowChange::Changed { from, to, .. } if from != to),
        "an update was reported without what it stopped saying: {changed:?}"
    );
}

#[test]
fn a_diff_from_a_point_to_itself_is_empty() {
    // Otherwise every consumer that polls faster than writes arrive sees churn
    // that did not happen.
    let (mut engine, _dir) = engine();
    put(&mut engine, "k", 1, 1_000);
    let at = engine
        .read_as_of(BranchId::MAIN, AsOf::Now)
        .expect("snapshot")
        .commit;

    let changes = engine
        .diff_between(BranchId::MAIN, AsOf::Commit(at), AsOf::Commit(at))
        .expect("diff");
    assert!(
        changes.is_empty(),
        "a point differed from itself: {changes:?}"
    );
}

#[test]
fn a_feed_resumes_exactly_where_it_stopped() {
    // A cursor that overlapped would replay changes a consumer already applied;
    // one that skipped would lose them. Neither is visible downstream.
    let (mut engine, _dir) = engine();
    for i in 0..10 {
        put(&mut engine, &format!("k{i}"), i, 1_000 + i);
    }

    let Feed::Delivered { changes, next } = engine
        .changes_since(BranchId::MAIN, Cursor::beginning(), 4)
        .expect("feed")
    else {
        panic!("a fresh branch reported a gap");
    };
    assert_eq!(changes.len(), 4);

    let Feed::Delivered {
        changes: rest,
        next: after,
    } = engine
        .changes_since(BranchId::MAIN, next, 100)
        .expect("feed")
    else {
        panic!("resuming reported a gap");
    };

    let first: Vec<&str> = changes.iter().map(|c| c.key()).collect();
    let second: Vec<&str> = rest.iter().map(|c| c.key()).collect();
    assert!(
        first.iter().all(|k| !second.contains(k)),
        "the feed delivered the same key twice across a resume"
    );
    assert!(after.after_commit >= next.after_commit);
}

#[test]
fn a_feed_that_has_delivered_everything_reports_no_changes_rather_than_a_gap() {
    // A gap means "you lost data". Saying it when a consumer is simply
    // up to date would send them to re-seed from a snapshot for nothing.
    let (mut engine, _dir) = engine();
    put(&mut engine, "k", 1, 1_000);

    let Feed::Delivered { next, .. } = engine
        .changes_since(BranchId::MAIN, Cursor::beginning(), 100)
        .expect("feed")
    else {
        panic!("a fresh branch reported a gap");
    };

    let caught_up = engine
        .changes_since(BranchId::MAIN, next, 100)
        .expect("feed");
    match caught_up {
        Feed::Delivered { changes, .. } => {
            assert!(
                changes.is_empty(),
                "a caught-up consumer was sent changes again"
            )
        }
        Feed::Gap { .. } => panic!("a caught-up consumer was told it had lost data"),
    }
}

#[test]
fn the_feed_carries_row_changes_and_not_schema_bookkeeping() {
    // A CDC consumer wants rows. Delivering the entries that created a table as
    // though they were row changes would make every pipeline filter them out,
    // and the ones that forgot would be wrong.
    let (mut engine, _dir) = engine();
    put(&mut engine, "k", 1, 1_000);

    let Feed::Delivered { changes, .. } = engine
        .changes_since(BranchId::MAIN, Cursor::beginning(), 100)
        .expect("feed")
    else {
        panic!("gap");
    };
    assert!(
        changes.iter().all(|c| !c.key().is_empty()),
        "the feed delivered a change with no row behind it"
    );
}
