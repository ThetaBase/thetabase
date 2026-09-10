//! Shadow-branch validation (`07-agent-safety-layer.md` §5).
//!
//! The strongest gate says confirmation is not sufficient. What has to happen
//! instead is four steps, and each of them has to actually happen:
//!
//! 1. the change is applied to an ephemeral branch, not the target;
//! 2. verification runs against that branch;
//! 3. the results are surfaced — pass or fail, with samples;
//! 4. only on explicit promotion does it land, **by merge**, so what ships is
//!    exactly what was validated rather than a re-execution that might not be.
//!
//! Before this module, opening a shadow branch was enough on its own: the
//! branch was created empty, nothing was applied to it, no verification ran,
//! and the engine then accepted a confirmation. The strongest gate in the
//! product was one extra RPC call.
//!
//! # What verification means here
//!
//! The spec allows "the app's test suite, or a lightweight sampled query
//! comparison of before/after". Running someone's test suite is not something
//! the engine can do. The sampled comparison is, and it answers the question a
//! reviewer actually has: *what is different, and is any of it a surprise?*
//!
//! Every check is a deterministic function of the two views. No model, no
//! heuristic, nothing that could be argued with — the same rule the classifier
//! lives by (`docs/INVARIANTS.md`, invariant 2).

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;
use theta_core::{BranchId, ContentHash, RowAddress, Value};
use theta_safety::diff::ChangeId;
use theta_storage::MaterializedView;

/// Rows sampled per check.
///
/// Bounded because the result is read by a human. Two hundred sample diffs is
/// not a review, it is a scroll.
pub const SAMPLE_LIMIT: usize = 20;

/// One shadow branch, the change on it, and what validating it found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowValidation {
    pub change_id: ChangeId,
    pub target: BranchId,
    pub shadow: BranchId,
    /// Head of the shadow branch once the change was applied to it.
    pub applied_head: ContentHash,
    pub opened_at_ms: i64,
    /// `None` until verification has been run.
    pub outcome: Option<Validation>,
}

