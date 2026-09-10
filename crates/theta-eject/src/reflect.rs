//! Reflecting an existing Postgres schema.
//!
//! Read from `information_schema` and `pg_catalog` rather than from a dump,
//! because what matters is what the database currently believes — a dump is a
//! statement about a moment, and migrations are run against a live system.
//!
//! Nothing here writes. A migration that could alter its source is a migration
//! nobody can safely re-run, and re-running is exactly what happens when the
//! first attempt is interrupted.

use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::EjectError;

/// One column, as Postgres describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// `information_schema.columns.udt_name` — `int4`, `timestamptz`, `_text`.
    /// More precise than `data_type`, and the difference is where meaning is.
    pub udt_name: String,
    pub nullable: bool,
    pub has_default: bool,
    /// Character limit, for the types that carry one. A `varchar(10)` that
    /// becomes unbounded `Text` has lost a constraint, and the verification
    /// pass says so.
    pub character_maximum_length: Option<i32>,
    /// Precision and scale for `numeric`, which is where exactness lives.
    pub numeric_precision: Option<i32>,
    pub numeric_scale: Option<i32>,
}

/// One table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// Primary key columns, in key order. Empty means the table has none —
    /// which `eject` treats as a blocker rather than inventing one, since
    /// ThetaBase addresses every row by a primary key.
    pub primary_key: Vec<String>,
    pub unique_constraints: Vec<UniqueConstraint>,
    pub indexes: Vec<Index>,
    /// `reltuples`-based estimate. Used for progress and for deciding whether
    /// a full verification sweep is affordable; never for correctness.
    pub estimated_rows: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// A whole schema, as reflected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub schema_name: String,
    pub tables: Vec<Table>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// Reflect every base table in `schema_name`.
///
/// Views and materialized views are skipped: they are derived, and migrating a
/// derivation as though it were data produces two copies of one truth that are
/// free to disagree.
pub async fn reflect(client: &Client, schema_name: &str) -> Result<Schema, EjectError> {
    let table_rows = client
        .query(
            "SELECT c.relname, c.reltuples::bigint
               FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1
                AND c.relkind = 'r'
              ORDER BY c.relname",
            &[&schema_name],
        )
        .await?;

    let mut tables = Vec::with_capacity(table_rows.len());
    for row in table_rows {
        let name: String = row.get(0);
        let estimated_rows: i64 = row.get(1);
        tables.push(Table {
            columns: columns_of(client, schema_name, &name).await?,
            primary_key: primary_key_of(client, schema_name, &name).await?,
            unique_constraints: unique_constraints_of(client, schema_name, &name).await?,
            indexes: indexes_of(client, schema_name, &name).await?,
            estimated_rows: estimated_rows.max(0),
            name,
        });
    }

    Ok(Schema {
        schema_name: schema_name.to_string(),
        tables,
    })
}

async fn columns_of(client: &Client, schema: &str, table: &str) -> Result<Vec<Column>, EjectError> {
    let rows = client
        .query(
            "SELECT column_name,
                    udt_name,
                    is_nullable = 'YES',
                    column_default IS NOT NULL,
                    character_maximum_length,
                    numeric_precision,
                    numeric_scale
               FROM information_schema.columns
              WHERE table_schema = $1 AND table_name = $2
              ORDER BY ordinal_position",
            &[&schema, &table],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|row| Column {
            name: row.get(0),
            udt_name: row.get(1),
            nullable: row.get(2),
            has_default: row.get(3),
            character_maximum_length: row.get(4),
            numeric_precision: row.get(5),
            numeric_scale: row.get(6),
        })
        .collect())
}

async fn primary_key_of(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, EjectError> {
    // Read through pg_index rather than information_schema: key order matters
    // for a composite key, and `pg_index.indkey` is where it is recorded.
    let rows = client
        .query(
            "SELECT a.attname
               FROM pg_index i
               JOIN pg_class c ON c.oid = i.indrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_attribute a ON a.attrelid = c.oid
                                  AND a.attnum = ANY(i.indkey)
              WHERE n.nspname = $1 AND c.relname = $2 AND i.indisprimary
              ORDER BY array_position(i.indkey, a.attnum)",
            &[&schema, &table],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

async fn unique_constraints_of(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Vec<UniqueConstraint>, EjectError> {
    let rows = client
        .query(
            "SELECT c.conname, a.attname
               FROM pg_constraint c
               JOIN pg_class t ON t.oid = c.conrelid
               JOIN pg_namespace n ON n.oid = t.relnamespace
               JOIN pg_attribute a ON a.attrelid = t.oid
                                  AND a.attnum = ANY(c.conkey)
              WHERE n.nspname = $1 AND t.relname = $2 AND c.contype = 'u'
              ORDER BY c.conname, array_position(c.conkey, a.attnum)",
            &[&schema, &table],
        )
        .await?;

    Ok(group_named(rows)
        .into_iter()
        .map(|(name, columns)| UniqueConstraint { name, columns })
        .collect())
}

async fn indexes_of(client: &Client, schema: &str, table: &str) -> Result<Vec<Index>, EjectError> {
    let rows = client
        .query(
            "SELECT ic.relname, a.attname, i.indisunique
               FROM pg_index i
               JOIN pg_class c ON c.oid = i.indrelid
               JOIN pg_class ic ON ic.oid = i.indexrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_attribute a ON a.attrelid = c.oid
                                  AND a.attnum = ANY(i.indkey)
              WHERE n.nspname = $1 AND c.relname = $2 AND NOT i.indisprimary
              ORDER BY ic.relname, array_position(i.indkey, a.attnum)",
            &[&schema, &table],
        )
        .await?;

    let mut out: Vec<Index> = Vec::new();
    for row in &rows {
        let name: String = row.get(0);
        let column: String = row.get(1);
        let unique: bool = row.get(2);
        match out.last_mut() {
            Some(last) if last.name == name => last.columns.push(column),
            _ => out.push(Index {
                name,
                columns: vec![column],
                unique,
            }),
        }
    }
    Ok(out)
}

/// Collapse `(name, column)` rows into `(name, columns)`, preserving order.
fn group_named(rows: Vec<tokio_postgres::Row>) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for row in &rows {
        let name: String = row.get(0);
        let column: String = row.get(1);
        match out.last_mut() {
            Some((last, columns)) if *last == name => columns.push(column),
            _ => out.push((name, vec![column])),
        }
    }
    out
}
