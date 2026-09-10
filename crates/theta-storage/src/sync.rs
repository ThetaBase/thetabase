//! Synchronising two independent logs (ROADMAP-V3 M22).
//!
//! # Why this is not a merge
//!
//! Merging two branches is a three-way merge over **one** log: both sides share
//! a base commit, and the chain links them. An embedded instance and a hosted
//! one share no chain at all. They are two logs that happen to be about the same
//! data, and neither contains a hash the other has ever seen.
//!
//! So sync is: work out what the other side has that we do not, carry those
//! entries across, and **re-append** them here. Re-appending is what makes them
//! part of this chain, and it is also the thing that makes sync interesting.
//!
//! # Re-appending changes the hash, and that is why signatures cover content
//!
//! An entry's `hash()` commits to `prev_hash`, so an entry carried into another
//! log necessarily gets a new one. If a signature covered `hash()`, every synced
//! entry would arrive unverifiable — signed commits and sync would be mutually
//! exclusive, and nobody would find out until they tried to use both.
//!
//! Signatures therefore cover `LogEntry::content_hash`: the op, author, commit
//! id, branch and timestamp. **A synced entry keeps its proof of authorship**,
//! and the receiving chain still detects reordering because that is the chain's
//! job rather than the signature's.
//!
//! # What sync does not do
//!
//! **It does not resolve conflicts.** Two sides that wrote the same key
//! independently are a conflict, and `docs/INVARIANTS.md` invariant 5 says a non-CRDT
//! conflict goes to a human. Sync surfaces them and applies nothing; a sync that
//! silently picked a side would be doing the one thing this design refuses to
//! do, at the largest scale it could do it.
//!
//! **It does not make two instances one.** After a sync each side holds the
//! other's entries in its own chain. They agree about content and they do not
//! share an identity, which is the honest description and the reason two synced
//! instances still have two independent anchors.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use theta_core::branch::BranchId;
use theta_core::hash::ContentHash;
use theta_core::log::{LogEntry, OpType};

/// What one side has, as content hashes.
///
/// Content hashes rather than chain hashes: two instances that hold the same
/// write have the same content hash for it and different chain hashes, so a
/// digest built from chain hashes would report every entry as missing on both
/// sides and sync would carry everything, every time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest {
    pub branch: BranchId,
    pub content: BTreeSet<ContentHash>,
}

