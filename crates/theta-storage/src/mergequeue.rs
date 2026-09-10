//! Merge queues, with the conflict found at enqueue (ROADMAP-V3 M18).
//!
//! # The problem
//!
//! Branch-per-agent works until fifty agents branch from one head. Forty-nine of
//! their merges then conflict, and every one of them finds out at merge time —
//! which is minutes or hours after the agent made the change, by which point it
//! has moved on and the context that would let it fix the conflict is gone.
//!
//! **An agent that will conflict should be told while it still remembers why it
//! made the change.**
//!
//! # Speculative, not optimistic
//!
//! The obvious queue checks each merge against the target *as it is*. That is
//! the wrong question: by the time a merge reaches the head of the queue, the
//! target is the target plus everything that landed ahead of it. A queue that
//! validated against the old target would clear a merge and then land it into a
//! branch it was never checked against.
//!
//! So [`MergeQueue::enqueue`] validates against the target **as it will be** —
//! the current target with every already-queued merge applied. The
//! shadow-branch machinery already validates against a hypothetical branch; this
//! points the same idea at a different one.
//!
//! # Conflicting merges are refused, not queued
//!
//! A merge that will not land is rejected at enqueue with its conflicts, rather
//! than sitting in the queue waiting to fail. Queuing it would mean the agent
//! learns at the front of the queue, which is the delay this exists to remove.
//!
//! It also keeps the queue honest about what it is: a list of merges that are
//! currently expected to land.
//!
//! # A queued merge can stop being clean, and is evicted rather than forced
//!
//! Speculation is a prediction and predictions expire. If something ahead in the
//! queue is *withdrawn*, everything behind it was validated against a future
//! that is not going to happen. Re-checking can find a merge that has become
//! conflicted, and that merge is **evicted with its conflicts** rather than
//! quietly landed.
//!
//! Landing something conflicted because it was clean when it was queued is the
//! failure this whole module exists to prevent, arriving by a different route.

use std::collections::BTreeMap;

use theta_core::branch::BranchId;
use theta_core::hash::ContentHash;
use theta_core::log::{Author, CommitId, LogEntry, OpType};

use crate::error::Result;
use crate::merge::{merge, ConflictRef, MergeOutcome};
use crate::view::MaterializedView;
use crate::LogStore;

/// The target's log, as it will be once everything queued has landed.
///
/// # Why this exists rather than a speculative *view*
///
/// The first version of this module overlaid the queued merges onto a copy of
/// the target's materialised view and passed that to [`merge`]. It had no
/// effect whatsoever, and the tests said so.
///
/// `merge` is a three-way merge: it derives what each side did by reading the
/// two branches' **histories** since the base, not by comparing end states. The
/// view it is handed is used for CRDT state and type information, and never to
/// decide what the target has already changed. So overlaying a view changed
/// nothing about conflict detection, and every merge queued cleanly.
///
/// The honest fix is to speculate on the thing `merge` actually reads. This
/// wraps a store and answers `history(target, ..)` with the real history plus a
/// synthetic entry per queued merge, so the merge under test sees the queue
/// ahead of it as changes the target has already made.
///
/// It reuses `merge`'s own conflict logic rather than reimplementing overlap
/// detection here. A second implementation of "do these two conflict" would be
/// free to disagree with the first, and it would disagree exactly where it
/// mattered.
struct SpeculativeStore<'a, S: LogStore> {
    inner: &'a S,
    target: BranchId,
    /// Newest first, matching what `history` returns.
    pending: Vec<LogEntry>,
}

impl<S: LogStore> LogStore for SpeculativeStore<'_, S> {
    fn append(&mut self, _entry: LogEntry) -> Result<ContentHash> {
        // Nothing writes through a speculation. A caller that reached this has
        // confused predicting with doing, and the two must not share a path.
        Err(crate::error::StorageError::Corrupt {
            detail: "a speculative store is read-only".into(),
        })
    }

    fn get(&self, hash: &ContentHash) -> Option<&LogEntry> {
        self.inner.get(hash)
    }

    fn head(&self, branch: BranchId) -> Option<ContentHash> {
        self.inner.head(branch)
    }

    fn history(&self, branch: BranchId, until: Option<ContentHash>) -> Result<Vec<LogEntry>> {
        let mut real = self.inner.history(branch, until)?;
        if branch == self.target {
            // Newest first, so the pending entries go at the front: they land
            // after everything already on the branch.
            let mut speculative = self.pending.clone();
            speculative.append(&mut real);
            return Ok(speculative);
        }
        Ok(real)
    }
}

