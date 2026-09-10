//! Merge queues: the conflict found at enqueue, against the branch as it will be.
//!
//! The claim under test is M18's: **an agent that will conflict should be told
//! while it still remembers why it made the change.** Everything here is about
//! the difference between checking against the target as it is and as it will
//! be, and about what happens when a prediction stops being true.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange};
use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value, ValueType};
use theta_storage::mergequeue::{EnqueueError, MergeQueue};
use theta_storage::view::MaterializedView;
use theta_storage::{LogStore, MemLogStore};

const MAIN: BranchId = BranchId::MAIN;

/// Three agents forked from one head, which is the shape M18 is about.
struct Fleet {
    store: MemLogStore,
    views: BTreeMap<BranchId, MaterializedView>,
    heads: BTreeMap<BranchId, ContentHash>,
    base: Option<ContentHash>,
    commit: u64,
}

impl Fleet {
    fn new(agents: &[BranchId]) -> Self {
        let mut fleet = Self {
            store: MemLogStore::new(),
            views: BTreeMap::new(),
            heads: BTreeMap::new(),
            base: None,
            commit: 0,
        };
        fleet.views.insert(MAIN, MaterializedView::new());
        fleet.heads.insert(MAIN, ContentHash::ZERO);
        for agent in agents {
            fleet.views.insert(*agent, MaterializedView::new());
            fleet.heads.insert(*agent, ContentHash::ZERO);
        }
        fleet
    }

    fn write(&mut self, branch: BranchId, op: OpType) {
        let prev = self.heads[&branch];
        self.commit += 1;
        let entry = LogEntry {
            prev_hash: prev,
            commit_id: CommitId(self.commit),
            branch_id: branch,
            op,
            author: Author::agent(format!("sess_{}", branch.0), "alice"),
            timestamp_ms: self.commit as i64,
        };
        let hash = self.store.append(entry.clone()).expect("append");
        self.heads.insert(branch, hash);
        self.views.entry(branch).or_default().apply(&entry);
    }

    fn queue(&self) -> MergeQueue {
        MergeQueue::new(MAIN)
    }

    fn enqueue(
        &self,
        queue: &mut MergeQueue,
        source: BranchId,
    ) -> std::result::Result<u64, EnqueueError> {
        queue.enqueue(
            &self.store,
            source,
            self.base,
            &self.views[&MAIN],
            &self.views[&source],
            self.commit as i64,
        )
    }

    fn sources(&self) -> BTreeMap<BranchId, MaterializedView> {
        self.views
            .iter()
            .filter(|(branch, _)| **branch != MAIN)
            .map(|(branch, view)| (*branch, view.clone()))
            .collect()
    }
}

fn put(key: &str, value: i64) -> OpType {
    OpType::Put {
        key: key.into(),
        value: Value::Int(value),
    }
}

fn add_column(table: &str, column: &str) -> OpType {
    add_column_typed(table, column, ValueType::Text)
}

fn add_column_typed(table: &str, column: &str, ty: ValueType) -> OpType {
    OpType::Schema {
        change: SchemaChange::AddColumn {
            table: table.into(),
            field: FieldDef {
                name: column.into(),
                ty,
                nullable: true,
                crdt: None,
                declared_at: None,
            },
        },
    }
}

const A: BranchId = BranchId(1);
const B: BranchId = BranchId(2);
const C: BranchId = BranchId(3);

#[test]
fn independent_merges_all_queue_cleanly() {
    let mut fleet = Fleet::new(&[A, B, C]);
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:2", 2));
    fleet.write(C, put("orders:3", 3));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));
    assert_eq!(fleet.enqueue(&mut queue, B), Ok(2));
    assert_eq!(fleet.enqueue(&mut queue, C), Ok(3));
    assert_eq!(queue.len(), 3);
}

#[test]
fn a_conflict_with_a_queued_merge_is_found_at_enqueue_not_at_merge() {
    // The property M18 asks for. Without speculation, B would queue cleanly
    // against `main` as it is, reach the front, and only then discover that A
    // wrote the same key — by which point the agent has moved on.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:1", 2));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));

    match fleet.enqueue(&mut queue, B) {
        Err(EnqueueError::WouldConflict { conflicts, behind }) => {
            assert!(conflicts.iter().any(|c| c.key == "orders:1"));
            assert_eq!(
                behind,
                Some(1),
                "the caller needs the ticket it disagrees with, not just \\
                 'you conflict with the target'"
            );
        }
        other => panic!("expected a conflict against the queued merge, got {other:?}"),
    }
    assert_eq!(queue.len(), 1, "a refused merge must not join the queue");
}

#[test]
fn the_same_conflict_against_the_target_itself_reports_no_ticket() {
    // `behind: None` has to mean something specific: the disagreement is with
    // the branch rather than with anything queued, which sends the agent
    // somewhere different.
    let mut fleet = Fleet::new(&[A]);
    fleet.write(MAIN, put("orders:1", 1));
    fleet.write(A, put("orders:1", 2));

    let mut queue = fleet.queue();
    match fleet.enqueue(&mut queue, A) {
        Err(EnqueueError::WouldConflict { behind, .. }) => assert_eq!(behind, None),
        other => panic!("expected a conflict with the target, got {other:?}"),
    }
}

