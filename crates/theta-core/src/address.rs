//! How keys address rows.
//!
//! ThetaBase's storage surface is a flat key/value map, but a query asks about
//! *tables* and *rows*. This module is the single place that translates between
//! the two, so the write path, the query path and the schema all agree on what
//! `users:123` means.
//!
//! # The convention
//!
//! A key is `<table>:<primary key>`. Everything after the first colon is the
//! primary key, so `users:tenant-a:42` is row `tenant-a:42` of table `users`.
//!
//! The value at that key is the row:
//!
//! * a [`Value::Map`] is a multi-column row, its entries the columns;
//! * any other value is a single-column row whose column is named
//!   [`VALUE_COLUMN`].
//!
//! Either way the primary key is readable as [`KEY_COLUMN`], so `SELECT _key
//! FROM users` works without the row having to store its own key.
//!
//! A key with no colon addresses no table. It is still a perfectly good
//! key/value entry — the `get`/`put` surface does not require a schema — it is
//! simply not reachable by a table query.

use crate::schema::{FieldDef, Schema};
use crate::value::{Value, ValueType};

/// Column exposing a row's primary key.
pub const KEY_COLUMN: &str = "_key";

/// Column holding the whole value, for rows that are not maps.
pub const VALUE_COLUMN: &str = "value";

/// A key parsed into the table and row it addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowAddress<'a> {
    pub table: &'a str,
    pub primary_key: &'a str,
}

impl<'a> RowAddress<'a> {
    /// Parse a key. Returns `None` when the key addresses no table.
    pub fn parse(key: &'a str) -> Option<Self> {
        let (table, primary_key) = key.split_once(':')?;
        if table.is_empty() || primary_key.is_empty() {
            return None;
        }
        Some(Self { table, primary_key })
    }

    /// The prefix every key of `table` starts with.
    pub fn prefix(table: &str) -> String {
        format!("{table}:")
    }
}

/// Read a column out of a row value.
///
/// Returns `None` for a column the row does not have, which a query renders as
/// `NULL` — a missing column and a column holding null are the same thing to a
/// reader, and distinguishing them would leak the storage layout.
pub fn column<'a>(row: &'a Value, primary_key: &'a str, name: &str) -> Option<Value> {
    if name == KEY_COLUMN {
        return Some(Value::Text(primary_key.to_string()));
    }
    match row {
        Value::Map(fields) => fields.get(name).cloned(),
        // A scalar row has exactly one column.
        other if name == VALUE_COLUMN => Some(other.clone()),
        _ => None,
    }
}

/// Whether a row holds a non-null value for `name`.
///
/// Kept beside [`column`] and matching it case for case, because two places
/// that each decide what "the row has this column" means will eventually
/// disagree. This one exists because the callers that only need the answer —
/// counting rows a schema change would touch, for one — should not have to
/// clone every value to get it.
///
/// A column holding an explicit null does not count as held: a `SET NOT NULL`
/// rejects it exactly as it rejects a missing column.
pub fn has_column(row: &Value, primary_key_present: bool, name: &str) -> bool {
    if name == KEY_COLUMN {
        return primary_key_present;
    }
    match row {
        Value::Map(fields) => !matches!(fields.get(name), None | Some(Value::Null)),
        Value::Null => false,
        _ => name == VALUE_COLUMN,
    }
}

