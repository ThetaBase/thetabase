//! CRDT state as a fold over logged operations.
//!
//! The four primitives in this module's siblings are pure data types. This file
//! is what connects them to the log: a field's CRDT state is the fold of the
//! [`CrdtOp`](crate::log::CrdtOp)s applied to it, so it is derived state like
//! everything else (`03-data-model-consistency.md` §2.1) and merging two
//! branches is merging two folds.

use serde::{Deserialize, Serialize};

use super::{Crdt, LwwRegister, OrSet, PnCounter, ReplicaId, RgaSequence, WriteStamp};
use crate::schema::CrdtKind;
use crate::value::Value;

/// A [`Value`] usable as a set element or map key.
///
/// [`Value`] cannot be `Ord` — it holds `f64` — but an OR-Set needs a total
/// order over its elements. Ordering by canonical encoding gives one: the
/// encoding is deterministic because every map inside a `Value` is a `BTreeMap`,
/// so two equal values always encode identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalValue(pub Value);

impl CanonicalValue {
    fn encoded(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).expect("value is serializable")
    }
}

impl Eq for CanonicalValue {}

impl PartialOrd for CanonicalValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CanonicalValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.encoded().cmp(&other.encoded())
    }
}

/// The CRDT state of one field.
///
/// Variants are fixed by [`CrdtKind`]; a field's kind is declared in the schema,
/// so the state's shape is never inferred from the data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "crdt", rename_all = "snake_case")]
pub enum CrdtState {
    Counter(PnCounter),
    Register(LwwRegister<Value>),
    Set(OrSet<CanonicalValue>),
    Sequence(RgaSequence<Value>),
}

impl CrdtState {
    /// An empty state of the given kind.
    pub fn empty(kind: CrdtKind) -> Self {
        match kind {
            CrdtKind::Counter => CrdtState::Counter(PnCounter::new()),
            CrdtKind::Register => CrdtState::Register(LwwRegister::new()),
            CrdtKind::Set => CrdtState::Set(OrSet::new()),
            CrdtKind::Sequence => CrdtState::Sequence(RgaSequence::new()),
        }
    }

    pub fn kind(&self) -> CrdtKind {
        match self {
            CrdtState::Counter(_) => CrdtKind::Counter,
            CrdtState::Register(_) => CrdtKind::Register,
            CrdtState::Set(_) => CrdtKind::Set,
            CrdtState::Sequence(_) => CrdtKind::Sequence,
        }
    }

