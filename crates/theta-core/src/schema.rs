//! Versioned schema. Schema changes are ordinary log ops, so schema branches and
//! merges exactly like data does (`01-system-architecture.md` §3.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::value::ValueType;

/// Which CRDT backs a mutable field under concurrent branches
/// (`03-data-model-consistency.md` §2.3). `None` marks the field
/// *conflict-eligible*: concurrent edits produce an explicit conflict record
/// rather than any automatic resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrdtKind {
    /// PN-Counter: commutative, associative, idempotent merge.
    Counter,
    /// LWW-Register, tie-broken by (timestamp, branch_id).
    Register,
    /// OR-Set: concurrent add/remove of the same element converges.
    Set,
    /// RGA-style sequence: concurrent inserts converge to one order.
    Sequence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub ty: ValueType,
    pub nullable: bool,
    /// `None` = conflict-eligible on concurrent modification.
    pub crdt: Option<CrdtKind>,
    /// Type provenance: the commit that established this field's canonical type,
    /// so "what type is this, really" is answerable deterministically.
    pub declared_at: Option<crate::hash::ContentHash>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TableDef {
    pub name: String,
    pub fields: BTreeMap<String, FieldDef>,
    pub indexes: Vec<IndexDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// The complete set of schema mutations. Kept closed and explicit: the Safety
/// Layer classifies by matching on this enum, so a new variant is a compile
/// error at the classifier until someone decides how risky it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum SchemaChange {
    AddTable {
        table: TableDef,
    },
    DropTable {
        table: String,
    },
    AddColumn {
        table: String,
        field: FieldDef,
    },
    DropColumn {
        table: String,
        column: String,
    },
    /// Any type change; the Safety Layer decides widen (safe) vs. narrow
    /// (destructive) via `ValueType::widens_to`.
    AlterColumnType {
        table: String,
        column: String,
        from: ValueType,
        to: ValueType,
    },
    SetNullable {
        table: String,
        column: String,
        nullable: bool,
        backfill: Option<crate::value::Value>,
    },
    AddIndex {
        table: String,
        index: IndexDef,
    },
    DropIndex {
        table: String,
        index: String,
    },
    /// Ambiguous by nature — additive or destructive depending on whether old
    /// references survive — so it is classified destructive by default
    /// (`07-agent-safety-layer.md` §3).
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    SetCrdt {
        table: String,
        column: String,
        crdt: Option<CrdtKind>,
    },
}

/// The materialized schema at some commit: a fold over `SchemaChange` ops.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub tables: BTreeMap<String, TableDef>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.get(name)
    }

    pub fn field(&self, table: &str, column: &str) -> Option<&FieldDef> {
        self.tables.get(table)?.fields.get(column)
    }
}
