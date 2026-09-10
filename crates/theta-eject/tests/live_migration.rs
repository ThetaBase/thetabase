//! End-to-end migration against a real Postgres (ROADMAP M8 gate).
//!
//! `08-test-validation-plan.md` §6 gates M8 on **zero data-meaning mismatches
//! undetected by the verification pass** on a real migration. Read that
//! carefully: the gate is not "no mismatches". A migration is *allowed* to
//! change meaning — `numeric` to `Float` always does — and is not allowed to
//! change it quietly. So what is under test is the verification pass's ability
//! to notice, and asserting only that a clean migration comes back clean would
//! pass just as well if the pass returned "clean" unconditionally.
//!
//! So this suite does both halves:
//!
//! 1. Migrate a schema built out of the cases that actually go wrong — a
//!    composite key, a table with no key at all, `numeric`, arrays containing
//!    commas and the literal text `NULL`, `jsonb`, `bytea`, timestamps before
//!    the epoch and after 2038 — and assert nothing unexpected is reported.
//! 2. Then damage the migrated data, one way at a time, and assert the pass
//!    catches every one. A detector is only worth the failures it can find.
//!
//! Live, like `archive-live`: it needs a Postgres. `THETA_REQUIRE_LIVE=1`
//! turns a skip into a failure, because a suite that skipped for want of a
//! database reports the same green as one that passed.

use std::collections::BTreeMap;

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, RowSource, Value};
use theta_eject::import::{next_batch, Cursor};
use theta_eject::verify::{verify_schema, verify_table, Report, Severity, SourceRow};
use theta_eject::{connect, plan, reflect};
use theta_storage::MaterializedView;

/// Where the test Postgres is. Overridable so this can be pointed at a real
/// project, which is what the gate is ultimately about.
fn database_url() -> String {
    std::env::var("THETA_EJECT_TEST_URL")
        .unwrap_or_else(|_| "postgresql://thetabase:thetabase@127.0.0.1:55432/shopdb".to_string())
}

fn require_live() -> bool {
    std::env::var("THETA_REQUIRE_LIVE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Connect, or explain the skip. `None` means the suite did not run.
async fn client_or_skip() -> Option<tokio_postgres::Client> {
    match connect(&database_url()).await {
        Ok(client) => Some(client),
        Err(e) if require_live() => {
            panic!("THETA_REQUIRE_LIVE is set and Postgres is unreachable: {e}")
        }
        Err(e) => {
            eprintln!(
                "skipping: no Postgres at {} ({e}).\n  \
                 docker run -d --name thetabase-eject-pg -e POSTGRES_PASSWORD=thetabase \\\n    \
                 -e POSTGRES_USER=thetabase -e POSTGRES_DB=shopdb -p 55432:5432 postgres:16",
                database_url()
            );
            None
        }
    }
}

fn entry(op: OpType) -> LogEntry {
    LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(0),
        branch_id: BranchId::MAIN,
        op,
        author: Author::System,
        timestamp_ms: 0,
    }
}

/// Run the whole migration into a fresh view, in batches of `batch_size`.
///
/// Returns the view and the per-table cursors, so a test can assert on how far
/// the import got as well as on what it produced.
async fn migrate(
    client: &tokio_postgres::Client,
    plan: &plan::Plan,
    batch_size: usize,
) -> (MaterializedView, BTreeMap<String, Cursor>) {
    let mut view = MaterializedView::new();
    for change in plan.schema_changes() {
        view.apply(&entry(OpType::Schema { change }));
    }

    let mut cursors = BTreeMap::new();
    for table in &plan.tables {
        let mut cursor = Cursor::default();
        loop {
            let batch = next_batch(client, table, &cursor, batch_size)
                .await
                .expect("read a batch");
            if batch.is_empty() {
                break;
            }
            for row in &batch.rows {
                view.apply(&entry(OpType::Put {
                    key: format!("{}:{}", table.target_name, row.primary_key),
                    value: row.value.clone(),
                }));
            }
            // Cursor committed *after* the rows it describes. The other order
            // loses rows on a crash between the two.
            cursor = batch.cursor;
        }
        cursors.insert(table.source_name.clone(), cursor);
    }
    (view, cursors)
}

/// Re-read the source for comparison.
///
/// Deliberately a second read rather than reusing what the importer produced:
/// comparing the writer's output against the writer's input only proves it
/// agrees with itself.
async fn source_rows(client: &tokio_postgres::Client, table: &plan::TablePlan) -> Vec<SourceRow> {
    let mut rows = Vec::new();
    let mut cursor = Cursor::default();
    loop {
        let batch = next_batch(client, table, &cursor, 500)
            .await
            .expect("read source");
        if batch.is_empty() {
            break;
        }
        for row in &batch.rows {
            rows.push(SourceRow {
                primary_key: row.primary_key.clone(),
                value: row.value.clone(),
            });
        }
        cursor = batch.cursor;
    }
    rows
}

async fn reflected_and_planned(client: &tokio_postgres::Client) -> (reflect::Schema, plan::Plan) {
    let schema = reflect::reflect(client, "public").await.expect("reflect");
    let planned = plan::plan(&schema);
    (schema, planned)
}

#[tokio::test]
async fn reflection_finds_the_shape_postgres_actually_has() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, _) = reflected_and_planned(&client).await;

    let customers = schema.table("customers").expect("customers reflected");
    assert_eq!(customers.primary_key, vec!["id"]);
    assert!(
        customers.columns.iter().any(|c| c.udt_name == "numeric"),
        "the numeric column has to be reflected as numeric, not as its base type"
    );
    assert!(
        customers
            .unique_constraints
            .iter()
            .any(|u| u.columns == ["email"]),
        "the UNIQUE on email was not reflected: {:?}",
        customers.unique_constraints
    );
    assert!(
        customers
            .columns
            .iter()
            .find(|c| c.name == "email")
            .is_some_and(|c| c.character_maximum_length == Some(120)),
        "varchar(120) has to keep its bound, or nothing can warn that it is lost"
    );

    // Composite key, in key order. Reversed would silently re-address rows.
    let orders = schema.table("orders").expect("orders reflected");
    assert_eq!(orders.primary_key, vec!["customer_id", "seq"]);
}

