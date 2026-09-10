//! The quickstart has to actually demonstrate the product.
//!
//! `theta demo` seeds a table and rows, then hands the user a change file whose
//! whole purpose is to be *stopped*. The README, the launch demo recording and
//! the activation metric in `docs/business/MARKETING-PLAN.md` §6 all rest on
//! that sequence producing a gate.
//!
//! Nothing else would notice if it stopped. A classifier change making a small
//! nullable-column drop auto-apply would pass every other test in this
//! repository — they assert the *rules*, and this asserts that the rules, given
//! the exact data the demo seeds, still produce the demonstration. The failure
//! mode is a new user running the quickstart, seeing "Applied.", and concluding
//! the safety layer is marketing.

use std::collections::BTreeMap;

use theta_core::schema::{FieldDef, SchemaChange, TableDef};
use theta_core::{Author, BranchId, Value, ValueType};
use theta_safety::classify::Gate;
use thetad::engine::Engine;
use thetad::Config;

fn agent() -> Author {
    Author::agent("sess_quickstart", "agent")
}

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("quickstart");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn field(name: &str, ty: ValueType, nullable: bool) -> (String, FieldDef) {
    (
        name.to_string(),
        FieldDef {
            name: name.to_string(),
            ty,
            nullable,
            crdt: None,
            declared_at: None,
        },
    )
}

/// Exactly what `theta_cli::commands::demo` seeds. Kept in step by the test
/// below that reads the CLI's source — a copy that drifted would test a
/// quickstart nobody runs.
const DEMO_ROWS: usize = 5;

fn seed(engine: &mut Engine) {
    let table = TableDef {
        name: "customers".into(),
        fields: [
            field("email", ValueType::Text, false),
            field("plan", ValueType::Text, false),
            field("legacy_ref", ValueType::Text, true),
        ]
        .into_iter()
        .collect(),
        indexes: Vec::new(),
    };

    let added =
        engine.propose_schema_change(BranchId::MAIN, SchemaChange::AddTable { table }, agent(), 0);
    assert_eq!(
        added.gate,
        Gate::AutoApply,
        "adding a table must stay ungated, or the demo's first step becomes a \
         review queue and the point about proportionality is lost"
    );

    for i in 0..DEMO_ROWS {
        let mut row = BTreeMap::new();
        row.insert(
            "email".to_string(),
            Value::Text(format!("user{i}@example.com")),
        );
        row.insert("plan".to_string(), Value::Text("pro".into()));
        row.insert("legacy_ref".to_string(), Value::Text(format!("LEG-{i}")));
        engine
            .put(
                BranchId::MAIN,
                &format!("customers:cus_{i:03}"),
                Value::Map(row),
                agent(),
                i as i64,
            )
            .expect("seeding a row");
    }
}

#[test]
fn the_quickstart_change_is_gated_rather_than_applied() {
    // The whole demonstration. If this goes green-to-red, the first thing a new
    // user sees stops being the product working.
    let (mut engine, _dir) = engine();
    seed(&mut engine);

    let proposed = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::DropColumn {
            table: "customers".into(),
            column: "legacy_ref".into(),
        },
        agent(),
        100,
    );

    assert_ne!(
        proposed.gate,
        Gate::AutoApply,
        "the quickstart's drop applied without review, so `theta demo` now \
         demonstrates nothing"
    );
    assert_eq!(
        proposed.rows_affected, DEMO_ROWS as u64,
        "the diff has to name the rows at stake; a drop reported as affecting \
         zero rows reads as harmless"
    );
    assert!(
        !proposed.reversible,
        "a column drop is irreversible and the demo depends on it saying so"
    );
}

#[test]
fn the_quickstart_names_the_column_and_the_table_it_would_lose() {
    // The printed diff is the demo. A gate that fired without saying what it
    // was protecting would be a spinner.
    let (mut engine, _dir) = engine();
    seed(&mut engine);

    let proposed = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::DropColumn {
            table: "customers".into(),
            column: "legacy_ref".into(),
        },
        agent(),
        100,
    );

    assert_eq!(proposed.affected_schema.table, "customers");
    assert_eq!(
        proposed.affected_schema.column.as_deref(),
        Some("legacy_ref")
    );
    assert!(
        proposed.destructive,
        "the diff has to say the change is destructive; `theta schema propose` \
         prints that line and it is the sentence the demo turns on"
    );
}

#[test]
fn confirming_the_quickstart_change_still_drops_the_column() {
    // The other half of the honesty. `docs/legal/TERMS.md` §2.1 says a change
    // you confirm is a change you asked for, and the quickstart would be
    // misleading if the gate turned out to be a wall rather than a question.
    let (mut engine, _dir) = engine();
    seed(&mut engine);

    let proposed = engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::DropColumn {
            table: "customers".into(),
            column: "legacy_ref".into(),
        },
        agent(),
        100,
    );

    engine
        .apply_schema_change(&proposed.change_id, true, agent(), 101)
        .expect("a confirmed drop applies");

    let described = engine.describe(
        BranchId::MAIN,
        &thetad::describe::DescribeRequest {
            table: "customers".into(),
            include_examples: false,
            example_limit: 0,
        },
    );
    let customers = described
        .tables
        .first()
        .expect("customers still exists after dropping one of its columns");
    assert!(
        !customers.columns.iter().any(|c| c.name == "legacy_ref"),
        "the column survived a confirmation, so the gate is a wall rather than \
         a question, and `docs/legal/TERMS.md` §2.1 describes something the \
         product does not do"
    );
}

/// The seeded shape here and the shape `theta demo` writes must not drift.
///
/// This test reads the CLI's source rather than calling it, because `demo`
/// needs a live control plane and a provisioned project. Reading is a weaker
/// check than running and it is the one available — what it rules out is the
/// specific, likely failure: somebody edits the demo's table or rows and the
/// gate assertions above quietly start testing a quickstart nobody runs.
#[test]
fn the_seeded_example_matches_what_the_cli_writes() {
    let source = include_str!("../../theta-cli/src/commands.rs");

    // Read the CLI's own constants rather than looking for substrings. An
    // earlier version of this test searched for `"customers"`, which appears in
    // three places in that file — so renaming the table in one of them left the
    // test green while the quickstart broke on its last command.
    let constant = |name: &str| -> String {
        source
            .split(&format!("const {name}: &str = \""))
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("commands.rs no longer declares {name}"))
            .to_string()
    };

    assert_eq!(
        constant("DEMO_TABLE"),
        "customers",
        "`theta demo` seeds a different table from the one this suite gates"
    );
    assert_eq!(
        constant("DEMO_COLUMN"),
        "legacy_ref",
        "`theta demo` offers to drop a different column from the one this \
         suite gates"
    );
    assert!(
        source.contains("field(\"email\", ValueType::Text, false)"),
        "the demo's table shape changed; this suite asserts a row count and a \
         gate against the shape it used to have"
    );

    // The row count the diff above asserts.
    let rows = source
        .split("let rows = [")
        .nth(1)
        .expect("the demo seeds a row table")
        .split("];")
        .next()
        .expect("terminated")
        .matches("(\"cus_")
        .count();
    assert_eq!(
        rows, DEMO_ROWS,
        "the demo seeds {rows} rows and this suite asserts {DEMO_ROWS}"
    );
}
