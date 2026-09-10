//! The append-only log: ThetaBase's single source of truth.
//!
//! `01-system-architecture.md` §3.1 fixes the entry shape as
//! `{ prev_hash, op_type, payload, author, timestamp, branch_id }`. Every entry
//! is content-addressed over exactly those fields, so an entry's id commits to
//! its whole history — tampering with any ancestor changes every descendant id.

use serde::{Deserialize, Serialize};

use crate::branch::BranchId;
use crate::crdt::ElemId;
use crate::hash::ContentHash;
use crate::schema::SchemaChange;
use crate::value::Value;

/// Monotonic per-branch sequence number. Ordering *within* a branch only; two
/// commits on different branches with the same id are unrelated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommitId(pub u64);

/// What produced a change, beyond which session it arrived on (ROADMAP-V3 M18).
///
/// # Which half of this is trustworthy
///
/// This is the distinction to get right, because a control built on the wrong
/// half is a control that an attacker configures:
///
/// - `session_id` and `user_id` on [`Author::Agent`] come from the **credential**.
///   The server reads them off a verified token; a caller cannot choose them.
/// - **Everything in this struct is self-reported.** An agent says which agent it
///   is and which instruction it was following. Nothing verifies either, because
///   nothing can: the agent is the caller.
///
/// So this is forensic, not authorisation. It answers "what did this agent say
/// it was doing" — useful when reconstructing an incident with a cooperative
/// agent, worthless against a hostile one. **No gate may read it**, and none
/// does: [`crate::schema::SchemaChange`] classification never sees an author at
/// all (`docs/INVARIANTS.md` invariant 2).
///
/// What it *is* good for is that it becomes tamper-evident the moment it is
/// written. It is serialised inside the author, which is hashed into the entry,
/// so a self-report cannot be edited afterwards without breaking the chain. An
/// agent may lie at the time; nobody can change the lie later.
///
/// # Why a prompt hash and not the prompt
///
/// Prompts routinely contain customer data — a user pastes a row in and asks
/// what is wrong with it. The log is replicated, archived, and read by support
/// under a grant, so a prompt stored here would put customer data into all three
/// for a field whose only job is answering "was this the same instruction".
///
/// A hash answers that question exactly as well and carries none of the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvenance {
    /// Which agent implementation, as it describes itself. Free text, e.g.
    /// `"claude-code/1.4"`.
    pub agent: String,
    /// Hash of the instruction that produced this change. Never the instruction.
    pub prompt_hash: ContentHash,
    /// The unit of work this belongs to, where one spans several sessions.
    ///
    /// A long task that reconnects gets a new `session_id` and keeps this, which
    /// is what makes "everything this task did" answerable across a
    /// disconnection — the case where a forensic question is most likely to be
    /// asked and most likely to have been lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// Who authored an entry. Agent sessions are first-class so that the audit trail
/// can answer "what did which agent session do" without inference
/// (`04-threat-model-security.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    Human {
        user_id: String,
    },
    Agent {
        session_id: String,
        user_id: String,
        /// What the agent said it was, and what it said it was doing.
        ///
        /// `#[serde(skip_serializing_if)]` is load-bearing rather than tidy: the
        /// author is serialised into [`LogEntry::hash`], so an absent
        /// provenance must produce byte-identical JSON to what an entry written
        /// before this field existed produced. Every such entry keeps its hash,
        /// and the chain over them still verifies.
        ///
        /// Tamper-evidence survives in both directions anyway. Adding
        /// provenance to an entry that had none changes its serialised author
        /// and so its hash; removing it from one that had some does the same.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<AgentProvenance>,
    },
    System,
}

impl Author {
    pub fn is_agent(&self) -> bool {
        matches!(self, Author::Agent { .. })
    }

    /// An agent that has said nothing about itself beyond its session.
    ///
    /// The common constructor, and the reason the field is not a bare `Option`
    /// at every call site: most callers have nothing to say here, and the ones
    /// that do should have to reach for a different function.
    pub fn agent(session_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Author::Agent {
            session_id: session_id.into(),
            user_id: user_id.into(),
            provenance: None,
        }
    }

    /// The session this arrived on, where there is one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Author::Agent { session_id, .. } => Some(session_id),
            _ => None,
        }
    }

    /// What the agent said about itself. **Self-reported; see
    /// [`AgentProvenance`].**
    pub fn provenance(&self) -> Option<&AgentProvenance> {
        match self {
            Author::Agent { provenance, .. } => provenance.as_ref(),
            _ => None,
        }
    }

    /// The task this belongs to, where the agent named one.
    pub fn task_id(&self) -> Option<&str> {
        self.provenance().and_then(|p| p.task_id.as_deref())
    }
}