#[tokio::test]
async fn a_table_without_a_primary_key_blocks_rather_than_getting_one_invented() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (_, planned) = reflected_and_planned(&client).await;

    let blocker = planned
        .blockers
        .iter()
        .find(|b| b.table == "audit_log")
        .expect("audit_log has no primary key and must block");
    assert!(
        blocker.message.contains("primary key"),
        "got: {}",
        blocker.message
    );
    assert!(!planned.is_runnable(), "a blocker must stop the run");
    assert!(
        planned.table("audit_log").is_none(),
        "a blocked table must not also be planned"
    );
}

#[tokio::test]
async fn the_plan_warns_about_every_change_of_meaning_before_anything_runs() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (_, planned) = reflected_and_planned(&client).await;

    let said = |table: &str, needle: &str| {
        planned
            .warnings
            .iter()
            .any(|w| w.table == table && w.message.contains(needle))
    };

    assert!(
        said("customers", "arbitrary-precision"),
        "numeric -> Float is the classic silent money bug and must be warned about"
    );
    assert!(
        said("orders", "midnight UTC"),
        "a date gains a precision it did not have"
    );
    assert!(
        said("customers", "does not reject a duplicate"),
        "a UNIQUE that stops being enforced has to be called out"
    );
    assert!(
        said("customers", "120 characters"),
        "a length limit that stops being enforced has to be called out"
    );
    assert!(
        said("customers", "column default"),
        "defaults do not travel"
    );
    assert!(
        said("customers", "Counter"),
        "view_count is counter-shaped and should suggest a Counter"
    );
}

