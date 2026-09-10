//! Schema-semantics verification.
//!
//! `05-prd.md` §52 asks for a pass "comparing before/after schema semantics —
//! flags mismatches for human review rather than assuming a clean 1:1 mapping",
//! and `08-test-validation-plan.md` §6 gates the milestone on **zero
//! data-meaning mismatches undetected by the verification pass**.
//!
//! Note what that gate measures: not "no mismatches", but "no *undetected*
//! mismatches". A migration is allowed to change meaning — `numeric` to `Float`
//! always does — and is not allowed to change it quietly. So the thing under
//! test is this pass's ability to notice, and the suite that gates it plants
//! mismatches deliberately and asserts each one is found.
//!
//! Two halves, because they fail differently:
//!
//! * **Schema semantics** — is the target's *shape* faithful? Nullability,
//!   uniqueness, bounds, key structure. These break silently: nothing looks
//!   wrong until something the source would have refused is accepted.
//! * **Data** — did the *values* survive? Row counts, and a value-by-value
//!   comparison of the source against what was written.

use serde::{Deserialize, Serialize};
use theta_core::{RowSource, Value};

use crate::plan::{Plan, TablePlan};
use crate::types::Fidelity;

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Meaning changed, and the change was predicted by the plan. The operator
    /// was told before the migration ran and is being told again.
    Expected,
    /// Meaning changed in a way the plan did not predict. This is a bug in the
    /// mapping or in the import, and it is what the gate exists to catch.
    Unexpected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub rows_compared: u64,
    pub tables_compared: u64,
}

impl Report {
    /// Findings the plan did not predict. The gate is that this is empty.
    pub fn unexpected(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Unexpected)
    }

    pub fn is_clean(&self) -> bool {
        self.unexpected().count() == 0
    }

    fn flag(
        &mut self,
        severity: Severity,
        table: &str,
        column: Option<&str>,
        primary_key: Option<&str>,
        message: String,
    ) {
        self.findings.push(Finding {
            severity,
            table: table.to_string(),
            column: column.map(str::to_string),
            primary_key: primary_key.map(str::to_string),
            message,
        });
    }
}

/// Flag a row-count mismatch.
///
/// Split out so a caller counting rows over the wire reports it identically to
/// one scanning in process. Counting is the cheapest check and catches the
/// worst failure: rows that never arrived, or arrived twice.
pub fn verify_row_count(table: &str, source: usize, target: usize, report: &mut Report) {
    if source != target {
        report.flag(
            Severity::Unexpected,
            table,
            None,
            None,
            format!("row count differs: Postgres has {source}, ThetaBase has {target}"),
        );
    }
}

/// One row as the source held it, for comparison.
///
/// The verifier reads the source a second time rather than trusting what the
/// importer believed it read. Comparing the importer's output against the
/// importer's input would only prove the writer agrees with itself.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub primary_key: String,
    pub value: Value,
}

/// Compare the target against the source.
///
/// `source_rows` is what Postgres holds, re-read; `target` is what ThetaBase now
/// holds. Both are addressed by the same rendered primary key.
pub fn verify_table(
    plan: &TablePlan,
    source_rows: &[SourceRow],
    target: &impl RowSource,
    report: &mut Report,
) {
    report.tables_compared += 1;

    let target_rows = target.scan(&plan.target_name);
    if target_rows.len() != source_rows.len() {
        // Counting is the cheapest check and catches the worst failure: rows
        // that never arrived, or arrived twice.
        report.flag(
            Severity::Unexpected,
            &plan.source_name,
            None,
            None,
            format!(
                "row count differs: Postgres has {}, ThetaBase has {}",
                source_rows.len(),
                target_rows.len()
            ),
        );
    }

    for source in source_rows {
        report.rows_compared += 1;
        let actual = target.row(&plan.target_name, &source.primary_key);
        verify_row(
            plan,
            &source.primary_key,
            &source.value,
            actual.as_ref(),
            report,
        );
    }
}

/// Compare one migrated row against the source.
///
/// Public because the target is not always something that can be scanned. Over
/// the wire there is no `RowSource` — the CLI fetches each row with a `Get` and
/// brings it here — and a verification pass that only worked in-process would
/// be verifying a code path nobody runs against a real instance.
///
/// `actual` of `None` means the row is not there at all, which is always
/// unexpected: the plan can predict that a *value* will change, never that a
/// row will vanish.
pub fn verify_row(
    plan: &TablePlan,
    primary_key: &str,
    expected: &Value,
    actual: Option<&Value>,
    report: &mut Report,
) {
    let Some(actual) = actual else {
        report.flag(
            Severity::Unexpected,
            &plan.source_name,
            None,
            Some(primary_key),
            "row is in Postgres and not in ThetaBase".to_string(),
        );
        return;
    };
    compare_row(plan, primary_key, expected, actual, report);
}

