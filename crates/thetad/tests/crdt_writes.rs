//! Writing a CRDT value.
//!
//! Until this landed there was no way to. `view.rs` folded `OpType::Crdt`,
//! `merge.rs` converged it, `sync.rs` declined to call it a conflict,
//! `get_crdt` read it and the wire carried a field's declared kind — and no
//! production code anywhere constructed one. Every convergence guarantee the
//! repo demonstrated was demonstrated over state no caller could produce.
//!
//! So the tests that matter here are not the encoding ones. They are the ones
//! where two branches independently mutate a converging field and the merge
//! agrees, driven entirely through the public API.

use std::collections::BTreeMap;

use theta_core::schema::{CrdtKind, FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_proto::wire::{CrdtMutation, ElemId};
use theta_storage::merge::MergeOutcome;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn agent(session: &str) -> Author {
    Author::agent(session, "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("crdt");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

/// A table whose rows *are* a CRDT of `kind`.
///
/// The declaration lives on the row's `value` column, because a CRDT-typed key
/// is a whole row whose value is the CRDT.
fn crdt_table(engine: &mut Engine, name: &str, kind: CrdtKind) {
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddTable {
            table: TableDef {
                name: name.into(),
                fields: BTreeMap::from([(
                    "value".to_string(),
                    FieldDef {
                        name: "value".into(),
                        ty: ValueType::Int,
                        nullable: true,
                        crdt: Some(kind),
                        declared_at: None,
                    },
                )]),
                indexes: vec![],
            },
        },
        agent("setup"),
        1_000,
    );
    engine
        .apply_schema_change(&diff.change_id, true, agent("setup"), 1_001)
        .expect("the table lands");
}

/// A value in the encoding every request on the wire uses.
///
/// Not raw JSON. The signed and unsigned write paths once decoded these two
/// different ways, and a CRDT element that decoded differently from a row value
/// would converge on something the caller never sent.
fn encoded(value: Value) -> String {
    serde_json::to_string(&value).expect("encode")
}

fn text(s: &str) -> String {
    encoded(Value::Text(s.into()))
}

fn inc(by: i64) -> CrdtMutation {
    CrdtMutation::Increment { by }
}

// ---- the gap this closes --------------------------------------------------

#[test]
fn a_counter_can_be_written_and_read_back() {
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);

    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(3), agent("w"), 2_000)
        .expect("increment");
    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(4), agent("w"), 2_001)
        .expect("increment");

    assert_eq!(
        engine.get_crdt(BranchId::MAIN, "stats:views"),
        Some(Value::Int(7)),
        "a counter written through the public API did not read back"
    );
}

#[test]
fn two_branches_incrementing_the_same_counter_converge_on_merge() {
    // **The test this whole change exists for.** Deterministic CRDT convergence
    // is a published guarantee, and every test demonstrating it built the state
    // directly because no caller could produce it. This one does not: both
    // increments arrive through `apply_crdt`, and the merge is the ordinary one.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);
    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(1), agent("w"), 2_000)
        .expect("seed");

    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 3_000)
        .expect("branch");

    engine
        .apply_crdt(
            BranchId::MAIN,
            "stats:views",
            &inc(10),
            agent("main"),
            4_000,
        )
        .expect("main increments");
    engine
        .apply_crdt(side, "stats:views", &inc(100), agent("side"), 4_001)
        .expect("side increments");

    // Before the merge the two disagree, which is what makes the merge mean
    // something.
    assert_eq!(
        engine.get_crdt(BranchId::MAIN, "stats:views"),
        Some(Value::Int(11))
    );
    assert_eq!(engine.get_crdt(side, "stats:views"), Some(Value::Int(101)));

    let outcome = engine
        .merge(side, BranchId::MAIN, agent("merger"), 5_000)
        .expect("merge");
    assert!(
        matches!(outcome, MergeOutcome::Merged { .. }),
        "concurrent increments were not merged: {outcome:?}"
    );

    assert_eq!(
        engine.get_crdt(BranchId::MAIN, "stats:views"),
        Some(Value::Int(111)),
        "concurrent increments did not converge on their sum"
    );
}

