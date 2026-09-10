//! The migration plan: what will happen, decided before anything happens.
//!
//! `eject` builds this from a reflected schema and shows it. Nothing is written
//! until the plan is accepted, because the interesting failures of a migration
//! are decisions — a `numeric` becoming a float, a table with no primary key —
//! and those are cheap to change beforehand and expensive afterwards.

use serde::{Deserialize, Serialize};
use theta_core::schema::{FieldDef, IndexDef, SchemaChange, TableDef};

use crate::reflect::{Schema, Table};
use crate::types::{map_type, Mapping};

/// One column's journey.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnPlan {
    pub source_name: String,
    pub target_name: String,
    pub source_type: String,
    pub mapping: Mapping,
    pub nullable: bool,
}

/// One table's journey.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TablePlan {
    pub source_schema: String,
    pub source_name: String,
    pub target_name: String,
    pub primary_key: Vec<String>,
    pub columns: Vec<ColumnPlan>,
    pub indexes: Vec<IndexDef>,
    pub estimated_rows: i64,
}

/// Something the operator has to see before agreeing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub table: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub message: String,
}

/// Something that stops the migration.
///
/// Separate from a warning on purpose. A warning says "this changes meaning,
/// decide whether you mind"; a blocker says "there is no correct thing to do
/// here". Collapsing the two would mean either refusing migrations that are
/// fine, or proceeding through ones that are not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blocker {
    pub table: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub tables: Vec<TablePlan>,
    pub warnings: Vec<Warning>,
    pub blockers: Vec<Blocker>,
}

impl Plan {
    pub fn is_runnable(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn table(&self, name: &str) -> Option<&TablePlan> {
        self.tables.iter().find(|t| t.source_name == name)
    }

    /// The schema changes that create this plan's shape in ThetaBase.
    pub fn schema_changes(&self) -> Vec<SchemaChange> {
        let mut changes = Vec::new();
        for table in &self.tables {
            changes.push(SchemaChange::AddTable {
                table: TableDef {
                    name: table.target_name.clone(),
                    fields: table
                        .columns
                        .iter()
                        .map(|c| {
                            (
                                c.target_name.clone(),
                                FieldDef {
                                    name: c.target_name.clone(),
                                    ty: c.mapping.ty,
                                    nullable: c.nullable,
                                    // Suggested, never applied: choosing a CRDT
                                    // changes how concurrent writes resolve,
                                    // which is the application's decision.
                                    crdt: None,
                                    declared_at: None,
                                },
                            )
                        })
                        .collect(),
                    indexes: table.indexes.clone(),
                },
            });
        }
        changes
    }
}

/// Build a plan from a reflected schema.
pub fn plan(schema: &Schema) -> Plan {
    plan_excluding(schema, &[])
}

/// Build a plan, leaving `excluded` tables behind.
///
/// A table with no primary key blocks the whole migration, and the right answer
/// is sometimes "that table is a log, I do not want it". Excluding is *noisy*
/// rather than silent: an excluded table produces a warning, because the
/// difference between "migrated" and "migrated except for the one you skipped"
/// is exactly the thing someone will forget six months later.
pub fn plan_excluding(schema: &Schema, excluded: &[String]) -> Plan {
    let mut tables = Vec::new();
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    for name in excluded {
        if schema.table(name).is_none() {
            // A typo in `--exclude` would otherwise look like it worked, right
            // up until the table it was meant to skip blocked the run.
            blockers.push(Blocker {
                table: name.clone(),
                message: format!(
                    "`{name}` was excluded but is not in this schema. Check the \
                     name: excluding a table that does not exist silently \
                     excludes nothing."
                ),
            });
        }
    }

    for table in &schema.tables {
        if excluded.iter().any(|name| name == &table.name) {
            warnings.push(Warning {
                table: table.name.clone(),
                column: None,
                message: format!(
                    "`{}` was excluded and will not be migrated. Its {} row(s) \
                     stay in Postgres.",
                    table.name, table.estimated_rows
                ),
            });
            continue;
        }

        if table.primary_key.is_empty() {
            // ThetaBase addresses every row by a primary key. Inventing one —
            // a row number, a hash of the contents — would produce addresses
            // that change when the table does, so a re-run would write every
            // row again under new keys instead of updating the old ones.
            blockers.push(Blocker {
                table: table.name.clone(),
                message: format!(
                    "`{}` has no primary key. ThetaBase addresses rows by one, and \
                     a synthesized key would change between runs, so a resumed \
                     or repeated migration would duplicate every row rather than \
                     update it. Add a primary key, or exclude the table.",
                    table.name
                ),
            });
            continue;
        }

        let columns: Vec<ColumnPlan> = table
            .columns
            .iter()
            .map(|column| {
                let mapping = map_type(&column.udt_name, &column.name);
                if mapping.is_lossy() {
                    warnings.push(Warning {
                        table: table.name.clone(),
                        column: Some(column.name.clone()),
                        message: mapping
                            .note
                            .clone()
                            .unwrap_or_else(|| "meaning may not survive".into()),
                    });
                }
                if let Some(suggestion) = &mapping.crdt_suggestion {
                    warnings.push(Warning {
                        table: table.name.clone(),
                        column: Some(column.name.clone()),
                        message: format!(
                            "CRDT suggestion ({:?}): {}",
                            suggestion.kind, suggestion.because
                        ),
                    });
                }
                ColumnPlan {
                    source_name: column.name.clone(),
                    target_name: column.name.clone(),
                    source_type: column.udt_name.clone(),
                    nullable: column.nullable,
                    mapping,
                }
            })
            .collect();

        warnings.extend(constraint_warnings(table));

        tables.push(TablePlan {
            source_schema: schema.schema_name.clone(),
            source_name: table.name.clone(),
            target_name: table.name.clone(),
            primary_key: table.primary_key.clone(),
            indexes: table
                .indexes
                .iter()
                .map(|i| IndexDef {
                    name: i.name.clone(),
                    columns: i.columns.clone(),
                    unique: i.unique,
                })
                .collect(),
            estimated_rows: table.estimated_rows,
            columns,
        });
    }

    Plan {
        tables,
        warnings,
        blockers,
    }
}

/// Constraints the source enforces that ThetaBase will not.
///
/// These are the quiet ones. Nothing about the migrated data looks wrong; the
/// database simply stops refusing what it used to refuse, and the first bad row
/// arrives weeks later.
fn constraint_warnings(table: &Table) -> Vec<Warning> {
    let mut out = Vec::new();

    for unique in &table.unique_constraints {
        out.push(Warning {
            table: table.name.clone(),
            column: Some(unique.columns.join(", ")),
            message: format!(
                "`{}` is a UNIQUE constraint in Postgres. It travels as an index, \
                 which makes lookups fast but does not reject a duplicate - the \
                 source enforced this and the target will not.",
                unique.name
            ),
        });
    }

    for column in &table.columns {
        if column.has_default {
            out.push(Warning {
                table: table.name.clone(),
                column: Some(column.name.clone()),
                message: format!(
                    "`{}` has a column default. Existing rows carry their values \
                     across, but the default itself does not travel: a later \
                     insert that omits this column will leave it unset rather \
                     than filling it in.",
                    column.name
                ),
            });
        }
        if let Some(limit) = column.character_maximum_length {
            out.push(Warning {
                table: table.name.clone(),
                column: Some(column.name.clone()),
                message: format!(
                    "`{}` is limited to {limit} characters in Postgres. Text in \
                     ThetaBase is unbounded, so the limit stops being enforced.",
                    column.name
                ),
            });
        }
    }

    out
}
