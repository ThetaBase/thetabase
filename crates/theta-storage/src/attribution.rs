//! "Everything this agent did" as a query rather than a grep (ROADMAP-V3 M18).
//!
//! [`crate::provenance`] answers the same shape of question about the *schema*.
//! This answers it about everything: every entry an agent session or task wrote,
//! in the order it wrote them.
//!
//! # Why it is a fold and not an index
//!
//! Same reason as provenance: `docs/INVARIANTS.md` invariant 6 says no state that is not
//! a deterministic fold over log entries. An index maintained beside the log is
//! a second copy that can disagree with it, and the disagreement surfaces during
//! an incident, which is the worst possible time to discover that the forensic
//! record and the log say different things.
//!
//! # What the answer is worth, which is less than it looks
//!
//! `session_id` comes from the credential and cannot be chosen by the caller.
//! Everything in [`AgentProvenance`] is **self-reported** — the agent says which
//! agent it is and which instruction it was following, and nothing verifies
//! either, because nothing can.
//!
//! So a query by session is trustworthy and a query by agent name or task is
//! forensic: it reconstructs what a cooperative agent said it was doing. Against
//! a hostile one it establishes only what was claimed at the time, which is
//! still worth having — the claim is hashed into the entry, so nobody can change
//! it afterwards.
//!
//! Written down because the failure mode is somebody building an access control
//! on `agent == "trusted-migrator"`.

use std::collections::BTreeMap;

use theta_core::log::{AgentProvenance, Author, LogEntry};

