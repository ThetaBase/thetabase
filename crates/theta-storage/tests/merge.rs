//! Merge semantics.
//!
//! The two claims under test, both from `docs/specs/03-data-model-consistency.md`
//! §2.3-2.4 and `01-system-architecture.md` §3.3:
//!
//! 1. CRDT-typed fields merge automatically and converge regardless of merge
//!    order — no human, no model, no judgment.
//! 2. Everything else that genuinely diverged surfaces as an explicit conflict.
//!    Never auto-resolved, never silently picked, never partially applied.

use theta_core::crdt::ElemId;
use theta_core::{Author, BranchId, CommitId, ContentHash, CrdtOp, LogEntry, OpType, Value};
use theta_storage::merge::{merge, MergeOutcome};
use theta_storage::view::MaterializedView;
use theta_storage::{LogStore, MemLogStore};

const MAIN: BranchId = BranchId::MAIN;
const FEATURE: BranchId = BranchId(1);

struct Fixture {
    store: MemLogStore,
    main_view: MaterializedView,
    feature_view: MaterializedView,
    base: Option<ContentHash>,
    main_head: ContentHash,
    feature_head: ContentHash,
    commit: u64,
}

impl Fixture {
    fn new() -> Self {
        Self {
            store: MemLogStore::new(),
            main_view: MaterializedView::new(),
            feature_view: MaterializedView::new(),
            base: None,
            main_head: ContentHash::ZERO,
            feature_head: ContentHash::ZERO,
            commit: 0,
        }
    }

    fn write(&mut self, branch: BranchId, op: OpType) {
        let prev = if branch == MAIN {
            self.main_head
        } else {
            self.feature_head
        };
        self.commit += 1;
        let entry = LogEntry {
            prev_hash: prev,
            commit_id: CommitId(self.commit),
            branch_id: branch,
            op,
            author: Author::agent("s", "u"),
            timestamp_ms: self.commit as i64,
        };
        let hash = self.store.append(entry.clone()).expect("append");
        if branch == MAIN {
            self.main_head = hash;
            self.main_view.apply(&entry);
        } else {
            self.feature_head = hash;
            self.feature_view.apply(&entry);
        }
    }

    /// Branch `feature` off `main` at its current head.
    fn fork(&mut self) {
        self.base = Some(self.main_head);
        self.feature_head = self.main_head;
        self.feature_view = self.main_view.clone();
        self.store.set_head(FEATURE, self.main_head);
    }

    fn merge_into_main(&self) -> MergeOutcome {
        merge(
            &self.store,
            FEATURE,
            MAIN,
            self.base,
            &self.main_view,
            &self.feature_view,
        )
        .expect("merge")
    }
}

fn put(key: &str, value: i64) -> OpType {
    OpType::Put {
        key: key.into(),
        value: Value::Int(value),
    }
}

fn increment(key: &str, by: i64) -> OpType {
    OpType::Crdt {
        key: key.into(),
        mutation: CrdtOp::Increment { by },
    }
}

#[test]
fn disjoint_writes_merge_without_conflict() {
    let mut f = Fixture::new();
    f.write(MAIN, put("a", 1));
    f.fork();
    f.write(FEATURE, put("b", 2));
    f.write(MAIN, put("c", 3));

    match f.merge_into_main() {
        MergeOutcome::Merged { ops, .. } => {
            assert_eq!(ops.len(), 1, "only the source's own change should replay");
            assert_eq!(ops[0], put("b", 2));
        }
        other => panic!("expected a clean merge, got {other:?}"),
    }
}

#[test]
fn concurrent_counter_increments_both_survive_the_merge() {
    let mut f = Fixture::new();
    f.write(MAIN, increment("stats:views", 5));
    f.fork();
    f.write(MAIN, increment("stats:views", 3));
    f.write(FEATURE, increment("stats:views", 10));

    let outcome = f.merge_into_main();
    let MergeOutcome::Merged {
        crdt, converged, ..
    } = outcome
    else {
        panic!("counters must never conflict, got {outcome:?}");
    };
    assert_eq!(converged, vec!["stats:views".to_string()]);
    assert_eq!(
        crdt["stats:views"].value(),
        Value::Int(18),
        "5 + 3 + 10; a lost increment means the merge took a side"
    );
}

