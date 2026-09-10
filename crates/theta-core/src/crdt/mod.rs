//! CRDT primitives.
//!
//! Every type here implements [`Crdt`], whose `merge` must be commutative,
//! associative and idempotent. That trio is the whole guarantee behind "merges
//! of CRDT-typed fields are automatic and provably convergent"
//! (`01-system-architecture.md` §3.3) — so it is asserted by property test in
//! `tests/convergence.rs`, not just documented here.

mod counter;
mod orset;
mod register;
mod sequence;
mod state;

pub use counter::PnCounter;
pub use orset::OrSet;
pub use register::{LwwRegister, WriteStamp};
pub use sequence::{ElemId, RgaSequence};
pub use state::{apply_op, CanonicalValue, CrdtState, OpContext};

use serde::{Deserialize, Serialize};

/// Identifies the writer of an operation for CRDT bookkeeping. Derived from the
/// branch id, so two writers on the same branch never collide and two writers on
/// different branches always differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReplicaId(pub u64);

pub trait Crdt: Sized {
    /// The converged value this replica currently represents.
    type Output;

    /// Merge `other` into `self`. Must be commutative, associative and
    /// idempotent: `a.merge(b)` and `b.merge(a)` must yield equal `value()`,
    /// and merging the same state twice must change nothing.
    fn merge(&mut self, other: &Self);

    fn value(&self) -> Self::Output;

    fn merged(mut self, other: &Self) -> Self {
        self.merge(other);
        self
    }
}
