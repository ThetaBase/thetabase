//! Proving a result came from the log (ROADMAP-V3 M25).
//!
//! # What this is actually for
//!
//! Today a client trusts the server. It asks for a row, gets one, and has no way
//! to check that the answer is what the log implies — the server could return
//! anything and the client would believe it.
//!
//! An inclusion proof changes who has to be trusted. The log is already
//! content-addressed and hash-chained, so a client holding one trusted hash can
//! verify that a specific entry is in the history under it, without the server's
//! cooperation and without downloading the log.
//!
//! **That is what would let ThetaBase be run by somebody the data owner does not
//! trust** — a different market rather than a feature, which is why it is worth
//! building before anyone asks.
//!
//! # Where the trusted hash comes from, which is the whole question
//!
//! A proof is only as good as the root it is checked against. A client that
//! asked the server for the root and then verified against it has proved that
//! the server is internally consistent, which is not the property anyone wanted.
//!
//! So this composes with [`crate::anchor`]: the root a client checks against is
//! one that was published somewhere the operator does not control. Without an
//! anchor an inclusion proof is a consistency check; with one it is evidence.
//! Said here because the two features look independent and are not.
//!
//! # What a proof does and does not establish
//!
//! It establishes that an entry is in the history under a root. It does **not**
//! establish that the server told you about every entry it should have.
//!
//! That asymmetry is inherent and is stated rather than glossed: proving
//! *presence* is cheap and proving *completeness* — that a query result omitted
//! nothing — needs the client to know what the full key set should be, which is
//! the thing it asked the server for. A server can still lie by omission, and no
//! inclusion proof catches it.
//!
//! Completeness proofs need a different structure — an authenticated ordered
//! map, not a chain — and live in [`crate::completeness`]. Use that where the
//! question is "did the server tell me about everything?"; this module answers
//! "is what it told me real?", and the two are not interchangeable.
//!
//! The sentence that used to sit here said completeness was not built. It is
//! kept in spirit rather than deleted: an inclusion proof shipped without
//! somebody reading this paragraph would still be read as proving more than it
//! does.

use serde::{Deserialize, Serialize};
use theta_core::hash::ContentHash;
use theta_core::log::LogEntry;

/// A proof that an entry is in the history under a root.
///
/// The chain is linear, so the proof is the entries from the target to the root.
/// Not the smallest possible structure — a Merkle tree would give logarithmic
/// proofs — and it is what the existing log shape supports without changing the
/// on-disk format. A tree is the right answer once proof size is a cost somebody
/// is paying; the honest position is that nobody is yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InclusionProof {
    /// The entry being proved.
    pub entry: LogEntry,
    /// Every entry from the one after `entry` up to and including the root's,
    /// oldest first.
    pub path: Vec<LogEntry>,
    /// The head this proof is against.
    pub root: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProofError {
    #[error(
        "the proof does not reach the root: it ends at {reached} and was offered \
         against {root}"
    )]
    DoesNotReachRoot { reached: String, root: String },

    #[error(
        "the chain is broken at commit {commit_id}: it claims a predecessor \
         ({expected}) that is not the entry before it ({actual})"
    )]
    BrokenLink {
        commit_id: u64,
        expected: String,
        actual: String,
    },

    #[error("the proof is empty and proves nothing")]
    Empty,

    #[error(
        "this proof is against root {offered}, and the root you trust is {trusted}. \
         A proof verified against a root the server chose proves only that the \
         server is internally consistent."
    )]
    WrongRoot { offered: String, trusted: String },
}

/// Build a proof that `entry` is in `history` under its head.
///
/// `history` is the branch's entries, oldest first.
pub fn prove(history: &[LogEntry], target: &ContentHash) -> Option<InclusionProof> {
    let index = history.iter().position(|e| e.hash() == *target)?;
    let root = history.last()?.hash();

    Some(InclusionProof {
        entry: history[index].clone(),
        path: history[index + 1..].to_vec(),
        root,
    })
}

/// Check a proof against a root the caller already trusts.
///
/// `trusted_root` is a parameter rather than read off the proof, and that is the
/// point: verifying against the root the proof carries would prove only that the
/// proof is self-consistent. A caller passes a root it got from somewhere the
/// server does not control — an anchor, a counterparty, a previous session it
/// still remembers.
pub fn verify(proof: &InclusionProof, trusted_root: &ContentHash) -> Result<(), ProofError> {
    if proof.root != *trusted_root {
        return Err(ProofError::WrongRoot {
            offered: proof.root.to_hex(),
            trusted: trusted_root.to_hex(),
        });
    }

    // Walk from the entry forward, checking each link. This is the whole
    // argument: an entry's hash commits to its predecessor, so a chain that
    // links from the target to a root the caller trusts places the target in
    // that root's history and nowhere else.
    let mut previous = proof.entry.hash();
    for entry in &proof.path {
        if entry.prev_hash != previous {
            return Err(ProofError::BrokenLink {
                commit_id: entry.commit_id.0,
                expected: entry.prev_hash.to_hex(),
                actual: previous.to_hex(),
            });
        }
        previous = entry.hash();
    }

    if previous != *trusted_root {
        return Err(ProofError::DoesNotReachRoot {
            reached: previous.to_hex(),
            root: trusted_root.to_hex(),
        });
    }

    Ok(())
}