#[test]
fn crdt_merges_converge_regardless_of_direction() {
    let build = |flip: bool| {
        let mut f = Fixture::new();
        f.fork();
        let (a, b) = if flip { (10, 4) } else { (4, 10) };
        f.write(MAIN, increment("k", a));
        f.write(FEATURE, increment("k", b));

        let MergeOutcome::Merged { crdt, .. } = f.merge_into_main() else {
            panic!("counters must not conflict");
        };
        crdt.get("k").map(|s| s.value())
    };
    assert_eq!(
        build(false),
        build(true),
        "merge result depended on direction"
    );
    assert_eq!(build(false), Some(Value::Int(14)));
}

#[test]
fn concurrent_set_operations_converge() {
    let mut f = Fixture::new();
    f.fork();
    f.write(
        MAIN,
        OpType::Crdt {
            key: "user:tags".into(),
            mutation: CrdtOp::SetAdd {
                element: Value::Text("admin".into()),
            },
        },
    );
    f.write(
        FEATURE,
        OpType::Crdt {
            key: "user:tags".into(),
            mutation: CrdtOp::SetAdd {
                element: Value::Text("beta".into()),
            },
        },
    );

    let MergeOutcome::Merged { converged, .. } = f.merge_into_main() else {
        panic!("OR-Sets must not conflict");
    };
    assert_eq!(converged, vec!["user:tags".to_string()]);
}

#[test]
fn concurrent_sequence_inserts_converge() {
    let mut f = Fixture::new();
    f.fork();
    let mk = |branch: u64, counter: u64, text: &str| OpType::Crdt {
        key: "doc:body".into(),
        mutation: CrdtOp::SeqInsert {
            id: ElemId {
                counter,
                replica: theta_core::crdt::ReplicaId(branch),
            },
            after: None,
            value: Value::Text(text.into()),
        },
    };
    f.write(MAIN, mk(0, 1, "from main"));
    f.write(FEATURE, mk(1, 1, "from feature"));

    assert!(matches!(f.merge_into_main(), MergeOutcome::Merged { .. }));
}

#[test]
fn two_plain_writes_to_one_field_are_an_explicit_conflict() {
    let mut f = Fixture::new();
    f.write(MAIN, put("orders:total", 100));
    f.fork();
    f.write(MAIN, put("orders:total", 150));
    f.write(FEATURE, put("orders:total", 200));

    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!("divergent plain writes must conflict, never be picked between");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].key, "orders:total");
    assert_eq!(conflicts[0].ours, Some(Value::Int(150)));
    assert_eq!(conflicts[0].theirs, Some(Value::Int(200)));
}

#[test]
fn a_conflicted_merge_carries_no_applicable_operations() {
    let mut f = Fixture::new();
    f.fork();
    f.write(MAIN, put("k", 1));
    f.write(FEATURE, put("k", 2));
    f.write(FEATURE, put("unrelated", 9)); // would merge cleanly on its own

    let outcome = f.merge_into_main();
    // The type carries no ops in the conflicted variant, so there is nothing to
    // partially apply even by mistake. A partial merge would leave the branch in
    // a state neither side ever had.
    assert!(matches!(outcome, MergeOutcome::Conflicted { .. }));
    assert_eq!(outcome.conflict_count(), 1);
}

#[test]
fn identical_writes_on_both_sides_are_not_a_conflict() {
    let mut f = Fixture::new();
    f.fork();
    f.write(MAIN, put("k", 42));
    f.write(FEATURE, put("k", 42));

    assert!(
        matches!(f.merge_into_main(), MergeOutcome::Merged { .. }),
        "converging on the same value is agreement, not conflict"
    );
}

