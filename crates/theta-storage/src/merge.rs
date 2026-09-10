//! Branch merge.
//!
//! Merging is where the product's central promise is either kept or broken:
//! CRDT-typed fields converge automatically and provably, and everything else
//! that genuinely diverged goes to a human. Never an LLM, never a heuristic,
//! never a silent pick (`01-system-architecture.md` §3.3,
//! `03-data-model-consistency.md` §2.4).
//!
//! The structure enforces that. [`merge`] returns a [`MergeOutcome`], and the
//! only variant that carries applicable operations is [`MergeOutcome::Merged`].
//! A conflict produces [`MergeOutcome::Conflicted`], which carries no ops at
//! all — so there is nothing for a caller to apply even if it wanted to, and no
//! `force` flag to reach for.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use theta_core::crdt::CrdtState;
use theta_core::schema::SchemaChange;
use theta_core::{BranchId, ContentHash, LogEntry, OpType, Value};

use crate::error::Result;
use crate::log::LogStore;
use crate::view::MaterializedView;

/// One field that diverged and cannot be reconciled without a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictRef {
    pub key: String,
    /// The value on the target branch.
    pub ours: Option<Value>,
    /// The value on the source branch.
    pub theirs: Option<Value>,
    /// Why this could not be merged automatically, in plain language.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum MergeOutcome {
    /// Nothing to do — the source is already an ancestor of the target.
    UpToDate,
    /// Everything reconciled.
    Merged {
        /// Schema changes the source made that the target has not.
        ///
        /// Replayed as the source's own entries, not re-derived from a
        /// proposal: what lands on the target is the change that was on the
        /// branch. Without this a branch carrying only a migration merged as a
        /// no-op — the migration silently did not travel
        /// (`07-agent-safety-layer.md` §5.4).
        schema: Vec<SchemaChange>,
        /// Plain-value assignments to append to the target branch. Safe to
        /// apply more than once: an assignment is idempotent.
        ops: Vec<OpType>,
        /// Merged state for each CRDT field either side touched.
        ///
        /// States, not operations. Merging two CRDT *states* is idempotent —
        /// that is the actual CRDT guarantee — whereas replaying an increment
        /// twice double-counts. Returning states is what makes merging an
        /// already-merged branch a no-op instead of a corruption.
        crdt: BTreeMap<String, CrdtState>,
        /// Fields reconciled by CRDT convergence rather than by taking a side.
        converged: Vec<String>,
    },
    /// Divergence a human has to resolve. Deliberately carries no ops.
    Conflicted { conflicts: Vec<ConflictRef> },
}

impl MergeOutcome {
    pub fn conflict_count(&self) -> usize {
        match self {
            MergeOutcome::Conflicted { conflicts } => conflicts.len(),
            _ => 0,
        }
    }
}

/// The declaration a schema change touches.
///
/// Two changes with the same target are two answers to one question; two with
/// different targets are independent and both travel.
pub(crate) fn declaration_key(change: &SchemaChange) -> (String, Option<String>) {
    declaration_of(change)
}

fn declaration_of(change: &SchemaChange) -> (String, Option<String>) {
    match change {
        SchemaChange::AddTable { table } => (table.name.clone(), None),
        SchemaChange::DropTable { table } => (table.clone(), None),
        SchemaChange::AddColumn { table, field } => (table.clone(), Some(field.name.clone())),
        SchemaChange::DropColumn { table, column }
        | SchemaChange::AlterColumnType { table, column, .. }
        | SchemaChange::SetNullable { table, column, .. }
        | SchemaChange::SetCrdt { table, column, .. } => (table.clone(), Some(column.clone())),
        SchemaChange::RenameColumn { table, from, .. } => (table.clone(), Some(from.clone())),
        SchemaChange::AddIndex { table, index } => (table.clone(), Some(index.name.clone())),
        SchemaChange::DropIndex { table, index } => (table.clone(), Some(index.clone())),
    }
}