/// A merge waiting to land on one target.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    /// Position, assigned at enqueue and never reused.
    pub ticket: u64,
    pub source: BranchId,
    /// The base the merge was computed against.
    pub base: Option<ContentHash>,
    pub enqueued_at_ms: i64,
    /// What the merge produced when it was last speculated.
    pub outcome: MergeOutcome,
}

/// Why an enqueue was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum EnqueueError {
    /// It would conflict with the target as it will be.
    ///
    /// Carries `behind`: the ticket whose queued merge introduced the
    /// conflicting change, where one did. That is the actionable half — "you
    /// conflict with the target" sends an agent to look at `main`, and
    /// "you conflict with ticket 7" sends it to the branch that actually
    /// disagrees with it.
    WouldConflict {
        conflicts: Vec<ConflictRef>,
        behind: Option<u64>,
    },
    /// The source is already an ancestor of the target. Nothing to queue.
    NothingToMerge,
    /// This source is already queued for this target.
    ///
    /// Refused rather than deduplicated silently: a second enqueue usually
    /// means the caller believes the first did not happen, and answering "yes,
    /// fine" to both leaves them believing something untrue.
    AlreadyQueued { ticket: u64 },
}

/// A merge that was clean when queued and is not any more.
#[derive(Debug, Clone, PartialEq)]
pub struct Evicted {
    pub ticket: u64,
    pub source: BranchId,
    pub conflicts: Vec<ConflictRef>,
    /// Why it stopped being clean, in words a caller can act on.
    pub reason: String,
}

/// Merges waiting to land on one branch, in the order they will land.
#[derive(Debug, Clone)]
pub struct MergeQueue {
    target: BranchId,
    queued: Vec<Queued>,
    next_ticket: u64,
}

impl MergeQueue {
    pub fn new(target: BranchId) -> Self {
        Self {
            target,
            queued: Vec::new(),
            next_ticket: 1,
        }
    }

    pub fn target(&self) -> BranchId {
        self.target
    }

