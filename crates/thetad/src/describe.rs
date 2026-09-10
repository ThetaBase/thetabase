//! `describe`: what is in here, and where it came from (ROADMAP-V3 M19).
//!
//! # Why this is a call rather than a recipe
//!
//! Everything here could be assembled by a caller: `query` the schema, `audit`
//! the history, sample some rows. An agent arriving at an unfamiliar database
//! does exactly that today, and every step it has to take before it can ask the
//! question is a step it can get wrong — usually by sampling rows it did not
//! need in order to learn a shape the schema already knew.
//!
//! So it is one call, and the shape it returns is chosen for the reader that
//! actually makes it.
//!
//! # The field that matters most is `crdt`
//!
//! Not the type. An agent deciding whether two of its writes can race needs to
//! know whether concurrent modification of a field **converges** or becomes a
//! conflict a human resolves (`docs/INVARIANTS.md` invariant 5). That is the difference
//! between "retry freely" and "you have just created work for a person", and
//! nothing else in a schema description tells them.
//!
//! # Examples are off by default, and that is a decision
//!
//! It is not an authorisation boundary. A caller who can describe a table can
//! already query it, so examples reveal nothing they could not fetch — and
//! pretending otherwise would be security theatre.
//!
//! It is a blast-radius decision about the *default*. `describe` is the call an
//! agent makes to orient itself, often automatically and often first. One that
//! returns rows by default pulls customer data into a model's context, and into
//! whatever logs that context, for a call whose purpose was to learn the shape
//! of the data rather than any of it.
//!
//! Distribution facts that no individual row discloses — the null fraction, the
//! row count — are sent regardless, because those are what an agent usually
//! wanted when it reached for examples.
//!
//! # Withholding is reported, never silent
//!
//! When examples are asked for and not returned, the response says so and why.
//! A client that asked and got an empty list cannot otherwise tell "there were
//! none" from "we would not give them to you", and those lead to different next
//! steps.

use std::collections::BTreeMap;

use theta_core::log::LogEntry;
use theta_core::schema::{CrdtKind, Schema};
use theta_core::{RowAddress, Value, ValueType};
use theta_proto::wire::{ColumnDescriptionWire, SchemaDescriptionWire, TableDescriptionWire};
use theta_storage::provenance::{SchemaProvenance, Subject};
use theta_storage::MaterializedView;

/// Examples per column when the caller does not say.
///
/// Three. Enough to show the shape of a value — a UUID, an ISO date, a
/// currency-code string — and few enough that a description does not become a
/// row dump by another name.
pub const DEFAULT_EXAMPLE_LIMIT: u32 = 3;

/// The most a caller may ask for.
///
/// Twenty. Above this a caller is no longer characterising a column, they are
/// reading it, and `query` is the call for that — it is gated, logged and
/// counted, and this is none of those things.
pub const MAX_EXAMPLE_LIMIT: u32 = 20;

/// Rows sampled to compute distribution facts.
///
/// Bounded because `describe` must stay cheap enough to call reflexively. A
/// description that scales with table size is one an agent learns not to ask
/// for, and an agent that stops asking goes back to guessing.
const SAMPLE_ROWS: usize = 1_000;

/// What a caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribeRequest {
    /// Empty describes every table.
    pub table: String,
    pub include_examples: bool,
    /// Zero means [`DEFAULT_EXAMPLE_LIMIT`].
    pub example_limit: u32,
}

