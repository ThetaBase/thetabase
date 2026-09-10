//! Materialized state: the deterministic fold over the log.
//!
//! There is no "current row" that is not this fold
//! (`03-data-model-consistency.md` §2.1). The view is maintained incrementally —
//! [`MaterializedView::apply`] folds one entry forward — so reads never replay
//! from genesis. [`MaterializedView::replay`] exists for rebuilding after a
//! crash and for tests that assert the incremental path agrees with a full fold.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use imbl::OrdMap;

use serde::{Deserialize, Serialize};
use theta_core::crdt::{apply_op, CrdtState, OpContext, ReplicaId};
use theta_core::schema::{CrdtKind, Schema, SchemaChange, TableDef};
use theta_core::{LogEntry, OpType, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MaterializedView {
    /// The rows.
    ///
    /// Behind an `Arc`, and shared with the parent branch until something
    /// writes (M10.6). Forking clones this view, and cloning three maps keyed
    /// by the same strings was most of what a fork cost — so the clone now
    /// bumps three refcounts, and the first write to a branch pays for that
    /// branch's own copy. A branch that is forked and only read from never pays
    /// at all, which is the shape an agent exploring state actually has.
    ///
    /// Sharing is invisible here because `Arc` is transparent to a reader:
    /// every `view.keys.get(..)` and `view.keys.iter()` goes through `Deref`
    /// unchanged. Only the twelve places that *mutate* had to say
    /// `Arc::make_mut`, and that is the whole of the change.
    pub keys: OrdMap<String, Value>,
    /// The commit that last wrote each key (M10.5).
    ///
    /// What makes a conditional write possible: a caller can say "only if this
    /// row is still at the version I read". A branch-wide counter cannot serve
    /// that — it moves when any *other* key is written, so a precondition on it
    /// would fail for reasons that have nothing to do with the row in hand.
    ///
    /// A fold over the log like everything else (invariant 6), and serialized
    /// alongside `keys` for the same reason `keys` is: neither is derivable from
    /// the other, and both are reconstructible by replay if the snapshot is
    /// discarded.
    ///
    /// A deleted key is *removed* from here rather than left with a tombstone
    /// version. That makes "absent" a state a precondition can name exactly,
    /// and it stays correct across delete-then-recreate: the recreate writes a
    /// new commit, so a caller holding the old version is still refused.
    #[serde(default)]
    pub versions: OrdMap<String, u64>,
    /// CRDT-typed fields, held as state rather than as a resolved value so that
    /// merging two branches is merging two folds.
    #[serde(default)]
    pub crdts: OrdMap<String, CrdtState>,
    pub schema: Schema,
    pub applied: u64,
    /// Ops rejected during folding, with the reason. A rejected op is never
    /// reinterpreted; it is recorded so the mismatch is visible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedOp>,
    /// Secondary indexes over declared columns.
    ///
    /// Not serialized: an index is a fold over `keys` and `schema`, and
    /// invariant 6 says no state exists that is not a fold over the log. A
    /// snapshot that carried indexes would be carrying a second copy of
    /// something already in it, which can then disagree with it. They are
    /// rebuilt on load instead — see [`MaterializedView::rebuild_indexes`].
    ///
    /// Behind an `Arc`, and shared until something writes (M10.6). Forking a
    /// branch clones this view, and cloning the indexes meant copying every
    /// encoded value *and every primary key a third time* — after `keys` and
    /// `versions` had each copied it once. It was the largest single component
    /// of a fork's cost on indexed data.
    ///
    /// Sharing is safe here in a way it would not be for `keys`, and the
    /// reason is already written above: this is a derived cache. Two views
    /// holding the same rows under the same schema have identical indexes by
    /// construction, so pointing them at one allocation cannot make them
    /// disagree about anything a caller can observe. `PartialEq` already
    /// ignores the contents for the same reason.
    #[serde(skip)]
    pub indexes: Arc<Indexes>,
}

/// Every index, as table → column → encoded value → primary keys.
///
/// Equality ignores the contents, deliberately. Two views holding the same rows
/// under the same schema *are* the same view whether or not their indexes have
/// been built yet: the index is derived, and tests that compare an incremental
/// fold against a full replay are asking about the log's state, not about which
/// caches happen to be warm.
/// One column's index: encoded value → the primary keys holding it.
type ColumnIndex = BTreeMap<Vec<u8>, BTreeSet<String>>;
/// Every indexed column of one table.
type TableIndexes = BTreeMap<String, ColumnIndex>;

#[derive(Debug, Clone, Default)]
pub struct Indexes(BTreeMap<String, TableIndexes>);

impl PartialEq for Indexes {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Indexes {
    fn insert(&mut self, table: &str, column: &str, encoded: Vec<u8>, primary_key: &str) {
        self.0
            .entry(table.to_string())
            .or_default()
            .entry(column.to_string())
            .or_default()
            .entry(encoded)
            .or_default()
            .insert(primary_key.to_string());
    }

    fn remove(&mut self, table: &str, column: &str, encoded: &[u8], primary_key: &str) {
        let Some(column_index) = self
            .0
            .get_mut(table)
            .and_then(|columns| columns.get_mut(column))
        else {
            return;
        };
        let Some(keys) = column_index.get_mut(encoded) else {
            return;
        };
        keys.remove(primary_key);
        // An emptied bucket is removed rather than left behind, so a range sweep
        // walks values that exist rather than the history of values that did.
        if keys.is_empty() {
            column_index.remove(encoded);
        }
    }

    /// Is there an index on this column at all?
    fn covers(&self, table: &str, column: &str) -> bool {
        self.0
            .get(table)
            .is_some_and(|columns| columns.contains_key(column))
    }

    fn candidates(
        &self,
        table: &str,
        column: &str,
        bound: &theta_core::IndexBound,
    ) -> Option<Vec<String>> {
        let column_index = self.0.get(table)?.get(column)?;
        let spans = theta_core::encoded_range(bound)?;
        let mut out: BTreeSet<String> = BTreeSet::new();
        for (start, end) in spans {
            for keys in column_index
                .range::<[u8], _>((as_slice(&start), as_slice(&end)))
                .map(|(_, keys)| keys)
            {
                out.extend(keys.iter().cloned());
            }
        }
        Some(out.into_iter().collect())
    }
}

fn as_slice(bound: &std::ops::Bound<Vec<u8>>) -> std::ops::Bound<&[u8]> {
    use std::ops::Bound::*;
    match bound {
        Included(v) => Included(v.as_slice()),
        Excluded(v) => Excluded(v.as_slice()),
        Unbounded => Unbounded,
    }
}

/// One column out of a row value.
///
/// Rows are `Value::Map`s; anything else has no columns to index. The whole-row
/// pseudo-column is not indexed — an index on it would duplicate the primary
/// key ordering the row map already provides.
fn column_value(row: &Value, column: &str) -> Option<Value> {
    match row {
        Value::Map(fields) => fields.get(column).cloned(),
        _ => None,
    }
}

/// An op that could not be folded, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedOp {
    pub key: String,
    pub reason: String,
}

impl MaterializedView {
    pub fn new() -> Self {
        Self::default()
    }

    /// The commit that last wrote `key`, or `None` if it is not there.
    ///
    /// `None` and "version 0" are deliberately different answers. A sentinel
    /// zero would make "I forgot to send a version" and "I require this row to
    /// be absent" the same request, and the second is a much stronger claim
    /// than anyone makes by accident.
    pub fn version_of(&self, key: &str) -> Option<u64> {
        self.versions.get(key).copied()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.keys.get(key)
    }

    /// Resolved value of a CRDT-typed field.
    pub fn get_crdt(&self, key: &str) -> Option<Value> {
        self.crdts.get(key).map(|s| s.value())
    }

    /// Which columns of `table` carry an index.
    ///
    /// Only single-column indexes are served. A composite index can still
    /// answer a query on its leading column, but claiming that here would mean
    /// the executor asking for a column the index is not keyed on.
    fn indexed_columns(&self, table: &str) -> Vec<String> {
        self.schema
            .tables
            .get(table)
            .map(|t| {
                t.indexes
                    .iter()
                    .filter(|i| i.columns.len() == 1)
                    .map(|i| i.columns[0].clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Keep the indexes in step with one row changing.
    ///
    /// Called with the value that was there and the value that now is, because
    /// removing the old entry needs the old value — an index updated only on
    /// the way in accumulates entries pointing at values the row no longer has,
    /// and those are exactly the false positives a filter cannot catch, since
    /// the row it points at is real.
    fn reindex_row(&mut self, key: &str, before: Option<&Value>, after: Option<&Value>) {
        let Some(address) = theta_core::RowAddress::parse(key) else {
            return;
        };
        let table = address.table.to_string();
        let primary_key = address.primary_key.to_string();

        let columns = self.indexed_columns(&table);
        if columns.is_empty() {
            return;
        }

        for column in columns {
            let old = before.and_then(|v| column_value(v, &column));
            let new = after.and_then(|v| column_value(v, &column));
            if old == new {
                continue;
            }
            // `make_mut` copies only if this view is not the sole owner — which
            // is to say, only the first write after a fork pays for the branch's
            // own copy, and a branch that is forked and only read from never
            // pays at all. Taken once per changed column rather than per call,
            // because after the first the refcount is already one.
            let indexes = Arc::make_mut(&mut self.indexes);
            if let Some(old) = old.as_ref().and_then(theta_core::index::encode) {
                indexes.remove(&table, &column, &old, &primary_key);
            }
            if let Some(new) = new.as_ref().and_then(theta_core::index::encode) {
                indexes.insert(&table, &column, new, &primary_key);
            }
        }
    }

    /// Rebuild every index from the rows currently in the view.
    ///
    /// Needed after loading a snapshot, which does not carry them, and after a
    /// new index is declared over a table that already has rows. O(rows in the
    /// indexed tables) — a fold over the materialized state, not a replay of
    /// the log.
    pub fn rebuild_indexes(&mut self) {
        // A fresh allocation rather than `make_mut`: this discards everything
        // anyway, so copying the old contents first would be work done to throw
        // away.
        self.indexes = Arc::new(Indexes::default());
        let tables: Vec<String> = self.schema.tables.keys().cloned().collect();
        for table in tables {
            let columns = self.indexed_columns(&table);
            if columns.is_empty() {
                continue;
            }
            let prefix = theta_core::RowAddress::prefix(&table);
            let rows: Vec<(String, Value)> = self
                .keys
                .range(prefix.clone()..)
                .take_while(|(key, _)| key.starts_with(&prefix))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            for (key, value) in rows {
                self.reindex_row(&key, None, Some(&value));
            }
        }
    }

    /// Fold one entry forward. Entries must be applied oldest-first.
    pub fn apply(&mut self, entry: &LogEntry) {
        let ctx = OpContext {
            // The branch is the replica: two writers on the same branch are
            // sequential, two on different branches are concurrent.
            replica: ReplicaId(entry.branch_id.0),
            commit: entry.commit_id.0,
            timestamp_ms: entry.timestamp_ms,
        };
        self.apply_op(&entry.op, ctx);
        self.applied += 1;
    }

    fn apply_op(&mut self, op: &OpType, ctx: OpContext) {
        match op {
            OpType::Put { key, value } => {
                let previous = self.keys.insert(key.clone(), value.clone());
                self.versions.insert(key.clone(), ctx.commit);
                self.reindex_row(key, previous.as_ref(), Some(value));
            }
            OpType::Crdt { key, mutation } => {
                self.apply_crdt(key, mutation, ctx);
                // A CRDT mutation is a write to the row, so it moves the row's
                // version. Leaving it unchanged would let a conditional write
                // land on a row that has changed underneath it, which is the
                // exact update this exists to stop being lost.
                self.versions.insert(key.clone(), ctx.commit);
            }
            OpType::Delete { key } => {
                let previous = self.keys.remove(key);
                self.versions.remove(key);
                self.reindex_row(key, previous.as_ref(), None);
            }
            // All-or-nothing at commit time: by the time a transaction entry is
            // in the log it has already been accepted whole, so folding it is
            // just folding its ops in order.
            OpType::Transaction { ops } => {
                for op in ops {
                    self.apply_op(op, ctx);
                }
            }
            OpType::Schema { change } => self.apply_schema(change),
            // Branch and merge entries move pointers; they carry no state of
            // their own into the view.
            OpType::BranchCreate { .. } | OpType::Merge { .. } => {}
        }
    }

    /// Fold a schema change into the view.
    ///
    /// A change that claims to remove data removes it. This used to touch only
    /// the schema, so `DROP TABLE users` left every row of `users` readable and
    /// `DROP COLUMN users.email` left every email in place — the Safety Layer's
    /// strongest gate was guarding operations that destroyed nothing, and a
    /// user who checked would find the data still there and conclude the gate
    /// was confused.
    ///
    /// This is still a deterministic fold: the log is unchanged and replaying it
    /// from genesis produces exactly this view
    /// (`03-data-model-consistency.md` §2.1). What is gone is gone from the
    /// *state*, which is the only thing any reader can see.
    fn apply_schema(&mut self, change: &SchemaChange) {
        match change {
            SchemaChange::AddTable { table } => {
                self.schema.tables.insert(table.name.clone(), table.clone());
            }
            SchemaChange::DropTable { table } => {
                self.schema.tables.remove(table);
                self.drop_rows(table);
            }
            SchemaChange::AddColumn { table, field } => {
                self.table_mut(table)
                    .fields
                    .insert(field.name.clone(), field.clone());
            }
            SchemaChange::DropColumn { table, column } => {
                self.table_mut(table).fields.remove(column);
                self.drop_column_values(table, column);
            }
            SchemaChange::AlterColumnType {
                table, column, to, ..
            } => {
                if let Some(field) = self.table_mut(table).fields.get_mut(column) {
                    field.ty = *to;
                }
            }
            SchemaChange::SetNullable {
                table,
                column,
                nullable,
                ..
            } => {
                if let Some(field) = self.table_mut(table).fields.get_mut(column) {
                    field.nullable = *nullable;
                }
            }
            SchemaChange::AddIndex { table, index } => {
                let t = self.table_mut(table);
                t.indexes.retain(|i| i.name != index.name);
                t.indexes.push(index.clone());
                // Declared over a table that may already hold rows, so the
                // index has to be populated from them. A new index that only
                // saw future writes would answer queries about the past wrongly
                // — and wrongly by *omission*, which no filter can correct.
                self.rebuild_indexes();
            }
            SchemaChange::DropIndex { table, index } => {
                self.table_mut(table).indexes.retain(|i| &i.name != index);
                self.rebuild_indexes();
            }
            SchemaChange::RenameColumn { table, from, to } => {
                let t = self.table_mut(table);
                if let Some(mut field) = t.fields.remove(from) {
                    field.name = to.clone();
                    t.fields.insert(to.clone(), field);
                }
                // The rows too, or the rename is a declaration that disagrees
                // with every row it describes — which is the "rename that is
                // really a drop" the corpus already tests for.
                self.rename_column_values(table, from, to);
            }
            SchemaChange::SetCrdt {
                table,
                column,
                crdt,
            } => {
                if let Some(field) = self.table_mut(table).fields.get_mut(column) {
                    field.crdt = *crdt;
                }
            }
        }
    }

    /// Fold a CRDT op into the field's state.
    ///
    /// The state's kind comes from the schema when the field is declared, and
    /// from the op itself in dynamic mode. An op that does not match the
    /// existing kind is rejected and recorded — never coerced.
    fn apply_crdt(&mut self, key: &str, mutation: &theta_core::CrdtOp, ctx: OpContext) {
        use theta_core::CrdtOp;

        // A CRDT-typed key is a whole row whose value is the CRDT — the row's
        // single `value` column. Addressed like any other row, so the schema
        // lookup goes through the same convention the write path uses.
        let declared = theta_core::RowAddress::parse(key)
            .and_then(|address| self.schema.field(address.table, theta_core::VALUE_COLUMN))
            .and_then(|f| f.crdt);

        let inferred = match mutation {
            CrdtOp::Increment { .. } => CrdtKind::Counter,
            CrdtOp::SetRegister { .. } => CrdtKind::Register,
            CrdtOp::SetAdd { .. } | CrdtOp::SetRemove { .. } => CrdtKind::Set,
            CrdtOp::SeqInsert { .. } | CrdtOp::SeqRemove { .. } => CrdtKind::Sequence,
        };

        // A declared kind wins over the op's shape: a write that disagrees with
        // the schema is the caller's error, not a reason to change the schema.
        let kind = declared.unwrap_or(inferred);
        let state = self
            .crdts
            .entry(key.to_string())
            .or_insert_with(|| CrdtState::empty(kind));

        if !apply_op(state, mutation, ctx) {
            self.rejected.push(RejectedOp {
                key: key.to_string(),
                reason: format!("op does not apply to a {:?} field", state.kind()),
            });
        }
    }

    /// Every key of one table, plain and CRDT alike.
    fn keys_of(&self, table: &str) -> (Vec<String>, Vec<String>) {
        let prefix = theta_core::RowAddress::prefix(table);
        let plain = self
            .keys
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter(|(key, _)| theta_core::RowAddress::parse(key).is_some())
            .map(|(key, _)| key.clone())
            .collect();

        let crdt = self
            .crdts
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter(|(key, _)| theta_core::RowAddress::parse(key).is_some())
            .map(|(key, _)| key.clone())
            .collect();

        (plain, crdt)
    }

    /// Remove every row of a table.
    fn drop_rows(&mut self, table: &str) {
        let (plain, crdt) = self.keys_of(table);
        for key in plain {
            self.keys.remove(&key);
        }
        for key in crdt {
            self.crdts.remove(&key);
        }
    }

    /// Remove one column from every row of a table.
    ///
    /// A row whose only column was dropped is removed outright: a scalar row is
    /// its `value` column, and keeping a row with nothing in it would leave a
    /// key that reads back as an empty map nobody wrote.
    fn drop_column_values(&mut self, table: &str, column: &str) {
        let (plain, crdt) = self.keys_of(table);

        for key in plain {
            // Two steps rather than a `get_mut` held across the removal: an
            // `OrdMap`'s `get_mut` path-copies and borrows the map for as long
            // as the reference lives, so removing through it in the same match
            // is a second mutable borrow.
            let now_empty = match self.keys.get_mut(&key) {
                Some(Value::Map(fields)) => {
                    fields.remove(column);
                    fields.is_empty()
                }
                Some(_) => column == theta_core::VALUE_COLUMN,
                None => false,
            };
            if now_empty {
                self.keys.remove(&key);
            }
        }

        // A CRDT-typed field is the row's `value` column.
        if column == theta_core::VALUE_COLUMN {
            for key in crdt {
                self.crdts.remove(&key);
            }
        }
    }

    /// Move one column's values to a new name across every row of a table.
    fn rename_column_values(&mut self, table: &str, from: &str, to: &str) {
        if from == to {
            return;
        }
        let (plain, _) = self.keys_of(table);
        for key in plain {
            if let Some(Value::Map(fields)) = self.keys.get_mut(&key) {
                if let Some(value) = fields.remove(from) {
                    fields.insert(to.to_string(), value);
                }
            }
        }
    }

    fn table_mut(&mut self, name: &str) -> &mut TableDef {
        self.schema
            .tables
            .entry(name.to_string())
            .or_insert_with(|| TableDef {
                name: name.to_string(),
                ..Default::default()
            })
    }

    /// Rebuild from scratch. `entries` must be oldest-first.
    pub fn replay<'a>(entries: impl IntoIterator<Item = &'a LogEntry>) -> Self {
        let mut view = Self::new();
        for entry in entries {
            view.apply(entry);
        }
        view
    }
}

/// The view as a source of rows for query execution.
///
/// Rows are found by key prefix, following the addressing convention in
/// `theta_core::address`. CRDT-typed keys are included and resolve to their
/// converged value, so a query sees the same thing a `get` would rather than a
/// second, parallel world.
impl theta_core::RowSource for MaterializedView {
    fn scan(&self, table: &str) -> Vec<(String, Value)> {
        let prefix = theta_core::RowAddress::prefix(table);
        let mut rows: Vec<(String, Value)> = self
            .keys
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let address = theta_core::RowAddress::parse(key)?;
                Some((address.primary_key.to_string(), value.clone()))
            })
            .collect();

        rows.extend(
            self.crdts
                .range(prefix.clone()..)
                .take_while(|(key, _)| key.starts_with(&prefix))
                .filter_map(|(key, state)| {
                    let address = theta_core::RowAddress::parse(key)?;
                    Some((address.primary_key.to_string(), state.value()))
                }),
        );

        // `keys` and `crdts` are each ordered, but concatenating them is not.
        // A scan must be reproducible, so it is re-sorted.
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    fn row(&self, table: &str, primary_key: &str) -> Option<Value> {
        let key = format!("{table}:{primary_key}");
        self.keys
            .get(&key)
            .cloned()
            .or_else(|| self.crdts.get(&key).map(|state| state.value()))
    }

    fn scan_up_to(&self, table: &str, max: Option<usize>) -> Vec<(String, Value)> {
        let Some(max) = max else {
            return self.scan(table);
        };
        if max == 0 {
            return Vec::new();
        }

        // Only the plain-key path can stop early. `scan` concatenates two
        // ordered maps and re-sorts, so the first `max` of the merged order is
        // not the first `max` of either half — and a CRDT-typed row could sort
        // ahead of any prefix taken from `keys`. Taking a shortcut here would
        // return a different set of rows than `LIMIT` over a full scan, which
        // is a wrong answer rather than a slow one.
        //
        // So: the shortcut applies when this table has no CRDT-typed rows, and
        // otherwise the honest full scan runs and is truncated.
        let prefix = theta_core::RowAddress::prefix(table);
        let has_crdt_rows = self
            .crdts
            .range(prefix.clone()..)
            .any(|(key, _)| key.starts_with(&prefix));
        if has_crdt_rows {
            let mut rows = self.scan(table);
            rows.truncate(max);
            return rows;
        }

        self.keys
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter_map(|(key, value)| {
                let address = theta_core::RowAddress::parse(key)?;
                Some((address.primary_key.to_string(), value.clone()))
            })
            .take(max)
            .collect()
    }

    fn column_type(&self, table: &str, column: &str) -> Option<theta_core::ValueType> {
        self.schema.field(table, column).map(|f| f.ty)
    }

    fn index_candidates(
        &self,
        table: &str,
        column: &str,
        bound: &theta_core::IndexBound,
    ) -> Option<Vec<String>> {
        // Distinguish "no index here" from "an index that matched nothing".
        // Both are `Option`, and collapsing them would turn an empty result
        // into a full scan — correct, but it would hide the case the planner
        // most wants to know about.
        if !self.indexes.covers(table, column) {
            return None;
        }
        self.indexes.candidates(table, column, bound)
    }
}

#[cfg(test)]
mod tests {
    use theta_core::{Author, BranchId, CommitId, ContentHash};

    use super::*;

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

    fn row(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn seeded_users() -> Vec<LogEntry> {
        (0..5)
            .map(|i| {
                entry(OpType::Put {
                    key: format!("users:{i}"),
                    value: row(&[
                        ("email", Value::Text(format!("u{i}@example.com"))),
                        ("name", Value::Text(format!("User {i}"))),
                    ]),
                })
            })
            .collect()
    }

    fn folded(entries: &[LogEntry]) -> MaterializedView {
        let mut view = MaterializedView::new();
        for e in entries {
            view.apply(e);
        }
        view
    }

    #[test]
    fn dropping_a_table_removes_its_rows_and_not_another_tables() {
        let mut entries = seeded_users();
        entries.push(entry(OpType::Put {
            key: "orders:1".into(),
            value: Value::Int(99),
        }));
        entries.push(entry(OpType::Schema {
            change: SchemaChange::DropTable {
                table: "users".into(),
            },
        }));

        let view = folded(&entries);

        // Before this, `DROP TABLE users` removed the declaration and left
        // every row readable — the strongest gate in the product was guarding
        // an operation that destroyed nothing.
        assert!(
            view.get("users:0").is_none(),
            "a dropped table kept its rows"
        );
        assert!(view.get("users:4").is_none());
        assert_eq!(view.get("orders:1"), Some(&Value::Int(99)));
    }

    #[test]
    fn dropping_a_column_removes_that_column_and_leaves_the_rest_of_the_row() {
        let mut entries = seeded_users();
        entries.push(entry(OpType::Schema {
            change: SchemaChange::DropColumn {
                table: "users".into(),
                column: "email".into(),
            },
        }));

        let view = folded(&entries);

        let Some(Value::Map(fields)) = view.get("users:0") else {
            panic!("the row itself should survive dropping one of its columns");
        };
        assert!(
            !fields.contains_key("email"),
            "the dropped column is still there"
        );
        assert_eq!(fields.get("name"), Some(&Value::Text("User 0".into())));
    }

    #[test]
    fn a_row_whose_only_column_was_dropped_is_removed_rather_than_left_empty() {
        let entries = vec![
            entry(OpType::Put {
                key: "notes:1".into(),
                value: Value::Text("just a note".into()),
            }),
            entry(OpType::Schema {
                change: SchemaChange::DropColumn {
                    table: "notes".into(),
                    column: theta_core::VALUE_COLUMN.into(),
                },
            }),
        ];

        // A scalar row *is* its `value` column. Keeping the key would leave
        // something that reads back as a row nobody wrote.
        assert!(folded(&entries).get("notes:1").is_none());
    }

    #[test]
    fn renaming_a_column_moves_the_data_with_the_declaration() {
        let mut entries = seeded_users();
        entries.push(entry(OpType::Schema {
            change: SchemaChange::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "email_v2".into(),
            },
        }));

        let view = folded(&entries);
        let Some(Value::Map(fields)) = view.get("users:0") else {
            panic!("row missing");
        };

        // A rename that moved only the declaration would leave every row
        // described by a schema that does not match it.
        assert!(!fields.contains_key("email"));
        assert_eq!(
            fields.get("email_v2"),
            Some(&Value::Text("u0@example.com".into()))
        );
    }

    #[test]
    fn a_destructive_schema_change_folds_the_same_way_incrementally_and_on_replay() {
        // The whole data model rests on state being a deterministic fold over
        // the log (`03-data-model-consistency.md` §2.1). A schema change that
        // rewrites rows is the case most likely to break that.
        let mut entries = seeded_users();
        entries.push(entry(OpType::Schema {
            change: SchemaChange::DropColumn {
                table: "users".into(),
                column: "email".into(),
            },
        }));
        entries.push(entry(OpType::Put {
            key: "users:9".into(),
            value: row(&[("email", Value::Text("late@example.com".into()))]),
        }));
        entries.push(entry(OpType::Schema {
            change: SchemaChange::DropTable {
                table: "users".into(),
            },
        }));

        assert_eq!(folded(&entries), MaterializedView::replay(&entries));
    }

    #[test]
    fn a_write_after_a_drop_is_not_undone_by_it() {
        // The drop folds at the point it appears in the log. A row written
        // afterwards is a new row, not a resurrection of an old one.
        let entries = vec![
            entry(OpType::Put {
                key: "users:1".into(),
                value: Value::Int(1),
            }),
            entry(OpType::Schema {
                change: SchemaChange::DropTable {
                    table: "users".into(),
                },
            }),
            entry(OpType::Put {
                key: "users:2".into(),
                value: Value::Int(2),
            }),
        ];

        let view = folded(&entries);
        assert!(view.get("users:1").is_none());
        assert_eq!(view.get("users:2"), Some(&Value::Int(2)));
    }

    #[test]
    fn incremental_fold_matches_full_replay() {
        let entries = vec![
            entry(OpType::Put {
                key: "a".into(),
                value: Value::Int(1),
            }),
            entry(OpType::Put {
                key: "b".into(),
                value: Value::Int(2),
            }),
            entry(OpType::Put {
                key: "a".into(),
                value: Value::Int(3),
            }),
            entry(OpType::Delete { key: "b".into() }),
        ];

        let mut incremental = MaterializedView::new();
        for e in &entries {
            incremental.apply(e);
        }
        assert_eq!(incremental, MaterializedView::replay(&entries));
        assert_eq!(incremental.get("a"), Some(&Value::Int(3)));
        assert_eq!(incremental.get("b"), None);
    }

    #[test]
    fn transactions_apply_every_op() {
        let mut view = MaterializedView::new();
        view.apply(&entry(OpType::Transaction {
            ops: vec![
                OpType::Put {
                    key: "x".into(),
                    value: Value::Int(1),
                },
                OpType::Put {
                    key: "y".into(),
                    value: Value::Int(2),
                },
            ],
        }));
        assert_eq!(view.keys.len(), 2);
    }
}

#[cfg(test)]
mod version_tests {
    use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};

    use super::*;

    fn entry(commit: u64, op: OpType) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash::ZERO,
            commit_id: CommitId(commit),
            branch_id: BranchId::MAIN,
            op,
            author: Author::System,
            timestamp_ms: commit as i64,
        }
    }

    fn put(commit: u64, key: &str, value: &str) -> LogEntry {
        entry(
            commit,
            OpType::Put {
                key: key.into(),
                value: Value::Text(value.into()),
            },
        )
    }

    #[test]
    fn a_rows_version_is_the_commit_that_last_wrote_it() {
        let mut view = MaterializedView::new();
        view.apply(&put(7, "a", "one"));
        assert_eq!(view.version_of("a"), Some(7));

        view.apply(&put(9, "a", "two"));
        assert_eq!(view.version_of("a"), Some(9));
    }

    #[test]
    fn writing_one_key_does_not_move_anothers_version() {
        // The property a branch-wide counter cannot provide, and the reason a
        // conditional write needs this rather than `applied`.
        let mut view = MaterializedView::new();
        view.apply(&put(1, "a", "one"));
        view.apply(&put(2, "b", "two"));

        assert_eq!(view.version_of("a"), Some(1), "an unrelated write moved it");
        assert_eq!(view.applied, 2, "the branch counter did move, as it should");
    }

    #[test]
    fn a_missing_key_has_no_version_rather_than_version_zero() {
        // `None` and `Some(0)` must stay different answers: a sentinel would
        // make "I forgot the version" and "I require this row to be absent" the
        // same request.
        let view = MaterializedView::new();
        assert_eq!(view.version_of("nothing"), None);
    }

    #[test]
    fn deleting_returns_a_key_to_absent() {
        let mut view = MaterializedView::new();
        view.apply(&put(1, "a", "one"));
        view.apply(&entry(2, OpType::Delete { key: "a".into() }));

        assert_eq!(
            view.version_of("a"),
            None,
            "a deleted row kept a version, so `absent` could never be satisfied"
        );
    }

    #[test]
    fn recreating_a_deleted_key_refuses_a_stale_version() {
        // A reads version 1, B deletes, C recreates. A's conditional write must
        // still be refused — the row it read is gone.
        let mut view = MaterializedView::new();
        view.apply(&put(1, "a", "one"));
        view.apply(&entry(2, OpType::Delete { key: "a".into() }));
        view.apply(&put(3, "a", "three"));

        assert_eq!(view.version_of("a"), Some(3));
        assert_ne!(view.version_of("a"), Some(1));
    }

    #[test]
    fn a_transaction_versions_every_key_it_touches() {
        let mut view = MaterializedView::new();
        view.apply(&entry(
            5,
            OpType::Transaction {
                ops: vec![
                    OpType::Put {
                        key: "a".into(),
                        value: Value::Int(1),
                    },
                    OpType::Put {
                        key: "b".into(),
                        value: Value::Int(2),
                    },
                ],
            },
        ));

        // One commit, so one version — they changed together and a caller
        // holding either is holding the same moment.
        assert_eq!(view.version_of("a"), Some(5));
        assert_eq!(view.version_of("b"), Some(5));
    }

    #[test]
    fn versions_survive_a_snapshot_round_trip() {
        // The snapshot is a cache of the fold, and a cache that dropped this
        // would silently make every conditional write fail after a restart.
        let mut view = MaterializedView::new();
        view.apply(&put(4, "a", "one"));

        let json = serde_json::to_string(&view).expect("serialize");
        let restored: MaterializedView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.version_of("a"), Some(4));
    }
}