/// What an attribution question is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution<'a> {
    /// One session. Comes from the credential, so this is the trustworthy one.
    Session(&'a str),
    /// One task, which may span several sessions.
    ///
    /// The case a disconnection would otherwise lose: a long-running agent
    /// reconnects, gets a new session, and its work before and after would
    /// otherwise be two unrelated sets of entries.
    Task(&'a str),
    /// Every entry an agent describing itself this way wrote. **Self-reported.**
    Agent(&'a str),
    /// One user, however they were acting.
    User(&'a str),
}

impl Attribution<'_> {
    fn matches(&self, author: &Author) -> bool {
        match self {
            Attribution::Session(id) => author.session_id() == Some(id),
            Attribution::Task(id) => author.task_id() == Some(id),
            Attribution::Agent(name) => author.provenance().is_some_and(|p| p.agent == **name),
            Attribution::User(id) => match author {
                Author::Human { user_id } | Author::Agent { user_id, .. } => user_id == id,
                Author::System => false,
            },
        }
    }
}

/// A summary of one session or task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub entries: usize,
    /// Counts by op tag: `put`, `delete`, `schema`, and so on.
    ///
    /// A breakdown rather than a total, because "wrote 4,000 entries" and "made
    /// four schema changes" are answers to different questions and the first
    /// hides the second.
    pub by_op: BTreeMap<&'static str, usize>,
    pub first_commit: Option<u64>,
    pub last_commit: Option<u64>,
    pub first_timestamp_ms: Option<i64>,
    pub last_timestamp_ms: Option<i64>,
    /// Distinct prompt hashes seen. Not the prompts — see [`AgentProvenance`].
    ///
    /// Useful on its own: one prompt hash across four thousand writes is a loop,
    /// and four thousand distinct ones is a very different kind of session.
    pub distinct_prompts: usize,
    /// What the agent said it was, where it said anything. More than one entry
    /// here means a session that changed its story.
    pub agents: Vec<String>,
}

/// Every entry matching an attribution, oldest first.
pub fn entries_for<'a>(entries: &'a [LogEntry], who: &Attribution<'_>) -> Vec<&'a LogEntry> {
    let mut found: Vec<&LogEntry> = entries
        .iter()
        .filter(|entry| who.matches(&entry.author))
        .collect();
    // By commit, so the result reads as the sequence it was rather than as
    // whatever order the entries happened to be stored in.
    found.sort_by_key(|entry| entry.commit_id.0);
    found
}

/// Summarise what an attribution did.
pub fn activity(entries: &[LogEntry], who: &Attribution<'_>) -> Activity {
    let matched = entries_for(entries, who);

    let mut by_op: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut prompts: std::collections::BTreeSet<String> = Default::default();
    let mut agents: Vec<String> = Vec::new();

    for entry in &matched {
        *by_op.entry(entry.op.tag()).or_insert(0) += 1;
        if let Some(AgentProvenance {
            agent, prompt_hash, ..
        }) = entry.author.provenance()
        {
            prompts.insert(prompt_hash.to_hex());
            if !agents.contains(agent) {
                agents.push(agent.clone());
            }
        }
    }

    Activity {
        entries: matched.len(),
        by_op,
        first_commit: matched.first().map(|e| e.commit_id.0),
        last_commit: matched.last().map(|e| e.commit_id.0),
        first_timestamp_ms: matched.first().map(|e| e.timestamp_ms),
        last_timestamp_ms: matched.last().map(|e| e.timestamp_ms),
        distinct_prompts: prompts.len(),
        agents,
    }
}

/// Every session that wrote anything, oldest first by first commit.
///
/// The entry point for "who has been in here", which is the question somebody
/// actually starts an investigation with — they do not know the session id yet.
pub fn sessions(entries: &[LogEntry]) -> Vec<String> {
    let mut first_seen: BTreeMap<String, u64> = BTreeMap::new();
    for entry in entries {
        if let Some(session) = entry.author.session_id() {
            first_seen
                .entry(session.to_string())
                .or_insert(entry.commit_id.0);
        }
    }
    let mut sessions: Vec<(String, u64)> = first_seen.into_iter().collect();
    sessions.sort_by_key(|(_, commit)| *commit);
    sessions.into_iter().map(|(session, _)| session).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::hash::ContentHash;
    use theta_core::log::{CommitId, OpType};
    use theta_core::{BranchId, Value};

    fn provenance(agent: &str, prompt: u8, task: Option<&str>) -> AgentProvenance {
        AgentProvenance {
            agent: agent.into(),
            prompt_hash: ContentHash([prompt; 32]),
            task_id: task.map(str::to_string),
        }
    }

    fn entry(commit: u64, author: Author, op: OpType) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash([commit as u8; 32]),
            commit_id: CommitId(commit),
            branch_id: BranchId(0),
            op,
            author,
            timestamp_ms: 1_000 * commit as i64,
        }
    }

    fn put(key: &str) -> OpType {
        OpType::Put {
            key: key.into(),
            value: Value::Int(1),
        }
    }

    fn with_provenance(session: &str, p: AgentProvenance) -> Author {
        Author::Agent {
            session_id: session.into(),
            user_id: "alice".into(),
            provenance: Some(p),
        }
    }

    #[test]
    fn everything_one_session_did_is_one_query() {
        let log = vec![
            entry(1, Author::agent("sess_a", "alice"), put("a:1")),
            entry(2, Author::agent("sess_b", "bob"), put("b:1")),
            entry(3, Author::agent("sess_a", "alice"), put("a:2")),
        ];

        let theirs = entries_for(&log, &Attribution::Session("sess_a"));
        assert_eq!(
            theirs.iter().map(|e| e.commit_id.0).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn a_task_survives_the_session_it_started_in() {
        // The case a disconnection loses today. A long agent run reconnects, is
        // issued a new session, and its work before and after becomes two
        // unrelated sets of entries — which is exactly when somebody is trying
        // to reconstruct what it did.
        let log = vec![
            entry(
                1,
                with_provenance("sess_a", provenance("claude-code/1.4", 7, Some("task_9"))),
                put("a:1"),
            ),
            entry(
                2,
                with_provenance("sess_b", provenance("claude-code/1.4", 7, Some("task_9"))),
                put("a:2"),
            ),
            entry(
                3,
                with_provenance(
                    "sess_c",
                    provenance("claude-code/1.4", 9, Some("task_other")),
                ),
                put("a:3"),
            ),
        ];

        let by_task = entries_for(&log, &Attribution::Task("task_9"));
        assert_eq!(by_task.len(), 2, "both sessions of one task");

        let by_session = entries_for(&log, &Attribution::Session("sess_a"));
        assert_eq!(by_session.len(), 1, "and a session is still its own thing");
    }

    #[test]
    fn a_task_id_and_a_session_id_are_different_namespaces() {
        // Found by planting: making `Task` fall back to matching session ids
        // passed every test here, because no fixture used one id in both
        // namespaces. It is a real confusion — a session id comes from the
        // credential and a task id is self-reported, so a query that quietly
        // accepted either would return entries an agent chose to be returned
        // while looking like it returned entries the server attributed.
        let log = vec![
            entry(1, Author::agent("shared_id", "alice"), put("a:1")),
            entry(
                2,
                with_provenance("sess_b", provenance("agent/1", 1, Some("task_real"))),
                put("a:2"),
            ),
        ];

        assert!(
            entries_for(&log, &Attribution::Task("shared_id")).is_empty(),
            "a session id must not answer a question about tasks"
        );
        assert_eq!(entries_for(&log, &Attribution::Task("task_real")).len(), 1);
        assert!(
            entries_for(&log, &Attribution::Session("task_real")).is_empty(),
            "and a task id must not answer a question about sessions"
        );
    }

    #[test]
    fn one_prompt_across_many_writes_is_visible_as_a_loop() {
        // The signal that separates a runaway from a busy session, and the
        // reason the prompt hash is worth carrying at all.
        let looping: Vec<LogEntry> = (1..=50)
            .map(|i| {
                entry(
                    i,
                    with_provenance("sess_loop", provenance("agent/1", 3, None)),
                    put(&format!("a:{i}")),
                )
            })
            .collect();

        let summary = activity(&looping, &Attribution::Session("sess_loop"));
        assert_eq!(summary.entries, 50);
        assert_eq!(
            summary.distinct_prompts, 1,
            "fifty writes from one instruction is a loop"
        );
    }

    #[test]
    fn the_breakdown_does_not_let_schema_changes_hide_behind_writes() {
        // "Wrote 4,000 entries" and "made four schema changes" answer different
        // questions, and a total answers only the first.
        let mut log: Vec<LogEntry> = (1..=40)
            .map(|i| entry(i, Author::agent("sess_a", "alice"), put(&format!("a:{i}"))))
            .collect();
        log.push(entry(
            41,
            Author::agent("sess_a", "alice"),
            OpType::Schema {
                change: theta_core::schema::SchemaChange::DropTable {
                    table: "orders".into(),
                },
            },
        ));

        let summary = activity(&log, &Attribution::Session("sess_a"));
        assert_eq!(summary.by_op.get("put"), Some(&40));
        assert_eq!(
            summary.by_op.get("schema"),
            Some(&1),
            "the one change that matters must not be a rounding error in a total"
        );
    }

    #[test]
    fn a_session_that_changed_its_story_shows_both_claims() {
        // Self-reported, so an agent can say two things. Recording both is the
        // point: a session whose reported identity changed mid-run is worth
        // looking at, and a field that kept only the latest would hide it.
        let log = vec![
            entry(
                1,
                with_provenance("sess_a", provenance("agent/1", 1, None)),
                put("a:1"),
            ),
            entry(
                2,
                with_provenance("sess_a", provenance("migrator/9", 1, None)),
                put("a:2"),
            ),
        ];

        let summary = activity(&log, &Attribution::Session("sess_a"));
        assert_eq!(summary.agents, vec!["agent/1", "migrator/9"]);
    }

    #[test]
    fn who_has_been_in_here_is_answerable_without_knowing_a_session_id() {
        // Where an investigation actually starts.
        let log = vec![
            entry(1, Author::agent("sess_b", "bob"), put("a:1")),
            entry(
                2,
                Author::Human {
                    user_id: "carol".into(),
                },
                put("a:2"),
            ),
            entry(3, Author::agent("sess_a", "alice"), put("a:3")),
            entry(4, Author::agent("sess_b", "bob"), put("a:4")),
        ];

        assert_eq!(
            sessions(&log),
            vec!["sess_b", "sess_a"],
            "sessions in the order they first appear, humans excluded"
        );
    }

    #[test]
    fn an_agent_name_matches_only_what_was_actually_claimed() {
        // A query by agent name is forensic, not authorising. This pins that it
        // matches the claim rather than defaulting to anything: an entry with no
        // provenance must not match a name.
        let log = vec![
            entry(1, Author::agent("sess_a", "alice"), put("a:1")),
            entry(
                2,
                with_provenance("sess_b", provenance("migrator/9", 1, None)),
                put("a:2"),
            ),
        ];

        let claimed = entries_for(&log, &Attribution::Agent("migrator/9"));
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].commit_id.0, 2);
        assert!(
            entries_for(&log, &Attribution::Agent("")).is_empty(),
            "an entry with no provenance must not match an empty name"
        );
    }

    #[test]
    fn a_user_is_found_whether_they_acted_directly_or_through_an_agent() {
        let log = vec![
            entry(
                1,
                Author::Human {
                    user_id: "alice".into(),
                },
                put("a:1"),
            ),
            entry(2, Author::agent("sess_a", "alice"), put("a:2")),
            entry(3, Author::System, put("a:3")),
        ];

        assert_eq!(entries_for(&log, &Attribution::User("alice")).len(), 2);
        assert!(
            entries_for(&log, &Attribution::User("system")).is_empty(),
            "System is not a user and must not be matched as one"
        );
    }

    #[test]
    fn asking_about_a_session_that_did_nothing_is_empty_rather_than_wrong() {
        let log = vec![entry(1, Author::agent("sess_a", "alice"), put("a:1"))];
        let summary = activity(&log, &Attribution::Session("sess_nobody"));
        assert_eq!(summary.entries, 0);
        assert_eq!(summary.first_commit, None);
        assert_eq!(summary.distinct_prompts, 0);
    }
}