#[tokio::test]
async fn a_real_migration_verifies_with_nothing_unexpected() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, planned) = reflected_and_planned(&client).await;
    let (view, _) = migrate(&client, &planned, 2).await;

    let mut report = Report::default();
    verify_schema(&planned, &schema, &mut report);
    for table in &planned.tables {
        let rows = source_rows(&client, table).await;
        verify_table(table, &rows, &view, &mut report);
    }

    let unexpected: Vec<_> = report.unexpected().collect();
    assert!(
        unexpected.is_empty(),
        "a faithful migration reported {} unexpected mismatches: {unexpected:#?}",
        unexpected.len()
    );
    assert!(report.rows_compared >= 6, "nothing was actually compared");
    assert_eq!(report.tables_compared, 2, "customers and orders");

    // And the expected findings are present, because a pass that reports
    // nothing at all is indistinguishable from one that checked nothing.
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Expected),
        "the known losses should still be reported"
    );
}

#[tokio::test]
async fn the_awkward_values_survive_exactly() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (_, planned) = reflected_and_planned(&client).await;
    let (view, _) = migrate(&client, &planned, 100).await;

    let row = view.row("customers", "1").expect("customer 1 migrated");
    let Value::Map(fields) = &row else {
        panic!("a row must be a map, got {row:?}")
    };

    assert_eq!(
        fields.get("email"),
        Some(&Value::Text("ada@example.com".into()))
    );
    assert_eq!(fields.get("is_active"), Some(&Value::Bool(true)));
    assert_eq!(fields.get("view_count"), Some(&Value::Int(42)));
    assert_eq!(
        fields.get("signup_at"),
        Some(&Value::Timestamp(1_706_704_496_789)),
        "a timestamptz has to land on the exact instant"
    );
    assert_eq!(
        fields.get("avatar"),
        Some(&Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]))
    );
    assert_eq!(
        fields.get("tags"),
        Some(&Value::List(vec![
            Value::Text("vip".into()),
            Value::Text("early".into())
        ]))
    );
    assert_eq!(
        fields.get("prefs"),
        Some(&Value::Map(
            [
                ("theme".to_string(), Value::Text("dark".into())),
                ("n".to_string(), Value::Int(3)),
            ]
            .into_iter()
            .collect()
        ))
    );

    // A null column is absent rather than stored as null: "unset" and "set to
    // null" must not become the same thing.
    let row2 = view.row("customers", "2").expect("customer 2 migrated");
    let Value::Map(fields2) = &row2 else {
        panic!("expected a map")
    };
    assert!(
        !fields2.contains_key("display_name"),
        "a NULL was stored rather than omitted"
    );
    assert_eq!(
        fields2.get("signup_at"),
        Some(&Value::Timestamp(-86_400_000)),
        "a pre-epoch timestamp must be negative, not clamped"
    );

    // The array whose elements contain a comma and the literal text NULL - the
    // two cases a naive array parser gets wrong.
    let row3 = view.row("customers", "3").expect("customer 3 migrated");
    let Value::Map(fields3) = &row3 else {
        panic!("expected a map")
    };
    assert_eq!(
        fields3.get("tags"),
        Some(&Value::List(vec![
            Value::Text("a,b".into()),
            Value::Text("NULL".into()),
        ])),
        "a quoted comma is part of an element, and a quoted NULL is text"
    );
    assert_eq!(
        fields3.get("signup_at"),
        Some(&Value::Timestamp(1_907_798_399_999)),
        "a post-2038 timestamp must not wrap"
    );
}

#[tokio::test]
async fn a_composite_key_addresses_each_row_distinctly() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (_, planned) = reflected_and_planned(&client).await;
    let (view, _) = migrate(&client, &planned, 100).await;

    let orders = view.scan("orders");
    assert_eq!(orders.len(), 3, "three orders, three rows: {orders:?}");
    let keys: Vec<&String> = orders.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys.len(),
        keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
        "composite keys collided into one row: {keys:?}"
    );
}

