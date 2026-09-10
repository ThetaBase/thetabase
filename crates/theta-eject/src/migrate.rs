//! Driving a migration: schema, then rows, then verification.
//!
//! The orchestration lives here rather than in the CLI so that it can be
//! tested. A migration's ordering is the part most worth getting right — schema
//! before rows, the cursor after the batch it describes, verification against a
//! *re-read* of the source — and none of that is testable from inside a binary.
//!
//! What the target is stays abstract. The CLI's target is a `thetad` over the
//! wire; a test's is a materialized view in the same process. Both run the same
//! code, which is the point: an ordering that only holds in the in-process path
//! is an ordering nobody ships.

use theta_core::schema::SchemaChange;
use theta_core::Value;

use crate::import::{next_batch, Cursor};
use crate::plan::Plan;
use crate::reflect::Schema;
use crate::verify::{verify_row, verify_row_count, verify_schema, Report};
use crate::SourceClient;

/// What a target did with a proposed schema change.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaOutcome {
    /// Applied. The rules found nothing that needs a person.
    Applied,
    /// Held for review, with what the caller has to do next.
    ///
    /// A migration stops here rather than forcing it through. Creating a table
    /// is additive and should never be gated, so a gate at this point means the
    /// target already holds something this migration would change — and that is
    /// a decision for a person, not for a flag (invariant 5).
    Gated { table: String, next_step: String },
}

/// Somewhere a migration can write.
///
/// Deliberately three operations. A target that could do less could not be
/// verified; one that could do more would invite the migration to take
/// shortcuts the CLI's target cannot offer.
#[allow(async_fn_in_trait)]
pub trait Target {
    async fn apply_schema(&mut self, change: &SchemaChange) -> Result<SchemaOutcome, String>;
    async fn put(&mut self, key: &str, value: &Value) -> Result<(), String>;
    async fn get(&mut self, key: &str) -> Result<Option<Value>, String>;
}

/// Progress, reported as it happens.
///
/// A migration of any size runs for long enough that silence is
/// indistinguishable from a hang, and the operator's next decision — wait, or
/// kill it — depends on telling those apart.
pub trait Progress {
    fn table_progress(&mut self, table: &str, rows: u64);
    fn phase(&mut self, what: &str);
}

/// Ignores everything. For callers that only want the report.
pub struct Silent;

impl Progress for Silent {
    fn table_progress(&mut self, _table: &str, _rows: u64) {}
    fn phase(&mut self, _what: &str) {}
}

/// What a completed migration did.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub rows_written: u64,
    pub report: Report,
}

/// Run `plan` against `target`.
///
/// Stops at the first refusal rather than continuing. A partly-applied schema
/// with a full set of rows is a state neither database ever had, and it is
/// harder to reason about than a migration that stopped somewhere nameable.
pub async fn migrate(
    source: &SourceClient,
    plan: &Plan,
    reflected: &Schema,
    target: &mut impl Target,
    batch_size: usize,
    progress: &mut impl Progress,
) -> Result<Outcome, String> {
    progress.phase("schema");
    for change in plan.schema_changes() {
        match target.apply_schema(&change).await? {
            SchemaOutcome::Applied => {}
            SchemaOutcome::Gated { table, next_step } => {
                return Err(format!(
                    "the schema change on `{table}` was held for review. Nothing \
                     further was written.\n{next_step}"
                ));
            }
        }
    }

    progress.phase("rows");
    let mut rows_written = 0u64;
    for table in &plan.tables {
        let mut cursor = Cursor::default();
        loop {
            let batch = next_batch(source, table, &cursor, batch_size)
                .await
                .map_err(|e| e.to_string())?;
            if batch.is_empty() {
                break;
            }
            for row in &batch.rows {
                let key = format!("{}:{}", table.target_name, row.primary_key);
                target.put(&key, &row.value).await.map_err(|e| {
                    format!(
                        "{e}\n{rows_written} row(s) had already landed. Re-running \
                         resumes rather than duplicating, because a row is \
                         addressed by its primary key."
                    )
                })?;
                rows_written += 1;
            }
            // Advanced only after the rows it describes have been written. The
            // other order loses rows on a crash between the two, and a lost row
            // is found months later by whoever needed it.
            cursor = batch.cursor;
            progress.table_progress(&table.source_name, cursor.rows_imported);
        }
    }

    progress.phase("verifying");
    let mut report = Report::default();
    verify_schema(plan, reflected, &mut report);

    for table in &plan.tables {
        let mut cursor = Cursor::default();
        let mut in_source = 0usize;
        let mut in_target = 0usize;
        loop {
            // Re-read rather than reuse what was just written: comparing the
            // writer's output against the writer's input proves only that it
            // agrees with itself.
            let batch = next_batch(source, table, &cursor, batch_size)
                .await
                .map_err(|e| e.to_string())?;
            if batch.is_empty() {
                break;
            }
            for row in &batch.rows {
                in_source += 1;
                let key = format!("{}:{}", table.target_name, row.primary_key);
                let actual = target.get(&key).await?;
                if actual.is_some() {
                    in_target += 1;
                }
                verify_row(
                    table,
                    &row.primary_key,
                    &row.value,
                    actual.as_ref(),
                    &mut report,
                );
            }
            cursor = batch.cursor;
        }
        verify_row_count(&table.source_name, in_source, in_target, &mut report);
        report.rows_compared += in_source as u64;
        report.tables_compared += 1;
    }

    Ok(Outcome {
        rows_written,
        report,
    })
}
