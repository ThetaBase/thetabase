//! The append-only log store.
//!
//! `MemLogStore` is a complete, correct implementation used by tests and by the
//! embeddable core; the on-disk segment store that replaces it for production
//! lives behind the same [`LogStore`] trait (see ROADMAP M1).

use std::collections::HashMap;

use theta_core::{BranchId, ContentHash, LogEntry};

use crate::error::{Result, StorageError};

pub trait LogStore {
    /// Append `entry`, returning its content hash. Fails if `entry.prev_hash`
    /// is not the branch's current head — a compare-and-swap, so a concurrent
    /// writer can never interleave silently.
    fn append(&mut self, entry: LogEntry) -> Result<ContentHash>;

    fn get(&self, hash: &ContentHash) -> Option<&LogEntry>;

    fn head(&self, branch: BranchId) -> Option<ContentHash>;

    /// Entries from `branch`'s head back to (excluding) `until`, newest first.
    /// `until = None` walks to genesis.
    fn history(&self, branch: BranchId, until: Option<ContentHash>) -> Result<Vec<LogEntry>>;
}

#[derive(Debug, Default)]
pub struct MemLogStore {
    entries: HashMap<ContentHash, LogEntry>,
    heads: HashMap<BranchId, ContentHash>,
}

impl MemLogStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Point a (new) branch at an existing commit. O(1): copies a pointer, never
    /// data (`01-system-architecture.md` §3.2).
    pub fn set_head(&mut self, branch: BranchId, head: ContentHash) {
        self.heads.insert(branch, head);
    }
}

impl LogStore for MemLogStore {
    fn append(&mut self, entry: LogEntry) -> Result<ContentHash> {
        let current = self.heads.get(&entry.branch_id).copied();
        let expected = current.unwrap_or(ContentHash::ZERO);
        if entry.prev_hash != expected {
            return Err(StorageError::HeadMoved {
                expected: entry.prev_hash.to_hex(),
                actual: expected.to_hex(),
            });
        }
        let hash = entry.hash();
        let branch = entry.branch_id;
        self.entries.insert(hash, entry);
        self.heads.insert(branch, hash);
        Ok(hash)
    }

    fn get(&self, hash: &ContentHash) -> Option<&LogEntry> {
        self.entries.get(hash)
    }

    fn head(&self, branch: BranchId) -> Option<ContentHash> {
        self.heads.get(&branch).copied()
    }

    fn history(&self, branch: BranchId, until: Option<ContentHash>) -> Result<Vec<LogEntry>> {
        let mut out = Vec::new();
        let mut cursor = match self.heads.get(&branch) {
            Some(head) => *head,
            None => return Ok(out),
        };
        while !cursor.is_zero() && Some(cursor) != until {
            let entry = self
                .entries
                .get(&cursor)
                .ok_or_else(|| StorageError::UnknownCommit(cursor.to_hex()))?;
            cursor = entry.prev_hash;
            out.push(entry.clone());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use theta_core::{Author, CommitId, OpType, Value};

    use super::*;

    fn put(prev: ContentHash, n: u64, key: &str) -> LogEntry {
        LogEntry {
            prev_hash: prev,
            commit_id: CommitId(n),
            branch_id: BranchId::MAIN,
            op: OpType::Put {
                key: key.into(),
                value: Value::Int(n as i64),
            },
            author: Author::System,
            timestamp_ms: n as i64,
        }
    }

    #[test]
    fn append_chains_and_advances_head() {
        let mut store = MemLogStore::new();
        let h1 = store.append(put(ContentHash::ZERO, 1, "a")).unwrap();
        let h2 = store.append(put(h1, 2, "b")).unwrap();
        assert_eq!(store.head(BranchId::MAIN), Some(h2));
        assert_eq!(store.history(BranchId::MAIN, None).unwrap().len(), 2);
    }

    #[test]
    fn append_on_a_stale_head_is_rejected() {
        let mut store = MemLogStore::new();
        let h1 = store.append(put(ContentHash::ZERO, 1, "a")).unwrap();
        store.append(put(h1, 2, "b")).unwrap();
        // Second writer still believes the head is h1.
        assert!(matches!(
            store.append(put(h1, 3, "c")),
            Err(StorageError::HeadMoved { .. })
        ));
    }

    #[test]
    fn history_stops_at_the_fork_point() {
        let mut store = MemLogStore::new();
        let h1 = store.append(put(ContentHash::ZERO, 1, "a")).unwrap();
        store.append(put(h1, 2, "b")).unwrap();
        assert_eq!(store.history(BranchId::MAIN, Some(h1)).unwrap().len(), 1);
    }
}
