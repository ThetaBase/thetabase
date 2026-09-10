//! The write path: schema, rows, verification, in that order.
//!
//! `migrate` is the part of a migration whose *ordering* has to be right, and
//! ordering is what a test of the finished state cannot see. So the target here
//! records what it was asked to do, and the tests assert on the sequence as
//! much as on the result.
//!
//! Live, like the rest of the M8 suite: it reads a real Postgres. What it
//! writes to is a materialized view in this process, which is the same
//! `Target` the CLI implements over the wire — the ordering under test is
//! shared, so an ordering that only held in one of the two would be a bug in
//! the one nobody tested.

use std::collections::BTreeMap;

use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, RowSource, Value};
use theta_eject::migrate::{migrate, Progress, SchemaOutcome, Silent, Target};
use theta_eject::{connect, plan, reflect};
use theta_storage::MaterializedView;

fn database_url() -> String {
    std::env::var("THETA_EJECT_TEST_URL")
        .unwrap_or_else(|_| "postgresql://thetabase:thetabase@127.0.0.1:55432/shopdb".to_string())
}

fn require_live() -> bool {
    std::env::var("THETA_REQUIRE_LIVE").is_ok_and(|v| !v.is_empty() && v != "0")
}

async fn client_or_skip() -> Option<theta_eject::SourceClient> {
    match connect(&database_url()).await {
        Ok(client) => Some(client),
        Err(e) if require_live() => panic!("THETA_REQUIRE_LIVE is set: {e}"),
        Err(e) => {
            eprintln!("skipping: no Postgres ({e})");
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

/// What the target was asked to do, in order.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Schema(String),
    Put(String),
    Get(String),
}

/// A materialized view behind the same trait the CLI implements over the wire.
struct Recording {
    view: MaterializedView,
    calls: Vec<Call>,
    /// Tables whose schema change is held for review, standing in for the
    /// Safety Layer gating something.
    gate: Option<String>,
    /// Keys whose write fails, standing in for a mid-migration failure.
    refuse: Option<String>,
}

impl Recording {
    fn new() -> Self {
        Self {
            view: MaterializedView::new(),
            calls: Vec::new(),
            gate: None,
            refuse: None,
        }
    }

    fn schema_calls(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| matches!(c, Call::Schema(_)))
            .count()
    }
    fn put_calls(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| matches!(c, Call::Put(_)))
            .count()
    }
    fn get_calls(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| matches!(c, Call::Get(_)))
            .count()
    }
}

impl Target for Recording {
    async fn apply_schema(&mut self, change: &SchemaChange) -> Result<SchemaOutcome, String> {
        let table = match change {
            SchemaChange::AddTable { table } => table.name.clone(),
            other => format!("{other:?}"),
        };
        self.calls.push(Call::Schema(table.clone()));

        if self.gate.as_deref() == Some(table.as_str()) {
            return Ok(SchemaOutcome::Gated {
                table,
                next_step: "theta schema confirm <id>".to_string(),
            });
        }
        self.view.apply(&entry(OpType::Schema {
            change: change.clone(),
        }));
        Ok(SchemaOutcome::Applied)
    }

    async fn put(&mut self, key: &str, value: &Value) -> Result<(), String> {
        self.calls.push(Call::Put(key.to_string()));
        if self.refuse.as_deref() == Some(key) {
            return Err(format!("refused `{key}`"));
        }
        self.view.apply(&entry(OpType::Put {
            key: key.to_string(),
            value: value.clone(),
        }));
        Ok(())
    }

    async fn get(&mut self, key: &str) -> Result<Option<Value>, String> {
        self.calls.push(Call::Get(key.to_string()));
        let (table, primary_key) = key.split_once(':').expect("addressed key");
        Ok(self.view.row(table, primary_key))
    }
}

async fn planned(client: &theta_eject::SourceClient) -> (reflect::Schema, plan::Plan) {
    let schema = reflect::reflect(client, "public").await.expect("reflect");
    let planned = plan::plan(&schema);
    (schema, planned)
}

