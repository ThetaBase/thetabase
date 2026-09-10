//! `describe` against a real engine, real rows and a real log (M19).
//!
//! The unit tests in `describe.rs` cover the shaping. These cover the things
//! only a live engine can answer: that provenance comes from *this* branch's
//! log, that examples are genuinely absent unless asked for, and that the
//! numbers are the branch's own rather than another's.

use std::collections::BTreeMap;

use theta_core::schema::{CrdtKind, FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::SafetyPolicy;
use thetad::describe::{DescribeRequest, DEFAULT_EXAMPLE_LIMIT, MAX_EXAMPLE_LIMIT};
use thetad::engine::Engine;
use thetad::Config;

fn human() -> Author {
    Author::Human {
        user_id: "alice".into(),
    }
}

fn agent() -> Author {
    Author::agent("sess_night", "alice")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("describe");
    config.data_dir = dir.path().to_path_buf();
    // Development rather than protected: this suite is about description, and a
    // strict policy would gate the setup changes it needs to make.
    config.safety = SafetyPolicy::development();
    (Engine::open(config).expect("open"), dir)
}

fn field(name: &str, ty: ValueType, crdt: Option<CrdtKind>) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty,
        nullable: true,
        crdt,
        declared_at: None,
    }
}

fn everything(table: &str) -> DescribeRequest {
    DescribeRequest {
        table: table.into(),
        include_examples: false,
        example_limit: 0,
    }
}

/// Propose a schema change and land it.
///
/// **Every** proposal is applied by id, including the ones the gate would
/// auto-apply. The engine holds all of them: a caller does not know a change's
/// gate before proposing it, so a proposal that applied itself when the gate
/// turned out to be `autoApply` would mean the caller learns what happened only
/// after it has happened.
///
/// The first version of this helper skipped `apply` when `requires_confirm` was
/// false, on the assumption that an auto-applied change had already landed. It
/// had not, and every assertion in this file quietly ran against an empty
/// schema.
fn land(engine: &mut Engine, change: SchemaChange, author: Author, now_ms: i64) {
    let diff = engine.propose_schema_change(BranchId(0), change, author.clone(), now_ms);
    engine
        .apply_schema_change(&diff.change_id, true, author, now_ms + 1)
        .expect("the setup change must land");
}

/// A table with two columns and `rows` rows; every other row omits `optional`.
fn seed(engine: &mut Engine, rows: usize) {
    land(
        engine,
        SchemaChange::AddTable {
            table: TableDef {
                name: "orders".into(),
                fields: BTreeMap::from([
                    ("total".to_string(), field("total", ValueType::Int, None)),
                    (
                        "views".to_string(),
                        field("views", ValueType::Int, Some(CrdtKind::Counter)),
                    ),
                ]),
                indexes: vec![],
            },
        },
        human(),
        1_000,
    );

    for i in 0..rows {
        let mut row = BTreeMap::from([("total".to_string(), Value::Int(i as i64 % 3))]);
        if i % 2 == 0 {
            row.insert("views".to_string(), Value::Int(i as i64));
        }
        engine
            .put(
                BranchId(0),
                &format!("orders:{i}"),
                Value::Map(row),
                human(),
                2_000 + i as i64,
            )
            .expect("put");
    }
}

#[test]
fn a_description_reports_the_branchs_own_row_count() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 40);

    let described = engine.describe(BranchId(0), &everything("orders"));
    assert_eq!(described.tables.len(), 1);
    assert_eq!(described.tables[0].name, "orders");
    assert_eq!(described.tables[0].row_count, 40);
}

#[test]
fn examples_are_absent_unless_asked_for() {
    // The default this whole call is shaped around. `describe` is what an agent
    // reaches for to orient itself, often automatically; one that returns rows
    // by default pulls customer data into a model's context for a call whose
    // purpose was to learn the shape of the data rather than any of it.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 40);

    let described = engine.describe(BranchId(0), &everything("orders"));
    for column in &described.tables[0].columns {
        assert!(
            column.examples.is_empty(),
            "column `{}` returned examples nobody asked for",
            column.name
        );
    }
    assert!(
        !described.examples_withheld,
        "nothing was withheld; the caller did not ask"
    );
}

#[test]
fn distribution_facts_arrive_even_without_examples() {
    // The reason the default is defensible. An agent reaching for examples
    // usually wants to know how sparse a column is, and a null fraction
    // discloses no individual value.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 40);

    let described = engine.describe(BranchId(0), &everything("orders"));
    let views = described.tables[0]
        .columns
        .iter()
        .find(|c| c.name == "views")
        .expect("the views column");

    assert!(views.examples.is_empty());
    assert_eq!(
        views.null_basis_points, 5_000,
        "half the rows omit `views`, so 5000 basis points"
    );
}

#[test]
fn asking_for_examples_returns_distinct_ones() {
    // Three examples of a column holding `0, 1, 2, 0, 1, 2, ...` should be
    // three different values, not the same one three times.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 40);

    let described = engine.describe(
        BranchId(0),
        &DescribeRequest {
            table: "orders".into(),
            include_examples: true,
            example_limit: 0,
        },
    );
    let total = described.tables[0]
        .columns
        .iter()
        .find(|c| c.name == "total")
        .expect("the total column");

    assert_eq!(total.examples.len(), DEFAULT_EXAMPLE_LIMIT as usize);
    let distinct: std::collections::BTreeSet<&String> = total.examples.iter().collect();
    assert_eq!(
        distinct.len(),
        total.examples.len(),
        "examples repeated a value: {:?}",
        total.examples
    );
}