#[test]
fn concurrent_increments_are_not_reported_as_a_conflict() {
    // Converging under concurrent modification is what a CRDT field is *for*.
    // Sending this to a human would be sending them the one class of change that
    // does not need one.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);
    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(1), agent("w"), 2_000)
        .expect("seed");
    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 3_000)
        .expect("branch");

    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(5), agent("m"), 4_000)
        .expect("main");
    engine
        .apply_crdt(side, "stats:views", &inc(5), agent("s"), 4_001)
        .expect("side");

    let outcome = engine
        .merge(side, BranchId::MAIN, agent("merger"), 5_000)
        .expect("merge");
    assert!(
        !matches!(outcome, MergeOutcome::Conflicted { .. }),
        "a converging field was sent to a human"
    );
}

#[test]
fn a_set_converges_on_the_union_of_concurrent_additions() {
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "tags", CrdtKind::Set);
    engine
        .apply_crdt(
            BranchId::MAIN,
            "tags:post1",
            &CrdtMutation::SetAdd {
                element_json: text("original"),
            },
            agent("w"),
            2_000,
        )
        .expect("seed");

    let side = engine
        .create_branch("side", BranchId::MAIN, Author::System, 3_000)
        .expect("branch");

    engine
        .apply_crdt(
            BranchId::MAIN,
            "tags:post1",
            &CrdtMutation::SetAdd {
                element_json: text("from-main"),
            },
            agent("m"),
            4_000,
        )
        .expect("main adds");
    engine
        .apply_crdt(
            side,
            "tags:post1",
            &CrdtMutation::SetAdd {
                element_json: text("from-side"),
            },
            agent("s"),
            4_001,
        )
        .expect("side adds");

    engine
        .merge(side, BranchId::MAIN, agent("merger"), 5_000)
        .expect("merge");

    let Some(Value::List(elements)) = engine.get_crdt(BranchId::MAIN, "tags:post1") else {
        panic!("a set did not read back as a list");
    };
    let mut found: Vec<String> = elements
        .iter()
        .filter_map(|v| match v {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec![
            "from-main".to_string(),
            "from-side".to_string(),
            "original".to_string()
        ],
        "concurrent additions did not converge on their union"
    );
}

// ---- the mutation is checked, not merely recorded -------------------------

#[test]
fn a_mutation_that_disagrees_with_the_declared_kind_is_refused_not_appended() {
    // The fold records a mismatched op as rejected and moves on. That is right
    // for a fold — it must never reinterpret what it is replaying — and wrong as
    // an answer to a caller: the entry would be in the log, the write would have
    // been reported as succeeding, and the value would not have changed.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);

    let before = engine.commits_applied();
    let result = engine.apply_crdt(
        BranchId::MAIN,
        "stats:views",
        &CrdtMutation::SetAdd {
            element_json: text("not a counter operation"),
        },
        agent("confused"),
        2_000,
    );

    assert!(
        matches!(result, Err(EngineError::TypeMismatch { .. })),
        "a Set operation on a Counter field was accepted: {result:?}"
    );
    assert_eq!(
        engine.commits_applied(),
        before,
        "the refused mutation was appended to the log anyway"
    );
}

#[test]
fn a_mutation_that_disagrees_with_what_the_row_already_is_is_refused() {
    // A branch can hold state written under a schema that has since changed, so
    // the declaration is not the only thing worth checking.
    let (mut engine, _dir) = engine();

    // No declaration at all: the kind comes from the first op.
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddTable {
            table: TableDef {
                name: "loose".into(),
                fields: BTreeMap::new(),
                indexes: vec![],
            },
        },
        agent("setup"),
        1_000,
    );
    engine
        .apply_schema_change(&diff.change_id, true, agent("setup"), 1_001)
        .expect("table");

    engine
        .apply_crdt(BranchId::MAIN, "loose:x", &inc(1), agent("w"), 2_000)
        .expect("the first op establishes the kind");

    let result = engine.apply_crdt(
        BranchId::MAIN,
        "loose:x",
        &CrdtMutation::SetAdd {
            element_json: text("wrong"),
        },
        agent("w"),
        2_001,
    );
    assert!(
        matches!(result, Err(EngineError::TypeMismatch { .. })),
        "an op disagreeing with the row's existing kind was accepted: {result:?}"
    );
}

#[test]
fn a_key_that_is_not_a_row_address_is_refused() {
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);

    assert!(
        engine
            .apply_crdt(BranchId::MAIN, "no-table-here", &inc(1), agent("w"), 2_000)
            .is_err(),
        "a key with no table was accepted"
    );
}

