use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Crdt, ReplicaId};

/// PN-Counter: two grow-only maps, one for increments and one for decrements.
/// Merge takes the per-replica maximum, which is idempotent by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnCounter {
    positive: BTreeMap<ReplicaId, u64>,
    negative: BTreeMap<ReplicaId, u64>,
}

impl PnCounter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn increment(&mut self, replica: ReplicaId, by: u64) {
        *self.positive.entry(replica).or_default() += by;
    }

    pub fn decrement(&mut self, replica: ReplicaId, by: u64) {
        *self.negative.entry(replica).or_default() += by;
    }
}

impl Crdt for PnCounter {
    type Output = i64;

    fn merge(&mut self, other: &Self) {
        for (replica, count) in &other.positive {
            let slot = self.positive.entry(*replica).or_default();
            *slot = (*slot).max(*count);
        }
        for (replica, count) in &other.negative {
            let slot = self.negative.entry(*replica).or_default();
            *slot = (*slot).max(*count);
        }
    }

    fn value(&self) -> i64 {
        let pos: u64 = self.positive.values().sum();
        let neg: u64 = self.negative.values().sum();
        pos as i64 - neg as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_increments_both_survive() {
        let mut a = PnCounter::new();
        let mut b = PnCounter::new();
        a.increment(ReplicaId(1), 5);
        b.increment(ReplicaId(2), 3);
        a.merge(&b);
        assert_eq!(a.value(), 8);
    }

    #[test]
    fn merging_twice_changes_nothing() {
        let mut a = PnCounter::new();
        a.increment(ReplicaId(1), 5);
        let b = a.clone();
        a.merge(&b);
        a.merge(&b);
        assert_eq!(a.value(), 5);
    }
}