#[tokio::test]
async fn an_interrupted_migration_resumes_without_losing_or_duplicating_rows() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (_, planned) = reflected_and_planned(&client).await;
    let customers = planned.table("customers").expect("planned");

    // Import one batch, then stop as though the process died.
    let first = next_batch(&client, customers, &Cursor::default(), 2)
        .await
        .expect("first batch");
    assert_eq!(first.rows.len(), 2);

    let mut view = MaterializedView::new();
    for change in planned.schema_changes() {
        view.apply(&entry(OpType::Schema { change }));
    }
    let apply = |view: &mut MaterializedView, rows: &[theta_eject::import::ImportedRow]| {
        for row in rows {
            view.apply(&entry(OpType::Put {
                key: format!("customers:{}", row.primary_key),
                value: row.value.clone(),
            }));
        }
    };
    apply(&mut view, &first.rows);

    // Resume from the committed cursor. Re-reading the last batch is allowed -
    // that is what at-least-once means - so replay it deliberately and assert
    // the row count is unaffected, which is the property idempotence buys.
    apply(&mut view, &first.rows);

    let mut cursor = first.cursor.clone();
    loop {
        let batch = next_batch(&client, customers, &cursor, 2)
            .await
            .expect("resumed batch");
        if batch.is_empty() {
            break;
        }
        apply(&mut view, &batch.rows);
        cursor = batch.cursor;
    }

    assert_eq!(
        view.scan("customers").len(),
        3,
        "a resumed migration must end with the source's row count, not more"
    );
    assert_eq!(cursor.rows_imported, 3, "the cursor lost count");

    // And it agrees with an uninterrupted run.
    let (whole, _) = migrate(&client, &planned, 100).await;
    let mut resumed_rows = view.scan("customers");
    let mut whole_rows = whole.scan("customers");
    resumed_rows.sort_by(|a, b| a.0.cmp(&b.0));
    whole_rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        resumed_rows, whole_rows,
        "an interrupted-and-resumed migration differs from an uninterrupted one"
    );
}

// ---- the half that matters: can the pass actually catch anything? -----------

/// Migrate, then let `damage` corrupt the result, and return what verification
/// reported. Every case below plants exactly one fault.
async fn detect(damage: impl FnOnce(&mut MaterializedView, &plan::Plan)) -> Report {
    let client = connect(&database_url()).await.expect("postgres");
    let (schema, planned) = reflected_and_planned(&client).await;
    let (mut view, _) = migrate(&client, &planned, 100).await;

    damage(&mut view, &planned);

    let mut report = Report::default();
    verify_schema(&planned, &schema, &mut report);
    for table in &planned.tables {
        let rows = source_rows(&client, table).await;
        verify_table(table, &rows, &view, &mut report);
    }
    report
}

#[tokio::test]
async fn a_missing_row_is_detected() {
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        view.apply(&entry(OpType::Delete {
            key: "customers:2".into(),
        }));
    })
    .await;

    let found: Vec<_> = report.unexpected().collect();
    assert!(
        found.iter().any(|f| f.message.contains("not in ThetaBase")),
        "a row that never arrived went unreported: {found:#?}"
    );
    assert!(
        found
            .iter()
            .any(|f| f.message.contains("row count differs")),
        "the row count should also have caught it: {found:#?}"
    );
}

#[tokio::test]
async fn an_extra_row_is_detected() {
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        view.apply(&entry(OpType::Put {
            key: "customers:999".into(),
            value: Value::Map(
                [("email".to_string(), Value::Text("ghost@example.com".into()))]
                    .into_iter()
                    .collect(),
            ),
        }));
    })
    .await;

    assert!(
        report
            .unexpected()
            .any(|f| f.message.contains("row count differs")),
        "a row ThetaBase has and Postgres does not went unreported"
    );
}

#[tokio::test]
async fn a_changed_value_in_an_exactly_mapped_column_is_detected() {
    // The case the gate is really about: data that arrived, and arrived wrong.
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        view.apply(&entry(OpType::Put {
            key: "customers:1".into(),
            value: Value::Map(
                [(
                    "email".to_string(),
                    Value::Text("tampered@example.com".into()),
                )]
                .into_iter()
                .collect(),
            ),
        }));
    })
    .await;

    let found: Vec<_> = report.unexpected().collect();
    assert!(
        found
            .iter()
            .any(|f| f.column.as_deref() == Some("email") && f.primary_key.as_deref() == Some("1")),
        "a changed email went unreported: {found:#?}"
    );
}

