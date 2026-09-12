//! A declared unique index is enforced on the write path.
//!
//! # Why this did not exist until now
//!
//! `IndexDef::unique` has been in the schema since schemas existed, and nothing
//! ever read it. A caller who declared a unique index got a flag in a document
//! and no enforcement at all — which is worse than not offering the flag,
//! because the natural reading of a declared constraint is that something is
//! checking it. The capability matrix had to warn customers about it in
//! writing.
//!
//! # The case that matters most
//!
//! `an_update_to_an_existing_row_is_not_a_violation_of_its_own_constraint`. A
//! naive implementation compares the incoming tuple against every row and finds
//! the row being updated, so the second write to any row fails. That bug makes
//! a unique index look like an append-only constraint, and it is the reason
//! this file leads with the happy path rather than the refusal.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, IndexDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::SafetyPolicy;
use thetad::{Config, Engine};

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("unique-test");
    config.data_dir = dir.path().to_path_buf();
    // Development rather than protected: this suite is about the constraint,
    // and a strict policy would gate the setup changes it needs to make.
    config.safety = SafetyPolicy::development();
    (Engine::open(config).expect("opens"), dir)
}

fn author() -> Author {
    Author::agent("sess", "user")
}

/// Propose a schema change and land it by id.
///
/// Every proposal is applied explicitly, including ones the gate would
/// auto-apply — the engine holds all of them, and a helper that skipped `apply`
/// for an auto-applied change would leave every assertion running against an
/// empty schema. `describe.rs` records that mistake having been made once.
fn land(engine: &mut Engine, change: SchemaChange) {
    let diff = engine.propose_schema_change(BranchId(0), change, author(), 0);
    engine
        .apply_schema_change(&diff.change_id, true, author(), 1)
        .expect("the setup change must land");
}

/// A table whose `email` column carries a unique index.
fn with_unique_email(engine: &mut Engine) {
    land(
        engine,
        SchemaChange::AddTable {
            table: TableDef {
                name: "users".into(),
                fields: BTreeMap::from([("email".to_string(), text_field("email"))]),
                indexes: vec![IndexDef {
                    name: "users_email".into(),
                    columns: vec!["email".into()],
                    unique: true,
                }],
            },
        },
    );
}

fn text_field(name: &str) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: ValueType::Text,
        nullable: true,
        crdt: None,
        declared_at: None,
    }
}

fn row(email: &str) -> Value {
    Value::Map(BTreeMap::from([(
        "email".to_string(),
        Value::Text(email.to_string()),
    )]))
}

#[test]
fn a_second_row_with_the_same_unique_value_is_refused() {
    let (mut engine, _dir) = engine();
    with_unique_email(&mut engine);

    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("a@example.com"),
            author(),
            0,
        )
        .expect("the first row lands");

    let err = engine
        .put(
            BranchId::MAIN,
            "users:bob",
            row("a@example.com"),
            author(),
            0,
        )
        .expect_err("a declared unique index must refuse a duplicate");

    let message = err.to_string();
    // The row already holding the value, by name. A refusal that does not say
    // which row conflicts leaves the caller to scan for it.
    assert!(
        message.contains("alice"),
        "the refusal does not name the conflicting row: {message}"
    );
    assert!(
        message.contains("users_email"),
        "the refusal does not name the index: {message}"
    );
}

#[test]
fn an_update_to_an_existing_row_is_not_a_violation_of_its_own_constraint() {
    let (mut engine, _dir) = engine();
    with_unique_email(&mut engine);

    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("a@example.com"),
            author(),
            0,
        )
        .expect("the first write lands");

    // The same row, the same value. A naive check compares against every row
    // including this one and refuses — which would make a unique index mean
    // "write once" and break every update to a constrained table.
    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("a@example.com"),
            author(),
            1,
        )
        .expect("a row does not collide with itself");

    // And changing the value on the same row is equally fine.
    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("new@example.com"),
            author(),
            2,
        )
        .expect("updating the constrained column on its own row is legal");
}

#[test]
fn a_different_value_on_the_same_index_is_allowed() {
    let (mut engine, _dir) = engine();
    with_unique_email(&mut engine);

    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("a@example.com"),
            author(),
            0,
        )
        .expect("first");
    engine
        .put(
            BranchId::MAIN,
            "users:bob",
            row("b@example.com"),
            author(),
            1,
        )
        .expect("a distinct value is not a duplicate");
}

#[test]
fn a_row_missing_the_indexed_column_collides_with_nothing() {
    let (mut engine, _dir) = engine();
    with_unique_email(&mut engine);

    let empty = Value::Map(BTreeMap::new());
    engine
        .put(BranchId::MAIN, "users:ghost", empty.clone(), author(), 0)
        .expect("an incomplete tuple is not a duplicate");
    // Two of them, and still no collision — the same rule SQL uses for nulls in
    // a unique index. Refusing the second would make the constraint mean
    // something it does not say.
    engine
        .put(BranchId::MAIN, "users:spectre", empty, author(), 1)
        .expect("two incomplete tuples do not collide with each other");
}

#[test]
fn a_table_without_a_unique_index_is_not_constrained() {
    let (mut engine, _dir) = engine();

    land(
        &mut engine,
        SchemaChange::AddTable {
            table: TableDef {
                name: "logs".into(),
                fields: BTreeMap::from([("email".to_string(), text_field("email"))]),
                // Declared, and *not* unique.
                indexes: vec![IndexDef {
                    name: "logs_email".into(),
                    columns: vec!["email".into()],
                    unique: false,
                }],
            },
        },
    );

    engine
        .put(
            BranchId::MAIN,
            "logs:one",
            row("a@example.com"),
            author(),
            0,
        )
        .expect("first");
    engine
        .put(
            BranchId::MAIN,
            "logs:two",
            row("a@example.com"),
            author(),
            1,
        )
        .expect("a non-unique index constrains nothing");
}

#[test]
fn a_transaction_cannot_smuggle_a_duplicate_past_the_constraint() {
    let (mut engine, _dir) = engine();
    with_unique_email(&mut engine);

    engine
        .put(
            BranchId::MAIN,
            "users:alice",
            row("a@example.com"),
            author(),
            0,
        )
        .expect("first");

    // The write path a transaction takes is a different one, and a constraint
    // checked on `put` but not inside a transaction would be a constraint with
    // a documented bypass.
    let ops = vec![thetad::engine::TxWrite {
        key: "users:carol".into(),
        expect: None,
        action: thetad::engine::TxWriteAction::Put {
            value: row("a@example.com"),
            ttl: 0,
        },
    }];

    engine
        .transaction(BranchId::MAIN, ops, author(), 1)
        .expect_err("a transaction must not be a way around a unique index");
}