/// Every column a row value carries, in a stable order.
pub fn columns_of(row: &Value) -> Vec<String> {
    match row {
        // `BTreeMap` iterates in key order, so two rows of the same shape always
        // produce their columns in the same order.
        Value::Map(fields) => fields.keys().cloned().collect(),
        _ => vec![VALUE_COLUMN.to_string()],
    }
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldTypeError {
    pub table: String,
    pub column: String,
    pub declared: ValueType,
    pub actual: ValueType,
}

/// Check a row against its table's declared schema.
///
/// Returns the first column whose value contradicts its declared type. Columns
/// the schema does not declare are allowed: dynamic writes are legal during
/// prototyping, and the engine simply never *reinterprets* a value it has
/// already accepted (`03-data-model-consistency.md` §2.2).
pub fn check_row(schema: &Schema, table: &str, row: &Value) -> Result<(), FieldTypeError> {
    let Some(def) = schema.table(table) else {
        return Ok(()); // undeclared table: dynamic mode
    };

    let refuse = |column: &str, field: &FieldDef, actual: &Value| FieldTypeError {
        table: table.to_string(),
        column: column.to_string(),
        declared: field.ty,
        actual: actual.inferred_type(),
    };

    match row {
        Value::Map(fields) => {
            for (name, value) in fields {
                if let Some(field) = def.fields.get(name) {
                    if !field.ty.accepts(value) {
                        return Err(refuse(name, field, value));
                    }
                }
            }
            Ok(())
        }
        // A scalar row is checked against the `value` column if one is declared.
        other => match def.fields.get(VALUE_COLUMN) {
            Some(field) if !field.ty.accepts(other) => Err(refuse(VALUE_COLUMN, field, other)),
            _ => Ok(()),
        },
    }
}

/// What a query executor reads.
///
/// Lives here rather than in `theta-query` because the materialized view
/// implements it and `theta-storage` cannot depend on the query crate — the
/// dependency runs the other way. A trait rather than a concrete type so the
/// executor can be tested against fixtures without a storage engine, and so an
/// index-backed source can be slotted in without touching plan execution.
pub trait RowSource {
    /// Every row of `table`, as (primary key, row value), in a stable order.
    fn scan(&self, table: &str) -> Vec<(String, Value)>;

    /// One row by primary key. Separate from `scan` because this is the `get`
    /// hot path and must not walk the table.
    fn row(&self, table: &str, primary_key: &str) -> Option<Value>;

    /// At most `max` rows of `table`, in the same order [`scan`] gives.
    ///
    /// This is what makes `LIMIT` cost less than the table it limits. The
    /// default is honest rather than fast — it scans and truncates, so a source
    /// that has not implemented a bounded scan is merely no worse than before —
    /// and a source that can stop early should override it.
    ///
    /// `None` means unbounded, which is exactly [`scan`].
    ///
    /// [`scan`]: RowSource::scan
    fn scan_up_to(&self, table: &str, max: Option<usize>) -> Vec<(String, Value)> {
        let mut rows = self.scan(table);
        if let Some(max) = max {
            rows.truncate(max);
        }
        rows
    }

    /// Declared column type, used to type the result set. `None` means the
    /// column is undeclared and its type is inferred from the data.
    fn column_type(&self, table: &str, column: &str) -> Option<ValueType>;

    /// Primary keys that *may* satisfy `bound` on `column`, from an index.
    ///
    /// `None` means no index can answer this and the caller must scan. `Some`
    /// is a **superset**: the caller re-applies the predicate to what comes
    /// back, so an index is free to be generous where being exact would be
    /// delicate (`crate::index` says where it is). Over-approximating costs a
    /// row the filter then discards; under-approximating would lose a row and
    /// silently answer the wrong question, so the direction is not symmetric.
    ///
    /// Defaulted to `None` so a source without indexes — every test fixture,
    /// and any future source that has not built them yet — keeps working by
    /// scanning, which is the documented degraded mode rather than an error.
    fn index_candidates(
        &self,
        _table: &str,
        _column: &str,
        _bound: &crate::IndexBound,
    ) -> Option<Vec<String>> {
        None
    }
}

#[cfg(test)]
mod has_column_tests {
    use std::collections::BTreeMap;

    use super::*;

    fn row(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn has_column_agrees_with_column_on_every_shape_of_row() {
        let rows = [
            row(&[("email", Value::Text("a@b".into()))]),
            row(&[("email", Value::Null)]),
            row(&[("name", Value::Text("someone".into()))]),
            Value::Int(7),
            Value::Null,
        ];

        // Two functions deciding what "the row has this column" means will
        // eventually disagree, so this pins them together. `column` returns
        // `Some(Null)` where `has_column` says no — that difference is the
        // whole point of the second function and is asserted, not glossed.
        for value in &rows {
            for name in ["email", "name", VALUE_COLUMN, "absent"] {
                let held = column(value, "pk", name);
                let expected = !matches!(held, None | Some(Value::Null));
                assert_eq!(
                    has_column(value, true, name),
                    expected,
                    "disagreement on column `{name}` of {value:?}"
                );
            }
        }
    }

    #[test]
    fn every_row_carries_its_own_key() {
        assert!(has_column(&Value::Int(1), true, KEY_COLUMN));
        assert!(!has_column(&Value::Int(1), false, KEY_COLUMN));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::schema::TableDef;

    use super::*;

    fn row(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn schema_with(column: &str, ty: ValueType) -> Schema {
        let mut fields = BTreeMap::new();
        fields.insert(
            column.to_string(),
            FieldDef {
                name: column.to_string(),
                ty,
                nullable: true,
                crdt: None,
                declared_at: None,
            },
        );
        let mut tables = BTreeMap::new();
        tables.insert(
            "users".to_string(),
            TableDef {
                name: "users".into(),
                fields,
                indexes: Vec::new(),
            },
        );
        Schema { tables }
    }

    #[test]
    fn a_key_splits_on_its_first_colon_only() {
        let address = RowAddress::parse("users:tenant-a:42").expect("parses");
        assert_eq!(address.table, "users");
        assert_eq!(address.primary_key, "tenant-a:42");
    }

    #[test]
    fn a_key_without_a_table_addresses_no_row() {
        assert_eq!(RowAddress::parse("standalone"), None);
        assert_eq!(RowAddress::parse(":no-table"), None);
        assert_eq!(RowAddress::parse("no-key:"), None);
    }

    #[test]
    fn a_map_row_exposes_its_entries_as_columns() {
        let value = row(&[
            ("name", Value::Text("Alice".into())),
            ("age", Value::Int(30)),
        ]);
        assert_eq!(columns_of(&value), vec!["age", "name"]);
        assert_eq!(
            column(&value, "1", "name"),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(column(&value, "1", "missing"), None);
    }

    #[test]
    fn a_scalar_row_has_exactly_one_column() {
        let value = Value::Int(42);
        assert_eq!(columns_of(&value), vec![VALUE_COLUMN]);
        assert_eq!(column(&value, "1", VALUE_COLUMN), Some(Value::Int(42)));
        assert_eq!(column(&value, "1", "name"), None);
    }

    #[test]
    fn every_row_exposes_its_primary_key() {
        assert_eq!(
            column(&Value::Int(1), "pk-7", KEY_COLUMN),
            Some(Value::Text("pk-7".into()))
        );
        assert_eq!(
            column(&row(&[("a", Value::Int(1))]), "pk-7", KEY_COLUMN),
            Some(Value::Text("pk-7".into()))
        );
    }

    #[test]
    fn a_column_contradicting_its_declared_type_is_refused() {
        let schema = schema_with("age", ValueType::Int);
        let bad = row(&[("age", Value::Text("thirty".into()))]);

        let err = check_row(&schema, "users", &bad).expect_err("must refuse");
        assert_eq!(err.column, "age");
        assert_eq!(err.declared, ValueType::Int);
        assert_eq!(err.actual, ValueType::Text);
    }

    #[test]
    fn undeclared_columns_and_tables_are_allowed() {
        let schema = schema_with("age", ValueType::Int);
        // Dynamic mode: writing a column the schema does not mention is legal.
        assert!(check_row(
            &schema,
            "users",
            &row(&[("nickname", Value::Text("A".into()))])
        )
        .is_ok());
        assert!(check_row(&schema, "unknown_table", &Value::Int(1)).is_ok());
    }

    #[test]
    fn a_null_is_accepted_by_any_declared_column() {
        let schema = schema_with("age", ValueType::Int);
        assert!(check_row(&schema, "users", &row(&[("age", Value::Null)])).is_ok());
    }
}