/// The table a change belongs to.
fn table_of(change: &SchemaChange) -> &str {
    match change {
        SchemaChange::AddTable { table } => &table.name,
        SchemaChange::DropTable { table } => table,
        SchemaChange::AddColumn { table, .. }
        | SchemaChange::DropColumn { table, .. }
        | SchemaChange::AlterColumnType { table, .. }
        | SchemaChange::SetNullable { table, .. }
        | SchemaChange::SetCrdt { table, .. }
        | SchemaChange::RenameColumn { table, .. }
        | SchemaChange::AddIndex { table, .. }
        | SchemaChange::DropIndex { table, .. } => table,
    }
}

/// Does this change redefine the table as a whole, rather than one column of it?
///
/// The distinction matters because a whole-table change answers a question every
/// column-scoped change on that table also answers. "Drop `users`" and "add
/// `users.nickname`" are not independent, however different their declarations
/// look.
fn is_table_scoped(change: &SchemaChange) -> bool {
    matches!(
        change,
        SchemaChange::AddTable { .. } | SchemaChange::DropTable { .. }
    )
}

/// The declaration a change *creates*, where that differs from the one it
/// targets.
///
/// Only a rename does: `declaration_of` keys it on the old name, because that is
/// the declaration it consumes, which leaves the name it produces invisible to
/// the comparison. Two branches racing for one column name is exactly the
/// collision that needs catching.
fn creates_declaration(change: &SchemaChange) -> Option<(String, Option<String>)> {
    match change {
        SchemaChange::RenameColumn { table, to, .. } => Some((table.clone(), Some(to.clone()))),
        _ => None,
    }
}

/// Schema changes one side made, in the order it made them.
fn schema_changes(entries: &[LogEntry]) -> Vec<SchemaChange> {
    let mut out = Vec::new();
    collect_schema(entries, &mut out);
    out
}

fn collect_schema(entries: &[LogEntry], out: &mut Vec<SchemaChange>) {
    for entry in entries {
        collect_schema_op(&entry.op, out);
    }
}

fn collect_schema_op(op: &OpType, out: &mut Vec<SchemaChange>) {
    match op {
        OpType::Schema { change } => out.push(change.clone()),
        OpType::Transaction { ops } => {
            for op in ops {
                collect_schema_op(op, out);
            }
        }
        _ => {}
    }
}

/// What one side of a merge did to a key.
#[derive(Debug, Clone)]
enum SideEffect {
    /// A whole-value write. Two of these on one key is a conflict.
    Assigned(Option<Value>),
    /// CRDT operations. These compose with the other side's.
    Crdt(Vec<OpType>),
}