impl ShadowValidation {
    /// Whether this validation still describes what promotion would ship.
    ///
    /// A result that was recorded against an earlier head is stale: something
    /// was written to the shadow branch afterwards, and promoting would merge
    /// content nothing verified. Promoting on a stale pass is the same failure
    /// as promoting with no pass at all, only harder to notice.
    pub fn is_current(&self, shadow_head: ContentHash) -> bool {
        match &self.outcome {
            Some(outcome) => outcome.validated_head == shadow_head,
            None => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Validation {
    pub passed: bool,
    pub ran_at_ms: i64,
    /// The shadow head the checks ran against.
    pub validated_head: ContentHash,
    pub checks: Vec<Check>,
}

impl Validation {
    /// One line a human can act on, in the register `07-agent-safety-layer.md`
    /// §8 asks for.
    pub fn summary(&self) -> String {
        let failed: Vec<&str> = self
            .checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.name.as_str())
            .collect();

        match failed.as_slice() {
            [] => format!("validation passed ({} checks)", self.checks.len()),
            names => format!("validation failed: {}", names.join(", ")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub passed: bool,
    /// Plain language. The reader is doing a five-minute review.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<Sample>,
}

/// One row, before and after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub key: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// Run the verification suite for `change` by comparing `target` against
/// `shadow`.
pub fn validate(
    target: &MaterializedView,
    shadow: &MaterializedView,
    change: &SchemaChange,
) -> Vec<Check> {
    vec![
        change_took_effect(target, shadow, change),
        only_the_named_table_changed(target, shadow, change),
        row_count_matches_the_change(target, shadow, change),
    ]
}

/// Did the change do the specific thing it says it does?
///
/// Not "did anything at all move". Folding a change can shift the schema
/// incidentally — declaring a column drop on an undeclared table creates the
/// table entry on the way past — so "the schema differs" would pass for a drop
/// of a column that was never there. A validation that passes because nothing
/// happened is worse than no validation: it leaves a record saying the change
/// was checked.
fn change_took_effect(
    target: &MaterializedView,
    shadow: &MaterializedView,
    change: &SchemaChange,
) -> Check {
    let declared = |view: &MaterializedView, table: &str, column: &str| {
        view.schema.field(table, column).is_some()
    };

    let (passed, detail) = match change {
        SchemaChange::DropTable { table } => {
            let before = count_rows(target, table);
            let declared_before = target.schema.tables.contains_key(table);
            let after = count_rows(shadow, table);
            match (before > 0 || declared_before, after) {
                (false, _) => (
                    false,
                    format!("`{table}` does not exist, so dropping it does nothing"),
                ),
                (true, 0) => (true, format!("`{table}` and its {before} rows are gone")),
                (true, left) => (
                    false,
                    format!("`{table}` still holds {left} rows after the drop"),
                ),
            }
        }

        SchemaChange::DropColumn { table, column } => {
            let dropped_declaration =
                declared(target, table, column) && !declared(shadow, table, column);
            let data_moved = rows_differ(target, shadow, table);
            match dropped_declaration || data_moved {
                true => (true, format!("`{table}.{column}` is gone from the branch")),
                false => (
                    false,
                    format!(
                        "`{table}.{column}` was not there to drop: this change removes nothing, \
                         so validating it proves nothing"
                    ),
                ),
            }
        }

        SchemaChange::RenameColumn { table, from, to } => {
            let moved = declared(target, table, from) && declared(shadow, table, to);
            let data_moved = rows_differ(target, shadow, table);
            match moved || data_moved {
                true => (true, format!("`{table}.{from}` is now `{table}.{to}`")),
                false => (false, format!("`{table}.{from}` was not there to rename")),
            }
        }

        // Additive and metadata changes legitimately leave every row alone, so
        // the declaration moving is the whole of the effect.
        other => {
            let table = table_of(other).unwrap_or_default();
            let before = target.schema.tables.get(&table);
            let after = shadow.schema.tables.get(&table);
            match before != after {
                true => (true, format!("the declaration of `{table}` changed")),
                false => (
                    false,
                    format!("the declaration of `{table}` is unchanged: this change does nothing"),
                ),
            }
        }
    };

    Check {
        name: "change_took_effect".into(),
        passed,
        detail,
        samples: Vec::new(),
    }
}

/// Whether any row of one table differs between the two views.
fn rows_differ(target: &MaterializedView, shadow: &MaterializedView, table: &str) -> bool {
    let prefix = RowAddress::prefix(table);
    let side = |view: &MaterializedView| -> Vec<(String, Value)> {
        view.keys
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    side(target) != side(shadow)
}

/// Did anything outside the change's own table move?
///
/// This is the check worth having. A migration that quietly touched a table it
/// did not name is the failure a reviewer cannot spot by reading the proposal,
/// and it is exactly what comparing two folds can catch.
fn only_the_named_table_changed(
    target: &MaterializedView,
    shadow: &MaterializedView,
    change: &SchemaChange,
) -> Check {
    let named = table_of(change);
    let mut strays = Vec::new();

    for key in target.keys.keys().chain(shadow.keys.keys()) {
        if strays.len() >= SAMPLE_LIMIT {
            break;
        }
        let table = RowAddress::parse(key).map(|a| a.table.to_string());
        if table.as_deref() == named.as_deref() {
            continue;
        }
        let before = target.keys.get(key);
        let after = shadow.keys.get(key);
        if before != after && !strays.iter().any(|s: &Sample| s.key == *key) {
            strays.push(Sample {
                key: key.clone(),
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }

    let passed = strays.is_empty();
    Check {
        name: "only_the_named_table_changed".into(),
        passed,
        detail: match (&named, passed) {
            (Some(table), true) => format!("nothing outside `{table}` moved"),
            (None, true) => "nothing moved outside the change's own scope".into(),
            (_, false) => format!(
                "{} row(s) outside this change's table differ; \
                 a change that touches a table it does not name is not the change that was reviewed",
                strays.len()
            ),
        },
        samples: strays,
    }
}

/// Does the row count move the way this kind of change should?
fn row_count_matches_the_change(
    target: &MaterializedView,
    shadow: &MaterializedView,
    change: &SchemaChange,
) -> Check {
    let Some(table) = table_of(change) else {
        return Check {
            name: "row_count_matches_the_change".into(),
            passed: true,
            detail: "this change names no table".into(),
            samples: Vec::new(),
        };
    };

    let before = count_rows(target, &table);
    let after = count_rows(shadow, &table);

    // A table drop empties the table. Everything else keeps its rows: a column
    // change that lost rows destroyed more than it said it would.
    let (passed, detail) = match change {
        SchemaChange::DropTable { .. } => (
            after == 0,
            match after {
                0 => format!("`{table}` went from {before} rows to none, as a table drop should"),
                left => format!("`{table}` still holds {left} of its {before} rows after a drop"),
            },
        ),
        _ => (
            before == after,
            match before == after {
                true => format!("`{table}` still holds all {before} of its rows"),
                false => format!(
                    "`{table}` went from {before} rows to {after}: this change was not \
                     supposed to remove rows"
                ),
            },
        ),
    };

    Check {
        name: "row_count_matches_the_change".into(),
        passed,
        detail,
        samples: sample_missing_rows(target, shadow, &table),
    }
}

/// A few rows the shadow branch no longer has, for the human reading the result.
fn sample_missing_rows(
    target: &MaterializedView,
    shadow: &MaterializedView,
    table: &str,
) -> Vec<Sample> {
    let prefix = RowAddress::prefix(table);
    target
        .keys
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, value)| shadow.keys.get(*key) != Some(*value))
        .take(SAMPLE_LIMIT)
        .map(|(key, value)| Sample {
            key: key.clone(),
            before: Some(value.clone()),
            after: shadow.keys.get(key).cloned(),
        })
        .collect()
}

/// The table a change names, if it names one.
fn table_of(change: &SchemaChange) -> Option<String> {
    match change {
        SchemaChange::AddTable { table } => Some(table.name.clone()),
        SchemaChange::DropTable { table } => Some(table.clone()),
        SchemaChange::AddColumn { table, .. }
        | SchemaChange::DropColumn { table, .. }
        | SchemaChange::AlterColumnType { table, .. }
        | SchemaChange::SetNullable { table, .. }
        | SchemaChange::AddIndex { table, .. }
        | SchemaChange::DropIndex { table, .. }
        | SchemaChange::RenameColumn { table, .. }
        | SchemaChange::SetCrdt { table, .. } => Some(table.clone()),
    }
}

fn count_rows(view: &MaterializedView, table: &str) -> u64 {
    let prefix = RowAddress::prefix(table);
    let plain = view
        .keys
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, _)| RowAddress::parse(key).is_some())
        .count();
    let crdt = view
        .crdts
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, _)| RowAddress::parse(key).is_some())
        .count();
    (plain + crdt) as u64
}