    pub fn len(&self) -> usize {
        self.queued.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    pub fn entries(&self) -> &[Queued] {
        &self.queued
    }

    /// The merge that will land next.
    pub fn head(&self) -> Option<&Queued> {
        self.queued.first()
    }

    pub fn position_of(&self, source: BranchId) -> Option<usize> {
        self.queued.iter().position(|q| q.source == source)
    }

    /// Try to join the queue.
    ///
    /// Speculates against the target with every queued merge already applied.
    /// A merge that would conflict is refused here, where the agent still has
    /// the context that would let it fix the problem.
    pub fn enqueue<S: LogStore>(
        &mut self,
        store: &S,
        source: BranchId,
        base: Option<ContentHash>,
        target_view: &MaterializedView,
        source_view: &MaterializedView,
        now_ms: i64,
    ) -> std::result::Result<u64, EnqueueError> {
        if let Some(existing) = self.queued.iter().find(|q| q.source == source) {
            return Err(EnqueueError::AlreadyQueued {
                ticket: existing.ticket,
            });
        }

        let speculative_store = self.speculative_store(store);
        let speculative_view = self.speculative_view(target_view);
        let outcome = merge(
            &speculative_store,
            source,
            self.target,
            base,
            &speculative_view,
            source_view,
        )
        .map_err(|_| EnqueueError::WouldConflict {
            // A merge that cannot be computed is not a merge that can be
            // queued. Reported as a conflict with no detail rather than
            // swallowed: a caller told "queued" for a merge the store could
            // not read would wait forever.
            conflicts: Vec::new(),
            behind: None,
        })?;

        match outcome {
            MergeOutcome::UpToDate => Err(EnqueueError::NothingToMerge),
            MergeOutcome::Conflicted { conflicts } => {
                let behind = self.who_touched(&conflicts);
                Err(EnqueueError::WouldConflict { conflicts, behind })
            }
            merged @ MergeOutcome::Merged { .. } => {
                let ticket = self.next_ticket;
                self.next_ticket += 1;
                self.queued.push(Queued {
                    ticket,
                    source,
                    base,
                    enqueued_at_ms: now_ms,
                    outcome: merged,
                });
                Ok(ticket)
            }
        }
    }

    /// Remove the head, because it has landed.
    ///
    /// Does **not** re-speculate: the caller applied the head to the real
    /// target, so the next `revalidate` will speculate against a target that
    /// already includes it. Re-speculating here against a view the caller has
    /// not yet updated would validate against a state that exists nowhere.
    pub fn pop_head(&mut self) -> Option<Queued> {
        if self.queued.is_empty() {
            None
        } else {
            Some(self.queued.remove(0))
        }
    }

    /// Withdraw a queued merge.
    ///
    /// The event that makes everything behind it stale: those were validated
    /// against a future that now will not happen. Call [`MergeQueue::revalidate`]
    /// afterwards.
    pub fn withdraw(&mut self, ticket: u64) -> Option<Queued> {
        let index = self.queued.iter().position(|q| q.ticket == ticket)?;
        Some(self.queued.remove(index))
    }

    /// Re-speculate every queued merge and evict the ones that stopped landing.
    ///
    /// Returns what was evicted, so the caller can tell the owners. Eviction is
    /// the only correct answer: a merge validated against a future that did not
    /// happen has not been validated at all, and landing it would be exactly the
    /// failure this module exists to prevent.
    pub fn revalidate<S: LogStore>(
        &mut self,
        store: &S,
        target_view: &MaterializedView,
        source_views: &BTreeMap<BranchId, MaterializedView>,
    ) -> Result<Vec<Evicted>> {
        let mut kept: Vec<Queued> = Vec::new();
        let mut evicted = Vec::new();
        let mut speculative = target_view.clone();
        // Rebuilt as we go, so each merge is re-checked against the target plus
        // everything *ahead of it that still lands* — not against the queue as
        // it was when the merge joined.
        let mut ahead: Vec<Queued> = Vec::new();

        for entry in std::mem::take(&mut self.queued) {
            let Some(source_view) = source_views.get(&entry.source) else {
                // No view for a source means the branch is gone. Evicting is
                // the honest answer; keeping it queued would leave a merge that
                // can never be computed at the front of somebody's queue.
                evicted.push(Evicted {
                    ticket: entry.ticket,
                    source: entry.source,
                    conflicts: Vec::new(),
                    reason: "the source branch is no longer available".into(),
                });
                continue;
            };

            let speculative_store = SpeculativeStore {
                inner: store,
                target: self.target,
                pending: pending_entries(&ahead),
            };
            let outcome = merge(
                &speculative_store,
                entry.source,
                self.target,
                entry.base,
                &speculative,
                source_view,
            )?;

            match outcome {
                MergeOutcome::Merged { .. } => {
                    apply_to(&mut speculative, &outcome);
                    let kept_entry = Queued { outcome, ..entry };
                    ahead.push(kept_entry.clone());
                    kept.push(kept_entry);
                }
                MergeOutcome::UpToDate => {
                    // Somebody else landed the same content. Not an eviction to
                    // apologise for: the merge got what it wanted.
                    evicted.push(Evicted {
                        ticket: entry.ticket,
                        source: entry.source,
                        conflicts: Vec::new(),
                        reason: "these changes are already on the target".into(),
                    });
                }
                MergeOutcome::Conflicted { conflicts } => {
                    evicted.push(Evicted {
                        ticket: entry.ticket,
                        source: entry.source,
                        conflicts,
                        reason: "the target changed while this was queued; \
                                 it no longer merges cleanly"
                            .into(),
                    });
                }
            }
        }

        self.queued = kept;
        Ok(evicted)
    }

    fn speculative_store<'a, S: LogStore>(&self, store: &'a S) -> SpeculativeStore<'a, S> {
        SpeculativeStore {
            inner: store,
            target: self.target,
            pending: pending_entries(&self.queued),
        }
    }

    /// The target as it will be once everything queued has landed.
    fn speculative_view(&self, target_view: &MaterializedView) -> MaterializedView {
        let mut view = target_view.clone();
        for entry in &self.queued {
            apply_to(&mut view, &entry.outcome);
        }
        view
    }

    /// Which queued merge introduced a change that conflicts.
    ///
    /// Best effort and honest about it: the conflict keys name the declarations
    /// in dispute, and this finds the first queued merge whose outcome touches
    /// one of them. Where nothing queued touches it, the disagreement is with
    /// the target itself, which is what `None` means.
    fn who_touched(&self, conflicts: &[ConflictRef]) -> Option<u64> {
        let keys: std::collections::BTreeSet<&str> =
            conflicts.iter().map(|c| c.key.as_str()).collect();

        self.queued
            .iter()
            .find(|entry| {
                let MergeOutcome::Merged { ops, schema, .. } = &entry.outcome else {
                    return false;
                };
                ops.iter().any(|op| touches(op, &keys))
                    || schema.iter().any(|change| {
                        let (table, column) = crate::merge::declaration_key(change);
                        keys.contains(
                            format!(
                                "schema:{table}{}",
                                column.map(|c| format!(".{c}")).unwrap_or_default()
                            )
                            .as_str(),
                        )
                    })
            })
            .map(|entry| entry.ticket)
    }
}