#[test]
fn a_delete_against_a_write_is_a_conflict() {
    let mut f = Fixture::new();
    f.write(MAIN, put("k", 1));
    f.fork();
    f.write(MAIN, put("k", 2));
    f.write(FEATURE, OpType::Delete { key: "k".into() });

    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!("delete-vs-write must surface");
    };
    assert_eq!(conflicts[0].theirs, None, "the source deleted it");
    assert_eq!(conflicts[0].ours, Some(Value::Int(2)));
}

#[test]
fn a_field_treated_as_plain_on_one_branch_and_crdt_on_the_other_conflicts() {
    let mut f = Fixture::new();
    f.fork();
    f.write(MAIN, put("counter", 5));
    f.write(FEATURE, increment("counter", 3));

    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!("a type divergence must not be merged either way");
    };
    assert!(
        conflicts[0].reason.contains("CRDT-typed"),
        "reason should name the type divergence, got: {}",
        conflicts[0].reason
    );
}

#[test]
fn merging_a_branch_that_never_diverged_is_a_no_op() {
    let mut f = Fixture::new();
    f.write(MAIN, put("a", 1));
    f.fork();
    assert_eq!(f.merge_into_main(), MergeOutcome::UpToDate);
}

#[test]
fn transactions_contribute_every_key_they_touch() {
    let mut f = Fixture::new();
    f.fork();
    f.write(
        FEATURE,
        OpType::Transaction {
            ops: vec![put("debit", -100), put("credit", 100)],
        },
    );

    let MergeOutcome::Merged { ops, .. } = f.merge_into_main() else {
        panic!("expected a clean merge");
    };
    assert_eq!(ops.len(), 2, "both legs of the transaction must replay");
}

#[test]
fn conflicts_from_a_transaction_block_the_whole_merge() {
    let mut f = Fixture::new();
    f.fork();
    f.write(MAIN, put("credit", 50));
    f.write(
        FEATURE,
        OpType::Transaction {
            ops: vec![put("debit", -100), put("credit", 100)],
        },
    );

    assert!(
        matches!(f.merge_into_main(), MergeOutcome::Conflicted { .. }),
        "half a transaction must never land"
    );
}

// ---- properties ------------------------------------------------------------
//
// The hand-written cases above cover the shapes we thought of. These cover the
// ones we did not.

use proptest::prelude::*;

proptest! {
    // Persistence is off because these live in an integration test, where
    // proptest cannot find a source root to write the regression file into.
    #![proptest_config(ProptestConfig {
        cases: 500,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// No merge of concurrent plain writes to the same key ever produces ops.
    /// This is the "never silently resolve" claim, stated as a property rather
    /// than as a list of examples.
    #[test]
    fn divergent_plain_writes_never_merge_silently(
        ours in -1000i64..1000,
        theirs in -1000i64..1000,
    ) {
        let mut f = Fixture::new();
        f.fork();
        f.write(MAIN, put("k", ours));
        f.write(FEATURE, put("k", theirs));

        match f.merge_into_main() {
            // Agreeing on a value is not divergence.
            MergeOutcome::Merged { .. } => prop_assert_eq!(ours, theirs),
            MergeOutcome::Conflicted { conflicts } => {
                prop_assert_ne!(ours, theirs);
                prop_assert_eq!(conflicts.len(), 1);
            }
            MergeOutcome::UpToDate => {
                return Err(TestCaseError::fail("branches did diverge"));
            }
        }
    }

    /// Counter merges are order-independent and lose nothing, for any
    /// interleaving of increments across the two branches.
    #[test]
    fn counter_merges_are_order_independent_and_lossless(
        ours in prop::collection::vec(-100i64..100, 0..8),
        theirs in prop::collection::vec(-100i64..100, 0..8),
    ) {
        let mut f = Fixture::new();
        f.fork();
        for n in &ours {
            f.write(MAIN, increment("k", *n));
        }
        for n in &theirs {
            f.write(FEATURE, increment("k", *n));
        }

        let outcome = f.merge_into_main();

        // A source branch that wrote nothing after the fork has nothing to
        // merge, whatever the target did.
        let merged = match outcome {
            MergeOutcome::UpToDate => {
                prop_assert!(theirs.is_empty(), "a source with writes is not up to date");
                f.main_view.get_crdt("k")
            }
            MergeOutcome::Merged { crdt, .. } => match crdt.get("k") {
                Some(state) => Some(state.value()),
                // Nothing merged for this key means main's own state stands.
                None => f.main_view.get_crdt("k"),
            },
            MergeOutcome::Conflicted { .. } => {
                return Err(TestCaseError::fail("counters must never conflict"));
            }
        };

        // A counter that was never written has no state at all, which is
        // distinct from a counter holding zero — the same distinction an
        // unwritten LWW-Register makes.
        let expected = match ours.is_empty() && theirs.is_empty() {
            true => None,
            false => Some(Value::Int(ours.iter().chain(theirs.iter()).sum())),
        };
        prop_assert_eq!(merged, expected);
    }
}