#[test]
fn two_agents_adding_different_columns_both_queue() {
    // `specs/03` §3.2 says conflicts go to a human; M18's fourth item is about
    // making this case not be one. Checked through the queue as well as through
    // `merge`, because the queue is what an agent actually meets.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, add_column("orders", "currency"));
    fleet.write(B, add_column("orders", "discount"));

    let mut queue = fleet.queue();
    assert!(fleet.enqueue(&mut queue, A).is_ok());
    assert!(
        fleet.enqueue(&mut queue, B).is_ok(),
        "two different columns are not one question"
    );
}

#[test]
fn two_agents_declaring_one_column_differently_do_not_both_queue() {
    // Same name, *different types*, which is the actual disagreement. Nothing
    // in the data says which is right and picking either would discard a
    // decision somebody made.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, add_column_typed("orders", "currency", ValueType::Text));
    fleet.write(B, add_column_typed("orders", "currency", ValueType::Int));

    let mut queue = fleet.queue();
    assert!(fleet.enqueue(&mut queue, A).is_ok());
    match fleet.enqueue(&mut queue, B) {
        Err(EnqueueError::WouldConflict { behind, .. }) => {
            assert_eq!(behind, Some(1), "the schema change it lost to is nameable");
        }
        other => panic!("one declaration, two answers: {other:?}"),
    }
}

#[test]
fn two_agents_adding_the_identical_column_both_queue() {
    // This test asserted a conflict at first and the code was right to disagree.
    // Two agents adding `orders.currency` as the same nullable text column have
    // not disagreed about anything — it is one change proposed twice, and
    // landing it once is the correct outcome rather than a merge somebody has to
    // adjudicate.
    //
    // Worth pinning explicitly, because the naive reading of "two agents touched
    // one declaration" is that it must be a conflict, and a future change to the
    // schema-merge rules could quietly make it one.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, add_column("orders", "currency"));
    fleet.write(B, add_column("orders", "currency"));

    let mut queue = fleet.queue();
    assert!(fleet.enqueue(&mut queue, A).is_ok());
    assert!(
        fleet.enqueue(&mut queue, B).is_ok(),
        "the same change proposed twice is not a disagreement"
    );
}

#[test]
fn queuing_the_same_branch_twice_is_refused_rather_than_deduplicated() {
    // A second enqueue usually means the caller believes the first did not
    // happen. Answering "yes, fine" to both leaves them believing something
    // untrue about their position.
    let mut fleet = Fleet::new(&[A]);
    fleet.write(A, put("orders:1", 1));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));
    assert_eq!(
        fleet.enqueue(&mut queue, A),
        Err(EnqueueError::AlreadyQueued { ticket: 1 })
    );
    assert_eq!(queue.len(), 1);
}

#[test]
fn a_branch_with_nothing_to_merge_is_told_so_rather_than_queued() {
    let fleet = Fleet::new(&[A]);
    let mut queue = fleet.queue();
    assert_eq!(
        fleet.enqueue(&mut queue, A),
        Err(EnqueueError::NothingToMerge)
    );
}

#[test]
fn withdrawing_a_merge_can_make_a_later_one_stale_and_it_is_evicted() {
    // Speculation is a prediction and predictions expire. B was validated
    // against a future containing A; if A withdraws, B was validated against
    // something that will not happen.
    //
    // Here A and B write the same *schema* declaration, so B queues only
    // because A is ahead of it establishing the column... and once A leaves, B
    // is re-checked against the real target.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(MAIN, put("orders:9", 9));
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:2", 2));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));
    assert_eq!(fleet.enqueue(&mut queue, B), Ok(2));

    assert!(queue.withdraw(1).is_some());
    let evicted = queue
        .revalidate(&fleet.store, &fleet.views[&MAIN], &fleet.sources())
        .expect("revalidate");

    // B is independent of A, so it survives. The point of the test is that
    // revalidation *runs* and reports rather than assuming the queue is still
    // correct.
    assert!(evicted.is_empty(), "B does not depend on A: {evicted:?}");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.head().map(|q| q.ticket), Some(2));
}

#[test]
fn a_merge_that_stops_being_clean_is_evicted_rather_than_landed() {
    // The central promise of revalidation, and the one nothing tested until a
    // planted violation walked straight through: `revalidate` could have kept a
    // now-conflicting merge and every test still passed.
    //
    // A merge validated against a future that did not happen has not been
    // validated at all. Landing it because it was clean when it was queued is
    // the failure this whole module exists to prevent, arriving by a different
    // route.
    let mut fleet = Fleet::new(&[A]);
    fleet.write(A, put("orders:1", 1));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));

    // Somebody writes the same key directly on the target after A queued.
    fleet.write(MAIN, put("orders:1", 99));

    let evicted = queue
        .revalidate(&fleet.store, &fleet.views[&MAIN], &fleet.sources())
        .expect("revalidate");

    assert_eq!(evicted.len(), 1, "A no longer merges cleanly: {evicted:?}");
    assert_eq!(evicted[0].ticket, 1);
    assert!(evicted[0].conflicts.iter().any(|c| c.key == "orders:1"));
    assert!(evicted[0].reason.contains("no longer merges cleanly"));
    assert!(
        queue.is_empty(),
        "a merge that will not land must not stay at the front of the queue"
    );
}

