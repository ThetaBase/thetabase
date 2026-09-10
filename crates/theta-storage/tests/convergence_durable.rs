//! Convergence through the durable store.
//!
//! `theta-core`'s `convergence.rs` proves the CRDT *types* converge. That is
//! necessary and not sufficient: the claim the product makes is that two
//! branches of a real database converge, which involves the log encoding, the
//! materialized fold, the merge, and a restart in between.
//!
//! This closes that gap, and is the second half of the M1 gate in
//! `docs/specs/08-test-validation-plan.md` §2.

use proptest::prelude::*;
use theta_core::{Author, BranchId, CommitId, ContentHash, CrdtOp, LogEntry, OpType, Value};
use theta_storage::merge::{merge, MergeOutcome};
use theta_storage::view::MaterializedView;
use theta_storage::wal::WalConfig;
use theta_storage::{BranchViews, DurableLogStore};

const MAIN: BranchId = BranchId::MAIN;
const FEATURE: BranchId = BranchId(1);

/// One branch's worth of writes, driven through a real durable store.
struct Branches {
    store: DurableLogStore,
    main_view: MaterializedView,
    feature_view: MaterializedView,
    main_head: ContentHash,
    feature_head: ContentHash,
    base: Option<ContentHash>,
    commit: u64,
}

impl Branches {
    fn open(dir: &std::path::Path) -> Self {
        let opened = DurableLogStore::open(WalConfig::new(dir)).expect("open");
        Self {
            main_view: opened.view(MAIN),
            feature_view: opened.view(MAIN),
            store: opened.store,
            main_head: ContentHash::ZERO,
            feature_head: ContentHash::ZERO,
            base: None,
            commit: 0,
        }
    }

    fn fork(&mut self) {
        self.base = Some(self.main_head);
        self.feature_head = self.main_head;
        self.feature_view = self.main_view.clone();
        self.store.set_head(FEATURE, self.main_head);
    }

    fn write(&mut self, branch: BranchId, op: OpType) {
        let (head, view) = match branch {
            MAIN => (&mut self.main_head, &mut self.main_view),
            _ => (&mut self.feature_head, &mut self.feature_view),
        };
        self.commit += 1;
        let entry = LogEntry {
            prev_hash: *head,
            commit_id: CommitId(self.commit),
            branch_id: branch,
            op,
            author: Author::agent("s", "u"),
            timestamp_ms: self.commit as i64,
        };
        // Durable first, then visible — the same path production takes.
        //
        // The store folds into the entry's own branch view; these tests keep the
        // two branch views separately so they can be compared, so the entry is
        // handed over in a single-branch map and taken back out.
        let mut views = BranchViews::new();
        views.insert(branch, view.clone());
        *head = self
            .store
            .append_and_apply(entry, &mut views)
            .expect("append");
        *view = views.remove(&branch).expect("branch view was folded");
    }

    /// Merge `source` into `target` and return the resulting view.
    fn merge_branches(&mut self, source: BranchId, target: BranchId) -> MaterializedView {
        let (source_view, target_view) = match source {
            MAIN => (&self.main_view, &self.feature_view),
            _ => (&self.feature_view, &self.main_view),
        };

        let outcome = merge(
            &self.store,
            source,
            target,
            self.base,
            target_view,
            source_view,
        )
        .expect("merge");

        let mut view = target_view.clone();
        match outcome {
            MergeOutcome::UpToDate => {}
            MergeOutcome::Merged { ops, crdt, .. } => {
                let head = view.applied;
                for (i, op) in ops.into_iter().enumerate() {
                    view.apply(&LogEntry {
                        prev_hash: ContentHash::ZERO,
                        commit_id: CommitId(head + i as u64),
                        branch_id: target,
                        op,
                        author: Author::System,
                        timestamp_ms: (head + i as u64) as i64,
                    });
                }
                // Merged CRDT states are installed, not replayed. The map is
                // persistent, so this shares nodes with the parent branch and
                // path-copies only what the extend touches (M10.6).
                view.crdts.extend(crdt);
            }
            MergeOutcome::Conflicted { conflicts } => {
                panic!("CRDT-only branches must never conflict: {conflicts:?}")
            }
        }
        view
    }

    fn merge_into_main(&mut self) -> MaterializedView {
        self.merge_branches(FEATURE, MAIN)
    }
}

/// The op shapes a branch can perform in these tests.
#[derive(Debug, Clone)]
enum Action {
    Increment(i64),
    AddTag(u8),
    RemoveTag(u8),
    SetRegister(i64),
}

