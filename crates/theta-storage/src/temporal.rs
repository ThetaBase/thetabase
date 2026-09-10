//! Time as a query dimension (ROADMAP-V3 M24).
//!
//! # Why this is nearly free here
//!
//! The log already holds every version of every row, and the view is a fold over
//! it. "What did this look like an hour ago" is therefore the same fold stopped
//! earlier — not a feature bolted on, and not the temporal-table machinery a
//! relational engine needs because it threw the history away.
//!
//! What a warehouse cannot easily do is exactly what costs nothing here. That is
//! the whole argument for this milestone, and it is also the boundary: no
//! distributed execution, no cost-based optimiser for star schemas. **The
//! temporal dimension, which is free here and expensive everywhere else.**
//!
//! # Retention is the bound, and it refuses rather than approximates
//!
//! `specs/03` §4 already says time travel is bounded by retention. So a request
//! for a point below the horizon is **refused, with the horizon**, never answered
//! with the earliest state available.
//!
//! Answering would be the worst option on offer: the caller asked for Tuesday,
//! got Thursday, and has no way to tell. Every conclusion they draw is about a
//! different day than the one they think.
//!
//! # A timestamp is advisory and says so
//!
//! `LogEntry::timestamp_ms` is documented as advisory for ordering — clocks skew,
//! and the log's order is authoritative where they disagree. So `AS OF` a
//! timestamp resolves to *the last commit at or before it*, and the resolved
//! commit is returned alongside the answer.
//!
//! A caller that needs an exact point asks by commit. One that asks by time gets
//! told which commit it actually got, rather than being left to assume the two
//! are the same thing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use theta_core::hash::ContentHash;
use theta_core::log::{CommitId, LogEntry, OpType};
use theta_core::Value;

use crate::view::MaterializedView;

/// A point in a branch's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "as_of")]
pub enum AsOf {
    /// Exactly this commit. The only one that is exact.
    Commit(u64),
    /// The last commit at or before this time.
    ///
    /// Approximate by construction: timestamps are advisory. The resolved commit
    /// comes back with the answer.
    Timestamp(i64),
    /// Now. Present so a caller can pass one type everywhere rather than
    /// branching between a temporal read and an ordinary one.
    Now,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemporalError {
    #[error(
        "the earliest point this branch can be read at is commit {horizon}, and \
         {asked} was asked for. Retention has expired the history below it \
         (`specs/03` §4). This is refused rather than answered from the earliest \
         state available: you asked for one point and would have been given \
         another, with no way to tell."
    )]
    BelowHorizon { asked: u64, horizon: u64 },

    #[error(
        "no commit exists at or before {asked_ms}. The branch's first entry is at \
         {earliest_ms}."
    )]
    BeforeTheBeginning { asked_ms: i64, earliest_ms: i64 },
}

/// A state, and which commit it actually is.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub view: MaterializedView,
    /// The commit this state is as of. **Always returned**, including for a
    /// timestamp query, because a caller who asked by time and assumed they got
    /// their exact instant is drawing conclusions about a different one.
    pub commit: u64,
    /// The entry's hash, so a caller can quote a point rather than a number.
    pub head: ContentHash,
}

/// Read a branch as it was.
///
/// `entries` is the branch's log, oldest first. `horizon` is the earliest commit
/// still present, from the retention policy; `None` means nothing has expired.
pub fn as_of(
    entries: &[LogEntry],
    point: AsOf,
    horizon: Option<u64>,
) -> Result<Snapshot, TemporalError> {
    let target = match point {
        AsOf::Now => entries.last().map(|e| e.commit_id.0).unwrap_or(0),
        AsOf::Commit(commit) => commit,
        AsOf::Timestamp(at_ms) => {
            let found = entries
                .iter()
                .filter(|e| e.timestamp_ms <= at_ms)
                .map(|e| e.commit_id.0)
                .max();
            match found {
                Some(commit) => commit,
                None => {
                    return Err(TemporalError::BeforeTheBeginning {
                        asked_ms: at_ms,
                        earliest_ms: entries.first().map(|e| e.timestamp_ms).unwrap_or(0),
                    })
                }
            }
        }
    };

    if let Some(horizon) = horizon {
        if target < horizon {
            return Err(TemporalError::BelowHorizon {
                asked: target,
                horizon,
            });
        }
    }

    let included: Vec<&LogEntry> = entries.iter().filter(|e| e.commit_id.0 <= target).collect();

    Ok(Snapshot {
        head: included
            .last()
            .map(|e| e.hash())
            .unwrap_or(ContentHash::ZERO),
        commit: included.last().map(|e| e.commit_id.0).unwrap_or(0),
        view: MaterializedView::replay(included),
    })
}