#[test]
fn malformed_json_is_refused_rather_than_stored_as_text() {
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "reg", CrdtKind::Register);

    let result = engine.apply_crdt(
        BranchId::MAIN,
        "reg:x",
        &CrdtMutation::SetRegister {
            value_json: "{not json".into(),
        },
        agent("w"),
        2_000,
    );
    assert!(
        matches!(result, Err(EngineError::BadRequest { .. })),
        "invalid JSON was accepted: {result:?}"
    );
}

// ---- the element id is the server's to assign -----------------------------

#[test]
fn two_inserts_get_distinct_server_assigned_element_ids() {
    // An RGA id fixes both an element's identity and its order among concurrent
    // siblings. The wire type cannot carry one for an insert, so a caller cannot
    // collide with another writer's element or displace one — and this pins that
    // the server is actually minting distinct ones rather than a constant.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "doc", CrdtKind::Sequence);

    for i in 0..3 {
        engine
            .apply_crdt(
                BranchId::MAIN,
                "doc:body",
                &CrdtMutation::SeqInsert {
                    after: None,
                    value_json: text(&format!("line {i}")),
                },
                agent("w"),
                2_000 + i,
            )
            .expect("insert");
    }

    let Some(Value::List(elements)) = engine.get_crdt(BranchId::MAIN, "doc:body") else {
        panic!("a sequence did not read back as a list");
    };
    assert_eq!(
        elements.len(),
        3,
        "inserts collided, so they were given the same id: {elements:?}"
    );
}

#[test]
fn an_element_can_be_removed_by_the_id_the_caller_read() {
    // `seqRemove` is the one place an id travels inward, and it must: it names
    // an element the caller already saw.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "doc", CrdtKind::Sequence);

    engine
        .apply_crdt(
            BranchId::MAIN,
            "doc:body",
            &CrdtMutation::SeqInsert {
                after: None,
                value_json: text("only line"),
            },
            agent("w"),
            2_000,
        )
        .expect("insert");

    // The id the server minted is (commit, branch). The caller learns it by
    // reading the sequence's live ids — without that there is no way to name an
    // element, and `SeqRemove` would be a request nobody could issue.
    let ids = engine.sequence_ids(BranchId::MAIN, "doc:body");
    assert_eq!(ids.len(), 1, "the inserted element has no readable id");
    let only = ElemId {
        counter: ids[0].counter,
        replica: ids[0].replica.0,
    };

    let removed = engine.apply_crdt(
        BranchId::MAIN,
        "doc:body",
        &CrdtMutation::SeqRemove { id: only },
        agent("w"),
        2_001,
    );
    removed.expect("remove");

    let Some(Value::List(elements)) = engine.get_crdt(BranchId::MAIN, "doc:body") else {
        panic!("a sequence did not read back as a list");
    };
    assert!(
        elements.is_empty(),
        "the element was not removed: {elements:?}"
    );
}

// ---- it is a write, and behaves like one ----------------------------------

#[test]
fn a_crdt_write_is_attributed_like_any_other() {
    // A converging field is still somebody's change, and a forensic question
    // about who touched what must not have a hole shaped like CRDTs.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);
    engine
        .apply_crdt(
            BranchId::MAIN,
            "stats:views",
            &inc(1),
            agent("counter-bot"),
            2_000,
        )
        .expect("increment");

    assert!(
        engine.sessions().iter().any(|s| s == "counter-bot"),
        "a CRDT write left no attributable trace: {:?}",
        engine.sessions()
    );
    let activity = engine.activity(&theta_storage::attribution::Attribution::Session(
        "counter-bot",
    ));
    assert_eq!(activity.by_op.get("crdt").copied(), Some(1));
}

#[test]
fn a_crdt_write_counts_against_the_blast_radius_breaker() {
    // A runaway loop costs money whether it writes rows or increments counters,
    // and a write path the breaker cannot see is a hole in the ceiling.
    let (mut engine, _dir) = engine();
    crdt_table(&mut engine, "stats", CrdtKind::Counter);

    let before = engine.breaker().window_rows();
    engine
        .apply_crdt(BranchId::MAIN, "stats:views", &inc(1), agent("w"), 2_000)
        .expect("increment");
    assert!(
        engine.breaker().window_rows() > before,
        "a CRDT write was invisible to the breaker"
    );
}