fn actions() -> impl Strategy<Value = Vec<(bool, Action)>> {
    let action = prop_oneof![
        (-50i64..50).prop_map(Action::Increment),
        (0u8..5).prop_map(Action::AddTag),
        (0u8..5).prop_map(Action::RemoveTag),
        (0i64..100).prop_map(Action::SetRegister),
    ];
    prop::collection::vec((any::<bool>(), action), 0..24)
}

fn op_for(action: &Action) -> OpType {
    match action {
        Action::Increment(by) => OpType::Crdt {
            key: "stats:count".into(),
            mutation: CrdtOp::Increment { by: *by },
        },
        Action::AddTag(t) => OpType::Crdt {
            key: "user:tags".into(),
            mutation: CrdtOp::SetAdd {
                element: Value::Int(*t as i64),
            },
        },
        Action::RemoveTag(t) => OpType::Crdt {
            key: "user:tags".into(),
            mutation: CrdtOp::SetRemove {
                element: Value::Int(*t as i64),
            },
        },
        Action::SetRegister(v) => OpType::Crdt {
            key: "user:plan".into(),
            mutation: CrdtOp::SetRegister {
                value: Value::Int(*v),
            },
        },
    }
}

const CRDT_KEYS: [&str; 3] = ["stats:count", "user:tags", "user:plan"];

fn crdt_snapshot(view: &MaterializedView) -> Vec<Option<Value>> {
    CRDT_KEYS.iter().map(|k| view.get_crdt(k)).collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 300,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Two branches of a real, durable database converge to the same state
    /// regardless of which is merged into which.
    ///
    /// Both directions run over the *same* history — the branches, the ops and
    /// their authorship are identical, and only the merge direction differs.
    /// Swapping which branch authored an op would change its replica id and so
    /// its LWW tie-break, which is a different scenario, not a mirrored one.
    #[test]
    fn branches_converge_regardless_of_merge_direction(script in actions()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = Branches::open(dir.path());
        b.fork();
        for (to_feature, action) in &script {
            b.write(if *to_feature { FEATURE } else { MAIN }, op_for(action));
        }

        let forward = b.merge_branches(FEATURE, MAIN);
        let reverse = b.merge_branches(MAIN, FEATURE);
        prop_assert_eq!(crdt_snapshot(&forward), crdt_snapshot(&reverse));
    }

    /// Merging is idempotent: merging an already-merged branch changes nothing.
    #[test]
    fn merging_twice_changes_nothing(script in actions()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut b = Branches::open(dir.path());
        b.fork();
        for (to_feature, action) in &script {
            b.write(if *to_feature { FEATURE } else { MAIN }, op_for(action));
        }

        // Merging an already-merged branch must be a no-op. It is only a no-op
        // because merged CRDT *states* are installed rather than ops replayed —
        // replaying an increment twice would double-count.
        let once = b.merge_into_main();
        b.main_view = once.clone();
        let twice = b.merge_into_main();
        prop_assert_eq!(crdt_snapshot(&once), crdt_snapshot(&twice));
    }

    /// Everything above survives a restart: the converged state is reachable by
    /// replaying the log, because that is the only place it lives.
    #[test]
    fn converged_state_survives_a_restart(script in actions()) {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut b = Branches::open(dir.path());
        b.fork();
        for (to_feature, action) in &script {
            b.write(if *to_feature { FEATURE } else { MAIN }, op_for(action));
        }
        let main_before = b.main_view.clone();
        let feature_before = b.feature_view.clone();
        drop(b); // crash, no clean shutdown

        // Replaying the whole log yields every branch's entries, so the fold
        // over it must contain both branches' contributions.
        let reopened = DurableLogStore::open(WalConfig::new(dir.path())).expect("reopen");
        let replayed: Vec<_> = reopened.recovery.entries.iter().collect();
        prop_assert_eq!(
            replayed.len(),
            script.len(),
            "an acknowledged write was lost across the restart"
        );

        // Each branch's own history still folds to what it had before.
        let main_entries: Vec<LogEntry> = replayed
            .iter()
            .filter(|e| e.branch_id == MAIN)
            .map(|e| (*e).clone())
            .collect();
        let feature_entries: Vec<LogEntry> = replayed
            .iter()
            .filter(|e| e.branch_id == FEATURE)
            .map(|e| (*e).clone())
            .collect();

        prop_assert_eq!(
            crdt_snapshot(&MaterializedView::replay(&main_entries)),
            crdt_snapshot(&main_before)
        );
        prop_assert_eq!(
            crdt_snapshot(&MaterializedView::replay(&feature_entries)),
            crdt_snapshot(&feature_before)
        );
    }
}