// ---- schema changes travel with the branch ---------------------------------
//
// A branch carrying a migration used to merge as a no-op: `collect` skipped
// `OpType::Schema`, so the migration silently did not travel. That breaks the
// promise shadow-branch promotion rests on — what lands has to be what was
// validated (`07-agent-safety-layer.md` §5.4) — and it would have broken
// branch-per-PR the same way.

fn drop_column(table: &str, column: &str) -> OpType {
    OpType::Schema {
        change: theta_core::schema::SchemaChange::DropColumn {
            table: table.into(),
            column: column.into(),
        },
    }
}

fn add_column(table: &str, column: &str, ty: theta_core::ValueType) -> OpType {
    OpType::Schema {
        change: theta_core::schema::SchemaChange::AddColumn {
            table: table.into(),
            field: theta_core::schema::FieldDef {
                name: column.into(),
                ty,
                nullable: true,
                crdt: None,
                declared_at: None,
            },
        },
    }
}

#[test]
fn a_schema_change_made_on_a_branch_travels_when_it_merges() {
    let mut f = Fixture::new();
    f.fork();
    f.write(FEATURE, drop_column("users", "email"));

    let MergeOutcome::Merged { schema, .. } = f.merge_into_main() else {
        panic!("expected a clean merge");
    };

    assert_eq!(
        schema.len(),
        1,
        "the migration did not travel with its branch"
    );
}

#[test]
fn a_schema_change_the_target_already_made_does_not_travel_twice() {
    let mut f = Fixture::new();
    f.fork();
    // Both branches dropped the same column: one answer, arrived at twice.
    f.write(MAIN, drop_column("users", "email"));
    f.write(FEATURE, drop_column("users", "email"));

    let MergeOutcome::Merged { schema, .. } = f.merge_into_main() else {
        panic!("same change on both sides is not a conflict");
    };
    assert!(schema.is_empty());
}

#[test]
fn independent_schema_changes_on_both_sides_both_survive() {
    let mut f = Fixture::new();
    f.fork();
    f.write(
        MAIN,
        add_column("users", "nickname", theta_core::ValueType::Text),
    );
    f.write(
        FEATURE,
        add_column("users", "phone", theta_core::ValueType::Text),
    );

    let MergeOutcome::Merged { schema, .. } = f.merge_into_main() else {
        panic!("two different columns are not one question");
    };
    assert_eq!(schema.len(), 1, "the source's column has to travel");
}