/// Merge `source` into `target`.
///
/// `base` is the commit the two branches diverged from. Both sides are read as
/// the operations applied since that point, which is what makes this a
/// three-way merge rather than a guess between two end states.
pub fn merge<S: LogStore>(
    store: &S,
    source: BranchId,
    target: BranchId,
    base: Option<ContentHash>,
    target_view: &MaterializedView,
    source_view: &MaterializedView,
) -> Result<MergeOutcome> {
    let source_head = store.head(source);
    if source_head.is_none() || source_head == base {
        return Ok(MergeOutcome::UpToDate);
    }
    if store.head(target) == source_head {
        return Ok(MergeOutcome::UpToDate);
    }

    // History walks newest-first; reverse so ops replay in the order they were
    // written.
    let mut theirs = store.history(source, base)?;
    theirs.reverse();

    // A source that has written nothing since the fork has nothing to merge.
    //
    // The head comparison above catches "the source is an ancestor", and a
    // freshly forked branch is not one: forking appends a `BranchCreate` entry,
    // so the source head differs from the base by exactly that bookkeeping.
    // Every untouched branch therefore fell through and produced `Merged` with
    // four empty collections — a merge that lands nothing, indistinguishable
    // from one that does something.
    //
    // Found by wiring the merge queue, where it mattered: `EnqueueError::
    // NothingToMerge` was unreachable, so an agent could queue a branch it had
    // never written to and watch a no-op wait its turn. A queue is a list of
    // merges expected to land, and an empty one lands nothing.
    //
    // Tested on the source's *entries*, not on the computed result. The broader
    // version — "the merge came out empty" — also swallows the case where both
    // branches independently wrote the same value, and that is a different fact:
    // there, the source genuinely changed something and the target happened to
    // agree. Three existing tests pin that distinction, and they were right to.
    if theirs.iter().all(|e| !contributes_to_a_merge(&e.op)) {
        return Ok(MergeOutcome::UpToDate);
    }
    let mut ours = store.history(target, base)?;
    ours.reverse();

    let their_effects = effects(&theirs);
    let our_effects = effects(&ours);

    let their_schema = schema_changes(&theirs);
    let our_schema = schema_changes(&ours);

    let touched: BTreeSet<&String> = their_effects.keys().chain(our_effects.keys()).collect();

    let mut ops = Vec::new();
    let mut crdt: BTreeMap<String, CrdtState> = BTreeMap::new();
    let mut converged = Vec::new();
    let mut conflicts = Vec::new();

    // Schema first: both sides redefining the same declaration is a divergence
    // no rule can settle, so it goes to a human like any other
    // (`03-data-model-consistency.md` §2.4).
    let ours_by_declaration: BTreeMap<(String, Option<String>), &SchemaChange> = our_schema
        .iter()
        .map(|change| (declaration_of(change), change))
        .collect();

    let mut schema = Vec::new();
    for change in &their_schema {
        let declaration = declaration_of(change);
        match ours_by_declaration.get(&declaration) {
            Some(ours) if *ours != change => {
                let (table, column) = &declaration;
                let name = match column {
                    Some(column) => format!("{table}.{column}"),
                    None => table.clone(),
                };
                conflicts.push(ConflictRef {
                    key: format!("schema:{name}"),
                    ours: serde_json::to_value(ours)
                        .ok()
                        .and_then(|v| serde_json::from_value(v).ok()),
                    theirs: serde_json::to_value(change)
                        .ok()
                        .and_then(|v| serde_json::from_value(v).ok()),
                    reason: format!(
                        "both branches redefined `{name}`; merging would discard one \
                         side's definition"
                    ),
                });
            }
            // Already applied on this side, by the same change.
            Some(_) => {}
            None => schema.push(change.clone()),
        }
    }

    // A whole-table change against a column-scoped one on the same table.
    //
    // These are different declarations, so the loop above never compared them —
    // and yet "drop `users`" and "add `users.nickname`" are two answers to one
    // question. Merging both lands a change describing a table the other change
    // removed; whichever replays second is wrong. A rule cannot pick, so it goes
    // to a human like any other divergence (`03-data-model-consistency.md` §2.4,
    // and invariant 5).
    let mut already_reported: BTreeSet<String> = conflicts
        .iter()
        .map(|c: &ConflictRef| c.key.clone())
        .collect();

    let changes_to = |changes: &[SchemaChange], table: &str| -> Vec<SchemaChange> {
        changes
            .iter()
            .filter(|c| table_of(c) == table)
            .cloned()
            .collect()
    };

    let tables_redefined = |changes: &[SchemaChange]| -> BTreeSet<String> {
        changes
            .iter()
            .filter(|c| is_table_scoped(c))
            .map(|c| table_of(c).to_string())
            .collect()
    };

    let contested: BTreeSet<String> = tables_redefined(&their_schema)
        .into_iter()
        .chain(tables_redefined(&our_schema))
        .collect();

    for table in contested {
        let ours_here = changes_to(&our_schema, &table);
        let theirs_here = changes_to(&their_schema, &table);
        // Only one side touched this table: nothing to disagree with.
        if ours_here.is_empty() || theirs_here.is_empty() {
            continue;
        }
        // The same migration on both branches, which is a duplicate rather than
        // a divergence — the same reasoning the per-declaration loop applies.
        if ours_here == theirs_here {
            continue;
        }
        let key = format!("schema:{table}");
        if !already_reported.insert(key.clone()) {
            continue;
        }
        conflicts.push(ConflictRef {
            key,
            ours: None,
            theirs: None,
            reason: format!(
                "one branch redefined the table `{table}` while the other changed \
                 it; merging would apply a change to a table the other side had \
                 already replaced or removed"
            ),
        });
    }

    // A rename racing another branch for the name it produces.
    let ours_declared: BTreeSet<(String, Option<String>)> = our_schema
        .iter()
        .map(declaration_of)
        .chain(our_schema.iter().filter_map(creates_declaration))
        .collect();

    for change in &their_schema {
        let Some((table, column)) = creates_declaration(change) else {
            continue;
        };
        if !ours_declared.contains(&(table.clone(), column.clone())) {
            continue;
        }
        let name = match &column {
            Some(column) => format!("{table}.{column}"),
            None => table.clone(),
        };
        let key = format!("schema:{name}");
        if !already_reported.insert(key.clone()) {
            continue;
        }
        conflicts.push(ConflictRef {
            key,
            ours: None,
            theirs: None,
            reason: format!(
                "one branch renamed a column to `{name}` while the other declared \
                 `{name}` itself; merging would leave two definitions competing \
                 for one name"
            ),
        });
    }

    for key in touched {
        match (our_effects.get(key), their_effects.get(key)) {
            // Only the source touched it.
            (None, Some(SideEffect::Assigned(_))) => {
                ops.extend(replay(key, their_effects.get(key).expect("just matched")));
            }
            (None, Some(SideEffect::Crdt(_))) => {
                if let Some(state) = source_view.crdts.get(key) {
                    crdt.insert(key.clone(), state.clone());
                    converged.push(key.clone());
                }
            }

            // Only the target touched it: already in place.
            (Some(_), None) => {}

            // Both sides ran CRDT ops: merge the two states, which is
            // commutative, associative and idempotent.
            (Some(SideEffect::Crdt(_)), Some(SideEffect::Crdt(_))) => {
                match (target_view.crdts.get(key), source_view.crdts.get(key)) {
                    (Some(ours_state), Some(theirs_state)) => {
                        let mut merged = ours_state.clone();
                        if merged.merge(theirs_state) {
                            crdt.insert(key.clone(), merged);
                            converged.push(key.clone());
                        } else {
                            // Same key, different CRDT kinds on each branch.
                            conflicts.push(ConflictRef {
                                key: key.clone(),
                                ours: Some(ours_state.value()),
                                theirs: Some(theirs_state.value()),
                                reason: "the branches gave this field different CRDT types; \
                                         merging would discard one side's type"
                                    .into(),
                            });
                        }
                    }
                    (None, Some(theirs_state)) => {
                        crdt.insert(key.clone(), theirs_state.clone());
                        converged.push(key.clone());
                    }
                    _ => {}
                }
            }

            // Both sides assigned. Nothing in the data says which is right.
            (Some(SideEffect::Assigned(ours_val)), Some(SideEffect::Assigned(theirs_val))) => {
                if ours_val == theirs_val {
                    // Same destination by different routes — not a conflict.
                    continue;
                }
                conflicts.push(ConflictRef {
                    key: key.clone(),
                    ours: ours_val.clone(),
                    theirs: theirs_val.clone(),
                    reason: "both branches wrote this field, and it has no CRDT type \
                             that would let the writes converge"
                        .into(),
                });
            }

            // One side treated the field as a plain value, the other as a CRDT.
            // That is a schema divergence, not a data conflict, and merging
            // either way would silently discard one side's model of the field.
            (Some(a), Some(b)) => conflicts.push(ConflictRef {
                key: key.clone(),
                ours: describe(a, target_view, key),
                theirs: describe(b, source_view, key),
                reason: "the branches disagree on whether this field is CRDT-typed; \
                         merging would discard one side's type"
                    .into(),
            }),

            (None, None) => unreachable!("key came from one of the two maps"),
        }
    }

    if !conflicts.is_empty() {
        // Conflicts win outright. A partial merge would land some of the
        // source's changes while leaving the rest unresolved, which is a state
        // neither branch ever had.
        return Ok(MergeOutcome::Conflicted { conflicts });
    }

    Ok(MergeOutcome::Merged {
        schema,
        ops,
        crdt,
        converged,
    })
}