#[tokio::test]
async fn a_migration_writes_the_schema_then_the_rows_then_reads_them_back() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, plan) = planned(&client).await;
    let mut target = Recording::new();

    let outcome = migrate(&client, &plan, &schema, &mut target, 2, &mut Silent)
        .await
        .expect("migrate");

    assert_eq!(outcome.rows_written, 6, "3 customers and 3 orders");
    assert!(
        outcome.report.is_clean(),
        "unexpected mismatches: {:#?}",
        outcome.report.unexpected().collect::<Vec<_>>()
    );

    // The ordering, which the finished state cannot show: every schema change
    // before every write, and every write before every read-back.
    let first_put = target
        .calls
        .iter()
        .position(|c| matches!(c, Call::Put(_)))
        .expect("something was written");
    let last_schema = target
        .calls
        .iter()
        .rposition(|c| matches!(c, Call::Schema(_)))
        .expect("schema was applied");
    let first_get = target
        .calls
        .iter()
        .position(|c| matches!(c, Call::Get(_)))
        .expect("something was verified");
    let last_put = target
        .calls
        .iter()
        .rposition(|c| matches!(c, Call::Put(_)))
        .expect("something was written");

    assert!(
        last_schema < first_put,
        "a row was written before the schema was in place"
    );
    assert!(
        last_put < first_get,
        "verification began before the last row had landed"
    );
    assert_eq!(target.schema_calls(), 2, "customers and orders");
    assert_eq!(target.put_calls(), 6);
    assert_eq!(target.get_calls(), 6, "every row is read back");
}

#[tokio::test]
async fn a_gated_schema_change_stops_the_migration_before_any_row_is_written() {
    // The safety-critical path. A migration that forced its schema through the
    // gate would be the one caller allowed to skip the review everything else
    // submits to — and would then write rows against a schema nobody approved.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, plan) = planned(&client).await;

    let mut target = Recording::new();
    target.gate = Some("customers".to_string());

    let error = migrate(&client, &plan, &schema, &mut target, 100, &mut Silent)
        .await
        .expect_err("a gated change must stop the migration");

    assert!(error.contains("customers"), "got: {error}");
    assert!(
        error.contains("theta schema confirm"),
        "the error has to name what to do next, got: {error}"
    );
    assert_eq!(
        target.put_calls(),
        0,
        "rows were written against a schema that was never approved"
    );
}

#[tokio::test]
async fn a_refused_write_stops_and_says_what_had_already_landed() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, plan) = planned(&client).await;

    let mut target = Recording::new();
    target.refuse = Some("customers:2".to_string());

    let error = migrate(&client, &plan, &schema, &mut target, 100, &mut Silent)
        .await
        .expect_err("a refused write must stop the migration");

    assert!(error.contains("customers:2"), "got: {error}");
    // The operator's next question is "what do I do now", and the answer is
    // "run it again" only if re-running is safe. It is, and the message says so.
    assert!(
        error.contains("resumes rather than duplicating"),
        "the error has to say re-running is safe, got: {error}"
    );
    assert!(
        error.contains("1 row(s) had already landed"),
        "the error has to say how far it got, got: {error}"
    );
}

#[tokio::test]
async fn a_migration_run_twice_ends_with_the_same_rows() {
    // Which is what makes the "just run it again" advice above true. Every
    // write is addressed by its primary key, so a repeat is an update.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, plan) = planned(&client).await;

    let mut target = Recording::new();
    migrate(&client, &plan, &schema, &mut target, 2, &mut Silent)
        .await
        .expect("first run");
    let after_first = target.view.scan("customers");

    let outcome = migrate(&client, &plan, &schema, &mut target, 2, &mut Silent)
        .await
        .expect("second run");
    let after_second = target.view.scan("customers");

    assert_eq!(
        after_first, after_second,
        "running the migration twice changed the data"
    );
    assert!(outcome.report.is_clean(), "the second run did not verify");
}

#[tokio::test]
async fn progress_is_reported_for_every_table_as_it_goes() {
    // A migration of any size runs long enough that silence and a hang look
    // identical, and the operator's next decision depends on telling them apart.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let (schema, plan) = planned(&client).await;

    #[derive(Default)]
    struct Recorder {
        phases: Vec<String>,
        tables: BTreeMap<String, u64>,
    }
    impl Progress for Recorder {
        fn phase(&mut self, what: &str) {
            self.phases.push(what.to_string());
        }
        fn table_progress(&mut self, table: &str, rows: u64) {
            self.tables.insert(table.to_string(), rows);
        }
    }

    let mut progress = Recorder::default();
    let mut target = Recording::new();
    migrate(&client, &plan, &schema, &mut target, 2, &mut progress)
        .await
        .expect("migrate");

    assert_eq!(progress.phases, vec!["schema", "rows", "verifying"]);
    assert_eq!(progress.tables.get("customers"), Some(&3));
    assert_eq!(progress.tables.get("orders"), Some(&3));
}