#[test]
fn two_branches_redefining_one_declaration_is_a_conflict_a_human_settles() {
    let mut f = Fixture::new();
    f.fork();
    // One side drops the column, the other retypes it. Nothing in the data says
    // which is right, and picking either would discard a decision someone made.
    f.write(MAIN, drop_column("users", "email"));
    f.write(
        FEATURE,
        OpType::Schema {
            change: theta_core::schema::SchemaChange::AlterColumnType {
                table: "users".into(),
                column: "email".into(),
                from: theta_core::ValueType::Text,
                to: theta_core::ValueType::Int,
            },
        },
    );

    let outcome = f.merge_into_main();
    let MergeOutcome::Conflicted { conflicts } = &outcome else {
        panic!("a redefinition of one declaration on both sides was auto-resolved: {outcome:?}");
    };
    assert!(conflicts.iter().any(|c| c.key == "schema:users.email"));

    // The conflicted variant carries no applicable operations at all, so there
    // is nothing for a caller to apply even if it wanted to.
    assert_eq!(outcome.conflict_count(), conflicts.len());
}

// ---- a table-scoped change against a column-scoped one ----------------------
//
// `declaration_of` scopes a table change to `(table, None)` and a column change
// to `(table, Some(column))`. Those are different keys, so nothing compared
// them, and a branch that dropped a table merged cleanly with one that had just
// added a column to it — landing a `DropTable` and an `AddColumn` for the same
// table in one merge. Whichever order they replay in, one of them describes a
// table that is not there.

fn drop_table(table: &str) -> OpType {
    OpType::Schema {
        change: theta_core::schema::SchemaChange::DropTable {
            table: table.into(),
        },
    }
}

fn rename_column(table: &str, from: &str, to: &str) -> OpType {
    OpType::Schema {
        change: theta_core::schema::SchemaChange::RenameColumn {
            table: table.into(),
            from: from.into(),
            to: to.into(),
        },
    }
}

#[test]
fn dropping_a_table_conflicts_with_a_column_added_to_it_on_the_other_branch() {
    let mut f = Fixture::new();
    f.fork();
    f.write(
        MAIN,
        add_column("users", "nickname", theta_core::ValueType::Text),
    );
    f.write(FEATURE, drop_table("users"));

    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!(
            "dropping a table under a branch that is still extending it must \
                go to a human, not merge"
        );
    };
    assert!(
        conflicts.iter().any(|c| c.key == "schema:users"),
        "the conflict has to name the table, got {conflicts:?}"
    );
}

#[test]
fn dropping_a_table_conflicts_with_a_column_dropped_from_it_on_the_other_branch() {
    let mut f = Fixture::new();
    f.fork();
    f.write(MAIN, drop_table("users"));
    f.write(FEATURE, drop_column("users", "email"));

    // Both sides are destructive, and they still disagree: one says the table
    // is gone, the other says it survives without a column.
    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!("a table drop against a column drop on the same table must conflict");
    };
    assert!(
        conflicts.iter().any(|c| c.key == "schema:users"),
        "got {conflicts:?}"
    );
}

#[test]
fn a_rename_conflicts_with_a_column_the_other_branch_added_under_the_new_name() {
    let mut f = Fixture::new();
    f.fork();
    f.write(
        MAIN,
        add_column("users", "handle", theta_core::ValueType::Text),
    );
    f.write(FEATURE, rename_column("users", "nickname", "handle"));

    // The rename's destination is the column the other side just declared.
    // Merging both leaves two definitions competing for one name.
    let MergeOutcome::Conflicted { conflicts } = f.merge_into_main() else {
        panic!("a rename onto a name the other branch just declared must conflict");
    };
    assert!(
        conflicts.iter().any(|c| c.key == "schema:users.handle"),
        "the conflict has to name the colliding column, got {conflicts:?}"
    );
}

#[test]
fn a_table_added_on_one_branch_and_extended_on_the_other_still_merges() {
    // The guard must not over-fire: adding a table and adding a column to a
    // *different* table are independent and both have to travel.
    let mut f = Fixture::new();
    f.fork();
    f.write(
        MAIN,
        add_column("orders", "total", theta_core::ValueType::Int),
    );
    f.write(
        FEATURE,
        add_column("users", "nickname", theta_core::ValueType::Text),
    );

    let MergeOutcome::Merged { schema, .. } = f.merge_into_main() else {
        panic!("independent tables must still merge");
    };
    assert_eq!(schema.len(), 1, "the source's column has to travel");
}