#[tokio::test]
async fn a_truncated_timestamp_is_detected() {
    // Off by a millisecond. The kind of error a round-trip test with tidy
    // fixtures never produces and a real migration does.
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        let Some(Value::Map(mut fields)) = view.row("customers", "1") else {
            panic!("customer 1 must exist")
        };
        fields.insert("signup_at".to_string(), Value::Timestamp(1_706_704_496_000));
        view.apply(&entry(OpType::Put {
            key: "customers:1".into(),
            value: Value::Map(fields),
        }));
    })
    .await;

    assert!(
        report
            .unexpected()
            .any(|f| f.column.as_deref() == Some("signup_at")),
        "a timestamp silently rounded to the second went unreported"
    );
}

#[tokio::test]
async fn a_dropped_column_is_detected() {
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        let Some(Value::Map(mut fields)) = view.row("customers", "1") else {
            panic!("customer 1 must exist")
        };
        fields.remove("tags");
        view.apply(&entry(OpType::Put {
            key: "customers:1".into(),
            value: Value::Map(fields),
        }));
    })
    .await;

    assert!(
        report
            .unexpected()
            .any(|f| f.column.as_deref() == Some("tags")),
        "a column that vanished from a row went unreported"
    );
}

#[tokio::test]
async fn a_lossy_column_changing_is_reported_but_does_not_fail_the_gate() {
    // `numeric` cannot survive as a float, the plan said so before the run, and
    // the operator agreed. Reporting it as unexpected would mean the gate could
    // never pass on any schema containing money; not reporting it at all would
    // leave the operator guessing where the loss landed.
    if client_or_skip().await.is_none() {
        return;
    }
    let report = detect(|view, _| {
        let Some(Value::Map(mut fields)) = view.row("customers", "1") else {
            panic!("customer 1 must exist")
        };
        fields.insert("balance".to_string(), Value::Float(1234.5599999));
        view.apply(&entry(OpType::Put {
            key: "customers:1".into(),
            value: Value::Map(fields),
        }));
    })
    .await;

    assert!(
        report
            .findings
            .iter()
            .any(|f| f.column.as_deref() == Some("balance") && f.severity == Severity::Expected),
        "the drift in a lossy column should be reported as expected"
    );
    assert!(
        report.is_clean(),
        "a predicted loss must not fail the gate: {:#?}",
        report.unexpected().collect::<Vec<_>>()
    );
}

// ---- excluding a table ------------------------------------------------------
//
// The blocker for a keyless table says "add a primary key, or exclude the
// table", and for a while there was no way to exclude one - the error told the
// operator to do something the tool could not do.

#[tokio::test]
async fn excluding_the_keyless_table_makes_the_plan_runnable() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let schema = reflect::reflect(&client, "public").await.expect("reflect");
    let planned = plan::plan_excluding(&schema, &["audit_log".to_string()]);

    assert!(
        planned.is_runnable(),
        "excluding the keyless table should clear the blocker: {:?}",
        planned.blockers
    );
    assert!(planned.table("audit_log").is_none());
    assert_eq!(
        planned.tables.len(),
        2,
        "customers and orders still migrate"
    );
}

#[tokio::test]
async fn an_excluded_table_is_reported_rather_than_quietly_skipped() {
    // "Migrated" and "migrated except the one you skipped" are different
    // claims, and the difference is what someone forgets six months later.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let schema = reflect::reflect(&client, "public").await.expect("reflect");
    let planned = plan::plan_excluding(&schema, &["audit_log".to_string()]);

    assert!(
        planned
            .warnings
            .iter()
            .any(|w| w.table == "audit_log" && w.message.contains("stay in Postgres")),
        "an exclusion has to appear in the warnings: {:?}",
        planned.warnings
    );
}

#[tokio::test]
async fn excluding_a_table_that_does_not_exist_is_an_error_not_a_no_op() {
    // A typo in `--exclude` would otherwise look like it worked, right up until
    // the table it was meant to skip blocked the run.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let schema = reflect::reflect(&client, "public").await.expect("reflect");
    let planned = plan::plan_excluding(&schema, &["audit_logs".to_string()]);

    assert!(
        planned
            .blockers
            .iter()
            .any(|b| b.table == "audit_logs" && b.message.contains("not in this schema")),
        "a mistyped exclusion has to be caught: {:?}",
        planned.blockers
    );
}
