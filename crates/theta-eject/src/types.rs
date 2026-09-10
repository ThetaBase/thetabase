//! Mapping Postgres types onto the ThetaBase type lattice.
//!
//! The lattice is small — `Bool`, `Int`, `Float`, `Text`, `Bytes`,
//! `Timestamp`, `List`, `Map` — and Postgres's type system is not, so a
//! mapping is a series of decisions about meaning rather than a lookup table.
//!
//! Two rules run through all of it, both from `07-agent-safety-layer.md` §2:
//!
//! 1. **Nothing is silently narrowed.** `numeric` is arbitrary-precision and
//!    `Float` is not; `int8` spans more than an `f64` can name exactly. Where a
//!    mapping can lose meaning, it is still offered — a migration that refuses
//!    every awkward column migrates nothing — but it carries a
//!    [`Fidelity::Lossy`] and the reason, and `eject` surfaces that rather than
//!    deciding for the user.
//! 2. **Nothing is guessed.** A type with no honest home in the lattice maps to
//!    `Text` carrying its Postgres rendering, flagged, rather than being coerced
//!    into a shape that merely parses.

use serde::{Deserialize, Serialize};
use theta_core::schema::CrdtKind;
use theta_core::ValueType;

/// How faithfully a Postgres type survives the trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Every value of the source type is representable, and comes back equal.
    Exact,
    /// Representable, but a value can come back different — precision, range,
    /// or a structure flattened to its text form.
    Lossy,
}

/// What one Postgres column becomes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub ty: ValueType,
    pub fidelity: Fidelity,
    /// Why, in the operator's language. Present whenever fidelity is `Lossy`,
    /// because a warning without a reason is a warning nobody can act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// A CRDT type worth considering for this column, and why.
    ///
    /// A suggestion, never an application: choosing a CRDT changes how
    /// concurrent writes resolve, which is a decision about the application's
    /// semantics that a migration tool is not entitled to make.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crdt_suggestion: Option<CrdtSuggestion>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrdtSuggestion {
    pub kind: CrdtKind,
    pub because: String,
}

impl Mapping {
    fn exact(ty: ValueType) -> Self {
        Self {
            ty,
            fidelity: Fidelity::Exact,
            note: None,
            crdt_suggestion: None,
        }
    }

    fn lossy(ty: ValueType, note: impl Into<String>) -> Self {
        Self {
            ty,
            fidelity: Fidelity::Lossy,
            note: Some(note.into()),
            crdt_suggestion: None,
        }
    }

    fn suggesting(mut self, kind: CrdtKind, because: impl Into<String>) -> Self {
        self.crdt_suggestion = Some(CrdtSuggestion {
            kind,
            because: because.into(),
        });
        self
    }

    pub fn is_lossy(&self) -> bool {
        self.fidelity == Fidelity::Lossy
    }
}

/// Map a Postgres type name onto the lattice.
///
/// `udt_name` is what `information_schema` reports — `int4`, `timestamptz`,
/// `_text` for an array of text — which is more precise than `data_type` and is
/// what distinguishes the cases that matter.
pub fn map_type(udt_name: &str, column: &str) -> Mapping {
    let mapping = match udt_name {
        "bool" => Mapping::exact(ValueType::Bool),

        // Every one of these fits an i64 exactly.
        "int2" | "int4" | "int8" => Mapping::exact(ValueType::Int),

        // Postgres serials are integers with a sequence attached; the sequence
        // does not travel, and neither does its next value.
        "float4" | "float8" => Mapping::exact(ValueType::Float),

        // The one that bites people. `numeric` is arbitrary precision and
        // exact; `f64` is neither. A money column migrated this way is the
        // classic silent data-meaning change, so it is flagged every time.
        "numeric" | "decimal" => Mapping::lossy(
            ValueType::Float,
            "`numeric` is arbitrary-precision and exact; `Float` is binary and \
             is not. Values beyond 2^53, and most decimal fractions, will not \
             come back identical. For money, consider storing minor units as an \
             integer instead.",
        ),

        "text" | "varchar" | "char" | "bpchar" | "name" | "citext" => {
            Mapping::exact(ValueType::Text)
        }

        "bytea" => Mapping::exact(ValueType::Bytes),

        // Both become an instant. `timestamp` (without time zone) has no zone
        // to preserve, so reading it as UTC is a decision, not a translation.
        "timestamptz" => Mapping::exact(ValueType::Timestamp),
        "timestamp" => Mapping::lossy(
            ValueType::Timestamp,
            "`timestamp without time zone` carries no zone. It is read as UTC, \
             which is a choice rather than a conversion - if the application \
             meant local time, the instant changes.",
        ),
        "date" => Mapping::lossy(
            ValueType::Timestamp,
            "a `date` becomes an instant at midnight UTC, gaining a precision \
             the source did not have.",
        ),
        "time" | "timetz" | "interval" => Mapping::lossy(
            ValueType::Text,
            "no lattice type means a time of day or a duration; kept as its \
             Postgres text rendering so nothing is invented.",
        ),

        "uuid" => Mapping::exact(ValueType::Text),
        "json" | "jsonb" => Mapping::exact(ValueType::Map),

        // Arrays. `_x` is Postgres's spelling for "array of x".
        name if name.starts_with('_') => Mapping::exact(ValueType::List),

        // Enums, ranges, geometry, network addresses, and anything a user
        // defined. Kept as text rather than guessed at.
        other => Mapping::lossy(
            ValueType::Text,
            format!(
                "`{other}` has no counterpart in the lattice; kept as its \
                 Postgres text rendering. Nothing is inferred about its \
                 structure."
            ),
        ),
    };

    match crdt_for(udt_name, column) {
        Some((kind, because)) => mapping.suggesting(kind, because),
        None => mapping,
    }
}

