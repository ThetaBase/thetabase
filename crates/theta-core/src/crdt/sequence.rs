use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Crdt, ReplicaId};

/// Identity of one inserted element. Ordered so that sibling insertions have a
/// deterministic tie-break independent of the order replicas merge in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ElemId {
    pub counter: u64,
    pub replica: ReplicaId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Elem<T> {
    /// The element this one was inserted immediately after; `None` = list head.
    origin: Option<ElemId>,
    value: T,
    tombstone: bool,
}

/// RGA-style replicated sequence.
///
/// Each element records the element it was inserted after. Concurrent inserts at
/// the same position are siblings, ordered by descending [`ElemId`] — a total
/// order, so every replica reads the same sequence regardless of merge order.
/// Removal tombstones rather than deletes, so a concurrent insert after a
/// removed element still has a well-defined anchor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RgaSequence<T> {
    elements: BTreeMap<ElemId, Elem<T>>,
    next_counter: u64,
}

impl<T> Default for RgaSequence<T> {
    fn default() -> Self {
        Self {
            elements: BTreeMap::new(),
            next_counter: 0,
        }
    }
}

impl<T: Clone + PartialEq> RgaSequence<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `value` immediately after `origin` (or at the head if `None`).
    /// Returns the new element's id so callers can chain insertions.
    pub fn insert_after(&mut self, origin: Option<ElemId>, value: T, replica: ReplicaId) -> ElemId {
        let id = ElemId {
            counter: self.next_counter,
            replica,
        };
        self.next_counter += 1;
        self.elements.insert(
            id,
            Elem {
                origin,
                value,
                tombstone: false,
            },
        );
        id
    }

    /// Insert with a caller-supplied id.
    ///
    /// Log-driven inserts take this path: the id comes from (branch, commit),
    /// which is unique by construction, so replaying the same log entry twice
    /// is idempotent rather than producing a duplicate element.
    pub fn insert_with_id(&mut self, id: ElemId, origin: Option<ElemId>, value: T) -> ElemId {
        self.elements.insert(
            id,
            Elem {
                origin,
                value,
                tombstone: false,
            },
        );
        self.next_counter = self.next_counter.max(id.counter + 1);
        id
    }

    pub fn push(&mut self, value: T, replica: ReplicaId) -> ElemId {
        let last = self.live_ids().last().copied();
        self.insert_after(last, value, replica)
    }

    pub fn remove(&mut self, id: ElemId) {
        if let Some(elem) = self.elements.get_mut(&id) {
            elem.tombstone = true;
        }
    }

    /// Ids of live (non-tombstoned) elements, in sequence order.
    pub fn live_ids(&self) -> Vec<ElemId> {
        self.ordered_ids()
            .into_iter()
            .filter(|id| !self.elements[id].tombstone)
            .collect()
    }

    /// Full traversal including tombstones. Depth-first from the head, siblings
    /// in descending id order.
    fn ordered_ids(&self) -> Vec<ElemId> {
        let mut children: BTreeMap<Option<ElemId>, Vec<ElemId>> = BTreeMap::new();
        for (id, elem) in &self.elements {
            children.entry(elem.origin).or_default().push(*id);
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|a, b| b.cmp(a));
        }

        let mut out = Vec::with_capacity(self.elements.len());
        let mut stack: Vec<ElemId> = children.get(&None).cloned().unwrap_or_default();
        stack.reverse(); // pop() yields the highest sibling first
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Some(kids) = children.get(&Some(id)) {
                // Children come directly after their origin, so they are pushed
                // last-first and popped in sibling order.
                for kid in kids.iter().rev() {
                    stack.push(*kid);
                }
            }
        }
        out
    }
}

impl<T: Clone + PartialEq> Crdt for RgaSequence<T> {
    type Output = Vec<T>;

    fn merge(&mut self, other: &Self) {
        for (id, elem) in &other.elements {
            match self.elements.get_mut(id) {
                // Tombstones are monotonic: once removed anywhere, removed everywhere.
                Some(existing) => existing.tombstone |= elem.tombstone,
                None => {
                    self.elements.insert(*id, elem.clone());
                }
            }
        }
        self.next_counter = self.next_counter.max(other.next_counter);
    }

    fn value(&self) -> Vec<T> {
        self.live_ids()
            .into_iter()
            .map(|id| self.elements[&id].value.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_insertion_order_on_one_replica() {
        let mut s = RgaSequence::new();
        let a = s.push("a", ReplicaId(1));
        let b = s.insert_after(Some(a), "b", ReplicaId(1));
        s.insert_after(Some(b), "c", ReplicaId(1));
        assert_eq!(s.value(), vec!["a", "b", "c"]);
    }

    #[test]
    fn concurrent_inserts_converge_regardless_of_merge_order() {
        let mut base = RgaSequence::new();
        let anchor = base.push("anchor", ReplicaId(1));

        let mut a = base.clone();
        let mut b = base.clone();
        a.insert_after(Some(anchor), "from-a", ReplicaId(1));
        b.insert_after(Some(anchor), "from-b", ReplicaId(2));

        let ab = a.clone().merged(&b);
        let ba = b.clone().merged(&a);
        assert_eq!(ab.value(), ba.value());
        assert_eq!(ab.value().len(), 3, "neither concurrent insert was lost");
    }

    #[test]
    fn tombstones_survive_a_merge_from_a_replica_that_never_saw_the_remove() {
        let mut a = RgaSequence::new();
        let id = a.push("x", ReplicaId(1));
        let b = a.clone();
        a.remove(id);
        let merged = a.merged(&b);
        assert_eq!(merged.value(), Vec::<&str>::new());
    }
}