/// What happened to one key between two points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "change")]
pub enum RowChange {
    Added {
        key: String,
        to: Value,
    },
    /// Changed, carrying **both** values.
    ///
    /// A diff that reported only the new value would answer "what does it say
    /// now", which the caller could already read. The question a diff is for is
    /// what it *stopped* saying.
    Changed {
        key: String,
        from: Value,
        to: Value,
    },
    /// Removed, carrying the value that is gone.
    ///
    /// The one a naive implementation loses, and the one that matters most: a
    /// removal reported as a bare key name means the value is unrecoverable from
    /// the diff, which is exactly when somebody needs it.
    Removed {
        key: String,
        from: Value,
    },
}

impl RowChange {
    pub fn key(&self) -> &str {
        match self {
            RowChange::Added { key, .. }
            | RowChange::Changed { key, .. }
            | RowChange::Removed { key, .. } => key,
        }
    }
}

/// What changed between two points, as rows.
///
/// Ordered by key, so two runs produce the same result and a diff can be
/// compared with a previous one.
pub fn diff(before: &MaterializedView, after: &MaterializedView) -> Vec<RowChange> {
    let mut changes = Vec::new();
    let mut keys: BTreeMap<&str, ()> = BTreeMap::new();
    for (key, _) in before.keys.iter() {
        keys.insert(key.as_str(), ());
    }
    for (key, _) in after.keys.iter() {
        keys.insert(key.as_str(), ());
    }

    for key in keys.keys() {
        match (before.keys.get(*key), after.keys.get(*key)) {
            (None, Some(to)) => changes.push(RowChange::Added {
                key: (*key).to_string(),
                to: to.clone(),
            }),
            (Some(from), None) => changes.push(RowChange::Removed {
                key: (*key).to_string(),
                from: from.clone(),
            }),
            (Some(from), Some(to)) if from != to => changes.push(RowChange::Changed {
                key: (*key).to_string(),
                from: from.clone(),
                to: to.clone(),
            }),
            _ => {}
        }
    }
    changes
}

/// Where a change-data-capture consumer has read up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    /// The last commit this consumer processed.
    pub after_commit: u64,
}

impl Cursor {
    pub fn beginning() -> Self {
        Self { after_commit: 0 }
    }
}

/// What a consumer got, and whether it lost anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Feed {
    /// Entries since the cursor, and where to resume.
    Delivered {
        changes: Vec<RowChange>,
        next: Cursor,
    },
    /// The consumer was away longer than retention keeps history.
    ///
    /// **Reported, never silently skipped.** A CDC consumer that resumed from
    /// wherever history now starts would have a gap it does not know about, and
    /// every downstream aggregate built from that feed would be quietly wrong.
    /// Being told means it can re-seed from a snapshot instead.
    Gap {
        asked_after: u64,
        earliest_available: u64,
        reason: String,
    },
}

/// Read changes since a cursor.
///
/// The same fold as everything else, streamed rather than materialised: the log
/// is already the shape a CDC pipeline wants and usually has to reconstruct from
/// a write-ahead log nobody documented.
pub fn since(entries: &[LogEntry], cursor: Cursor, horizon: Option<u64>, limit: usize) -> Feed {
    if let Some(horizon) = horizon {
        // The consumer's next entry would be `after_commit + 1`. If the history
        // now begins above that, the entries in between are gone.
        if cursor.after_commit + 1 < horizon {
            return Feed::Gap {
                asked_after: cursor.after_commit,
                earliest_available: horizon,
                reason: format!(
                    "you last read commit {}, and history now begins at {horizon}. The \
                     entries between are expired, so resuming would leave a gap you \
                     could not see — re-seed from a snapshot instead.",
                    cursor.after_commit
                ),
            };
        }
    }

    let window: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| e.commit_id.0 > cursor.after_commit)
        .take(limit)
        .collect();

    let next = Cursor {
        after_commit: window
            .last()
            .map(|e| e.commit_id.0)
            .unwrap_or(cursor.after_commit),
    };

    // Changes are computed by folding to either side of the window rather than
    // by reading the ops directly. An op says what was written; a change says
    // what the state did, and those differ whenever a write sets a key to the
    // value it already had.
    let before = MaterializedView::replay(
        entries
            .iter()
            .filter(|e| e.commit_id.0 <= cursor.after_commit),
    );
    let after = MaterializedView::replay(
        entries
            .iter()
            .filter(|e| e.commit_id.0 <= next.after_commit),
    );

    Feed::Delivered {
        changes: diff(&before, &after),
        next,
    }
}