#[test]
fn a_merge_whose_source_disappeared_is_evicted_rather_than_left_at_the_front() {
    // Otherwise the queue stalls on a merge that can never be computed, and
    // everything behind it waits on something that will never happen.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:2", 2));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));
    assert_eq!(fleet.enqueue(&mut queue, B), Ok(2));

    let mut sources = fleet.sources();
    sources.remove(&A);

    let evicted = queue
        .revalidate(&fleet.store, &fleet.views[&MAIN], &sources)
        .expect("revalidate");

    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].ticket, 1);
    assert!(evicted[0].reason.contains("no longer available"));
    assert_eq!(queue.head().map(|q| q.ticket), Some(2));
}

#[test]
fn the_head_lands_first_and_popping_it_does_not_reorder_the_rest() {
    // FIFO, and deliberately so. Letting a clean merge past a slower one sounds
    // efficient and changes what the slower one merges against, which is the
    // thing speculation exists to keep honest.
    let mut fleet = Fleet::new(&[A, B, C]);
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:2", 2));
    fleet.write(C, put("orders:3", 3));

    let mut queue = fleet.queue();
    fleet.enqueue(&mut queue, A).unwrap();
    fleet.enqueue(&mut queue, B).unwrap();
    fleet.enqueue(&mut queue, C).unwrap();

    assert_eq!(queue.pop_head().map(|q| q.source), Some(A));
    assert_eq!(
        queue.entries().iter().map(|q| q.source).collect::<Vec<_>>(),
        vec![B, C]
    );
}

#[test]
fn a_ticket_is_never_reused() {
    // Tickets are how an agent is told what it is behind. Reusing one after a
    // withdrawal would point a second agent at a merge that is not the one it
    // was told about.
    let mut fleet = Fleet::new(&[A, B]);
    fleet.write(A, put("orders:1", 1));
    fleet.write(B, put("orders:2", 2));

    let mut queue = fleet.queue();
    assert_eq!(fleet.enqueue(&mut queue, A), Ok(1));
    queue.withdraw(1);
    assert_eq!(
        fleet.enqueue(&mut queue, B),
        Ok(2),
        "the withdrawn ticket must not come back"
    );
}

#[test]
fn a_queued_merge_does_not_touch_the_real_target() {
    // Speculation happens on a copy. A queue that mutated the branch it was
    // predicting about would make the prediction true by construction.
    //
    // The *compiler* is what enforces this: `enqueue` takes `&MaterializedView`
    // and `&S`, so there is no mutation to make. This test is documentation of
    // an intended property rather than a guard against losing it — said plainly,
    // because a test that cannot fail reads in a list of test names exactly like
    // one that can.
    let mut fleet = Fleet::new(&[A]);
    fleet.write(A, put("orders:1", 1));

    let before = fleet.views[&MAIN].clone();
    let mut queue = fleet.queue();
    fleet.enqueue(&mut queue, A).unwrap();

    assert_eq!(
        fleet.views[&MAIN].keys, before.keys,
        "enqueuing changed the target it was only supposed to predict about"
    );
}

#[test]
fn fifty_agents_that_do_not_overlap_all_queue() {
    // The scale M18 names. Nothing here should be quadratic in a way that
    // matters, and nothing should conflict.
    let agents: Vec<BranchId> = (1..=50).map(BranchId).collect();
    let mut fleet = Fleet::new(&agents);
    for agent in &agents {
        fleet.write(*agent, put(&format!("orders:{}", agent.0), agent.0 as i64));
    }

    let mut queue = fleet.queue();
    for agent in &agents {
        assert!(
            fleet.enqueue(&mut queue, *agent).is_ok(),
            "agent {} was refused and should not have been",
            agent.0
        );
    }
    assert_eq!(queue.len(), 50);
}

#[test]
fn forty_nine_agents_writing_one_key_are_told_at_enqueue() {
    // The case the milestone opens with, and the one that is intolerable
    // without speculation: forty-nine agents each finding out at the front of
    // the queue, long after the change was made.
    let agents: Vec<BranchId> = (1..=50).map(BranchId).collect();
    let mut fleet = Fleet::new(&agents);
    for agent in &agents {
        fleet.write(*agent, put("orders:1", agent.0 as i64));
    }

    let mut queue = fleet.queue();
    let mut accepted = 0;
    let mut refused = 0;
    for agent in &agents {
        match fleet.enqueue(&mut queue, *agent) {
            Ok(_) => accepted += 1,
            Err(EnqueueError::WouldConflict { behind, .. }) => {
                refused += 1;
                assert!(
                    behind.is_some(),
                    "each refusal must name the queued merge it lost to"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    assert_eq!(accepted, 1, "one of them wins the key");
    assert_eq!(refused, 49, "and the other forty-nine are told now");
}
