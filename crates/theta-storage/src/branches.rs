//! Branch registry: names, kinds, and head pointers.

use std::collections::HashMap;

use theta_core::branch::BranchKind;
use theta_core::{Branch, BranchId, CommitId, ContentHash};

use crate::error::{Result, StorageError};

#[derive(Debug, Default)]
pub struct BranchStore {
    by_id: HashMap<BranchId, Branch>,
    by_name: HashMap<String, BranchId>,
    next_id: u64,
}

impl BranchStore {
    /// A fresh registry containing only `main`, which is protected.
    pub fn with_main() -> Self {
        let mut store = Self::default();
        store.by_id.insert(
            BranchId::MAIN,
            Branch {
                id: BranchId::MAIN,
                name: "main".into(),
                kind: BranchKind::Protected,
                head: ContentHash::ZERO,
                fork_point: None,
                next_commit: CommitId(0),
            },
        );
        store.by_name.insert("main".into(), BranchId::MAIN);
        store.next_id = 1;
        store
    }

    /// Create a branch pointing at `from`. O(1) — no data is copied.
    pub fn create(&mut self, name: &str, from: ContentHash, kind: BranchKind) -> Result<BranchId> {
        if self.by_name.contains_key(name) {
            return Err(StorageError::BranchExists(name.to_string()));
        }
        let id = BranchId(self.next_id);
        self.next_id += 1;
        self.by_id.insert(
            id,
            Branch {
                id,
                name: name.to_string(),
                kind,
                head: from,
                fork_point: Some(from),
                next_commit: CommitId(0),
            },
        );
        self.by_name.insert(name.to_string(), id);
        Ok(id)
    }

    /// Recreate a branch pointer with a known id, as recovery does when
    /// replaying `BranchCreate` entries. Unlike [`BranchStore::create`] this
    /// takes the id rather than allocating one, so ids survive a restart.
    pub fn restore(&mut self, id: BranchId, name: &str, from: ContentHash, kind: BranchKind) {
        self.next_id = self.next_id.max(id.0 + 1);
        self.by_id.insert(
            id,
            Branch {
                id,
                name: name.to_string(),
                kind,
                head: from,
                fork_point: Some(from),
                next_commit: CommitId(0),
            },
        );
        self.by_name.insert(name.to_string(), id);
    }

    pub fn get(&self, id: BranchId) -> Option<&Branch> {
        self.by_id.get(&id)
    }

    pub fn by_name(&self, name: &str) -> Option<&Branch> {
        self.by_id.get(self.by_name.get(name)?)
    }

    pub fn set_head(&mut self, id: BranchId, head: ContentHash) -> Result<()> {
        let branch = self
            .by_id
            .get_mut(&id)
            .ok_or_else(|| StorageError::UnknownBranch(id.0.to_string()))?;
        branch.head = head;
        branch.next_commit = CommitId(branch.next_commit.0 + 1);
        Ok(())
    }

    /// Drop a branch pointer. The commits it referenced stay in the log —
    /// discarding a branch never destroys history.
    pub fn discard(&mut self, id: BranchId) -> Result<()> {
        if id == BranchId::MAIN {
            return Err(StorageError::UnknownBranch("cannot discard main".into()));
        }
        let branch = self
            .by_id
            .remove(&id)
            .ok_or_else(|| StorageError::UnknownBranch(id.0.to_string()))?;
        self.by_name.remove(&branch.name);
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Branch> {
        self.by_id.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_is_protected_by_default() {
        let store = BranchStore::with_main();
        assert!(store.by_name("main").unwrap().is_protected());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut store = BranchStore::with_main();
        store
            .create("feat", ContentHash::ZERO, BranchKind::Standard)
            .unwrap();
        assert!(store
            .create("feat", ContentHash::ZERO, BranchKind::Standard)
            .is_err());
    }

    #[test]
    fn main_cannot_be_discarded() {
        let mut store = BranchStore::with_main();
        assert!(store.discard(BranchId::MAIN).is_err());
    }
}