    /// Merge `other` into `self`.
    ///
    /// Returns `false` if the two states are of different kinds, which is not a
    /// CRDT conflict but a schema conflict — the field's declared type diverged
    /// between branches — and is surfaced as such rather than merged.
    #[must_use]
    pub fn merge(&mut self, other: &CrdtState) -> bool {
        match (self, other) {
            (CrdtState::Counter(a), CrdtState::Counter(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::Register(a), CrdtState::Register(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::Set(a), CrdtState::Set(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::Sequence(a), CrdtState::Sequence(b)) => {
                a.merge(b);
                true
            }
            _ => false,
        }
    }

    /// The converged value, in the shape a reader sees.
    pub fn value(&self) -> Value {
        match self {
            CrdtState::Counter(c) => Value::Int(c.value()),
            CrdtState::Register(r) => r.value().unwrap_or(Value::Null),
            CrdtState::Set(s) => Value::List(s.value().into_iter().map(|c| c.0).collect()),
            CrdtState::Sequence(s) => Value::List(s.value()),
        }
    }
}

/// Apply one logged CRDT operation.
///
/// Returns `false` when the op does not match the state's kind — e.g. an
/// increment against a register. The write is rejected rather than reinterpreted
/// (`07-agent-safety-layer.md` §2: prevent, don't correct).
#[must_use]
pub fn apply_op(state: &mut CrdtState, op: &crate::log::CrdtOp, ctx: OpContext) -> bool {
    use crate::log::CrdtOp;

    match (state, op) {
        (CrdtState::Counter(c), CrdtOp::Increment { by }) => {
            if *by >= 0 {
                c.increment(ctx.replica, *by as u64);
            } else {
                c.decrement(ctx.replica, by.unsigned_abs());
            }
            true
        }
        (CrdtState::Register(r), CrdtOp::SetRegister { value }) => {
            r.set(value.clone(), ctx.stamp());
            true
        }
        (CrdtState::Set(s), CrdtOp::SetAdd { element }) => {
            s.add(CanonicalValue(element.clone()), ctx.replica);
            true
        }
        (CrdtState::Set(s), CrdtOp::SetRemove { element }) => {
            s.remove(&CanonicalValue(element.clone()));
            true
        }
        (CrdtState::Sequence(seq), CrdtOp::SeqInsert { id, after, value }) => {
            seq.insert_with_id(*id, *after, value.clone());
            true
        }
        (CrdtState::Sequence(seq), CrdtOp::SeqRemove { id }) => {
            seq.remove(*id);
            true
        }
        _ => false,
    }
}

/// Where an operation came from, which is what makes CRDT bookkeeping
/// deterministic: the replica is the branch and the commit is unique within it.
#[derive(Debug, Clone, Copy)]
pub struct OpContext {
    pub replica: ReplicaId,
    pub commit: u64,
    pub timestamp_ms: i64,
}

impl OpContext {
    pub fn stamp(&self) -> WriteStamp {
        WriteStamp::new(self.timestamp_ms, self.replica, self.commit)
    }
}

#[cfg(test)]
mod tests {
    use crate::log::CrdtOp;

    use super::*;

    fn ctx(branch: u64, commit: u64) -> OpContext {
        OpContext {
            replica: ReplicaId(branch),
            commit,
            timestamp_ms: commit as i64,
        }
    }

    #[test]
    fn counters_fold_increments_and_decrements() {
        let mut state = CrdtState::empty(CrdtKind::Counter);
        assert!(apply_op(
            &mut state,
            &CrdtOp::Increment { by: 5 },
            ctx(1, 0)
        ));
        assert!(apply_op(
            &mut state,
            &CrdtOp::Increment { by: -2 },
            ctx(1, 1)
        ));
        assert_eq!(state.value(), Value::Int(3));
    }

    #[test]
    fn an_op_against_the_wrong_crdt_kind_is_rejected_not_reinterpreted() {
        let mut register = CrdtState::empty(CrdtKind::Register);
        assert!(
            !apply_op(&mut register, &CrdtOp::Increment { by: 1 }, ctx(1, 0)),
            "an increment against a register must be rejected"
        );
        assert_eq!(
            register.value(),
            Value::Null,
            "and must not have mutated it"
        );
    }

    #[test]
    fn merging_states_of_different_kinds_is_refused() {
        let mut counter = CrdtState::empty(CrdtKind::Counter);
        let register = CrdtState::empty(CrdtKind::Register);
        assert!(!counter.merge(&register));
    }

    #[test]
    fn canonical_values_order_deterministically() {
        let a = CanonicalValue(Value::Text("a".into()));
        let b = CanonicalValue(Value::Text("b".into()));
        assert!(a < b);
        assert_eq!(a.cmp(&a.clone()), std::cmp::Ordering::Equal);
    }

    #[test]
    fn sets_converge_over_logged_adds_and_removes() {
        let mut left = CrdtState::empty(CrdtKind::Set);
        let mut right = CrdtState::empty(CrdtKind::Set);

        let add = |v: &str| CrdtOp::SetAdd {
            element: Value::Text(v.into()),
        };
        assert!(apply_op(&mut left, &add("x"), ctx(1, 0)));
        assert!(apply_op(&mut right, &add("y"), ctx(2, 0)));

        let mut merged_lr = left.clone();
        assert!(merged_lr.merge(&right));
        let mut merged_rl = right.clone();
        assert!(merged_rl.merge(&left));

        assert_eq!(merged_lr.value(), merged_rl.value());
        assert_eq!(
            merged_lr.value(),
            Value::List(vec![Value::Text("x".into()), Value::Text("y".into())])
        );
    }
}