/// Whether an op could put anything into a merge.
///
/// `BranchCreate` and `Merge` are a branch's own bookkeeping: they record what
/// happened to the branch, not what happened to the data, and neither travels to
/// a target. An entry list containing only these is a branch that has not been
/// written to.
fn contributes_to_a_merge(op: &OpType) -> bool {
    match op {
        OpType::Put { .. }
        | OpType::Crdt { .. }
        | OpType::Delete { .. }
        | OpType::Transaction { .. }
        | OpType::Schema { .. } => true,
        OpType::BranchCreate { .. } | OpType::Merge { .. } => false,
    }
}

/// Collapse a branch's entries into per-key effects.
fn effects(entries: &[LogEntry]) -> BTreeMap<String, SideEffect> {
    let mut out: BTreeMap<String, SideEffect> = BTreeMap::new();
    for entry in entries {
        collect(&entry.op, &mut out);
    }
    out
}

fn collect(op: &OpType, out: &mut BTreeMap<String, SideEffect>) {
    match op {
        OpType::Put { key, value } => {
            // Last write on this branch wins *within* the branch; ordering here
            // is total because it is one branch's own history.
            out.insert(key.clone(), SideEffect::Assigned(Some(value.clone())));
        }
        OpType::Delete { key } => {
            out.insert(key.clone(), SideEffect::Assigned(None));
        }
        OpType::Crdt { key, .. } => match out.get_mut(key) {
            Some(SideEffect::Crdt(ops)) => ops.push(op.clone()),
            // A plain write followed by CRDT ops on one branch is a type change
            // mid-branch; the CRDT ops are what the branch ended up meaning.
            _ => {
                out.insert(key.clone(), SideEffect::Crdt(vec![op.clone()]));
            }
        },
        OpType::Transaction { ops } => {
            for op in ops {
                collect(op, out);
            }
        }
        // Schema, branch and merge entries do not assign field values.
        OpType::Schema { .. } | OpType::BranchCreate { .. } | OpType::Merge { .. } => {}
    }
}

