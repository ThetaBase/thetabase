use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Crdt, ReplicaId};

/// Uniquely identifies one `add` of an element, so that removing an element
/// removes exactly the adds observed at the time — a concurrent add on another
/// branch survives, which is the whole point of an OR-Set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tag {
    pub replica: ReplicaId,
    pub counter: u64,
}

/// Observed-Remove Set.
///
/// State is the set of live tags per element plus a tombstone set of removed
/// tags. Merge is a union of both, which makes it trivially commutative,
/// associative and idempotent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrSet<T: Ord> {
    adds: BTreeMap<T, BTreeSet<Tag>>,
    removed: BTreeSet<Tag>,
    next_counter: u64,
}

impl<T: Ord> Default for OrSet<T> {
    fn default() -> Self {
        Self {
            adds: BTreeMap::new(),
            removed: BTreeSet::new(),
            next_counter: 0,
        }
    }
}

impl<T: Ord + Clone> OrSet<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, element: T, replica: ReplicaId) {
        let tag = Tag {
            replica,
            counter: self.next_counter,
        };
        self.next_counter += 1;
        self.adds.entry(element).or_default().insert(tag);
    }

    /// Removes only the adds this replica has observed. Adds that arrive later
    /// from another branch are not affected.
    pub fn remove(&mut self, element: &T) {
        if let Some(tags) = self.adds.get(element) {
            self.removed.extend(tags.iter().copied());
        }
    }

    pub fn contains(&self, element: &T) -> bool {
        self.adds
            .get(element)
            .is_some_and(|tags| tags.iter().any(|t| !self.removed.contains(t)))
    }
}

impl<T: Ord + Clone> Crdt for OrSet<T> {
    type Output = BTreeSet<T>;

    fn merge(&mut self, other: &Self) {
        for (element, tags) in &other.adds {
            self.adds
                .entry(element.clone())
                .or_default()
                .extend(tags.iter().copied());
        }
        self.removed.extend(other.removed.iter().copied());
        // Keep tag allocation collision-free after a merge: a tag is
        // (replica, counter), and replicas differ per branch, but advancing the
        // counter keeps ids monotonic within this replica too.
        self.next_counter = self.next_counter.max(other.next_counter);
    }

    fn value(&self) -> BTreeSet<T> {
        self.adds
            .iter()
            .filter(|(_, tags)| tags.iter().any(|t| !self.removed.contains(t)))
            .map(|(e, _)| e.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_add_survives_a_remove() {
        let mut a: OrSet<&str> = OrSet::new();
        a.add("x", ReplicaId(1));
        let mut b = a.clone();

        a.remove(&"x");
        b.add("x", ReplicaId(2)); // concurrent re-add on another branch

        a.merge(&b);
        assert!(
            a.contains(&"x"),
            "the concurrent add was not observed by the remove"
        );
    }

    #[test]
    fn remove_after_merge_removes_everything_observed() {
        let mut a: OrSet<&str> = OrSet::new();
        a.add("x", ReplicaId(1));
        let mut b: OrSet<&str> = OrSet::new();
        b.add("x", ReplicaId(2));
        a.merge(&b);
        a.remove(&"x");
        assert!(!a.contains(&"x"));
    }
}