#[test]
fn an_over_large_example_request_is_capped_rather_than_honoured() {
    // Above the cap a caller is not characterising a column, they are reading
    // it — and `query` is the call for that, which is gated, logged and counted.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 500);

    let described = engine.describe(
        BranchId(0),
        &DescribeRequest {
            table: "orders".into(),
            include_examples: true,
            example_limit: 10_000,
        },
    );
    for column in &described.tables[0].columns {
        assert!(
            column.examples.len() <= MAX_EXAMPLE_LIMIT as usize,
            "column `{}` returned {} examples",
            column.name,
            column.examples.len()
        );
    }
}

#[test]
fn the_crdt_kind_is_reported_because_it_decides_whether_writes_can_race() {
    // The most useful field here for an agent. With a CRDT, concurrent
    // modification converges; without one it becomes a conflict a human
    // resolves (`docs/INVARIANTS.md` invariant 5), and nothing else in a schema
    // description tells them which they are looking at.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let described = engine.describe(BranchId(0), &everything("orders"));
    let by_name: BTreeMap<&str, &_> = described.tables[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    assert_eq!(by_name["views"].crdt, "counter");
    assert_eq!(
        by_name["total"].crdt, "",
        "a field with no CRDT must report an empty kind, not a default one"
    );
}

#[test]
fn provenance_says_when_an_agent_touched_a_column() {
    // The first question a person asks about a column they do not recognise.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    land(
        &mut engine,
        SchemaChange::AddColumn {
            table: "orders".into(),
            field: field("currency", ValueType::Text, None),
        },
        agent(),
        3_000,
    );

    let described = engine.describe(BranchId(0), &everything("orders"));
    let by_name: BTreeMap<&str, &_> = described.tables[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    assert!(
        by_name["currency"].touched_by_agent,
        "the agent's column must say so"
    );
    assert!(
        !by_name["total"].touched_by_agent,
        "a column only a human ever touched must not"
    );
}

#[test]
fn declared_at_points_at_a_commit_this_branch_actually_has() {
    // The stub `FieldDef::declared_at` promised and never delivered. A wrong
    // pointer would be worse than the `None` it replaced, so this checks the
    // hash is non-empty and hex rather than merely present.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let described = engine.describe(BranchId(0), &everything("orders"));
    let total = described.tables[0]
        .columns
        .iter()
        .find(|c| c.name == "total")
        .expect("the total column");

    assert_eq!(
        total.declared_at.len(),
        64,
        "declared_at should be a 32-byte hash in hex, got {:?}",
        total.declared_at
    );
    assert!(total.declared_at.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn a_branchs_description_is_its_own() {
    // Provenance and counts must come from the branch being described. A
    // description assembled from another branch's history would carry
    // `declaredAt` pointers to commits this branch never saw.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 10);

    let preview = engine
        .create_branch("preview", BranchId(0), human(), 5_000)
        .expect("fork");

    for i in 100..120 {
        engine
            .put(
                BranchId(0),
                &format!("orders:{i}"),
                Value::Map(BTreeMap::from([("total".to_string(), Value::Int(1))])),
                human(),
                6_000 + i as i64,
            )
            .expect("write to main only");
    }

    let on_main = engine.describe(BranchId(0), &everything("orders"));
    let on_preview = engine.describe(preview, &everything("orders"));

    assert_eq!(on_main.tables[0].row_count, 30);
    assert_eq!(
        on_preview.tables[0].row_count, 10,
        "the fork must not see writes made to main after it forked"
    );
}

#[test]
fn describing_one_table_does_not_describe_the_others() {
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);
    land(
        &mut engine,
        SchemaChange::AddTable {
            table: TableDef {
                name: "customers".into(),
                fields: BTreeMap::from([(
                    "email".to_string(),
                    field("email", ValueType::Text, None),
                )]),
                indexes: vec![],
            },
        },
        human(),
        4_000,
    );

    let one = engine.describe(BranchId(0), &everything("orders"));
    assert_eq!(one.tables.len(), 1);
    assert_eq!(one.tables[0].name, "orders");

    let all = engine.describe(BranchId(0), &everything(""));
    assert_eq!(
        all.tables.len(),
        2,
        "an empty table name describes every table"
    );
}

#[test]
fn describing_a_table_that_does_not_exist_is_empty_rather_than_an_error() {
    // An agent orienting itself asks about names it has guessed. A hard failure
    // there trains it to stop asking, which puts it back to guessing — and the
    // empty answer is the true one.
    let (mut engine, _dir) = engine();
    seed(&mut engine, 4);

    let described = engine.describe(BranchId(0), &everything("no_such_table"));
    assert!(described.tables.is_empty());
    assert!(!described.examples_withheld);
}

#[test]
fn a_description_survives_the_wire_unchanged() {
    // The whole point of it being an RPC. Encoded and decoded through the same
    // path a real client uses.
    use theta_proto::wire::{Response, ResponseBody};

    let (mut engine, _dir) = engine();
    seed(&mut engine, 40);
    let described = engine.describe(
        BranchId(0),
        &DescribeRequest {
            table: "orders".into(),
            include_examples: true,
            example_limit: 3,
        },
    );

    let response = Response {
        request_id: 1,
        body: ResponseBody::Description(described.clone()),
    };
    let decoded = Response::decode(&response.encode()).expect("decode");
    assert_eq!(decoded.body, ResponseBody::Description(described));
}