/// Build a description of `view`.
///
/// `entries` is the branch's log, used for provenance. Passing it rather than a
/// pre-folded provenance keeps the two from being computed over different logs —
/// a description stamped from another branch's history would carry `declaredAt`
/// pointers to commits this branch never saw, which is worse than the absence
/// they replaced because it is a wrong pointer rather than a missing one.
pub fn describe(
    view: &MaterializedView,
    entries: &[LogEntry],
    request: &DescribeRequest,
) -> SchemaDescriptionWire {
    let provenance = SchemaProvenance::fold(entries);

    let limit = match request.example_limit {
        0 => DEFAULT_EXAMPLE_LIMIT,
        asked => asked.min(MAX_EXAMPLE_LIMIT),
    } as usize;

    let mut tables = Vec::new();
    for (table_name, table_def) in &view.schema.tables {
        if !request.table.is_empty() && request.table != *table_name {
            continue;
        }

        let sample = sample_rows(view, table_name);
        let row_count = count_rows(view, table_name);

        let columns = table_def
            .fields
            .iter()
            .map(|(field_name, field)| {
                // `has_column` rather than `column`: whether a value is there
                // and what it is are separate questions, and only the first is
                // needed to compute a null fraction. Reading every value to
                // count the missing ones would make the distribution facts —
                // which are safe to send — cost the same as the examples, which
                // are not sent by default.
                let present: Vec<&Value> = sample
                    .iter()
                    .filter(|row| theta_core::has_column(row, true, field_name))
                    .copied()
                    .collect();

                // Basis points of the *sample*, not of the table. A fraction
                // computed over 1,000 rows and reported as though it covered ten
                // million is the kind of number that gets quoted.
                let null_basis_points = if sample.is_empty() {
                    0
                } else {
                    let missing = sample.len() - present.len();
                    ((missing as f64 / sample.len() as f64) * 10_000.0).round() as u32
                };

                let examples = if request.include_examples {
                    // Distinct, so three examples of a boolean column are not
                    // `true, true, true`. Ordering follows the sample, which
                    // follows the key order, so this is deterministic.
                    let mut seen = Vec::new();
                    for row in &present {
                        let Some(value) = theta_core::address::column(row, "", field_name) else {
                            continue;
                        };
                        let encoded = serde_json::to_string(&value)
                            .unwrap_or_else(|_| "\"<unencodable>\"".into());
                        if !seen.contains(&encoded) {
                            seen.push(encoded);
                        }
                        if seen.len() >= limit {
                            break;
                        }
                    }
                    seen
                } else {
                    Vec::new()
                };

                let subject = Subject::column(table_name, field_name);
                let record = provenance.of(&subject);

                ColumnDescriptionWire {
                    name: field_name.clone(),
                    ty: type_name(field.ty).to_string(),
                    nullable: field.nullable,
                    crdt: field.crdt.map(crdt_name).unwrap_or_default().to_string(),
                    declared_at: record
                        .and_then(|r| r.declared_at())
                        .map(|hash| hash.to_hex())
                        .unwrap_or_default(),
                    touched_by_agent: record.is_some_and(|r| r.touched_by_an_agent()),
                    examples,
                    null_basis_points,
                }
            })
            .collect();

        tables.push(TableDescriptionWire {
            name: table_name.clone(),
            row_count,
            columns,
        });
    }

    // `describe` never withholds — it either includes examples or was not asked
    // for them, and an empty list here means the columns genuinely had no
    // values. Refusal is `describe_withholding`'s job, so that the two cases
    // cannot be produced by the same code path and confused.
    SchemaDescriptionWire {
        tables,
        examples_withheld: false,
        withheld_reason: String::new(),
    }
}

/// A description with examples refused, and the reason said out loud.
///
/// Separate from [`describe`] because refusing is a decision a caller made
/// about, not an empty result: the response must distinguish "there were no
/// values" from "we would not give them to you".
pub fn describe_withholding(
    view: &MaterializedView,
    entries: &[LogEntry],
    request: &DescribeRequest,
    reason: &str,
) -> SchemaDescriptionWire {
    let mut description = describe(
        view,
        entries,
        &DescribeRequest {
            include_examples: false,
            ..request.clone()
        },
    );
    description.examples_withheld = request.include_examples;
    description.withheld_reason = if request.include_examples {
        reason.to_string()
    } else {
        String::new()
    };
    description
}

fn type_name(ty: ValueType) -> &'static str {
    match ty {
        ValueType::Null => "null",
        ValueType::Bool => "bool",
        ValueType::Int => "int",
        ValueType::Float => "float",
        ValueType::Text => "text",
        ValueType::Bytes => "bytes",
        ValueType::Timestamp => "timestamp",
        ValueType::List => "list",
        ValueType::Map => "map",
    }
}

fn crdt_name(kind: CrdtKind) -> &'static str {
    match kind {
        CrdtKind::Counter => "counter",
        CrdtKind::Register => "register",
        CrdtKind::Set => "set",
        CrdtKind::Sequence => "sequence",
    }
}

/// Rows of one table, as a prefix range rather than a scan.
///
/// The same choice `impact.rs` makes and for the same reason: this runs on every
/// `describe`, and walking the whole keyspace to size one table would make the
/// cost of asking scale with the size of every other table.
fn count_rows(view: &MaterializedView, table: &str) -> u64 {
    let prefix = RowAddress::prefix(table);
    view.keys
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .count() as u64
}

fn sample_rows<'a>(view: &'a MaterializedView, table: &str) -> Vec<&'a Value> {
    let prefix = RowAddress::prefix(table);
    view.keys
        .range(prefix.clone()..)
        .take_while(|(key, _)| key.starts_with(&prefix))
        .take(SAMPLE_ROWS)
        .map(|(_, value)| value)
        .collect()
}

/// Every table name in the schema, for a caller that only wants the index.
pub fn table_names(schema: &Schema) -> Vec<String> {
    schema.tables.keys().cloned().collect()
}

/// Column descriptions keyed by name, for callers that want a lookup.
pub fn columns_by_name(table: &TableDescriptionWire) -> BTreeMap<&str, &ColumnDescriptionWire> {
    table.columns.iter().map(|c| (c.name.as_str(), c)).collect()
}