impl Digest {
    /// Summarise a branch's entries.
    pub fn of(branch: BranchId, entries: &[LogEntry]) -> Self {
        Self {
            branch,
            content: entries.iter().map(|e| e.content_hash()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// What one side should send the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlan {
    /// Entries we have that they do not.
    pub to_send: Vec<LogEntry>,
    /// Keys both sides wrote independently.
    ///
    /// Carried on the plan rather than resolved, because a sync that picked a
    /// side would be auto-resolving a non-CRDT conflict at the scale of a whole
    /// database (`docs/INVARIANTS.md` invariant 5).
    pub conflicts: Vec<String>,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.to_send.is_empty() && self.conflicts.is_empty()
    }

    /// Whether this plan can be applied without a human.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Work out what to send, given what the other side already has.
///
/// `ours` is our branch's entries, oldest first. `theirs` is their digest.
pub fn plan(ours: &[LogEntry], theirs: &Digest) -> SyncPlan {
    let to_send: Vec<LogEntry> = ours
        .iter()
        .filter(|entry| !theirs.content.contains(&entry.content_hash()))
        .cloned()
        .collect();

    SyncPlan {
        to_send,
        conflicts: Vec::new(),
    }
}

/// Work out what to send *and* which keys both sides changed independently.
///
/// Needs both sides' entries, so it runs where both are available — after an
/// exchange rather than before one. Split from [`plan`] because the cheap
/// version is what a client sends first, and making every caller pay for
/// conflict detection would make the cheap path the one people skip.
pub fn plan_with_conflicts(ours: &[LogEntry], theirs: &[LogEntry]) -> SyncPlan {
    let their_digest = Digest::of(BranchId::MAIN, theirs);
    let our_digest = Digest::of(BranchId::MAIN, ours);

    let ours_only: Vec<&LogEntry> = ours
        .iter()
        .filter(|e| !their_digest.content.contains(&e.content_hash()))
        .collect();
    let theirs_only: Vec<&LogEntry> = theirs
        .iter()
        .filter(|e| !our_digest.content.contains(&e.content_hash()))
        .collect();

    // A key each side wrote independently is a conflict. CRDT operations are
    // excluded: converging under concurrent modification is what they are for,
    // and reporting them as conflicts would send a human the one class of
    // change that does not need one.
    let our_keys = assigned_keys(&ours_only);
    let their_keys = assigned_keys(&theirs_only);
    let conflicts: Vec<String> = our_keys.intersection(&their_keys).cloned().collect();

    SyncPlan {
        to_send: ours_only.into_iter().cloned().collect(),
        conflicts,
    }
}

/// Keys a set of entries assigned outright, ignoring CRDT operations.
fn assigned_keys(entries: &[&LogEntry]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for entry in entries {
        collect(&entry.op, &mut keys);
    }
    keys
}

fn collect(op: &OpType, keys: &mut BTreeSet<String>) {
    match op {
        OpType::Put { key, .. } | OpType::Delete { key } => {
            keys.insert(key.clone());
        }
        OpType::Transaction { ops } => {
            for op in ops {
                collect(op, keys);
            }
        }
        // A CRDT mutation converges with the other side's by construction, and
        // a schema change is reconciled by the merge machinery's declaration
        // rules rather than by key.
        OpType::Crdt { .. }
        | OpType::Schema { .. }
        | OpType::BranchCreate { .. }
        | OpType::Merge { .. } => {}
    }
}

/// Prepare received entries for appending to our chain.
///
/// Rewrites `prev_hash` so the entries link into *our* head, in the order they
/// were made. Everything else is left exactly as it was: the op, the author and
/// the timestamp are what the other side recorded, and altering any of them
/// would make the synced copy a different statement from the one that was
/// signed.
///
/// Returns the entries and the new head.
pub fn rebase(received: &[LogEntry], onto: ContentHash) -> (Vec<LogEntry>, ContentHash) {
    let mut prev = onto;
    let mut out = Vec::with_capacity(received.len());
    for entry in received {
        let rebased = LogEntry {
            prev_hash: prev,
            ..entry.clone()
        };
        prev = rebased.hash();
        out.push(rebased);
    }
    (out, prev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use theta_core::log::{Author, CommitId};
    use theta_core::Value;

    use crate::signing::{agent_entries, KeyRegistry, SignatureBook};

    fn entry(commit: u64, author: Author, op: OpType) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash([commit as u8; 32]),
            commit_id: CommitId(commit),
            branch_id: BranchId::MAIN,
            op,
            author,
            timestamp_ms: commit as i64,
        }
    }

    fn put(key: &str, value: i64) -> OpType {
        OpType::Put {
            key: key.into(),
            value: Value::Int(value),
        }
    }

    #[test]
    fn only_what_the_other_side_lacks_is_sent() {
        let shared = entry(1, Author::System, put("a:1", 1));
        let ours = vec![shared.clone(), entry(2, Author::System, put("a:2", 2))];
        let theirs = vec![shared];

        let plan = plan(&ours, &Digest::of(BranchId::MAIN, &theirs));
        assert_eq!(plan.to_send.len(), 1);
        assert_eq!(plan.to_send[0].commit_id.0, 2);
    }

    #[test]
    fn an_entry_both_sides_hold_is_recognised_despite_different_chain_positions() {
        // The reason the digest is built from content hashes. Two instances
        // holding the same write have the same content hash and different chain
        // hashes, so a digest of chain hashes would report everything as missing
        // on both sides and sync would carry the whole log every time.
        let ours = vec![entry(1, Author::System, put("a:1", 1))];
        let mut theirs = ours.clone();
        theirs[0].prev_hash = ContentHash([200; 32]);

        assert_ne!(ours[0].hash(), theirs[0].hash(), "different positions");
        assert_eq!(
            ours[0].content_hash(),
            theirs[0].content_hash(),
            "and the same content"
        );

        let plan = plan(&ours, &Digest::of(BranchId::MAIN, &theirs));
        assert!(plan.to_send.is_empty(), "nothing to carry");
    }

    #[test]
    fn a_key_both_sides_wrote_independently_is_a_conflict_and_nothing_is_applied() {
        // `docs/INVARIANTS.md` invariant 5. A sync that picked a side would be
        // auto-resolving a non-CRDT conflict at the scale of a whole database.
        let ours = vec![entry(1, Author::System, put("orders:1", 1))];
        let theirs = vec![entry(1, Author::System, put("orders:1", 2))];

        let plan = plan_with_conflicts(&ours, &theirs);
        assert_eq!(plan.conflicts, vec!["orders:1"]);
        assert!(!plan.is_clean());
    }

    #[test]
    fn independent_keys_are_not_a_conflict() {
        let ours = vec![entry(1, Author::System, put("orders:1", 1))];
        let theirs = vec![entry(1, Author::System, put("orders:2", 2))];

        let plan = plan_with_conflicts(&ours, &theirs);
        assert!(plan.is_clean());
        assert_eq!(plan.to_send.len(), 1);
    }

    #[test]
    fn concurrent_crdt_mutations_are_not_reported_as_conflicts() {
        // Converging under concurrent modification is what a CRDT field is for.
        // Reporting them would send a human the one class of change that does
        // not need one.
        let ours = vec![entry(
            1,
            Author::System,
            OpType::Crdt {
                key: "stats:views".into(),
                mutation: theta_core::CrdtOp::Increment { by: 1 },
            },
        )];
        let theirs = vec![entry(
            1,
            Author::System,
            OpType::Crdt {
                key: "stats:views".into(),
                mutation: theta_core::CrdtOp::Increment { by: 5 },
            },
        )];

        assert!(plan_with_conflicts(&ours, &theirs).is_clean());
    }

    #[test]
    fn rebased_entries_link_into_our_chain_in_order() {
        let received = vec![
            entry(1, Author::System, put("a:1", 1)),
            entry(2, Author::System, put("a:2", 2)),
        ];
        let onto = ContentHash([42; 32]);

        let (rebased, head) = rebase(&received, onto);
        assert_eq!(rebased[0].prev_hash, onto);
        assert_eq!(rebased[1].prev_hash, rebased[0].hash());
        assert_eq!(head, rebased[1].hash());
    }

    #[test]
    fn a_synced_entry_keeps_its_proof_of_authorship() {
        // The property the content/position split exists for. If a signature
        // covered the chain hash, every synced entry would arrive unverifiable
        // and signed commits and sync would be mutually exclusive.
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut keys = KeyRegistry::new();
        keys.register("sess_a", key.verifying_key());

        let original = entry(1, Author::agent("sess_a", "alice"), put("a:1", 1));
        let mut book = SignatureBook::new();
        book.sign(&original, "sess_a", &key);

        let (rebased, _) = rebase(std::slice::from_ref(&original), ContentHash([99; 32]));

        assert_ne!(
            original.hash(),
            rebased[0].hash(),
            "sync necessarily moved it in the chain"
        );
        assert!(
            book.verify_all(&rebased, &keys, agent_entries).is_ok(),
            "the signature must survive the move"
        );
    }

    #[test]
    fn rebasing_changes_only_the_link_and_nothing_it_was_signed_over() {
        // Altering the op, the author or the timestamp would make the synced
        // copy a different statement from the one that was signed — and it
        // would be a statement attributed to somebody who never made it.
        let original = entry(1, Author::agent("sess_a", "alice"), put("a:1", 1));
        let (rebased, _) = rebase(std::slice::from_ref(&original), ContentHash([99; 32]));

        assert_eq!(rebased[0].op, original.op);
        assert_eq!(rebased[0].author, original.author);
        assert_eq!(rebased[0].timestamp_ms, original.timestamp_ms);
        assert_eq!(rebased[0].commit_id, original.commit_id);
        assert_ne!(rebased[0].prev_hash, original.prev_hash);
    }

    #[test]
    fn syncing_twice_carries_nothing_the_second_time() {
        // Idempotence, and the reason it holds: after the first sync the
        // receiving side's content hashes include everything sent, so the second
        // plan is empty even though every entry now sits at a different chain
        // position than it did on the sender.
        let ours = vec![
            entry(1, Author::System, put("a:1", 1)),
            entry(2, Author::System, put("a:2", 2)),
        ];
        let theirs: Vec<LogEntry> = Vec::new();

        let first = plan(&ours, &Digest::of(BranchId::MAIN, &theirs));
        assert_eq!(first.to_send.len(), 2);

        let (applied, _) = rebase(&first.to_send, ContentHash([5; 32]));
        let second = plan(&ours, &Digest::of(BranchId::MAIN, &applied));
        assert!(
            second.to_send.is_empty(),
            "a second sync must carry nothing"
        );
    }

    #[test]
    fn two_synced_instances_still_have_two_chains() {
        // The honest description. After a sync each side holds the other's
        // entries in its own chain: they agree about content and do not share an
        // identity, which is why two synced instances still need two anchors.
        let ours = vec![entry(1, Author::System, put("a:1", 1))];
        let (theirs, _) = rebase(&ours, ContentHash([77; 32]));

        assert_eq!(ours[0].content_hash(), theirs[0].content_hash());
        assert_ne!(ours[0].hash(), theirs[0].hash());
    }
}