/// The entries a queue would append to its target, newest first.
///
/// One per queued merge, carrying exactly what the engine appends when it lands
/// one: the schema changes in order, then the plain assignments as a
/// transaction. CRDT state is deliberately absent — `merge` reconciles CRDT
/// fields from the two sides\' operations and never treats them as conflicts, so
/// a speculative history that replayed merged states would be adding operations
/// nobody performed.
fn pending_entries(queued: &[Queued]) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    for item in queued {
        let MergeOutcome::Merged { schema, ops, .. } = &item.outcome else {
            continue;
        };
        for change in schema {
            entries.push(synthetic(OpType::Schema {
                change: change.clone(),
            }));
        }
        if !ops.is_empty() {
            entries.push(synthetic(OpType::Transaction { ops: ops.clone() }));
        }
    }
    // `history` returns newest first.
    entries.reverse();
    entries
}

fn touches(op: &OpType, keys: &std::collections::BTreeSet<&str>) -> bool {
    match op {
        OpType::Put { key, .. } | OpType::Delete { key } | OpType::Crdt { key, .. } => {
            keys.contains(key.as_str())
        }
        OpType::Transaction { ops } => ops.iter().any(|op| touches(op, keys)),
        _ => false,
    }
}

/// Fold a merge outcome into a view, as landing it would.
///
/// Synthesises entries rather than mutating the view directly, so a speculative
/// application goes through exactly the fold a real one does. A second
/// implementation of "what this merge does to a view" would be free to disagree
/// with the first, and the disagreement would only appear as a merge that was
/// promised clean and landed dirty.
fn apply_to(view: &mut MaterializedView, outcome: &MergeOutcome) {
    let MergeOutcome::Merged {
        schema, ops, crdt, ..
    } = outcome
    else {
        return;
    };

    // Schema first, in the order the source made the changes — the same order
    // the engine applies them in when landing a merge for real.
    for change in schema {
        view.apply(&synthetic(OpType::Schema {
            change: change.clone(),
        }));
    }
    if !ops.is_empty() {
        view.apply(&synthetic(OpType::Transaction { ops: ops.clone() }));
    }
    view.crdts.extend(crdt.clone());
}

/// An entry that exists only to be folded into a speculative view.
///
/// Never appended to any log. The hash and commit id are placeholders, and
/// nothing reads them: `MaterializedView::apply` folds the op and the author,
/// and a speculative fold has no author because nobody has done anything yet.
fn synthetic(op: OpType) -> LogEntry {
    LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(0),
        branch_id: BranchId(0),
        op,
        author: Author::System,
        timestamp_ms: 0,
    }
}