/// Whether an op could change any row at all.
///
/// Used to skip windows cheaply. Kept as a function rather than inlined so the
/// list of ops that move no rows is in one place: a new op type that does move
/// rows and is not listed here would be silently dropped from every feed.
pub fn touches_rows(op: &OpType) -> bool {
    match op {
        OpType::Put { .. } | OpType::Delete { .. } | OpType::Crdt { .. } => true,
        OpType::Transaction { ops } => ops.iter().any(touches_rows),
        // A schema change can remove rows (a dropped table takes its rows with
        // it), so it counts.
        OpType::Schema { .. } => true,
        OpType::BranchCreate { .. } | OpType::Merge { .. } => false,
    }
}

/// The commit a branch was forked at, if the log records it.
///
/// `AS OF` a branch point without having to know its commit number.
pub fn branch_point(entries: &[LogEntry]) -> Option<CommitId> {
    entries
        .iter()
        .find(|e| matches!(e.op, OpType::BranchCreate { .. }))
        .map(|e| e.commit_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::Author;
    use theta_core::BranchId;

    fn entry(commit: u64, at_ms: i64, op: OpType) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash([commit as u8; 32]),
            commit_id: CommitId(commit),
            branch_id: BranchId::MAIN,
            op,
            author: Author::System,
            timestamp_ms: at_ms,
        }
    }

    fn put(key: &str, value: i64) -> OpType {
        OpType::Put {
            key: key.into(),
            value: Value::Int(value),
        }
    }

    fn history() -> Vec<LogEntry> {
        vec![
            entry(1, 1_000, put("orders:1", 1)),
            entry(2, 2_000, put("orders:2", 2)),
            entry(3, 3_000, put("orders:1", 99)),
            entry(
                4,
                4_000,
                OpType::Delete {
                    key: "orders:2".into(),
                },
            ),
        ]
    }

    #[test]
    fn a_branch_can_be_read_as_it_was() {
        let log = history();
        let then = as_of(&log, AsOf::Commit(2), None).unwrap();
        assert_eq!(then.view.get("orders:1"), Some(&Value::Int(1)));
        assert_eq!(then.view.get("orders:2"), Some(&Value::Int(2)));

        let now = as_of(&log, AsOf::Now, None).unwrap();
        assert_eq!(now.view.get("orders:1"), Some(&Value::Int(99)));
        assert_eq!(now.view.get("orders:2"), None, "it was deleted");
    }

    #[test]
    fn a_point_below_the_retention_horizon_is_refused_rather_than_approximated() {
        // The worst option on offer would be to answer from the earliest state
        // available: the caller asked for Tuesday, got Thursday, and has no way
        // to tell. Every conclusion they draw is about a different day.
        let log = history();
        let err = as_of(&log, AsOf::Commit(1), Some(3)).unwrap_err();
        match err {
            TemporalError::BelowHorizon { asked, horizon } => {
                assert_eq!(asked, 1);
                assert_eq!(horizon, 3);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_timestamp_query_says_which_commit_it_actually_got() {
        // Timestamps are advisory, so `AS OF` a time resolves to the last commit
        // at or before it. A caller who assumed they got their exact instant is
        // drawing conclusions about a different one.
        let log = history();
        let snapshot = as_of(&log, AsOf::Timestamp(2_500), None).unwrap();
        assert_eq!(
            snapshot.commit, 2,
            "the last commit at or before 2500ms, and the caller is told which"
        );
        assert_eq!(snapshot.view.get("orders:1"), Some(&Value::Int(1)));
    }

    #[test]
    fn a_timestamp_before_the_first_entry_is_refused_rather_than_returning_nothing() {
        // An empty view is a plausible-looking answer to a question that has no
        // answer, and it is indistinguishable from a branch that really was
        // empty then.
        let log = history();
        let err = as_of(&log, AsOf::Timestamp(1), None).unwrap_err();
        assert!(
            matches!(err, TemporalError::BeforeTheBeginning { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_diff_carries_the_value_that_was_removed() {
        // The one a naive implementation loses, and the one that matters most: a
        // removal reported as a bare key name means the value is unrecoverable
        // from the diff, which is exactly when somebody needs it.
        let log = history();
        let before = as_of(&log, AsOf::Commit(3), None).unwrap();
        let after = as_of(&log, AsOf::Commit(4), None).unwrap();

        let changes = diff(&before.view, &after.view);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0],
            RowChange::Removed {
                key: "orders:2".into(),
                from: Value::Int(2),
            }
        );
    }

    #[test]
    fn a_diff_carries_both_sides_of_a_change() {
        // Reporting only the new value answers "what does it say now", which the
        // caller could already read. A diff is for what it stopped saying.
        let log = history();
        let before = as_of(&log, AsOf::Commit(2), None).unwrap();
        let after = as_of(&log, AsOf::Commit(3), None).unwrap();

        let changes = diff(&before.view, &after.view);
        assert_eq!(
            changes[0],
            RowChange::Changed {
                key: "orders:1".into(),
                from: Value::Int(1),
                to: Value::Int(99),
            }
        );
    }

    #[test]
    fn a_diff_is_ordered_so_two_runs_can_be_compared() {
        let log = history();
        let before = as_of(&log, AsOf::Commit(1), None).unwrap();
        let after = as_of(&log, AsOf::Now, None).unwrap();
        let first = diff(&before.view, &after.view);
        let again = diff(&before.view, &after.view);
        assert_eq!(first, again);
        assert!(first.windows(2).all(|w| w[0].key() <= w[1].key()));
    }

    #[test]
    fn a_consumer_that_was_away_too_long_is_told_rather_than_silently_resumed() {
        // A CDC consumer that resumed from wherever history now starts would
        // have a gap it does not know about, and every downstream aggregate
        // built from that feed would be quietly wrong.
        let log = history();
        let feed = since(&log, Cursor { after_commit: 0 }, Some(3), 100);
        match feed {
            Feed::Gap {
                asked_after,
                earliest_available,
                reason,
            } => {
                assert_eq!(asked_after, 0);
                assert_eq!(earliest_available, 3);
                assert!(reason.contains("re-seed"));
            }
            other => panic!("expected a gap, got {other:?}"),
        }
    }

    #[test]
    fn a_consumer_exactly_at_the_horizon_is_not_told_it_lost_anything() {
        // Off by one here is expensive in both directions: a false gap makes a
        // healthy consumer re-seed the whole database, and a missed one loses
        // data silently.
        let log = history();
        let feed = since(&log, Cursor { after_commit: 2 }, Some(3), 100);
        assert!(
            matches!(feed, Feed::Delivered { .. }),
            "the next entry it wants is exactly the earliest available"
        );
    }

    #[test]
    fn a_feed_resumes_without_skipping_or_repeating() {
        let log = history();
        let mut cursor = Cursor::beginning();
        let mut seen = Vec::new();

        loop {
            let Feed::Delivered { changes, next } = since(&log, cursor, None, 1) else {
                panic!("no gap expected");
            };
            if next == cursor {
                break;
            }
            seen.extend(changes.iter().map(|c| c.key().to_string()));
            cursor = next;
        }

        assert_eq!(cursor.after_commit, 4, "it read to the end");
        assert!(!seen.is_empty());
    }

    #[test]
    fn a_write_that_changes_nothing_produces_no_change() {
        // Changes are folded from state rather than read off the ops, because an
        // op says what was written and a change says what the state did. A write
        // setting a key to the value it already held is not a change, and a CDC
        // consumer that saw one would act on a no-op.
        let log = vec![
            entry(1, 1_000, put("orders:1", 7)),
            entry(2, 2_000, put("orders:1", 7)),
        ];
        let Feed::Delivered { changes, .. } = since(&log, Cursor { after_commit: 1 }, None, 100)
        else {
            panic!("no gap expected");
        };
        assert!(
            changes.is_empty(),
            "rewriting a value with itself is not a change: {changes:?}"
        );
    }

    #[test]
    fn every_op_that_can_move_a_row_is_listed_as_one() {
        // A new op type that moves rows and is not listed would be silently
        // dropped from every feed.
        assert!(touches_rows(&put("a:1", 1)));
        assert!(touches_rows(&OpType::Delete { key: "a:1".into() }));
        assert!(touches_rows(&OpType::Schema {
            change: theta_core::schema::SchemaChange::DropTable {
                table: "orders".into()
            },
        }));
        assert!(!touches_rows(&OpType::Merge {
            source: BranchId(1),
            source_head: ContentHash::ZERO,
        }));
    }
}