/// A mutation of a CRDT-typed field.
///
/// CRDT fields are logged as *operations*, not as resulting values. That is what
/// makes concurrent branches mergeable without judgment: replaying both sides'
/// ops converges, whereas two absolute values cannot be reconciled without
/// choosing one (`01-system-architecture.md` §3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "crdt_op", rename_all = "snake_case")]
pub enum CrdtOp {
    /// PN-Counter. Negative values decrement.
    Increment {
        by: i64,
    },
    /// LWW-Register, tie-broken by (timestamp, branch, commit).
    SetRegister {
        value: Value,
    },
    SetAdd {
        element: Value,
    },
    SetRemove {
        element: Value,
    },
    /// RGA insert. `id` is supplied by the writer as (branch, commit), so
    /// replaying the same entry twice is idempotent.
    SeqInsert {
        id: ElemId,
        after: Option<ElemId>,
        value: Value,
    },
    SeqRemove {
        id: ElemId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OpType {
    Put {
        key: String,
        value: Value,
    },
    /// Mutate a CRDT-typed field. Distinct from `Put` because the merge
    /// behaviour differs entirely: two concurrent `Crdt` ops on one key
    /// converge, two concurrent `Put`s on one key are a conflict.
    Crdt {
        key: String,
        mutation: CrdtOp,
    },
    Delete {
        key: String,
    },
    /// All-or-nothing batch, scoped to a single branch
    /// (`03-data-model-consistency.md` §3.3). Nested transactions are not legal.
    Transaction {
        ops: Vec<OpType>,
    },
    /// Schema changes live in the same log, which is what gives schema the same
    /// branch/merge semantics as data (`01-system-architecture.md` §3.4).
    Schema {
        change: SchemaChange,
    },
    BranchCreate {
        name: String,
        from: ContentHash,
    },
    Merge {
        source: BranchId,
        source_head: ContentHash,
    },
}

impl OpType {
    pub fn tag(&self) -> &'static str {
        match self {
            OpType::Put { .. } => "put",
            OpType::Crdt { .. } => "crdt",
            OpType::Delete { .. } => "delete",
            OpType::Transaction { .. } => "transaction",
            OpType::Schema { .. } => "schema",
            OpType::BranchCreate { .. } => "branch_create",
            OpType::Merge { .. } => "merge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub prev_hash: ContentHash,
    pub commit_id: CommitId,
    pub branch_id: BranchId,
    pub op: OpType,
    pub author: Author,
    /// Milliseconds since the Unix epoch. Advisory for ordering; never the sole
    /// basis for a merge decision except as the documented LWW tie-break.
    pub timestamp_ms: i64,
}

impl LogEntry {
    /// This entry's content address. Canonical JSON encoding of the entry's
    /// fields; stable because `Value::Map` is a `BTreeMap` and every field is
    /// length-prefixed in the hash.
    pub fn hash(&self) -> ContentHash {
        let op = serde_json::to_vec(&self.op).expect("op is always serializable");
        let author = serde_json::to_vec(&self.author).expect("author is always serializable");
        ContentHash::of_fields(&[
            &self.prev_hash.0,
            &self.commit_id.0.to_le_bytes(),
            &self.branch_id.0.to_le_bytes(),
            &op,
            &author,
            &self.timestamp_ms.to_le_bytes(),
        ])
    }

    /// What this entry *says*, independent of where it sits in the chain.
    ///
    /// [`LogEntry::hash`] commits to `prev_hash`, which is what makes the chain
    /// tamper-evident: an entry's id depends on its whole history. That is
    /// exactly right for detecting a rewrite and exactly wrong for a signature.
    ///
    /// A signature over `hash()` is a signature over a chain *position*, so it
    /// dies the moment an entry is legitimately re-appended — which is what
    /// synchronising two independent logs does to every entry it carries
    /// (ROADMAP-V3 M22). Signing that would have made signed commits and sync
    /// mutually exclusive, and the incompatibility would have surfaced only when
    /// somebody tried to use both.
    ///
    /// So the two are separated. The chain answers "is this history intact";
    /// this answers "who wrote this, and what did it say", and it survives the
    /// entry being carried to another log.
    ///
    /// Reordering is still caught: moving a signed entry to a different position
    /// changes its `prev_hash`, and the chain notices even though the signature
    /// does not.
    pub fn content_hash(&self) -> ContentHash {
        let op = serde_json::to_vec(&self.op).expect("op is always serializable");
        let author = serde_json::to_vec(&self.author).expect("author is always serializable");
        ContentHash::of_fields(&[
            &self.commit_id.0.to_le_bytes(),
            &self.branch_id.0.to_le_bytes(),
            &op,
            &author,
            &self.timestamp_ms.to_le_bytes(),
        ])
    }

    pub fn is_genesis(&self) -> bool {
        self.prev_hash.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prev: ContentHash, key: &str) -> LogEntry {
        LogEntry {
            prev_hash: prev,
            commit_id: CommitId(1),
            branch_id: BranchId(0),
            op: OpType::Put {
                key: key.into(),
                value: Value::Int(1),
            },
            author: Author::System,
            timestamp_ms: 0,
        }
    }

    #[test]
    fn hash_commits_to_history() {
        let a = entry(ContentHash::ZERO, "k");
        let b = entry(ContentHash::of(b"different parent"), "k");
        assert_ne!(a.hash(), b.hash(), "same op under a different parent");
    }

    #[test]
    fn hash_is_stable_across_clones() {
        let a = entry(ContentHash::ZERO, "k");
        assert_eq!(a.hash(), a.clone().hash());
    }

    #[test]
    fn an_agent_with_no_provenance_hashes_exactly_as_it_did_before_the_field_existed() {
        // The load-bearing property of the whole design. `provenance` is
        // serialised inside the author, and the author is hashed into the
        // entry, so a field that serialised as `"provenance":null` when absent
        // would change the hash of every entry ever written by an agent — and
        // the chain over them would stop verifying.
        //
        // Pinned against the literal JSON rather than against another
        // computation of the same thing, because two computations that share a
        // bug agree with each other.
        let author = Author::agent("sess_1", "alice");
        let encoded = serde_json::to_string(&author).expect("serialisable");
        assert_eq!(
            encoded, r#"{"kind":"agent","session_id":"sess_1","user_id":"alice"}"#,
            "an absent provenance must not appear in the serialised author at all"
        );
    }

    #[test]
    fn adding_or_removing_provenance_breaks_the_entrys_hash() {
        // Tamper-evidence, in both directions. `skip_serializing_if` keeps old
        // hashes stable, and the worry that creates is whether it also lets
        // somebody add or strip a self-report without trace. It does not: the
        // serialised author differs, so the entry hash differs, so the next
        // entry's `prev_hash` no longer matches.
        let bare = LogEntry {
            prev_hash: ContentHash::ZERO,
            commit_id: CommitId(1),
            branch_id: crate::branch::BranchId(0),
            op: OpType::Put {
                key: "a:1".into(),
                value: Value::Int(1),
            },
            author: Author::agent("sess_1", "alice"),
            timestamp_ms: 10,
        };

        let claimed = LogEntry {
            author: Author::Agent {
                session_id: "sess_1".into(),
                user_id: "alice".into(),
                provenance: Some(AgentProvenance {
                    agent: "claude-code/1.4".into(),
                    prompt_hash: ContentHash([7; 32]),
                    task_id: None,
                }),
            },
            ..bare.clone()
        };

        assert_ne!(
            bare.hash(),
            claimed.hash(),
            "provenance could be added to an entry without changing its hash"
        );

        let relabelled = LogEntry {
            author: Author::Agent {
                session_id: "sess_1".into(),
                user_id: "alice".into(),
                provenance: Some(AgentProvenance {
                    agent: "trusted-migrator".into(),
                    prompt_hash: ContentHash([7; 32]),
                    task_id: None,
                }),
            },
            ..bare.clone()
        };
        assert_ne!(
            claimed.hash(),
            relabelled.hash(),
            "an agent's self-report could be edited after the fact"
        );
    }

    #[test]
    fn a_prompt_hash_is_carried_and_a_prompt_is_not() {
        // Prompts routinely contain customer data — someone pastes a row in and
        // asks what is wrong with it. The log is replicated, archived and read
        // by support under a grant, so a prompt here would put customer data
        // into all three for a field whose only job is answering "was this the
        // same instruction".
        let author = Author::Agent {
            session_id: "sess_1".into(),
            user_id: "alice".into(),
            provenance: Some(AgentProvenance {
                agent: "claude-code/1.4".into(),
                prompt_hash: ContentHash::of_fields(&[b"delete every row where email = bob@x.com"]),
                task_id: Some("task_9".into()),
            }),
        };

        let encoded = serde_json::to_string(&author).expect("serialisable");
        assert!(
            !encoded.contains("bob@x.com"),
            "the prompt text reached the log: {encoded}"
        );
        assert!(encoded.contains("promptHash"));
    }
}