/// What a proof establishes, in words a caller can act on.
///
/// Returned alongside a successful verification rather than left to
/// documentation, because the gap between "this row is in the log" and "this
/// result is complete" is exactly the one a reader will close on their own if
/// nobody closes it for them.
pub fn establishes() -> &'static str {
    "this entry is in the history under the root you trust. It does not establish \
     that the server told you about every entry it should have: proving a result \
     omitted nothing needs an authenticated ordered map rather than a chain, and \
     that is not built."
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::{Author, CommitId, OpType};
    use theta_core::{BranchId, Value};

    fn chain(n: usize) -> Vec<LogEntry> {
        seeded_chain(n, 0)
    }

    /// A chain whose contents depend on `seed`.
    ///
    /// The seed exists because the first version of the forgery test built its
    /// "invented" chain with the same helper as the real one — so the two were
    /// byte-identical, the forged proof was a genuine proof, and it verified.
    /// The test was wrong and the code was right, which is the more useful of
    /// the two ways round.
    fn seeded_chain(n: usize, seed: i64) -> Vec<LogEntry> {
        let mut entries: Vec<LogEntry> = Vec::new();
        let mut prev = ContentHash::ZERO;
        for i in 1..=n {
            let entry = LogEntry {
                prev_hash: prev,
                commit_id: CommitId(i as u64),
                branch_id: BranchId::MAIN,
                op: OpType::Put {
                    key: format!("orders:{i}"),
                    value: Value::Int(i as i64 + seed),
                },
                author: Author::System,
                timestamp_ms: i as i64,
            };
            prev = entry.hash();
            entries.push(entry);
        }
        entries
    }

    #[test]
    fn an_entry_in_the_log_can_be_proved_to_a_caller_who_did_not_download_it() {
        let log = chain(20);
        let target = log[4].hash();
        let root = log.last().unwrap().hash();

        let proof = prove(&log, &target).expect("the entry is in the log");
        assert!(verify(&proof, &root).is_ok());
        assert_eq!(proof.path.len(), 15, "the entries between it and the head");
    }

    #[test]
    fn a_proof_is_checked_against_the_root_the_caller_trusts_not_the_one_it_carries() {
        // The whole question. Verifying against the proof's own root would prove
        // only that the server is internally consistent, which is not the
        // property anyone wanted.
        let log = chain(10);
        let proof = prove(&log, &log[2].hash()).unwrap();

        let somebody_elses_root = ContentHash([9; 32]);
        let err = verify(&proof, &somebody_elses_root).unwrap_err();
        assert!(matches!(err, ProofError::WrongRoot { .. }), "{err:?}");
    }

    #[test]
    fn an_entry_edited_after_the_fact_fails_its_own_proof() {
        let log = chain(10);
        let root = log.last().unwrap().hash();
        let mut proof = prove(&log, &log[3].hash()).unwrap();

        // The server returns a different value for the row.
        proof.entry.op = OpType::Put {
            key: "orders:4".into(),
            value: Value::Int(9_999),
        };

        let err = verify(&proof, &root).unwrap_err();
        assert!(matches!(err, ProofError::BrokenLink { .. }), "{err:?}");
    }

    #[test]
    fn a_forged_path_does_not_reach_the_trusted_root() {
        // A server that wanted to prove a row it never wrote would have to
        // produce a chain from that row to a root somebody else published, which
        // is the thing hashing makes hard.
        let real = chain(10);
        let root = real.last().unwrap().hash();

        let invented = seeded_chain(10, 5_000);
        let mut forged = prove(&invented, &invented[2].hash()).unwrap();
        forged.root = root;

        let err = verify(&forged, &root).unwrap_err();
        assert!(
            matches!(err, ProofError::DoesNotReachRoot { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_entry_that_is_not_in_the_log_cannot_be_proved() {
        let log = chain(5);
        assert!(prove(&log, &ContentHash([200; 32])).is_none());
    }

    #[test]
    fn proving_the_head_itself_works_and_has_an_empty_path() {
        // The boundary case, and the one an off-by-one gets wrong: the head is
        // in the log and its path to itself is empty.
        let log = chain(5);
        let root = log.last().unwrap().hash();
        let proof = prove(&log, &root).unwrap();

        assert!(proof.path.is_empty());
        assert!(verify(&proof, &root).is_ok());
    }

    #[test]
    fn a_proof_with_a_gap_in_its_path_is_refused() {
        let log = chain(10);
        let root = log.last().unwrap().hash();
        let mut proof = prove(&log, &log[1].hash()).unwrap();
        proof.path.remove(3);

        let err = verify(&proof, &root).unwrap_err();
        assert!(matches!(err, ProofError::BrokenLink { .. }), "{err:?}");
    }

    #[test]
    fn what_a_proof_establishes_says_what_it_does_not() {
        // The gap between "this row is in the log" and "this result is complete"
        // is one a reader will close on their own if nobody closes it for them.
        // A server can still lie by omission and no inclusion proof catches it.
        let said = establishes();
        assert!(said.contains("does not establish"));
        assert!(said.contains("omitted nothing"));
    }
}
