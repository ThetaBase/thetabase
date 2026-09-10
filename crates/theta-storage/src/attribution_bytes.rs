//! What a branch actually costs to keep (ROADMAP-V3 M21).
//!
//! # The number everybody reaches for is wrong
//!
//! "How much storage does this branch use" has an obvious answer — walk the
//! branch's view and add up the bytes — and it is wrong in a direction that
//! overcharges.
//!
//! Branches share structure. A fork is `imbl::OrdMap`'s persistent map with its
//! nodes shared, so a branch of a million-row table that changed four rows holds
//! four rows' worth of new nodes and points at the rest. Summing each branch's
//! whole view counts the shared million once per branch, and the total comes out
//! at some multiple of what the disk actually holds.
//!
//! On a bill, that is not a rounding error. Constant-time forks (M10.6) made
//! branching free to *create*, and the entire point of them was that a thousand
//! preview branches cost about what one costs. A naive attribution would report
//! a thousand times the storage and undo the feature in the invoice.
//!
//! # So attribution is exclusive, and the shared part is named
//!
//! Each branch is charged for what only it holds. Everything reachable from more
//! than one branch goes into a shared pool that is reported separately and
//! charged to the project rather than to any branch.
//!
//! The property that makes this honest is arithmetic:
//! **the exclusive bytes plus the shared pool equal the real total.** A billing
//! number whose parts do not sum to the whole is one somebody will eventually
//! find, usually a customer.
//!
//! # It is an estimate, and says so
//!
//! Bytes here are computed from the encoded size of what a branch holds, not
//! measured off the filesystem: the log is segments and the view is memory, and
//! neither maps to a per-branch file. So [`StorageAttribution::basis`] states
//! what was counted, in the same spirit as the reporting module's provenance —
//! a billing figure that cannot say what it measured is exactly the sort that
//! gets published and then defended.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use theta_core::branch::BranchId;

use crate::view::MaterializedView;

/// What the numbers were derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Encoded size of the keys and values a branch holds.
    ///
    /// Not the on-disk footprint: segments are shared, compressed and
    /// append-only, so no per-branch file exists to measure. This is what the
    /// branch would cost if it were stored alone, which is the question a
    /// customer deciding whether to delete it is asking.
    EncodedValues,
}

/// One branch's share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchShare {
    pub branch: BranchId,
    /// Bytes held by this branch and no other. What deleting it would recover.
    pub exclusive_bytes: u64,
    /// Rows held by this branch and no other.
    pub exclusive_rows: u64,
    /// Rows this branch can see. Reported alongside the exclusive count because
    /// the difference between them is the whole point: a branch that sees a
    /// million rows and exclusively holds four is cheap, and the first number
    /// alone makes it look expensive.
    pub visible_rows: u64,
}

impl BranchShare {
    /// What deleting this branch would actually recover.
    ///
    /// The same number as `exclusive_bytes`, named for the question. A caller
    /// deciding whether to discard a branch is asking this, and reading it off
    /// a field called "size" is how somebody concludes that deleting a preview
    /// branch will free a gigabyte it never had.
    pub fn recoverable_bytes(&self) -> u64 {
        self.exclusive_bytes
    }
}

/// Storage, divided so the parts sum to the whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageAttribution {
    pub basis: Basis,
    pub branches: Vec<BranchShare>,
    /// Bytes reachable from more than one branch.
    ///
    /// Charged to the project, not split across branches. Splitting would make
    /// one branch's bill change when an unrelated branch was deleted, which is
    /// impossible to explain to the person paying it.
    pub shared_bytes: u64,
    pub shared_rows: u64,
}

impl StorageAttribution {
    /// Exclusive plus shared. **Equals the real total**, which is the property
    /// that makes this defensible as a bill.
    pub fn total_bytes(&self) -> u64 {
        self.shared_bytes + self.branches.iter().map(|b| b.exclusive_bytes).sum::<u64>()
    }

    pub fn total_rows(&self) -> u64 {
        self.shared_rows + self.branches.iter().map(|b| b.exclusive_rows).sum::<u64>()
    }

    pub fn branch(&self, branch: BranchId) -> Option<&BranchShare> {
        self.branches.iter().find(|b| b.branch == branch)
    }
}