fn replay(key: &str, effect: &SideEffect) -> Vec<OpType> {
    match effect {
        SideEffect::Assigned(Some(value)) => {
            vec![OpType::Put {
                key: key.to_string(),
                value: value.clone(),
            }]
        }
        SideEffect::Assigned(None) => vec![OpType::Delete {
            key: key.to_string(),
        }],
        SideEffect::Crdt(ops) => ops.clone(),
    }
}

/// Best available rendering of a side's value, for a human reading the conflict.
fn describe(effect: &SideEffect, view: &MaterializedView, key: &str) -> Option<Value> {
    match effect {
        SideEffect::Assigned(v) => v.clone(),
        SideEffect::Crdt(_) => view.get_crdt(key),
    }
}

/// Merge the CRDT states of two views.
///
/// Used when promoting a shadow branch, where the states are already materialized
/// and there is no need to replay ops. Returns the keys whose kinds disagreed —
/// a schema conflict, which this function will not resolve.
pub fn merge_crdt_states(
    ours: &mut BTreeMap<String, CrdtState>,
    theirs: &BTreeMap<String, CrdtState>,
) -> Vec<String> {
    let mut mismatched = Vec::new();
    for (key, their_state) in theirs {
        match ours.get_mut(key) {
            Some(our_state) => {
                if !our_state.merge(their_state) {
                    mismatched.push(key.clone());
                }
            }
            None => {
                ours.insert(key.clone(), their_state.clone());
            }
        }
    }
    mismatched
}