fn compare_row(
    plan: &TablePlan,
    primary_key: &str,
    expected: &Value,
    actual: &Value,
    report: &mut Report,
) {
    for column in &plan.columns {
        let want = field(expected, &column.target_name);
        let got = field(actual, &column.target_name);

        if want == got {
            continue;
        }

        // A mapping the plan called lossy is allowed to differ, and is still
        // reported - the operator agreed to a risk, not to being kept in the
        // dark about where it landed.
        let severity = if column.mapping.fidelity == Fidelity::Lossy {
            Severity::Expected
        } else {
            Severity::Unexpected
        };

        report.flag(
            severity,
            &plan.source_name,
            Some(&column.source_name),
            Some(primary_key),
            format!("Postgres has {want:?}, ThetaBase has {got:?}"),
        );
    }
}

/// One field of a row, treating absent and null as the same thing.
///
/// The importer omits nulls rather than storing them, so "not present" and
/// "present and null" have to compare equal or every nullable column with a
/// null would look like a mismatch.
fn field(row: &Value, column: &str) -> Value {
    match row {
        Value::Map(fields) => fields.get(column).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Compare the *shape* of source and target, independently of any row.
///
/// Everything here is a constraint the source enforced. Losing one does not
/// corrupt the data that already migrated; it stops the database refusing the
/// next bad write, which is why these are worth naming individually rather than
/// summarising as "constraints not carried".
pub fn verify_schema(plan: &Plan, reflected: &crate::reflect::Schema, report: &mut Report) {
    for table in &plan.tables {
        let Some(source) = reflected.table(&table.source_name) else {
            report.flag(
                Severity::Unexpected,
                &table.source_name,
                None,
                None,
                "planned table is not in the reflected schema".to_string(),
            );
            continue;
        };

        if source.primary_key != table.primary_key {
            report.flag(
                Severity::Unexpected,
                &table.source_name,
                None,
                None,
                format!(
                    "primary key differs: Postgres {:?}, plan {:?}",
                    source.primary_key, table.primary_key
                ),
            );
        }

        for column in &source.columns {
            let Some(planned) = table.columns.iter().find(|c| c.source_name == column.name) else {
                report.flag(
                    Severity::Unexpected,
                    &table.source_name,
                    Some(&column.name),
                    None,
                    "column exists in Postgres and is not in the plan".to_string(),
                );
                continue;
            };

            if planned.nullable != column.nullable {
                report.flag(
                    Severity::Unexpected,
                    &table.source_name,
                    Some(&column.name),
                    None,
                    format!(
                        "nullability differs: Postgres nullable={}, plan nullable={}",
                        column.nullable, planned.nullable
                    ),
                );
            }
        }

        // Constraints the target does not enforce. Expected, because the plan
        // warned about each one before the migration ran.
        for unique in &source.unique_constraints {
            report.flag(
                Severity::Expected,
                &table.source_name,
                Some(&unique.columns.join(", ")),
                None,
                format!(
                    "UNIQUE `{}` is not enforced in ThetaBase; it travels as an index",
                    unique.name
                ),
            );
        }
        for column in &source.columns {
            if let Some(limit) = column.character_maximum_length {
                report.flag(
                    Severity::Expected,
                    &table.source_name,
                    Some(&column.name),
                    None,
                    format!("length limit of {limit} is not enforced in ThetaBase"),
                );
            }
            if column.has_default {
                report.flag(
                    Severity::Expected,
                    &table.source_name,
                    Some(&column.name),
                    None,
                    "column default does not travel; later inserts omitting it \
                     leave it unset"
                        .to_string(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_field_and_an_explicit_null_compare_equal() {
        let absent = Value::Map(Default::default());
        let explicit = Value::Map([("a".to_string(), Value::Null)].into_iter().collect());
        assert_eq!(field(&absent, "a"), field(&explicit, "a"));
    }

    #[test]
    fn a_report_with_only_expected_findings_is_clean() {
        let mut report = Report::default();
        report.flag(
            Severity::Expected,
            "t",
            None,
            None,
            "numeric became a float".into(),
        );
        assert!(
            report.is_clean(),
            "an expected change must not fail the gate"
        );

        report.flag(
            Severity::Unexpected,
            "t",
            None,
            None,
            "a row vanished".into(),
        );
        assert!(!report.is_clean());
    }
}