/// Attribute storage across branches.
///
/// Two passes: count how many branches can see each key, then charge each key to
/// its sole owner or to the shared pool. That is O(total rows across branches)
/// and runs on a schedule rather than on a request — an attribution that cost a
/// request would be one nobody could afford to compute often enough to bill on.
pub fn attribute(views: &BTreeMap<BranchId, MaterializedView>) -> StorageAttribution {
    // How many branches hold each key, and at what size. The size is taken from
    // the first branch that has it: two branches holding *different* values for
    // one key are not sharing, and that case is handled below.
    let mut holders: BTreeMap<&str, BTreeSet<BranchId>> = BTreeMap::new();
    for (branch, view) in views {
        for (key, _) in view.keys.iter() {
            holders.entry(key.as_str()).or_default().insert(*branch);
        }
    }

    let mut exclusive: BTreeMap<BranchId, (u64, u64)> = BTreeMap::new();
    let mut shared_bytes = 0u64;
    let mut shared_rows = 0u64;

    for (key, branches) in &holders {
        // A key several branches hold may still be several *different* values —
        // a fork that overwrote it shares nothing but the name. Distinct values
        // are counted per branch and are exclusive; only an identical value is
        // genuinely shared.
        let mut by_encoding: BTreeMap<String, Vec<BranchId>> = BTreeMap::new();
        for branch in branches {
            let Some(value) = views[branch].keys.get(*key) else {
                continue;
            };
            let encoded = serde_json::to_string(value).unwrap_or_default();
            by_encoding.entry(encoded).or_default().push(*branch);
        }

        for (encoded, owners) in by_encoding {
            let bytes = (key.len() + encoded.len()) as u64;
            if owners.len() == 1 {
                let entry = exclusive.entry(owners[0]).or_insert((0, 0));
                entry.0 += bytes;
                entry.1 += 1;
            } else {
                shared_bytes += bytes;
                shared_rows += 1;
            }
        }
    }

    let branches = views
        .iter()
        .map(|(branch, view)| {
            let (exclusive_bytes, exclusive_rows) =
                exclusive.get(branch).copied().unwrap_or((0, 0));
            BranchShare {
                branch: *branch,
                exclusive_bytes,
                exclusive_rows,
                visible_rows: view.keys.len() as u64,
            }
        })
        .collect();

    StorageAttribution {
        basis: Basis::EncodedValues,
        branches,
        shared_bytes,
        shared_rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::{Author, CommitId, OpType};
    use theta_core::{ContentHash, LogEntry, Value};

    const MAIN: BranchId = BranchId::MAIN;
    const A: BranchId = BranchId(1);
    const B: BranchId = BranchId(2);

    fn view_with(rows: &[(&str, i64)]) -> MaterializedView {
        let mut view = MaterializedView::new();
        for (i, (key, value)) in rows.iter().enumerate() {
            view.apply(&LogEntry {
                prev_hash: ContentHash([i as u8; 32]),
                commit_id: CommitId(i as u64 + 1),
                branch_id: MAIN,
                op: OpType::Put {
                    key: (*key).into(),
                    value: Value::Int(*value),
                },
                author: Author::System,
                timestamp_ms: i as i64,
            });
        }
        view
    }

    #[test]
    fn a_fork_that_changed_nothing_is_charged_nothing() {
        // The whole reason this module exists. Constant-time forks made
        // branching free to create; a naive attribution would bill for a full
        // copy and undo the feature in the invoice.
        let base = view_with(&[("orders:1", 1), ("orders:2", 2), ("orders:3", 3)]);
        let views = BTreeMap::from([(MAIN, base.clone()), (A, base)]);

        let attribution = attribute(&views);
        assert_eq!(attribution.branch(A).unwrap().exclusive_bytes, 0);
        assert_eq!(attribution.branch(MAIN).unwrap().exclusive_bytes, 0);
        assert!(attribution.shared_bytes > 0, "everything is shared");
    }

    #[test]
    fn the_parts_sum_to_the_whole() {
        // The property that makes this defensible as a bill. Parts that do not
        // sum to the total are what a customer eventually finds.
        let base = view_with(&[("orders:1", 1), ("orders:2", 2)]);
        let mut forked = base.clone();
        forked.apply(&LogEntry {
            prev_hash: ContentHash([9; 32]),
            commit_id: CommitId(9),
            branch_id: A,
            op: OpType::Put {
                key: "orders:9".into(),
                value: Value::Int(9),
            },
            author: Author::System,
            timestamp_ms: 9,
        });

        let views = BTreeMap::from([(MAIN, base), (A, forked)]);
        let attribution = attribute(&views);

        // Three distinct rows exist across both branches: two shared, one on A.
        assert_eq!(attribution.total_rows(), 3);
        assert_eq!(attribution.shared_rows, 2);
        assert_eq!(attribution.branch(A).unwrap().exclusive_rows, 1);
        assert_eq!(attribution.branch(MAIN).unwrap().exclusive_rows, 0);
    }

    #[test]
    fn a_branch_that_overwrote_a_shared_key_holds_its_own_copy() {
        // Two branches with the same *key* and different values are not sharing
        // anything but the name. Counting them as shared would undercharge a
        // branch that really does hold a full copy — the exact case the roadmap
        // names: "a thousand cheap branches are cheap until one holds a copy of
        // everything".
        let base = view_with(&[("orders:1", 1)]);
        let mut forked = base.clone();
        forked.apply(&LogEntry {
            prev_hash: ContentHash([9; 32]),
            commit_id: CommitId(9),
            branch_id: A,
            op: OpType::Put {
                key: "orders:1".into(),
                value: Value::Int(999),
            },
            author: Author::System,
            timestamp_ms: 9,
        });

        let views = BTreeMap::from([(MAIN, base), (A, forked)]);
        let attribution = attribute(&views);

        assert_eq!(attribution.shared_rows, 0, "different values share nothing");
        assert_eq!(attribution.branch(MAIN).unwrap().exclusive_rows, 1);
        assert_eq!(attribution.branch(A).unwrap().exclusive_rows, 1);
    }

    #[test]
    fn visible_rows_and_exclusive_rows_are_reported_separately() {
        // The difference between them is the point. A branch that sees a
        // million rows and exclusively holds four is cheap, and reporting only
        // the first makes it look expensive.
        let base = view_with(&[("orders:1", 1), ("orders:2", 2), ("orders:3", 3)]);
        let mut forked = base.clone();
        forked.apply(&LogEntry {
            prev_hash: ContentHash([9; 32]),
            commit_id: CommitId(9),
            branch_id: A,
            op: OpType::Put {
                key: "orders:9".into(),
                value: Value::Int(9),
            },
            author: Author::System,
            timestamp_ms: 9,
        });

        let views = BTreeMap::from([(MAIN, base), (A, forked)]);
        let share = attribute(&views).branch(A).unwrap().clone();

        assert_eq!(share.visible_rows, 4);
        assert_eq!(share.exclusive_rows, 1);
        assert_eq!(
            share.recoverable_bytes(),
            share.exclusive_bytes,
            "deleting a branch recovers what only it held"
        );
    }

    #[test]
    fn a_thousand_identical_forks_do_not_cost_a_thousand_times_one() {
        // The claim M10.6 made about creation, extended to keeping. Without
        // exclusive attribution the reported total here would be a thousand
        // times the real one.
        let base = view_with(&[("orders:1", 1), ("orders:2", 2), ("orders:3", 3)]);
        let mut views = BTreeMap::from([(MAIN, base.clone())]);
        for i in 1..=1_000u64 {
            views.insert(BranchId(i), base.clone());
        }

        let attribution = attribute(&views);
        let one_branch_alone = attribute(&BTreeMap::from([(MAIN, base)]));

        assert_eq!(
            attribution.total_bytes(),
            one_branch_alone.total_bytes(),
            "a thousand identical forks hold what one holds"
        );
        assert!(attribution.branches.iter().all(|b| b.exclusive_bytes == 0));
    }

    #[test]
    fn a_branch_holding_a_full_copy_is_charged_for_it() {
        // The other direction, and the one billing actually has to get right.
        let base = view_with(&[("orders:1", 1), ("orders:2", 2), ("orders:3", 3)]);
        let heavy = view_with(&[("orders:1", 9), ("orders:2", 9), ("orders:3", 9)]);
        let views = BTreeMap::from([(MAIN, base), (B, heavy)]);

        let attribution = attribute(&views);
        assert_eq!(attribution.shared_rows, 0);
        assert_eq!(attribution.branch(B).unwrap().exclusive_rows, 3);
        assert!(attribution.branch(B).unwrap().exclusive_bytes > 0);
    }

    #[test]
    fn an_empty_set_of_branches_attributes_nothing() {
        let attribution = attribute(&BTreeMap::new());
        assert_eq!(attribution.total_bytes(), 0);
        assert!(attribution.branches.is_empty());
    }

    #[test]
    fn the_basis_is_stated_rather_than_implied() {
        // A billing figure that cannot say what it measured is the sort that
        // gets published and then defended.
        let views = BTreeMap::from([(MAIN, view_with(&[("orders:1", 1)]))]);
        assert_eq!(attribute(&views).basis, Basis::EncodedValues);
    }
}
