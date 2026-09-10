use serde::{Deserialize, Serialize};

use super::{Crdt, ReplicaId};
use crate::log::CommitId;

/// The explicit tie-break rule from `03-data-model-consistency.md` §2.3:
/// commit timestamp first, then branch id, then the per-branch commit id.
///
/// The third component is what makes the order genuinely *total* rather than
/// merely usually-unambiguous. Timestamp and branch alone are not enough: one
/// branch can write the same field twice within the same millisecond, and if
/// those two writes reach different replicas the merge result would depend on
/// merge order — divergence, which is exactly what a CRDT must not do.
///
/// # Invariant
///
/// `(replica, commit)` must be unique for every write. It is by construction
/// when it comes from the log — `replica` is the branch id and `commit` is that
/// branch's monotonic commit id — and [`WriteStamp::from_commit`] is the
/// constructor that preserves it. Supplying a duplicate pair with differing
/// values breaks convergence, so callers outside the log path must guarantee
/// uniqueness themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WriteStamp {
    pub timestamp_ms: i64,
    pub replica: ReplicaId,
    pub commit: u64,
}

impl WriteStamp {
    /// Build a stamp from a log commit, which is where every real write's stamp
    /// comes from.
    pub fn from_commit(timestamp_ms: i64, replica: ReplicaId, commit: CommitId) -> Self {
        Self {
            timestamp_ms,
            replica,
            commit: commit.0,
        }
    }

    pub fn new(timestamp_ms: i64, replica: ReplicaId, commit: u64) -> Self {
        Self {
            timestamp_ms,
            replica,
            commit,
        }
    }
}

/// Last-writer-wins register.
///
/// A `None` value is a real state (the register was never written), not a
/// missing one; merging an unwritten register is a no-op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LwwRegister<T> {
    entry: Option<(WriteStamp, T)>,
}

impl<T> Default for LwwRegister<T> {
    fn default() -> Self {
        Self { entry: None }
    }
}

impl<T: Clone> LwwRegister<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, value: T, stamp: WriteStamp) {
        match &self.entry {
            Some((existing, _)) if *existing >= stamp => {}
            _ => self.entry = Some((stamp, value)),
        }
    }

    pub fn stamp(&self) -> Option<WriteStamp> {
        self.entry.as_ref().map(|(s, _)| *s)
    }
}

impl<T: Clone> Crdt for LwwRegister<T> {
    type Output = Option<T>;

    fn merge(&mut self, other: &Self) {
        if let Some((stamp, value)) = &other.entry {
            self.set(value.clone(), *stamp);
        }
    }

    fn value(&self) -> Option<T> {
        self.entry.as_ref().map(|(_, v)| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_timestamp_wins() {
        let mut a = LwwRegister::new();
        a.set("old", WriteStamp::new(1, ReplicaId(1), 0));
        let mut b = LwwRegister::new();
        b.set("new", WriteStamp::new(2, ReplicaId(2), 0));
        a.merge(&b);
        assert_eq!(a.value(), Some("new"));
    }

    #[test]
    fn same_branch_same_millisecond_still_has_a_deterministic_winner() {
        // Two writes from one branch inside the same millisecond. Without the
        // commit id these stamps would be equal and the merge result would
        // depend on merge order.
        let mut a = LwwRegister::new();
        a.set("first", WriteStamp::new(7, ReplicaId(1), 0));
        let mut b = LwwRegister::new();
        b.set("second", WriteStamp::new(7, ReplicaId(1), 1));

        assert_eq!(a.clone().merged(&b).value(), Some("second"));
        assert_eq!(a.clone().merged(&b).value(), b.clone().merged(&a).value());
    }

    #[test]
    fn equal_timestamps_break_on_replica_id_in_both_directions() {
        let mut a = LwwRegister::new();
        a.set("from-1", WriteStamp::new(7, ReplicaId(1), 0));
        let mut b = LwwRegister::new();
        b.set("from-2", WriteStamp::new(7, ReplicaId(2), 0));

        let ab = a.clone().merged(&b);
        let ba = b.clone().merged(&a);
        assert_eq!(ab.value(), Some("from-2"));
        assert_eq!(
            ab.value(),
            ba.value(),
            "tie-break must not depend on merge order"
        );
    }
}