/// A CRDT worth considering, where concurrency plausibly matters.
///
/// Suggestions are driven by the shape of the column, and by name only where
/// the name is a strong and conventional signal. This is a hint in a report a
/// human reads — the Safety Layer's rule-based classification is a different
/// thing entirely and stays that way (invariant 2).
fn crdt_for(udt_name: &str, column: &str) -> Option<(CrdtKind, String)> {
    let lower = column.to_ascii_lowercase();

    // Counters are the case CRDTs exist for: two branches each incrementing
    // resolves to the sum, where last-write-wins would drop one.
    let counter_ish = lower.ends_with("_count")
        || lower.ends_with("_total")
        || lower == "count"
        || lower.contains("views")
        || lower.contains("likes")
        || lower.contains("balance")
        || lower.contains("quantity")
        || lower.contains("stock");
    if counter_ish && matches!(udt_name, "int2" | "int4" | "int8") {
        return Some((
            CrdtKind::Counter,
            format!(
                "`{column}` reads as a running count. Two branches incrementing \
                 it concurrently converge to the sum under a Counter; under a \
                 plain value one of the two increments is lost."
            ),
        ));
    }

    // A set of things, concurrently added to and removed from.
    if udt_name.starts_with('_') {
        return Some((
            CrdtKind::Set,
            format!(
                "`{column}` is an array. If elements are added and removed \
                 independently, an OR-Set converges where a whole-array write \
                 makes concurrent edits collide."
            ),
        ));
    }

    // Anything else edited from more than one place at once. Only offered for
    // columns whose name says they change, to keep the report short enough to
    // read.
    let mutable_ish = lower.starts_with("updated")
        || lower.ends_with("_status")
        || lower == "status"
        || lower == "state";
    if mutable_ish {
        return Some((
            CrdtKind::Register,
            format!(
                "`{column}` looks like mutable state. A last-write-wins \
                 Register makes the tie-break explicit and deterministic rather \
                 than leaving it to arrival order."
            ),
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_and_text_survive_exactly() {
        for name in ["int2", "int4", "int8"] {
            assert_eq!(map_type(name, "id").ty, ValueType::Int);
            assert_eq!(map_type(name, "id").fidelity, Fidelity::Exact);
        }
        assert_eq!(map_type("text", "name").ty, ValueType::Text);
        assert_eq!(map_type("bool", "active").fidelity, Fidelity::Exact);
        assert_eq!(map_type("bytea", "blob").ty, ValueType::Bytes);
    }

    #[test]
    fn numeric_is_flagged_because_it_cannot_survive_as_a_float() {
        // The money case. If this ever silently becomes Exact, a migration will
        // change someone's balances by fractions of a cent and say nothing.
        let mapping = map_type("numeric", "price");
        assert_eq!(mapping.ty, ValueType::Float);
        assert!(mapping.is_lossy());
        let note = mapping.note.expect("a lossy mapping must say why");
        assert!(
            note.contains("arbitrary-precision"),
            "the note has to explain the loss, got: {note}"
        );
    }

    #[test]
    fn a_timestamp_without_a_zone_is_flagged_and_one_with_a_zone_is_not() {
        assert_eq!(
            map_type("timestamptz", "created_at").fidelity,
            Fidelity::Exact
        );
        assert!(map_type("timestamp", "created_at").is_lossy());
        assert!(map_type("date", "born_on").is_lossy());
    }

    #[test]
    fn an_unknown_type_is_kept_as_text_rather_than_guessed_at() {
        let mapping = map_type("mood_enum", "mood");
        assert_eq!(mapping.ty, ValueType::Text);
        assert!(mapping.is_lossy());
        assert!(mapping.note.unwrap().contains("no counterpart"));
    }

    #[test]
    fn arrays_become_lists_and_suggest_a_set() {
        let mapping = map_type("_text", "tags");
        assert_eq!(mapping.ty, ValueType::List);
        assert_eq!(
            mapping.crdt_suggestion.as_ref().map(|s| s.kind),
            Some(CrdtKind::Set)
        );
    }

    #[test]
    fn a_counter_shaped_column_suggests_a_counter() {
        let mapping = map_type("int8", "view_count");
        let suggestion = mapping.crdt_suggestion.expect("expected a suggestion");
        assert_eq!(suggestion.kind, CrdtKind::Counter);
        assert!(suggestion.because.contains("lost"));
    }

    #[test]
    fn a_counter_shaped_name_on_a_non_numeric_column_suggests_nothing() {
        // The name heuristic must not fire on a type where it makes no sense -
        // a text column called `status_count` is not a counter.
        assert!(map_type("text", "view_count").crdt_suggestion.is_none());
    }

    #[test]
    fn an_ordinary_column_gets_no_suggestion() {
        // The report has to stay readable. Suggesting a CRDT for every column
        // is the same as suggesting one for none.
        assert!(map_type("text", "email").crdt_suggestion.is_none());
        assert!(map_type("int4", "id").crdt_suggestion.is_none());
    }

    #[test]
    fn json_becomes_a_map_and_keeps_its_structure() {
        assert_eq!(map_type("jsonb", "payload").ty, ValueType::Map);
        assert_eq!(map_type("jsonb", "payload").fidelity, Fidelity::Exact);
    }
}
