//! The engine: log, materialized view, branches, and the Safety Layer wired
//! together into the operations the RPC surface exposes.
//!
//! Every mutating path here goes through the Safety Layer before it touches the
//! log. That ordering is the product — it is why an agent authoring changes
//! unsupervised is survivable — so it is enforced structurally: the only
//! functions that append to the log are private to this module and every public
//! entry point calls the gate first.

use std::collections::HashMap;

use theta_core::branch::BranchKind;
use theta_core::schema::SchemaChange;
use theta_core::{address, Author, BranchId, ContentHash, LogEntry, OpType, RowAddress, Value};
use theta_proto::wire::{CrdtMutation, Precondition, SignedOp};
use theta_query::{Explain, Plan, Planner};
use theta_safety::breaker::BreakerDecision;
use theta_safety::budget::{
    BudgetDecision, BudgetPolicy, BudgetScope, ReservationId, ReviewBudget, Settlement,
};
use theta_safety::classify::{classify, gate_of, Gate};
use theta_safety::diff::{ChangeDiff, ChangeId, Impact};
use theta_safety::policy::SafetyPolicy;
use theta_safety::proof::{self, Claim, RowSource, Verdict};
use theta_safety::replay::{replay, RecordedDecision, ReplayReport};
use theta_safety::signed::{PolicyError, PolicySync, SignedPolicy};
use theta_safety::spend::{SpendLedger, SpendPolicy, SpendScope};
use theta_safety::store::AuditStore;
use theta_safety::triage::{triage, ReviewBatch};

use crate::shadow::{self, ShadowValidation, Validation};
use theta_safety::{AuditEntry, CircuitBreaker};
use theta_storage::anchor::{AnchorError, AnchorLog, AnchorPolicy, AnchorSink};
use theta_storage::attribution_bytes::{self, StorageAttribution};
use theta_storage::completeness;
use theta_storage::inclusion::{self, InclusionProof};
use theta_storage::merge::{self, MergeOutcome};
use theta_storage::mergequeue::{EnqueueError, Evicted, MergeQueue, Queued};
use theta_storage::region::{Placement, Region, RegionError};
use theta_storage::signing::{KeyRegistry, SignatureBook, SigningError};
use theta_storage::temporal::{self, AsOf, Cursor, Feed, RowChange, Snapshot, TemporalError};
use theta_storage::verifier::{Coverage, Verifier, VerifierPolicy, VerifyError};
use theta_storage::wal::WalConfig;
use theta_storage::DataKey;
use theta_storage::{BranchStore, BranchViews, DurableLogStore, LogStore};

use crate::config::Config;

/// Renders why a precondition failed, in words a caller can act on.
fn describe_precondition(found: bool, actual: u64) -> String {
    match found {
        true => format!("the row is at version {actual}"),
        false => "the row does not exist".to_string(),
    }
}

/// One write inside an [`Engine::transaction`].
///
/// Deliberately not the wire's `TxOp`. That one carries a JSON string, because
/// that is what arrived; this one carries a decoded [`Value`], because decoding
/// is the dispatcher's job and an engine that took JSON would be an engine that
/// could fail on a parse error halfway through applying a transaction.
#[derive(Debug, Clone)]
pub struct TxWrite {
    pub key: String,
    pub expect: Option<Precondition>,
    pub action: TxWriteAction,
}

#[derive(Debug, Clone)]
pub enum TxWriteAction {
    Put { value: Value, ttl: u64 },
    Delete,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// This project has no human review left in the current window
    /// (`07-agent-safety-layer.md` §6.1.3).
    ///
    /// A refusal, never a downgrade: running out of review does not let a change
    /// through more cheaply and does not weaken a gate. An agent that burns its
    /// own budget denies itself and opens nothing.
    ///
    /// Distinct from `Gated` because the two need different responses. A gated
    /// change is waiting for somebody to look at it; this one was never accepted,
    /// and retrying it will fail identically until the window rolls or a reviewer
    /// clears something.
    #[error("no review budget left for {scope}: {detail}")]
    ReviewExhausted { scope: String, detail: String },

    /// An anchor could not be published or did not verify
    /// (`04-threat-model-security.md` §7.1).
    #[error(transparent)]
    Anchor(#[from] AnchorError),

    /// A point in history that cannot be answered (`specs/03` §4).
    #[error(transparent)]
    Temporal(#[from] TemporalError),

    /// A region that is not placed, or is placed twice (`specs/03` §3.2).
    #[error(transparent)]
    Region(#[from] RegionError),

    /// A signed write whose commit id is no longer the branch's next.
    ///
    /// Its own variant rather than `PreconditionFailed`, which is shaped for a
    /// precondition on a *row's* version and would report this as a fact about
    /// a key. What happened here is a race for a position in the log, and the
    /// caller's next move is to re-read the head and sign again — which the
    /// message has to be able to say.
    #[error(
        "this write was signed for commit {signed_for} and branch {branch} is at {branch_is_at}. Somebody wrote in between, so the entry you signed is not the entry that would land. Re-read the head and sign again."
    )]
    SignedWriteRaced {
        branch: u64,
        signed_for: u64,
        branch_is_at: u64,
    },

    /// A signature that did not check out (`04-threat-model-security.md` §7.2).
    #[error(transparent)]
    Signature(#[from] SigningError),

    /// A conditional write whose precondition did not hold (M10.5).
    ///
    /// An error type rather than a returned `Option`, so a caller cannot
    /// ignore it by pattern-matching only the happy path — but mapped to a
    /// distinct wire response rather than to `error`, because the request was
    /// well-formed and the server did exactly what was asked. A contended key
    /// is not a fault, and one that reported itself as a fault would show up in
    /// an error budget as an outage.
    #[error("precondition failed on `{key}`: {}", describe_precondition(*found, *actual))]
    PreconditionFailed {
        key: String,
        found: bool,
        actual: u64,
    },

    /// A transaction with nothing in it.
    ///
    /// Refused rather than committed. An empty transaction is a caller bug —
    /// most often a filter that matched nothing — and committing an empty entry
    /// while reporting success hides it behind a commit id that looks like work.
    /// Two rows on one unique index.
    ///
    /// Names the row already holding the value, because the caller's next
    /// question is always *which one* — and answering it is the difference
    /// between a constraint that helps and one that only refuses.
    #[error(
        "`{table}` has a unique index `{index}` on ({columns}), and `{conflicting_key}`          already holds these values"
    )]
    UniqueViolation {
        index: String,
        table: String,
        columns: String,
        conflicting_key: String,
    },

    #[error("a transaction must contain at least one operation")]
    EmptyTransaction,

    /// Two operations on one key in a single transaction.
    ///
    /// The second operation's precondition would be evaluated against a state
    /// the first is about to replace, so there is no reading of this that a
    /// caller would find unsurprising. Refused rather than resolved.
    #[error("`{key}` appears more than once in one transaction")]
    DuplicateKeyInTransaction { key: String },

    #[error("rejected by the safety layer: {reason}")]
    Gated {
        reason: String,
        diff: Box<ChangeDiff>,
    },

    #[error("circuit breaker open: {reason}")]
    BreakerOpen { reason: String },

    #[error("no pending change with id {0:?}")]
    UnknownChange(ChangeId),

    #[error("write rejected: field `{field}` is declared {declared} but the value is {actual}")]
    TypeMismatch {
        field: String,
        declared: String,
        actual: String,
    },

    #[error("{what} is not implemented yet — lands in {milestone}")]
    NotImplemented {
        what: &'static str,
        milestone: &'static str,
    },

    #[error("bad request: {detail}")]
    BadRequest { detail: String },

    #[error(transparent)]
    Storage(#[from] theta_storage::StorageError),

    // Reading the data key moved from `theta-storage` to `theta-core` when the
    // Safety Layer needed the same sealing for its audit log, so its failures
    // arrive as a core error now.
    #[error(transparent)]
    Core(#[from] theta_core::CoreError),

    /// Refusing to start beats starting without a forensic trail, or with
    /// another project's (`04-threat-model-security.md` §3, §5).
    #[error(transparent)]
    Audit(#[from] theta_safety::store::AuditStoreError),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// A proposal, held exactly as it was classified.
///
/// The change and the branch live here rather than arriving with the
/// confirmation, because a gate is only meaningful if what it classified is
/// what actually runs. When the caller supplied them, two substitutions worked:
/// confirming a `drop column` proposal while sending a `drop table` body ran the
/// drop table, and proposing against a branch where the table was empty then
/// confirming against `main` applied `main`'s rows under the empty branch's
/// classification. Both times the human confirmed one thing and a different
/// thing happened.
#[derive(Debug, Clone, PartialEq)]
struct PendingChange {
    diff: ChangeDiff,
    change: SchemaChange,
    branch: BranchId,
    proposed_at_ms: i64,
}

/// A proposal and everything a reviewer needs to answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub diff: ChangeDiff,
    /// Present when the gate demanded shadow validation. Its `outcome` carries
    /// what the checks found.
    pub shadow: Option<ShadowValidation>,
}

impl Proposal {
    /// One line for a human: what this change is, and what has to happen next
    /// (`07-agent-safety-layer.md` §7).
    pub fn summary(&self) -> String {
        let target = match &self.diff.affected_schema.column {
            Some(column) => format!("{}.{}", self.diff.affected_schema.table, column),
            None => self.diff.affected_schema.table.clone(),
        };
        let head = format!(
            "{} on `{}` ({} rows, {})",
            self.diff.affected_schema.change_type.replace('_', " "),
            target,
            self.diff.rows_affected,
            match self.diff.reversible {
                true => "reversible",
                false => "irreversible",
            },
        );

        match &self.shadow {
            None => format!("{head} — {}", self.diff.reason),
            Some(shadow) => match &shadow.outcome {
                Some(outcome) if outcome.passed => format!(
                    "{head} — redirected to shadow branch {}, {}, awaiting your promotion",
                    shadow.shadow.0,
                    outcome.summary(),
                ),
                Some(outcome) => format!(
                    "{head} — redirected to shadow branch {}, {}",
                    shadow.shadow.0,
                    outcome.summary(),
                ),
                None => format!(
                    "{head} — redirected to shadow branch {}, not yet validated",
                    shadow.shadow.0
                ),
            },
        }
    }
}

/// One line describing a policy's limits, for the audit trail.
fn describe_policy(policy: &SafetyPolicy) -> String {
    format!(
        "{} rows before review, {} before shadow validation is required, \
         breaker at {} rows per {}s",
        policy.row_impact_threshold,
        policy.effective_irreversible_shadow_threshold(),
        policy.breaker_row_ceiling,
        policy.breaker_window_ms / 1_000,
    )
}

/// A shadow branch reclaimed by garbage collection.
#[derive(Debug, Clone, PartialEq)]
pub struct ReclaimedShadow {
    pub change_id: ChangeId,
    pub branch: BranchId,
    pub age_ms: i64,
    /// Whether verification had run before the deadline passed. A validated
    /// branch expiring unpromoted means a human was asked and never answered.
    pub was_validated: bool,
}

pub struct Engine {
    config: Config,
    log: DurableLogStore,
    branches: BranchStore,
    /// Materialized state per branch, kept in lockstep with the log by
    /// `DurableLogStore::append_and_apply`.
    views: BranchViews,
    breaker: CircuitBreaker,
    planner: Planner,
    /// Proposals awaiting confirmation or shadow validation.
    pending: HashMap<ChangeId, PendingChange>,
    /// Shadow branches opened for changes that confirmation cannot clear, and
    /// what validating them found.
    shadows: HashMap<ChangeId, ShadowValidation>,
    /// The change each shadow branch carries, kept so promotion can re-check it
    /// rather than trusting the promoter to name the same one.
    shadow_changes: HashMap<ChangeId, SchemaChange>,
    /// Rows written since this instance started.
    ///
    /// Resets on restart, and the figure says so. The Control Plane accumulates
    /// across readings and treats a reading lower than the last as a restart
    /// rather than a negative delta, which is what turns this into a usage
    /// period (`06-provisioning-identity-flow.md` §5).
    rows_written: u64,
    /// When this instance started, and when it last served a request.
    ///
    /// Feeds `ProjectStatus::idle_ms`, which is how the Control Plane decides a
    /// project can be hibernated.
    ///
    /// `Instant` rather than the wall clock the rest of this engine runs on,
    /// and deliberately so. Idle time is a *duration*, and a duration taken
    /// from a monotonic clock cannot be pushed around by NTP stepping the
    /// system time — which on the wall clock would either stop a busy instance
    /// or keep an idle one running, depending on which way the step went.
    ///
    /// `None` before the first request, which reads as idle *since start*
    /// rather than idle for zero. An instance woken by a request that then went
    /// away is precisely what hibernation exists to stop paying for.
    started: std::time::Instant,
    last_request: Option<std::time::Instant>,
    /// This project's public keys, placed here by the Control Plane at
    /// provisioning time. `None` until they are installed, and a policy push
    /// with no keyset to check it against is refused — an instance that cannot
    /// verify a policy must not adopt one.
    keys: Option<theta_identity::keys::PublicKeyset>,
    /// The policy in force, and where it came from.
    ///
    /// Not read from `config` any more: a locally configured policy is a
    /// starting point, and what applies is whatever the Control Plane last
    /// signed for this project (`07-agent-safety-layer.md` §7).
    policy: PolicySync,
    /// The forensic trail, on disk and per-project.
    ///
    /// Kept in memory too for fast reads, but memory is the cache and the file
    /// is the record (`04-threat-model-security.md` §5).
    audit: AuditStore,
    /// How much human review this project has outstanding (`specs/07` §6.1).
    ///
    /// Held here rather than in the Control Plane because the decision it makes
    /// is per-proposal and has to happen before the proposal is accepted. A
    /// budget consulted after the fact is a report.
    review: ReviewBudget,
    /// Reservations held by pending proposals, so settling one returns its
    /// budget.
    ///
    /// Keyed by change id: a proposal is the unit that costs review, and the
    /// reservation has to outlive the call that made it.
    reservations: HashMap<ChangeId, ReservationId>,
    /// Work spent, per agent and per project, in a rolling window
    /// (`specs/07` §6.3).
    spend: SpendLedger,
    /// Background chain verification (`04-threat-model-security.md` §7.3).
    ///
    /// Its cursor is deliberately not restored across restarts: a resumed cursor
    /// would let an instance that restarts often report climbing coverage while
    /// no pass ever completes, which is the failure §7.3 exists to prevent. A
    /// restart starts a pass.
    verifier: Verifier,
    /// Anchors published for each branch (`04-threat-model-security.md` §7.1).
    anchors: AnchorLog,
    /// Where anchors go, if anywhere.
    ///
    /// `None` by default, and that is a finding rather than a default. The
    /// tempting default is an in-memory sink, which would let every branch
    /// report "anchored" while proving nothing — an anchor is worth exactly what
    /// the party holding it is worth, and a copy in our own process is our own
    /// record of our own action (§7.1). An unconfigured deployment is told it is
    /// unanchored; it is not quietly told it is fine.
    anchor_sink: Option<Box<dyn AnchorSink + Send>>,
    anchor_policy: AnchorPolicy,
    /// Merges waiting to land, per target branch (ROADMAP-V3 M18).
    ///
    /// One queue per target rather than one queue: two branches merging into
    /// different targets do not interact, and a single queue would make an agent
    /// merging into `staging` wait behind one merging into `main`.
    queues: HashMap<BranchId, MergeQueue>,
    /// Public keys of sessions that sign their writes
    /// (`04-threat-model-security.md` §7.2).
    ///
    /// Learned from the token, which the project key already signed. The private
    /// halves are never here and that is the entire value: a signature this
    /// process could have produced says nothing about who wrote an entry.
    session_keys: KeyRegistry,
    /// Signatures, beside the entries rather than inside them.
    ///
    /// A signature cannot live in the thing it signs. The consequence, stated
    /// rather than discovered: **a missing signature does not break the chain**,
    /// and stripping one is undetectable from the log alone. Verification is
    /// therefore driven by the entries that were *expected* to be signed, never
    /// by the signatures that happen to be present.
    signatures: SignatureBook,
    /// Which branch serves which region (`specs/03` §3.2, ROADMAP-V3 M23).
    ///
    /// Regional branches work in a single process today: a region is a branch,
    /// each region writes locally, and the branches merge explicitly. What does
    /// *not* work here is the other half of M23 — see the note on
    /// [`Engine::route_write`].
    placement: Placement,
}

/// What a caller attached to a write it signed.
///
/// One value rather than three parameters, because the three are one thing: the
/// commit id and the timestamp are inside the signature, so they are not
/// incidental metadata travelling alongside it — they are part of what it
/// asserts, and separating them at a call site invites passing one from a
/// different write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEnvelope {
    /// The commit id the caller signed for. Must be the branch's next.
    pub commit_id: u64,
    /// The timestamp the caller signed. Must be within the accepted skew.
    pub timestamp_ms: i64,
    /// Ed25519 over the entry's content hash.
    pub signature: Vec<u8>,
}

/// Turn a signed op into the log op it stands for.
///
/// The CRDT arm mints an element id from the *proposed* commit, which is the
/// commit the caller signed. So the id is server-assigned in the sense that
/// matters - the caller cannot name one - while still being what the signature
/// covers.
fn signed_op_to_optype(op: &SignedOp, engine: &Engine, branch: BranchId) -> Result<OpType> {
    Ok(match op {
        SignedOp::Put { key, value_json } => OpType::Put {
            key: key.clone(),
            value: decode_wire_value(value_json)?,
        },
        SignedOp::Delete { key } => OpType::Delete { key: key.clone() },
        SignedOp::Crdt { key, mutation } => OpType::Crdt {
            key: key.clone(),
            mutation: engine.lower_mutation(branch, mutation)?,
        },
    })
}

/// Decode a value as it travels on the wire.
///
/// The *same* encoding `put` uses — `Value`'s own serde representation, not raw
/// JSON coerced with `Value::from_json`. The first version of the signed path
/// used the latter, so the two write requests read the same field two different
/// ways.
///
/// That is worse here than an ordinary inconsistency. The signature is over the
/// entry containing the *decoded* value, so a client encoding it the way `put`
/// expects and a server decoding it the other way would build a different
/// `Value`, a different content hash, and a signature failure with nothing in
/// the message to suggest the encoding was the cause.
fn decode_wire_value(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| EngineError::BadRequest {
        detail: format!("value is not a valid encoding: {e}"),
    })
}

/// The kind of CRDT a mutation is for.
///
/// Spelled out rather than inferred from a catch-all, so a new mutation cannot
/// be silently grouped with an existing kind.
fn crdt_kind_of(mutation: &CrdtMutation) -> theta_core::schema::CrdtKind {
    use theta_core::schema::CrdtKind;
    match mutation {
        CrdtMutation::Increment { .. } => CrdtKind::Counter,
        CrdtMutation::SetRegister { .. } => CrdtKind::Register,
        CrdtMutation::SetAdd { .. } | CrdtMutation::SetRemove { .. } => CrdtKind::Set,
        CrdtMutation::SeqInsert { .. } | CrdtMutation::SeqRemove { .. } => CrdtKind::Sequence,
    }
}

/// A branch's rows, for checking a migration's claim against.
///
/// Materialised rather than borrowed because the checker needs the whole table
/// and the engine needs `&mut self` around it. Built from both the plain and
/// CRDT maps — see [`Engine::rows_of`].
struct BranchRows {
    rows: Vec<(String, Value)>,
}

impl RowSource for BranchRows {
    fn rows(&self, table: &str) -> Vec<(String, Value)> {
        let prefix = RowAddress::prefix(table);
        self.rows
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

/// A replay, together with what it could not cover.
///
/// The coverage number is not decoration. A replay reporting "nothing loosened"
/// across the decisions it could read says nothing about the ones it could not,
/// and a caller holding only the report has no way to tell the difference.
#[derive(Debug, Clone)]
pub struct DecisionReplay {
    pub report: ReplayReport,
    /// Decisions in the trail that lack the inputs needed to replay them.
    pub not_replayable: usize,
}

impl DecisionReplay {
    /// Whether this replay covered every decision in the trail.
    pub fn is_complete(&self) -> bool {
        self.not_replayable == 0
    }
}

/// What a maintenance pass found.
///
/// Returned rather than logged, because every field here is something an
/// operator may need to act on and a periodic task that only writes to a log is
/// one whose findings are discovered during the incident they predicted.
#[derive(Debug, Clone)]
pub struct Maintenance {
    /// How much of the chain has been verified, and since when.
    pub coverage: Coverage,
    /// A broken link, if the verifier walked into one.
    pub chain: Option<VerifyError>,
    /// Whether the verifier is completing passes fast enough to matter.
    pub pace: Option<VerifyError>,
    /// Branches anchored on this pass, with the receipt each sink returned.
    pub anchored: Vec<(BranchId, String)>,
    /// Branches that should have been anchored and were not.
    ///
    /// Includes the case where no sink is configured at all. Silence is a
    /// finding: a branch nobody has ever anchored must fail rather than pass
    /// vacuously (§7.1).
    pub unanchored: Vec<(BranchId, AnchorError)>,
}

impl Maintenance {
    /// Whether anything here needs a human.
    pub fn is_clean(&self) -> bool {
        self.chain.is_none() && self.pace.is_none() && self.unanchored.is_empty()
    }
}

impl Engine {
    /// Open a project's engine, replaying its log.
    ///
    /// The branch registry is rebuilt from `BranchCreate` entries rather than
    /// persisted separately — branches are derived state like everything else
    /// (`03-data-model-consistency.md` §2.1).
    pub fn open(config: Config) -> Result<Self> {
        let breaker = CircuitBreaker::new(&config.safety);
        let policy = PolicySync::new(config.safety.clone());

        // Encryption at rest (SEC-2). `specs/04` §3 requires a key per project
        // and none shared between them; one process serves exactly one project
        // (see `Config::project_id`), so a process-scoped variable is a
        // per-project key by construction rather than by convention.
        //
        // Read from the environment rather than fetched: `thetad` is a hot-path
        // crate and cannot make network calls, so it cannot ask a KMS to unwrap
        // anything. The Control Plane holds
        // the wrapped key and injects the plaintext into the instance's
        // environment at provision time, which is the same trust boundary that
        // already delivers `keys.json`.
        //
        // Logged either way. Whether a database is encrypted must be answerable
        // by reading the startup log, not by reading the source — that
        // ambiguity is the whole reason SEC-2 exists.
        let mut wal = WalConfig::new(config.data_dir.clone());
        wal.data_key = DataKey::from_env()?;
        match wal.data_key {
            Some(_) => tracing::info!(
                "encryption at rest is enabled; new segments will be sealed with the \
                 project data key"
            ),
            // Names the consequence, not just the setting. An external review
            // (R2-06) pointed out that the archive uploads on-disk segment bytes
            // as they are, adding no encryption of its own — so with no data key
            // the default is cleartext leaving this host for S3/R2 or AT-1.
            // "Encryption at rest is off" and "your backups are plaintext on
            // somebody else's disk" are the same fact, and an operator scanning a
            // startup log should not have to make that connection themselves.
            None => tracing::warn!(
                "encryption at rest is NOT enabled: set {} to seal segments and the view snapshot. Until it is, archived segments leave this host in cleartext — the archive uploads segment bytes as they are and adds no encryption of its own",
                theta_storage::DATA_KEY_ENV
            ),
        }

        let opened = DurableLogStore::open(wal)?;

        let mut branches = BranchStore::with_main();
        for entry in &opened.recovery.entries {
            if let OpType::BranchCreate { name, from } = &entry.op {
                // Recreate the pointer this entry established. The id is
                // whatever the entry recorded, so branch ids survive a restart.
                branches.restore(entry.branch_id, name, *from, BranchKind::Standard);
            }
        }
        for branch in opened.views.keys() {
            if let Some(head) = opened.store.head(*branch) {
                let _ = branches.set_head(*branch, head);
            }
        }

        if let Some(torn) = &opened.recovery.truncated {
            tracing::warn!(?torn, "log was truncated during recovery");
        }

        // Opened on this project's own directory, and stamped with this
        // project's id. A directory holding another project's trail fails here
        // rather than interleaving the two.
        // Sealed under the same key as the segments.
        //
        // `specs/04` §3 used to record this log as the one thing in the data
        // directory written in clear: no row values, but change summaries,
        // table and column names, risk classifications and who proposed what.
        // A copied volume gave up the shape of the schema and the history of
        // every gated decision.
        //
        // Read from the environment again rather than threaded down from the
        // WAL's copy: both read the same variable, and a parameter would let
        // one of them be given a different key without anything noticing.
        let audit = AuditStore::open_with_key(
            &config.data_dir.join("audit"),
            &config.project_id,
            DataKey::from_env()?,
        )?;

        Ok(Self {
            config,
            log: opened.store,
            branches,
            views: opened.views,
            breaker,
            planner: Planner::new(),
            pending: HashMap::new(),
            shadows: HashMap::new(),
            shadow_changes: HashMap::new(),
            keys: None,
            rows_written: 0,
            started: std::time::Instant::now(),
            last_request: None,
            policy,
            audit,
            review: ReviewBudget::new(BudgetPolicy::default()),
            reservations: HashMap::new(),
            spend: SpendLedger::new(SpendPolicy::default()),
            verifier: Verifier::new(VerifierPolicy::default(), 0),
            anchors: AnchorLog::new(),
            anchor_sink: None,
            anchor_policy: AnchorPolicy::default(),
            session_keys: KeyRegistry::new(),
            signatures: SignatureBook::new(),
            queues: HashMap::new(),
            placement: Placement::new().with_home(BranchId::MAIN),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The recent audit trail, oldest first.
    pub fn audit_log(&self) -> Vec<&AuditEntry> {
        self.audit.recent().collect()
    }

    /// The audit store itself, for reviews and forensic reads.
    pub fn audit(&self) -> &AuditStore {
        &self.audit
    }

    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// The safety policy currently in force.
    pub fn policy(&self) -> &SafetyPolicy {
        self.policy.policy()
    }

    /// The version of the policy in force. Zero means nothing has been pushed
    /// and the locally configured policy still applies.
    pub fn policy_version(&self) -> u64 {
        self.policy.version()
    }

    /// Install this project's public keyset.
    ///
    /// Public keys only: an instance verifies signatures and never produces
    /// them (`06-provisioning-identity-flow.md` §3).
    pub fn install_keyset(&mut self, keys: theta_identity::keys::PublicKeyset) {
        self.keys = Some(keys);
    }

    /// Install a policy the Control Plane signed for this project.
    ///
    /// Verified against the keyset this instance already holds, so authority
    /// comes from possession of the project's private key rather than from
    /// anything in the request. An agent session token cannot produce that
    /// signature, which is what makes "the agent whose changes are being gated
    /// cannot write the policy" structural rather than a permission check.
    pub fn apply_signed_policy(
        &mut self,
        signed: &SignedPolicy,
        now_ms: i64,
    ) -> std::result::Result<bool, PolicyError> {
        // No keyset means no way to tell a real policy from an invented one.
        // Refusing is the only safe answer: adopting an unverified policy is
        // precisely the bypass this mechanism exists to prevent.
        let keys = self.keys.as_ref().ok_or(PolicyError::BadSignature)?;
        let incoming = signed.verify(keys)?;
        let version = incoming.version;
        let summary = describe_policy(&incoming.policy);

        let applied = self.policy.apply(incoming, &self.config.project_id)?;
        if applied {
            // The breaker's ceiling and window come from the policy, so it has
            // to be rebuilt or the old limits stay in force while the audit
            // trail claims otherwise.
            self.breaker = CircuitBreaker::new(self.policy.policy());
            self.record_audit(AuditEntry::for_policy(version, &summary, now_ms));
        }
        Ok(applied)
    }

    // ---- reads ------------------------------------------------------------

    pub fn get(&self, branch: BranchId, key: &str) -> Option<&Value> {
        self.views.get(&branch)?.get(key)
    }

    /// Resolved value of a CRDT-typed field.
    pub fn get_crdt(&self, branch: BranchId, key: &str) -> Option<Value> {
        self.views.get(&branch)?.get_crdt(key)
    }

    /// Execute a typed plan against a branch.
    ///
    /// Reads only — a query cannot write, because the plan IR has no operator
    /// that mutates. That is structural, not a check that could be forgotten.
    pub fn query(
        &mut self,
        branch: BranchId,
        plan: &Plan,
        bindings: &theta_query::Bindings,
    ) -> Result<theta_query::ResultSet> {
        // Analyze before planning, so the optimizer decides against the table as
        // it is now rather than as it was when it was last written to.
        self.refresh_statistics(branch, plan.source_table());

        let schema = self
            .views
            .get(&branch)
            .map(|v| v.schema.clone())
            .unwrap_or_default();
        let optimized = self.planner.optimize(plan.clone(), &schema);

        let empty = theta_storage::MaterializedView::new();
        let view = self.views.get(&branch).unwrap_or(&empty);
        theta_query::execute(&optimized, view, bindings).map_err(|e| EngineError::BadRequest {
            detail: e.to_string(),
        })
    }

    /// Recompute statistics for one table from the branch's current rows.
    ///
    /// Exact rather than sampled. At the scale one project's view holds this is
    /// cheap, and an exact count removes a whole class of "the estimate was
    /// wrong" debugging. It is also why EXPLAIN's numbers can be read as
    /// *current* rather than as whenever ANALYZE last ran.
    fn refresh_statistics(&mut self, branch: BranchId, table: &str) {
        use theta_query::RowSource;

        let Some(view) = self.views.get(&branch) else {
            return;
        };
        let rows = view.scan(table);
        let indexes: Vec<(String, String)> = view
            .schema
            .table(table)
            .map(|t| {
                t.indexes
                    .iter()
                    .filter_map(|index| {
                        index
                            .columns
                            .first()
                            .map(|c| (c.clone(), index.name.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let stats = self.planner.statistics_mut();
        stats.analyze(table, &rows);
        for (column, index) in indexes {
            stats.declare_index(table, &column, &index);
        }
    }

    pub fn branches(&self) -> &BranchStore {
        &self.branches
    }

    /// Total commits folded across every branch.
    pub fn commits_applied(&self) -> u64 {
        self.log.entries_applied()
    }

    /// Bytes written to the log so far, in megabytes. Reported by `status` and
    /// used for usage-based tiering.
    /// Bytes this project occupies on disk.
    ///
    /// `None` when the figure cannot be read — a billing surface must be able to
    /// tell "nobody measured" from "zero", because rendering an unmeasured
    /// figure as zero tells a customer they are using nothing
    /// (`06-provisioning-identity-flow.md` §5).
    pub fn storage_bytes(&self) -> Option<u64> {
        self.log.storage_bytes().ok()
    }

    /// Rows written since this instance started.
    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    /// Note that a request was served, for [`idle_ms`].
    ///
    /// [`idle_ms`]: Engine::idle_ms
    pub fn mark_active(&mut self) {
        self.last_request = Some(std::time::Instant::now());
    }

    /// How long since this instance served a request, or since it started if it
    /// has served none.
    pub fn idle_ms(&self) -> u64 {
        self.last_request
            .unwrap_or(self.started)
            .elapsed()
            .as_millis()
            // Saturating rather than wrapping: an instance up for 584 million
            // years is not the interesting case, but silently reporting it as
            // freshly active would be.
            .min(u64::MAX as u128) as u64
    }

    pub fn write_volume_mb(&self) -> f32 {
        let position = self.log.position();
        // Segments before the active one are full by definition.
        let bytes = position.segment * theta_storage::wal::WalConfig::new(".").segment_bytes
            + position.offset;
        bytes as f32 / (1024.0 * 1024.0)
    }

    /// Flush the checkpoint so a restart replays as little as possible.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.log.checkpoint(&self.views)?;
        Ok(())
    }

    /// EXPLAIN a plan without executing it.
    ///
    /// Explains the *optimized* plan, because that is what would run. Showing
    /// the plan as written would describe a query the engine never executes.
    pub fn explain(&mut self, branch: BranchId, plan: &Plan) -> Explain {
        self.refresh_statistics(branch, plan.source_table());
        let schema = self
            .views
            .get(&branch)
            .map(|v| v.schema.clone())
            .unwrap_or_default();
        let optimized = self.planner.optimize(plan.clone(), &schema);
        self.planner.explain(&optimized, &schema)
    }

    // ---- writes -----------------------------------------------------------

    /// Single-key write. Gated by the blast-radius breaker but not by change
    /// classification: a single-row put is never destructive by type.
    pub fn put(
        &mut self,
        branch: BranchId,
        key: &str,
        value: Value,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        self.check_type(branch, key, &value)?;
        self.check_unique(branch, key, &value)?;
        self.check_breaker(1, now_ms, &author)?;
        self.append(
            branch,
            OpType::Put {
                key: key.to_string(),
                value,
            },
            author,
            now_ms,
        )
    }

    /// Single-key write, conditional on the row's current version (M10.5).
    ///
    /// The precondition is checked here, in the same `&mut self` call that
    /// appends. That is what makes it a precondition rather than a suggestion:
    /// nothing can write to this branch between the check and the append,
    /// because writing needs the same exclusive borrow this call is holding.
    ///
    /// A caller reads a row, gets its version, and offers the write back with
    /// that version attached. If anything moved it in between, the write is
    /// refused and the caller is told the *current* version — so a retry costs
    /// no extra round trip to discover what happened, which matters most on
    /// exactly the contended keys where retries happen.
    ///
    /// This is what closes the read-modify-write gap. Without it two clients
    /// doing get-then-put both succeed and one update is silently lost; with
    /// it, the loser is told.
    pub fn put_if(
        &mut self,
        branch: BranchId,
        key: &str,
        value: Value,
        expect: Precondition,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        let current = self.views.get(&branch).and_then(|v| v.version_of(key));

        let satisfied = match expect {
            Precondition::Absent => current.is_none(),
            Precondition::Version(wanted) => current == Some(wanted),
        };
        if !satisfied {
            return Err(EngineError::PreconditionFailed {
                key: key.to_string(),
                found: current.is_some(),
                actual: current.unwrap_or(0),
            });
        }

        // Checked *after* the precondition, so a caller holding a stale version
        // is told that rather than being told their value has the wrong type —
        // the first is actionable and the second sends them looking in the
        // wrong place.
        self.check_type(branch, key, &value)?;
        self.check_unique(branch, key, &value)?;
        self.check_breaker(1, now_ms, &author)?;
        self.append(
            branch,
            OpType::Put {
                key: key.to_string(),
                value,
            },
            author,
            now_ms,
        )
    }

    /// The commit that last wrote `key` on `branch`, if it is there.
    pub fn version_of(&self, branch: BranchId, key: &str) -> Option<u64> {
        self.views.get(&branch)?.version_of(key)
    }

    /// Batched write. `rows` is the caller-supplied impact estimate that the
    /// breaker accumulates — this is the path a runaway agent loop takes.
    pub fn write_batch(
        &mut self,
        branch: BranchId,
        ops: Vec<OpType>,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        let rows = ops.len() as u64;
        self.check_breaker(rows, now_ms, &author)?;
        self.append(branch, OpType::Transaction { ops }, author, now_ms)
    }

    /// Several writes as one commit, or none of them.
    ///
    /// # Why the preconditions are all checked first
    ///
    /// Checking and applying per operation would let the first half of a
    /// transaction land and the second half be refused — which is exactly the
    /// partial state a transaction exists to prevent, arrived at through the
    /// mechanism meant to prevent it. So every condition is evaluated against
    /// the branch as it stands, and only then is anything appended.
    ///
    /// That is sound because the engine is owned by a single task and requests
    /// are handled one at a time: nothing can write between the check and the
    /// append. The guarantee comes from that serialisation rather than from a
    /// lock here, and if the engine ever gains concurrent writers this becomes
    /// wrong — loudly, and in a way no test here would catch, which is why it
    /// is written down.
    ///
    /// # Why two operations on one key are refused
    ///
    /// A transaction touching the same key twice has no useful meaning that a
    /// caller could not express more clearly with one operation, and it has
    /// several confusing ones: the precondition on the second operation would
    /// be evaluated against a state that the first operation is about to
    /// replace. Refused rather than resolved, per "prevent, don't correct".
    pub fn transaction(
        &mut self,
        branch: BranchId,
        ops: Vec<TxWrite>,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        if ops.is_empty() {
            return Err(EngineError::EmptyTransaction);
        }

        let mut seen = std::collections::BTreeSet::new();
        for op in &ops {
            if !seen.insert(op.key.as_str()) {
                return Err(EngineError::DuplicateKeyInTransaction {
                    key: op.key.clone(),
                });
            }
        }

        // Every precondition, against the branch as it stands now.
        for op in &ops {
            let Some(expect) = op.expect else { continue };
            let current = self.views.get(&branch).and_then(|v| v.version_of(&op.key));
            let satisfied = match expect {
                Precondition::Absent => current.is_none(),
                Precondition::Version(wanted) => current == Some(wanted),
            };
            if !satisfied {
                return Err(EngineError::PreconditionFailed {
                    key: op.key.clone(),
                    found: current.is_some(),
                    actual: current.unwrap_or(0),
                });
            }
        }

        // Types too, before anything is appended, and for the same reason.
        for op in &ops {
            if let TxWriteAction::Put { value, .. } = &op.action {
                self.check_type(branch, &op.key, value)?;
                self.check_unique(branch, &op.key, value)?;
            }
        }

        self.check_breaker(ops.len() as u64, now_ms, &author)?;

        let entries = ops
            .into_iter()
            .map(|op| match op.action {
                TxWriteAction::Put { value, .. } => OpType::Put { key: op.key, value },
                TxWriteAction::Delete => OpType::Delete { key: op.key },
            })
            .collect();

        self.append(branch, OpType::Transaction { ops: entries }, author, now_ms)
    }

    /// Propose a schema change. Always returns a diff and never mutates state —
    /// propose → diff → confirm/auto is the only route a schema change takes
    /// (`02-api-wire-protocol.md` §3).
    /// The impact is measured here, against this branch's own view. There is no
    /// parameter for it, because a caller that could name its own row count
    /// could name zero and walk a drop straight past the gate that reads it
    /// (`07-agent-safety-layer.md` §4).
    pub fn propose_schema_change(
        &mut self,
        branch: BranchId,
        change: SchemaChange,
        author: Author,
        now_ms: i64,
    ) -> ChangeDiff {
        let protected = self.branches.get(branch).is_some_and(|b| b.is_protected());
        let impact = self.estimate_impact(branch, &change);
        let diff = classify(&change, impact, self.policy(), protected, branch.0);
        let gate = gate_of(&diff);

        let branch_name = self.branch_name(branch);
        // The inputs go in beside the diff. Without them the trail records what
        // was decided and not what it was decided from, so a rule change could
        // never be tested against the decisions it would have altered.
        self.record_audit(
            AuditEntry::for_change(&diff, gate, &branch_name, author, now_ms)
                .with_decision(&change, impact, protected, branch.0, gate),
        );

        // Every proposal is held, not only the gated ones. An auto-apply change
        // still has to be applied by id — the caller does not know its gate
        // before proposing — and the alternative was a fallback that
        // re-classified whatever body the caller sent, which is the door both
        // substitutions walked through.
        self.pending.insert(
            diff.change_id.clone(),
            PendingChange {
                diff: diff.clone(),
                change,
                branch,
                proposed_at_ms: now_ms,
            },
        );
        diff
    }

    /// The scopes a proposal is charged against.
    ///
    /// Project, branch and the proposing agent. An agent that has spent its own
    /// budget must not be able to start a new branch and continue, and a fleet
    /// of individually-compliant agents must not be able to saturate one
    /// project's reviewers between them (`specs/07` §6.1.2).
    fn review_scopes(branch: BranchId, author: &Author) -> Vec<BudgetScope> {
        let mut scopes = vec![BudgetScope::Project, BudgetScope::Branch(branch.0)];
        if let Some(session) = author.session_id() {
            scopes.push(BudgetScope::Agent(session.to_string()));
        }
        scopes
    }

    fn spend_scopes(author: &Author) -> Vec<SpendScope> {
        let mut scopes = vec![SpendScope::Project];
        if let Some(session) = author.session_id() {
            scopes.push(SpendScope::Agent(session.to_string()));
        }
        scopes
    }

    /// Propose a change, refusing when this project has no review left.
    ///
    /// The gated entry point. [`Engine::propose_schema_change`] classifies and
    /// holds unconditionally, which is what the internal callers need; this is
    /// what a *request* goes through, because the budget decision has to happen
    /// before the proposal is accepted rather than after.
    ///
    /// A change that needs no human costs nothing, so an agent doing provably
    /// safe work is never throttled by this however much of it it does
    /// (`specs/07` §6.1.1).
    pub fn propose_within_budget(
        &mut self,
        branch: BranchId,
        change: SchemaChange,
        author: Author,
        now_ms: i64,
    ) -> Result<ChangeDiff> {
        // Classify first: the cost depends on the gate, and the gate is not
        // knowable until the change has been classified. Nothing is held yet.
        let protected = self.branches.get(branch).is_some_and(|b| b.is_protected());
        let impact = self.estimate_impact(branch, &change);
        let provisional = classify(&change, impact, self.policy(), protected, branch.0);

        let scopes = Self::review_scopes(branch, &author);
        let decision = self.review.reserve(provisional.gate, &scopes, now_ms);

        let BudgetDecision::Granted { reservation, .. } = decision else {
            let BudgetDecision::Exhausted { scope, reason, .. } = decision else {
                unreachable!("reserve returns one of two variants")
            };
            // Recorded before refusing. An agent repeatedly running out of
            // review is the most interesting pattern this trail can hold, and a
            // refusal that leaves no trace is one nobody can see coming.
            let branch_name = self.branch_name(branch);
            let gate = gate_of(&provisional);
            self.record_audit(
                AuditEntry::for_change(&provisional, gate, &branch_name, author, now_ms)
                    .with_decision(&change, impact, protected, branch.0, gate),
            );
            return Err(EngineError::ReviewExhausted {
                scope: format!("{scope:?}"),
                detail: reason,
            });
        };

        let diff = self.propose_schema_change(branch, change, author, now_ms);
        self.reservations
            .insert(diff.change_id.clone(), reservation);
        Ok(diff)
    }

    /// Settle a proposal's review reservation.
    ///
    /// Approved and rejected proposals keep their spend, because saying no takes
    /// as long as saying yes and refunding rejections would make bad proposals
    /// free to generate. Withdrawn ones are refunded (`specs/07` §6.1.4).
    fn settle_review(&mut self, change_id: &ChangeId, settlement: Settlement, now_ms: i64) {
        let Some(reservation) = self.reservations.remove(change_id) else {
            return;
        };
        if let Err(e) = self.review.settle(reservation, settlement, now_ms) {
            // An expired reservation means the proposal outlived its review
            // window. The proposal has already been refused by the time this
            // runs, so there is nothing to undo — but it is worth saying, since
            // it means a reviewer answered something that had lapsed.
            tracing::warn!(error = %e, change = %change_id.0, "review reservation could not be settled");
        }
    }

    /// Record work that actually happened against the spend budget.
    ///
    /// Charged with a measurement rather than an estimate: billing a project for
    /// what the planner guessed would be trusting the number the ceiling already
    /// refuses to trust when it is unmeasured (`specs/07` §6.3.1).
    pub fn record_spend(&mut self, author: &Author, actual_ms: u64, now_ms: i64) {
        self.spend
            .record(&Self::spend_scopes(author), actual_ms, now_ms);
    }

    /// Whether this caller may spend `estimated_ms` more.
    pub fn may_spend(&self, author: &Author, estimated_ms: u64, now_ms: i64) -> bool {
        self.spend
            .may_spend(&Self::spend_scopes(author), estimated_ms, now_ms)
            .allowed()
    }

    /// The review budget, for the `status` surface.
    pub fn review_budget(&self) -> &ReviewBudget {
        &self.review
    }

    /// Propose a change carrying a claim about the data it touches.
    ///
    /// The claim buys **direction, never trust**. It tells the server which
    /// cheap check to run; the check runs against this branch's own view, and
    /// nothing the caller sent is read as evidence. Row impact used to arrive in
    /// the proposal and an agent that wanted a drop waved through only had to
    /// say `rowsAffected: 0` — a caller-supplied *proof* is the same hole with a
    /// longer name (`specs/07` §6.4).
    ///
    /// A verified claim may only lower a gate, and only where the proof removes
    /// the risk the gate was for. A drop is still a drop.
    pub fn propose_with_claim(
        &mut self,
        branch: BranchId,
        change: SchemaChange,
        claim: Claim,
        author: Author,
        now_ms: i64,
    ) -> (ChangeDiff, Verdict) {
        let protected = self.branches.get(branch).is_some_and(|b| b.is_protected());
        let impact = self.estimate_impact(branch, &change);
        let mut diff = classify(&change, impact, self.policy(), protected, branch.0);
        // The gate the *policy* produced, kept before any proof touches it.
        // Replay asks what a policy would have decided, and a proof is not part
        // of the policy — recording the lowered gate would make every
        // proof-carrying migration replay as a spurious divergence.
        let classified_gate = gate_of(&diff);

        let verdict = {
            let rows = self.rows_of(branch);
            proof::verify(&claim, &change, &rows)
        };
        let (gate, reason) = proof::apply(classified_gate, &change, &verdict);

        if gate != classified_gate {
            diff.gate = gate;
            diff.requires_confirm = gate != Gate::AutoApply;
            diff.reason = reason;
        }

        let branch_name = self.branch_name(branch);
        self.record_audit(
            AuditEntry::for_change(&diff, gate, &branch_name, author, now_ms)
                .with_decision(&change, impact, protected, branch.0, classified_gate)
                .with_proof(&claim, &verdict, classified_gate, gate),
        );

        self.pending.insert(
            diff.change_id.clone(),
            PendingChange {
                diff: diff.clone(),
                change,
                branch,
                proposed_at_ms: now_ms,
            },
        );
        (diff, verdict)
    }

    /// This branch's rows, as the proof checker needs to see them.
    ///
    /// Both maps. CRDT-typed fields live separately and are just as much rows of
    /// the table — checking a claim over `keys` alone would examine a subset and
    /// report that it holds, which is the direction that lets a false claim
    /// lower a gate.
    fn rows_of(&self, branch: BranchId) -> BranchRows {
        let mut rows = Vec::new();
        if let Some(view) = self.views.get(&branch) {
            for (key, value) in view.keys.iter() {
                if RowAddress::parse(key).is_some() {
                    rows.push((key.clone(), value.clone()));
                }
            }
            for (key, state) in view.crdts.iter() {
                if RowAddress::parse(key).is_some() {
                    rows.push((key.clone(), state.value()));
                }
            }
        }
        BranchRows { rows }
    }

    /// Everything waiting for a human, grouped so it can be answered together.
    ///
    /// **Nothing is filtered.** A queue view that hides a change has made a
    /// decision about it. When the queue is too long the answer is the review
    /// budget, which refuses new work at the door, not a view that conceals
    /// work already accepted (`specs/07` §6.2).
    ///
    /// Ordered and grouped by [`theta_safety::triage`], so two processes looking
    /// at the same queue see the same queue.
    pub fn review_queue(&self) -> Vec<ReviewBatch> {
        // `pending` is a HashMap, so what comes out of it is in no order at
        // all. That is fine here and it is worth saying why, because the obvious
        // defence — sorting by change id before handing the changes over — was
        // written, and was dead code.
        //
        // `triage` orders members by `(strictness, rows, change_id)`, and that
        // last component makes the order *total*: the result does not depend on
        // what order the changes arrived in. Pre-sorting could not change the
        // answer, and a planted violation removing it proved exactly that.
        // Sorting here would have implied the guarantee lives in this function,
        // and the next person to move the sort would have found out it doesn't.
        let waiting: Vec<ChangeDiff> = self
            .pending
            .values()
            .filter(|p| p.diff.gate != Gate::AutoApply)
            .map(|p| p.diff.clone())
            .collect();
        triage(&waiting, self.review.policy())
    }

    /// Every decision this instance took that carries the inputs to replay it.
    ///
    /// Read out of the audit trail rather than reconstructed. The impact is the
    /// number the server measured at the time; re-measuring against today's data
    /// would answer a different question from the one being asked.
    ///
    /// The second value is how many audit entries described a classification but
    /// could not be replayed — entries written before the inputs were captured.
    /// Returned rather than dropped, for the same reason the verifier reports
    /// coverage: a replay over three of a hundred decisions that finds no
    /// loosening has not established that nothing loosened.
    pub fn recorded_decisions(&self) -> (Vec<RecordedDecision>, usize) {
        let mut replayable = Vec::new();
        let mut opaque = 0usize;
        for entry in self.audit.recent() {
            // A classification entry is one whose detail carries a diff. Some of
            // those predate the decision inputs; the rest are not decisions.
            match entry.recorded_decision() {
                Some(decision) => replayable.push(decision),
                None if entry.describes_a_classification() => opaque += 1,
                None => {}
            }
        }
        (replayable, opaque)
    }

    /// Replay every decision taken against a policy, and report what differs.
    ///
    /// The question a rule change has to answer before it ships: not "do the
    /// tests pass" but "what would this have let through". `looser` is the field
    /// to read — a single difference count lets a hundred harmless tightenings
    /// hide one loosening.
    pub fn replay_decisions(&self, policy: &SafetyPolicy) -> DecisionReplay {
        let (decisions, not_replayable) = self.recorded_decisions();
        DecisionReplay {
            report: replay(&decisions, policy),
            not_replayable,
        }
    }

    /// Attach a destination for anchors.
    ///
    /// Taken as a trait object rather than a generic parameter on `Engine`,
    /// because where anchors go is a deployment decision and threading it
    /// through the type would make every caller of `Engine` name it.
    pub fn set_anchor_sink(&mut self, sink: Box<dyn AnchorSink + Send>) {
        self.anchor_sink = Some(sink);
    }

    /// The anchors published so far.
    pub fn anchor_log(&self) -> &AnchorLog {
        &self.anchors
    }

    /// The current head of a branch.
    pub fn head(&self, branch: BranchId) -> Option<ContentHash> {
        self.log.head(branch)
    }

    /// The commit id a caller must use to sign its next write on this branch.
    ///
    /// A signed entry commits to its own commit id, so the caller has to know it
    /// before it can sign. Published rather than guessable: deriving it from the
    /// number of entries a caller has seen would be wrong the moment anyone else
    /// wrote.
    ///
    /// It can be stale by the time the write arrives, and that is handled the
    /// way `putIf` handles it - the write is refused and the caller retries.
    pub fn next_commit(&self, branch: BranchId) -> u64 {
        self.branches
            .get(branch)
            .map(|b| b.next_commit.0)
            .unwrap_or(0)
    }

    /// Learn a session's public signing key.
    ///
    /// Comes off the token, which the project key signed, so this needs no
    /// authentication of its own - and a separate registration call would have
    /// needed some, authenticated with the very token this arrives in.
    pub fn register_session_key(&mut self, session_id: &str, hex_key: &str) -> Result<()> {
        self.session_keys
            .register_hex(session_id, hex_key)
            .map_err(|_| EngineError::BadRequest {
                detail: "a session signing key must be 32 hex-encoded bytes forming a valid Ed25519 public key"
                    .into(),
            })
    }

    /// Whether this session has given us a key we could check a signature with.
    pub fn session_signs(&self, session_id: &str) -> bool {
        self.session_keys.get(session_id).is_some()
    }

    /// How far a signed write's timestamp may sit from this instance's clock.
    ///
    /// Bounded because the timestamp is inside the signature: without a bound a
    /// caller could sign an entry dated years from now and hold it, and the
    /// record would say it was written then. Five minutes is generous for clock
    /// drift and short enough that a held signature is not a useful weapon.
    const SIGNED_WRITE_SKEW_MS: i64 = 5 * 60 * 1_000;

    /// Append an entry the caller built and signed
    /// (`04-threat-model-security.md` §7.2).
    ///
    /// The server validates and appends verbatim; it never assembles the entry
    /// and then attaches the caller's signature, because an entry the server
    /// composed is one the signature cannot be about.
    ///
    /// What this establishes is narrower than "signed commits" usually implies,
    /// and worth stating exactly: it proves the holder of the session key
    /// produced the entry. It does not establish which human or model was behind
    /// that key, and **a stolen session key signs exactly as well as an honest
    /// one**. What it removes is the operator: with it, `author` stops being
    /// something this server merely asserts.
    pub fn append_signed(
        &mut self,
        branch: BranchId,
        op: SignedOp,
        author: Author,
        envelope: SignedEnvelope,
        now_ms: i64,
    ) -> Result<ContentHash> {
        let SignedEnvelope {
            commit_id,
            timestamp_ms,
            signature,
        } = envelope;

        let session = author
            .session_id()
            .ok_or_else(|| EngineError::BadRequest {
                detail: "only an agent session can sign a write".into(),
            })?
            .to_string();

        // The caller signed a specific position in the log. If somebody else
        // took it first, this entry is not the one that was signed - refused,
        // and the caller retries against the new head, exactly as `putIf` does.
        let expected = self.next_commit(branch);
        if commit_id != expected {
            return Err(EngineError::SignedWriteRaced {
                branch: branch.0,
                signed_for: commit_id,
                branch_is_at: expected,
            });
        }

        if (timestamp_ms - now_ms).abs() > Self::SIGNED_WRITE_SKEW_MS {
            return Err(EngineError::BadRequest {
                detail: format!(
                    "the signed timestamp {timestamp_ms} is more than {}ms from this \
                     instance's clock ({now_ms}). The timestamp is inside the signature, \
                     so accepting it would let a signature made now assert a time it was \
                     not made at.",
                    Self::SIGNED_WRITE_SKEW_MS
                ),
            });
        }

        let entry = LogEntry {
            prev_hash: self.log.head(branch).unwrap_or(ContentHash::ZERO),
            commit_id: theta_core::CommitId(commit_id),
            branch_id: branch,
            op: signed_op_to_optype(&op, self, branch)?,
            author,
            timestamp_ms,
        };

        // Verified before anything else happens. A forged write must not reach
        // the type checker or spend breaker budget on its way to being refused.
        self.signatures
            .accept(&entry, &session, signature, &self.session_keys)
            .map_err(EngineError::Signature)?;

        // Then the checks every write goes through. A signature is evidence of
        // authorship, never an exemption: a signed drop is still a drop.
        if let OpType::Put { key, value } = &entry.op {
            self.check_type(branch, key, value)?;
            self.check_unique(branch, key, value)?;
        }
        self.check_breaker(1, now_ms, &entry.author)?;

        let hash = self.log.append_and_apply(entry, &mut self.views)?;
        self.branches.set_head(branch, hash)?;
        Ok(hash)
    }

    /// Check every entry that should have been signed.
    ///
    /// Driven by the entries, not by the book. Iterating the book would verify
    /// only the signatures that are present, which is exactly what an attacker
    /// who stripped one would want: nothing missing means nothing checked.
    ///
    /// Scoped to sessions that gave us a key. A session that never signed
    /// anything is not evidence of tampering, and treating it as such would make
    /// this unusable on any project that had not adopted signing everywhere.
    pub fn verify_signatures(&self, branch: BranchId) -> Result<usize> {
        let entries = self.chain(branch)?;
        self.signatures
            .verify_all(&entries, &self.session_keys, |author| {
                author
                    .session_id()
                    .is_some_and(|s| self.session_keys.get(s).is_some())
            })
            .map_err(EngineError::Signature)
    }

    /// The ids of a sequence's live elements, in order.
    ///
    /// Without this, [`CrdtMutation::SeqRemove`] is unusable: it names an
    /// element by id, and a caller reading through [`Engine::get_crdt`] sees
    /// values only. A removal that cannot name its target is a removal nobody
    /// can issue — the write path would have been half-built and looked whole.
    ///
    /// Ids rather than positions, because a position is not stable under a
    /// concurrent insert and the id is exactly what survives one.
    pub fn sequence_ids(&self, branch: BranchId, key: &str) -> Vec<theta_core::crdt::ElemId> {
        use theta_core::crdt::CrdtState;
        match self.views.get(&branch).and_then(|v| v.crdts.get(key)) {
            Some(CrdtState::Sequence(seq)) => seq.live_ids(),
            _ => Vec::new(),
        }
    }

    /// Mutate a CRDT-typed row (`01-system-architecture.md` §3.3).
    ///
    /// The write path for a converging field. Separate from [`Engine::put`]
    /// because a CRDT converges by replaying *operations*: a put would write an
    /// absolute value, and two absolute values cannot be reconciled without
    /// choosing one — which is the whole reason the field is declared this way.
    ///
    /// Refuses a mutation whose shape disagrees with the field's declared kind,
    /// rather than appending it and letting the fold record a rejection. The
    /// entry would be in the log, the caller would have been told the write
    /// succeeded, and the value would not have changed
    /// (`docs/INVARIANTS.md` invariant 3 — prevent, don't correct).
    pub fn apply_crdt(
        &mut self,
        branch: BranchId,
        key: &str,
        mutation: &CrdtMutation,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        let address = RowAddress::parse(key).ok_or_else(|| EngineError::BadRequest {
            detail: format!(
                "`{key}` is not a row address: a CRDT-typed row is addressed `table:id`, like every other row"
            ),
        })?;

        let requested = crdt_kind_of(mutation);

        // What the schema says this field is, if it says anything. A CRDT-typed
        // key is a whole row whose value is the CRDT, so the declaration lives
        // on the row's `value` column.
        let declared = self
            .views
            .get(&branch)
            .and_then(|v| v.schema.field(address.table, theta_core::VALUE_COLUMN))
            .and_then(|f| f.crdt);

        // And what it already is, which can differ from the declaration on a
        // branch that has been written to under a schema since changed.
        let existing = self
            .views
            .get(&branch)
            .and_then(|v| v.crdts.get(key))
            .map(|state| state.kind());

        for (kind, source) in [(declared, "declared"), (existing, "already")] {
            if let Some(kind) = kind {
                if kind != requested {
                    return Err(EngineError::TypeMismatch {
                        field: key.to_string(),
                        declared: format!("a {kind:?} ({source})"),
                        actual: format!("a {requested:?} operation"),
                    });
                }
            }
        }

        self.check_breaker(1, now_ms, &author)?;

        let op = self.lower_mutation(branch, mutation)?;
        self.append(
            branch,
            OpType::Crdt {
                key: key.to_string(),
                mutation: op,
            },
            author,
            now_ms,
        )
    }

    /// Turn a caller's mutation into the operation that goes in the log.
    ///
    /// The one place an element id is minted, and it is minted here rather than
    /// accepted from the caller. An RGA id fixes both an element's identity and
    /// its order among concurrent siblings, so a client-chosen id could collide
    /// with another writer's element or displace one. `(commit, branch)` is
    /// unique by construction — which is also why replaying an entry twice is
    /// idempotent.
    fn lower_mutation(
        &self,
        branch: BranchId,
        mutation: &CrdtMutation,
    ) -> Result<theta_core::CrdtOp> {
        use theta_core::crdt::{ElemId, ReplicaId};
        use theta_core::CrdtOp;

        let elem = |id: &theta_proto::wire::ElemId| ElemId {
            counter: id.counter,
            replica: ReplicaId(id.replica),
        };

        // Same encoding as every other value on the wire. A CRDT element that
        // decoded differently from a row value would converge on something the
        // caller never sent.
        let json = decode_wire_value;

        Ok(match mutation {
            CrdtMutation::Increment { by } => CrdtOp::Increment { by: *by },
            CrdtMutation::SetRegister { value_json } => CrdtOp::SetRegister {
                value: json(value_json)?,
            },
            CrdtMutation::SetAdd { element_json } => CrdtOp::SetAdd {
                element: json(element_json)?,
            },
            CrdtMutation::SetRemove { element_json } => CrdtOp::SetRemove {
                element: json(element_json)?,
            },
            CrdtMutation::SeqInsert { after, value_json } => CrdtOp::SeqInsert {
                id: ElemId {
                    counter: self
                        .branches
                        .get(branch)
                        .map(|b| b.next_commit.0)
                        .unwrap_or(0),
                    replica: ReplicaId(branch.0),
                },
                after: after.as_ref().map(elem),
                value: json(value_json)?,
            },
            CrdtMutation::SeqRemove { id } => CrdtOp::SeqRemove { id: elem(id) },
        })
    }

    /// Join the queue of merges waiting to land on `target`.
    ///
    /// Speculates against the target **as it will be** — the current target with
    /// every already-queued merge applied — rather than as it is. A queue that
    /// validated against the target as it stands would clear a merge and then
    /// land it into a branch it was never checked against.
    ///
    /// A merge that would conflict is refused here, not queued. The whole point
    /// is that the agent finds out while it still remembers why it made the
    /// change; queuing it would move that discovery to the front of the queue,
    /// which is the delay this removes.
    pub fn enqueue_merge(
        &mut self,
        source: BranchId,
        target: BranchId,
        now_ms: i64,
    ) -> std::result::Result<u64, EnqueueError> {
        let base = self.branches.get(source).and_then(|b| b.fork_point);
        let empty = theta_storage::MaterializedView::new();
        let target_view = self.views.get(&target).unwrap_or(&empty).clone();
        let source_view = self.views.get(&source).unwrap_or(&empty).clone();

        let queue = self
            .queues
            .entry(target)
            .or_insert_with(|| MergeQueue::new(target));
        queue.enqueue(&self.log, source, base, &target_view, &source_view, now_ms)
    }

    /// What is waiting to land on `target`, in the order it will land.
    pub fn merge_queue(&self, target: BranchId) -> &[Queued] {
        self.queues.get(&target).map(|q| q.entries()).unwrap_or(&[])
    }

    /// Withdraw a queued merge.
    ///
    /// Everything behind it was speculated against a future that now includes
    /// this merge and will not. The caller must revalidate; this does not do it
    /// silently, because eviction is news somebody has to be told.
    pub fn withdraw_merge(&mut self, target: BranchId, ticket: u64) -> Option<Queued> {
        self.queues.get_mut(&target)?.withdraw(ticket)
    }

    /// Re-speculate a target's queue and evict the merges that stopped landing.
    ///
    /// Returns what was evicted so the owners can be told. A merge validated
    /// against a future that did not happen has not been validated at all, and
    /// landing it would be exactly the failure the queue exists to prevent.
    pub fn revalidate_merges(&mut self, target: BranchId) -> Result<Vec<Evicted>> {
        let Some(mut queue) = self.queues.remove(&target) else {
            return Ok(Vec::new());
        };
        let empty = theta_storage::MaterializedView::new();
        let target_view = self.views.get(&target).unwrap_or(&empty).clone();
        // Every source's view, because revalidation re-speculates the whole
        // queue and a source whose view is missing is a branch that is gone.
        let sources: std::collections::BTreeMap<BranchId, theta_storage::MaterializedView> = queue
            .entries()
            .iter()
            .filter_map(|q| self.views.get(&q.source).map(|v| (q.source, v.clone())))
            .collect();

        let evicted = queue.revalidate(&self.log, &target_view, &sources);
        self.queues.insert(target, queue);
        Ok(evicted?)
    }

    /// Land the merge at the head of a target's queue.
    ///
    /// Popped and then merged for real. The speculation said it would land; this
    /// is where it actually does, through the same `merge` every other caller
    /// goes through — a queue that applied its own speculative result would be
    /// landing something no merge path had produced.
    pub fn land_next_merge(
        &mut self,
        target: BranchId,
        author: Author,
        now_ms: i64,
    ) -> Result<Option<MergeOutcome>> {
        let Some(queue) = self.queues.get_mut(&target) else {
            return Ok(None);
        };
        let Some(head) = queue.pop_head() else {
            return Ok(None);
        };
        Ok(Some(self.merge(head.source, target, author, now_ms)?))
    }

    /// Place a region's branch.
    ///
    /// Refuses a second placement for one region rather than replacing the
    /// first: replacing would silently move every subsequent write in that
    /// region, and the writes already on the old branch would stop being visible
    /// to callers still reading where they were told to.
    pub fn place_region(&mut self, region: Region, branch: BranchId) -> Result<()> {
        // The branch has to exist. Placing a region on a branch that does not
        // would produce a routing table that resolves cleanly and sends every
        // write in that region to nowhere — a failure that surfaces at write
        // time, in another region's timezone.
        if self.branches.get(branch).is_none() {
            return Err(EngineError::BadRequest {
                detail: format!(
                    "cannot place region `{}` on branch {}: no such branch",
                    region.0, branch.0
                ),
            });
        }
        self.placement.place(region, branch).map_err(Into::into)
    }

    /// Which branch a caller in this region writes to.
    ///
    /// Refuses an unplaced region rather than falling back to the home branch. A
    /// fallback would send a Frankfurt write across the Atlantic silently, which
    /// is the exact cost the arrangement exists to avoid, and the caller would
    /// have no way to tell it happened.
    ///
    /// **Only half of M23 is here.** The other half — follower reads, where a
    /// replica refuses below a version the caller has already seen and redirects
    /// rather than answering staler — is deliberately not wired. This instance is
    /// a primary and a primary is never behind, so `region::may_serve` against it
    /// can only ever return `Serve`: a decision function that structurally cannot
    /// take its interesting branch, reporting a guarantee it never had occasion
    /// to enforce. It lands with read replicas, which do not exist yet.
    pub fn route_write(&self, region: &Region) -> Result<BranchId> {
        self.placement.route(region).map_err(Into::into)
    }

    /// The regions this instance serves, for the status surface.
    pub fn regions(&self) -> Vec<String> {
        self.placement.regions().map(|(r, _)| r.0.clone()).collect()
    }

    /// How storage divides across branches (`specs/07` §6.3.3).
    ///
    /// Exclusive plus shared equals the real total, which is what makes it
    /// defensible as a bill. Runs over every branch's view at once because the
    /// question "who is this row's sole owner" cannot be answered one branch at
    /// a time.
    pub fn storage_attribution(&self) -> StorageAttribution {
        attribution_bytes::attribute(&self.views)
    }

    /// The ordered map for a branch, empty when the branch has no view.
    ///
    /// A branch nothing has been written to holds no keys, and a proof over an
    /// empty map is an honest statement that there is nothing to omit. Erroring
    /// instead would make "prove this empty branch returned everything" fail,
    /// which is a question with a correct answer.
    fn map_of(&self, branch: BranchId) -> theta_storage::completeness::OrderedMap {
        match self.views.get(&branch) {
            Some(view) => view.ordered_map(),
            None => theta_storage::completeness::OrderedMap::build(Vec::new()),
        }
    }

    /// The root of the authenticated ordered map over a branch's live keys.
    ///
    /// A caller needs this to check a completeness proof, and the caveat is the
    /// same one inclusion proofs carry: a root the server just handed you
    /// proves the server is self-consistent and nothing more. It is worth
    /// something when it arrives from somewhere else -- an anchor, a
    /// counterparty, a previous session.
    pub fn map_root(&self, branch: BranchId) -> Result<ContentHash> {
        Ok(self.map_of(branch).root())
    }

    /// Prove that every key in `[start, end)` is in the result, and none is
    /// missing.
    ///
    /// This is the thing `inclusion.rs` says it cannot do. An inclusion proof
    /// shows a row is real; it cannot show the server told you about all of
    /// them, because catching an omission needs the client to know the key set
    /// already. The ordered map fixes that by making adjacency provable: the
    /// proof carries the two keys immediately outside the range, so a hidden
    /// key would have to occupy an index the client has already accounted for.
    pub fn prove_range_complete(
        &self,
        branch: BranchId,
        start: &str,
        end: &str,
    ) -> Result<completeness::RangeProof> {
        Ok(self.map_of(branch).prove_range(start, end))
    }

    /// Prove that `key` is not in a branch.
    ///
    /// "Not found" is otherwise the one answer a client has to take on trust --
    /// an inclusion proof can show what is there and has nothing to say about
    /// what is not.
    pub fn prove_absent(&self, branch: BranchId, key: &str) -> Result<completeness::RangeProof> {
        Ok(self.map_of(branch).prove_absent(key))
    }

    /// Prove that an entry is in a branch's history, under its current head.
    ///
    /// Of limited value on its own, and the limitation is the point: a proof
    /// against a root the server chose shows only that the server is internally
    /// consistent. Use [`Engine::prove_inclusion_under_anchor`] where the caller
    /// needs a root the server does not control.
    pub fn prove_inclusion(
        &self,
        branch: BranchId,
        entry: &ContentHash,
    ) -> Result<Option<InclusionProof>> {
        Ok(inclusion::prove(&self.chain(branch)?, entry))
    }

    /// Prove that an entry is in a branch's history, under its newest *anchored*
    /// head.
    ///
    /// The composition that makes an inclusion proof worth anything. The root is
    /// one a counterparty already holds — it was published to a sink and can be
    /// produced independently — so a caller verifying against it is checking
    /// against something the server cannot quietly revise
    /// (`04-threat-model-security.md` §7.1).
    ///
    /// Fails when the branch has never been anchored, rather than falling back
    /// to the current head. A silent fallback would hand back a proof that looks
    /// identical to a strong one and establishes strictly less.
    pub fn prove_inclusion_under_anchor(
        &self,
        branch: BranchId,
        entry: &ContentHash,
    ) -> Result<Option<InclusionProof>> {
        let anchor = self
            .anchors
            .latest(branch)
            .ok_or(AnchorError::NeverAnchored { branch: branch.0 })?;

        // History as it was when the anchor was taken. Proving against the
        // anchored root requires the chain to *end* there — everything written
        // since is outside what the anchor covers, and including it would build
        // a proof whose root is not the one being trusted.
        let mut history = self.chain(branch)?;
        match history.iter().position(|e| e.hash() == anchor.head) {
            Some(at) => history.truncate(at + 1),
            // The anchored head is not in this branch's history. That is the
            // case anchoring exists to catch, so it is an error and not an
            // absent proof.
            None => {
                return Err(EngineError::Anchor(AnchorError::AnchoredHeadMissing {
                    head: anchor.head.to_hex(),
                    published_at_ms: anchor.published_at_ms,
                }))
            }
        }

        Ok(inclusion::prove(&history, entry))
    }

    /// The earliest commit this branch's history still holds.
    ///
    /// Passed to every temporal read as the horizon. The tempting value is
    /// `None` — "nothing has expired" — which is true today because nothing
    /// expires anything yet, and which would quietly become a lie the moment
    /// retention lands: a caller asking for a point below the horizon would be
    /// answered from the earliest state available instead of being refused, and
    /// would have no way to tell they were given a different instant from the one
    /// they asked for.
    ///
    /// Derived from the log instead, so it is correct now and stays correct
    /// without anybody remembering to come back here.
    fn horizon_of(&self, branch: BranchId) -> Option<u64> {
        self.chain(branch)
            .ok()
            .and_then(|entries| entries.first().map(|e| e.commit_id.0))
    }

    /// Read a branch as it was.
    ///
    /// The commit actually resolved comes back with the answer, including for a
    /// timestamp query: timestamps are advisory, and a caller who asked by time
    /// and assumed they got their exact instant is drawing conclusions about a
    /// different one (`specs/03` §4).
    pub fn read_as_of(&self, branch: BranchId, point: AsOf) -> Result<Snapshot> {
        let entries = self.chain(branch)?;
        temporal::as_of(&entries, point, self.horizon_of(branch)).map_err(EngineError::Temporal)
    }

    /// What changed between two points on a branch.
    pub fn diff_between(&self, branch: BranchId, from: AsOf, to: AsOf) -> Result<Vec<RowChange>> {
        let before = self.read_as_of(branch, from)?;
        let after = self.read_as_of(branch, to)?;
        Ok(temporal::diff(&before.view, &after.view))
    }

    /// Changes since a cursor, for a consumer following the branch.
    ///
    /// A consumer that was away longer than history is kept is told so rather
    /// than resumed from wherever history now begins — a gap it cannot see makes
    /// every downstream aggregate built from the feed quietly wrong.
    pub fn changes_since(&self, branch: BranchId, cursor: Cursor, limit: usize) -> Result<Feed> {
        let entries = self.chain(branch)?;
        Ok(temporal::since(
            &entries,
            cursor,
            self.horizon_of(branch),
            limit,
        ))
    }

    /// Every entry in the project, oldest first, each counted once.
    ///
    /// Branches share history up to their fork point, so concatenating each
    /// branch's history counts every shared ancestor once per branch that
    /// descends from it — which would report a session that wrote ten entries
    /// before anyone branched as having written thirty. Deduplicated by entry
    /// hash, which is exactly the identity that makes an entry the same entry.
    ///
    /// Ordered by commit id so the result reads as the sequence it was.
    fn all_entries(&self) -> Vec<LogEntry> {
        let mut seen: std::collections::HashSet<ContentHash> = Default::default();
        let mut out: Vec<LogEntry> = Vec::new();
        for branch in self.branches.iter().map(|b| b.id).collect::<Vec<_>>() {
            let Ok(history) = self.log.history(branch, None) else {
                continue;
            };
            for entry in history {
                if seen.insert(entry.hash()) {
                    out.push(entry);
                }
            }
        }
        out.sort_by_key(|e| (e.commit_id.0, e.branch_id.0));
        out
    }

    /// Who has written anything in this project.
    ///
    /// The question an investigation actually starts with, because nobody knows
    /// the session id yet. Across every branch, not one: a session that did its
    /// work on a branch is still a session that has been in here.
    pub fn sessions(&self) -> Vec<String> {
        theta_storage::attribution::sessions(&self.all_entries())
    }

    /// What one session, task, agent or user did.
    ///
    /// Note which of those are trustworthy. A session comes from the credential;
    /// an agent name is self-reported and is a *filter*, not evidence, until
    /// something signs for it (`04-threat-model-security.md` §7.2).
    pub fn activity(
        &self,
        who: &theta_storage::attribution::Attribution<'_>,
    ) -> theta_storage::attribution::Activity {
        theta_storage::attribution::activity(&self.all_entries(), who)
    }

    /// The entries themselves, for a reviewer who needs to read them.
    pub fn entries_for(&self, who: &theta_storage::attribution::Attribution<'_>) -> Vec<LogEntry> {
        let all = self.all_entries();
        theta_storage::attribution::entries_for(&all, who)
            .into_iter()
            .cloned()
            .collect()
    }

    /// How long the tail may stay unprotected.
    pub fn anchor_policy(&self) -> &AnchorPolicy {
        &self.anchor_policy
    }

    /// How many entries a verifier tick covers.
    ///
    /// Adjustable because the right number depends on how much of the machine
    /// serving is using, and a verifier an operator turns off during the first
    /// busy hour verifies nothing.
    pub fn set_verifier_entries_per_tick(&mut self, entries: usize) {
        let policy = VerifierPolicy {
            entries_per_tick: entries.max(1),
            ..VerifierPolicy::default()
        };
        self.verifier = Verifier::new(policy, 0);
    }

    /// The log a verifier pass walks, oldest first.
    ///
    /// `history` walks backwards from the head, so it arrives newest first. The
    /// verifier checks that each entry's predecessor has already been seen, so
    /// handing it the list in the order the store produced it would fail on the
    /// second entry of any log — a mistake that looks exactly like the tampering
    /// this is meant to detect, which is the argument for reversing here once
    /// rather than at each call site.
    fn chain(&self, branch: BranchId) -> Result<Vec<LogEntry>> {
        let mut entries = self.log.history(branch, None)?;
        entries.reverse();
        Ok(entries)
    }

    /// Run one periodic maintenance pass: verify a slice, anchor what is due.
    ///
    /// Separate from [`Engine::checkpoint`] deliberately. Checkpointing is a
    /// local durability optimisation that cannot fail in a way anyone must act
    /// on; this talks to an external sink, can find tampering, and returns
    /// findings. Folding the two together would have put a network call on the
    /// flush path and buried an integrity failure in a routine one.
    pub fn maintain(&mut self, now_ms: i64) -> Maintenance {
        let mut chain_error = None;
        let entries = self.chain(BranchId::MAIN).unwrap_or_default();
        let coverage = match self.verifier.tick(&entries, now_ms) {
            Ok(coverage) => coverage,
            Err(e) => {
                chain_error = Some(e);
                self.verifier.progress()
            }
        };

        // Asked separately from `tick`, because a tick that found no broken link
        // says nothing about whether the pass will ever finish (§7.3).
        let pace = self.verifier.check_pace(now_ms).err();

        let mut anchored = Vec::new();
        let mut unanchored = Vec::new();
        let branches: Vec<BranchId> = self.branches.iter().map(|b| b.id).collect();

        for branch in branches {
            let due = match self.anchors.latest(branch) {
                Some(last) => {
                    now_ms.saturating_sub(last.published_at_ms) >= self.anchor_policy.max_gap_ms
                }
                None => self.anchor_policy.require_first_anchor,
            };
            if !due {
                continue;
            }

            let Some(head) = self.log.head(branch) else {
                continue;
            };
            let covered = self.log.history(branch, None).map(|h| h.len()).unwrap_or(0) as u64;

            // No sink is not "nothing to do". A deployment that has never
            // configured one has an unprotected log, and reporting that as clean
            // would be the most confident wrong answer available here.
            let Some(sink) = self.anchor_sink.as_mut() else {
                unanchored.push((branch, AnchorError::NeverAnchored { branch: branch.0 }));
                continue;
            };

            match self
                .anchors
                .publish(sink.as_mut(), branch, head, covered, now_ms)
            {
                Ok(a) => anchored.push((branch, a.receipt.clone())),
                Err(e) => unanchored.push((branch, e)),
            }
        }

        Maintenance {
            coverage,
            chain: chain_error,
            pace,
            anchored,
            unanchored,
        }
    }

    /// Check published anchors against the log as it stands now.
    ///
    /// Asks the sink to reproduce what it took, so this fails when the sink
    /// cannot — an anchor the destination cannot produce is our own record of
    /// our own action, not evidence (§7.1).
    pub fn verify_anchors(
        &self,
        branch: BranchId,
        now_ms: i64,
    ) -> Result<theta_storage::anchor::Verified> {
        let Some(sink) = self.anchor_sink.as_ref() else {
            return Err(EngineError::Anchor(AnchorError::NeverAnchored {
                branch: branch.0,
            }));
        };
        let present: std::collections::HashSet<ContentHash> = self
            .log
            .history(branch, None)?
            .iter()
            .map(|e| e.hash())
            .collect();
        self.anchors
            .verify(sink.as_ref(), branch, &self.anchor_policy, now_ms, |h| {
                present.contains(h)
            })
            .map_err(EngineError::Anchor)
    }

    /// Apply a previously proposed change.
    ///
    /// `confirm` alone is never sufficient for a [`Gate::ShadowValidate`]
    /// change: that path requires the change to have been applied to a shadow
    /// branch and promoted (`07-agent-safety-layer.md` §4).
    /// Confirm a pending change.
    ///
    /// Takes the change id and nothing else that decides what happens. What
    /// runs and where both come from the proposal the gate classified: a
    /// caller that could supply either could confirm one change and execute
    /// another, which is exactly what used to happen — a `drop column`
    /// proposal confirmed with a `drop table` body dropped the table, and a
    /// proposal measured against an empty branch, confirmed against `main`,
    /// applied `main`'s rows under the empty branch's classification. In both
    /// the human confirmed one thing and a different thing ran.
    pub fn apply_schema_change(
        &mut self,
        change_id: &ChangeId,
        confirm: bool,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        // Nothing pending means the id was never issued, or already answered.
        // There is no re-classify-and-maybe-apply fallback: that path existed to
        // serve auto-applied changes, which by definition already applied and
        // have nothing to confirm.
        let Some(pending) = self.pending.get(change_id).cloned() else {
            return Err(EngineError::UnknownChange(change_id.clone()));
        };
        let PendingChange {
            diff,
            change,
            branch,
            ..
        } = pending;

        match gate_of(&diff) {
            // Not "confirmation plus a shadow branch". Confirmation, full stop,
            // is not a path to applying this change — `promote_shadow` is, and
            // it merges what was validated rather than re-running the change
            // here (`07-agent-safety-layer.md` §4, §5).
            //
            // This used to accept a confirmation as soon as a shadow branch
            // existed, so opening an empty one and confirming cleared the
            // strongest gate in the product.
            Gate::ShadowValidate => Err(EngineError::Gated {
                reason: format!(
                    "{} — confirmation is not sufficient; validate on a shadow branch \
                     and promote it",
                    diff.reason
                ),
                diff: Box::new(diff),
            }),
            Gate::Confirm if !confirm => Err(EngineError::Gated {
                reason: diff.reason.clone(),
                diff: Box::new(diff),
            }),
            _ => {
                self.pending.remove(change_id);
                // The review this proposal reserved was actually consumed.
                // Settled as approved, so it stays spent (`specs/07` §6.1.4).
                self.settle_review(change_id, Settlement::Approved, now_ms);
                self.append(branch, OpType::Schema { change }, author, now_ms)
            }
        }
    }

    /// The branch a pending proposal targets.
    pub fn pending_branch(&self, change_id: &ChangeId) -> Option<BranchId> {
        self.pending.get(change_id).map(|p| p.branch)
    }

    /// What a change would touch on one branch.
    ///
    /// A branch with no view yet is empty, so the estimate is zero — which is
    /// the truth, not a fallback.
    /// What is in this branch, and where it came from (M19).
    ///
    /// Reads the view for shape and the branch's *schema* chain for provenance.
    /// Both come from this branch, which is the point: a description stamped
    /// from another branch's history would carry `declaredAt` pointers to
    /// commits this branch never saw — a wrong pointer, which is worse than the
    /// absence it replaced.
    pub fn describe(
        &self,
        branch: BranchId,
        request: &crate::describe::DescribeRequest,
    ) -> theta_proto::wire::SchemaDescriptionWire {
        static EMPTY: std::sync::OnceLock<theta_storage::MaterializedView> =
            std::sync::OnceLock::new();
        let view = self
            .views
            .get(&branch)
            .unwrap_or_else(|| EMPTY.get_or_init(theta_storage::MaterializedView::new));
        let entries = self.log.schema_chain(branch);
        crate::describe::describe(view, &entries, request)
    }

    pub fn estimate_impact(&self, branch: BranchId, change: &SchemaChange) -> Impact {
        static EMPTY: std::sync::OnceLock<theta_storage::MaterializedView> =
            std::sync::OnceLock::new();
        let view = self
            .views
            .get(&branch)
            .unwrap_or_else(|| EMPTY.get_or_init(theta_storage::MaterializedView::new));
        crate::impact::estimate(view, change)
    }

    /// Propose a change and do whatever its gate demands before returning.
    ///
    /// This is the flow `07-agent-safety-layer.md` §5 describes end to end, and
    /// the one its own §8 example assumes: *"blocked, redirected to shadow
    /// branch shadow-4f2, validation passed, awaiting your promotion"*. The
    /// redirect and the validation are consequences of the gate, not two more
    /// commands someone has to know to run — a reviewer should be answering a
    /// question, not assembling one.
    ///
    /// Nothing here relaxes the gate. The shadow branch is opened and validated
    /// automatically; landing the change still takes an explicit promotion.
    pub fn propose(
        &mut self,
        branch: BranchId,
        change: SchemaChange,
        author: Author,
        now_ms: i64,
    ) -> Result<Proposal> {
        let diff = self.propose_schema_change(branch, change.clone(), author.clone(), now_ms);

        if gate_of(&diff) != Gate::ShadowValidate {
            return Ok(Proposal { diff, shadow: None });
        }

        self.open_shadow_branch(branch, &diff.change_id, change, author.clone(), now_ms)?;
        self.validate_shadow(&diff.change_id, author, now_ms)?;

        let shadow = self.shadows.get(&diff.change_id).cloned();
        // Re-read the diff: opening the branch stamped its id onto the pending
        // proposal, and the caller should see the branch its change is on.
        let diff = self
            .pending
            .get(&diff.change_id)
            .map(|p| p.diff.clone())
            .unwrap_or(diff);
        Ok(Proposal { diff, shadow })
    }

    /// A proposal awaiting an answer, whether or not it has a shadow branch.
    pub fn pending_change(&self, change_id: &ChangeId) -> Option<Proposal> {
        Some(Proposal {
            diff: self.pending.get(change_id)?.diff.clone(),
            shadow: self.shadows.get(change_id).cloned(),
        })
    }

    /// Open an ephemeral shadow branch and apply the proposed change to it.
    ///
    /// The change lands on the shadow branch, never on the target. Opening the
    /// branch without applying anything to it — which is all this used to do —
    /// produces a branch identical to the target, so any comparison against it
    /// finds nothing and "validated" means nothing
    /// (`07-agent-safety-layer.md` §5.1).
    pub fn open_shadow_branch(
        &mut self,
        target: BranchId,
        change_id: &ChangeId,
        change: SchemaChange,
        author: Author,
        now_ms: i64,
    ) -> Result<BranchId> {
        if let Some(existing) = self.shadows.get(change_id) {
            // Idempotent: a retried request must not leave a second branch
            // behind holding the same change.
            return Ok(existing.shadow);
        }

        let head = self.log.head(target).unwrap_or(ContentHash::ZERO);
        let name = format!("shadow-{}-{}", target.0, &change_id.0[4..12]);
        let shadow = self.branches.create(&name, head, BranchKind::Shadow)?;
        self.log.set_head(shadow, head);
        if let Some(view) = self.views.get(&target).cloned() {
            self.views.insert(shadow, view);
        }

        // The point of the branch: the change actually runs, somewhere that is
        // not the target.
        let applied_head = self.append(
            shadow,
            OpType::Schema {
                change: change.clone(),
            },
            author,
            now_ms,
        )?;

        self.shadows.insert(
            change_id.clone(),
            ShadowValidation {
                change_id: change_id.clone(),
                target,
                shadow,
                applied_head,
                opened_at_ms: now_ms,
                outcome: None,
            },
        );
        self.shadow_changes.insert(change_id.clone(), change);

        if let Some(pending) = self.pending.get_mut(change_id) {
            pending.diff.shadow_branch_id = Some(shadow.0);
        }
        Ok(shadow)
    }

    /// Run verification against a shadow branch and record the result.
    ///
    /// Deterministic comparison of two folds. Re-running it re-derives the same
    /// answer, so a caller cannot get a different verdict by asking twice.
    pub fn validate_shadow(
        &mut self,
        change_id: &ChangeId,
        author: Author,
        now_ms: i64,
    ) -> Result<Validation> {
        let record = self
            .shadows
            .get(change_id)
            .ok_or_else(|| EngineError::UnknownChange(change_id.clone()))?
            .clone();
        let change = self
            .shadow_changes
            .get(change_id)
            .ok_or_else(|| EngineError::UnknownChange(change_id.clone()))?
            .clone();

        let empty = theta_storage::MaterializedView::new();
        let target_view = self.views.get(&record.target).unwrap_or(&empty);
        let shadow_view = self.views.get(&record.shadow).unwrap_or(&empty);

        let checks = shadow::validate(target_view, shadow_view, &change);
        let validation = Validation {
            passed: checks.iter().all(|c| c.passed),
            ran_at_ms: now_ms,
            validated_head: self.log.head(record.shadow).unwrap_or(ContentHash::ZERO),
            checks,
        };

        self.record_audit(AuditEntry::for_validation(
            &change_id.0,
            &self.branch_name(record.target),
            validation.passed,
            &validation.summary(),
            author,
            now_ms,
        ));

        if let Some(stored) = self.shadows.get_mut(change_id) {
            stored.outcome = Some(validation.clone());
        }
        Ok(validation)
    }

    /// What validating this change found, if it has been validated.
    pub fn shadow_validation(&self, change_id: &ChangeId) -> Option<&ShadowValidation> {
        self.shadows.get(change_id)
    }

    /// Promote a validated change onto its target, by merge.
    ///
    /// Merge, not re-execution: what lands is the commit the checks ran
    /// against, so the thing that shipped is the thing that was validated
    /// (`07-agent-safety-layer.md` §5.4).
    pub fn promote_shadow(
        &mut self,
        change_id: &ChangeId,
        author: Author,
        now_ms: i64,
    ) -> Result<MergeOutcome> {
        let record = self
            .shadows
            .get(change_id)
            .ok_or_else(|| EngineError::UnknownChange(change_id.clone()))?
            .clone();

        let diff = self
            .pending
            .get(change_id)
            .map(|p| p.diff.clone())
            .ok_or_else(|| EngineError::UnknownChange(change_id.clone()))?;

        let Some(outcome) = &record.outcome else {
            return Err(EngineError::Gated {
                reason: "nothing has been validated on this shadow branch yet".into(),
                diff: Box::new(diff),
            });
        };

        if !outcome.passed {
            return Err(EngineError::Gated {
                reason: format!("cannot promote: {}", outcome.summary()),
                diff: Box::new(diff),
            });
        }

        let shadow_head = self.log.head(record.shadow).unwrap_or(ContentHash::ZERO);
        if !record.is_current(shadow_head) {
            // Something was written to the shadow branch after the checks ran.
            // Promoting now would merge content nothing verified, which is the
            // same failure as promoting unvalidated, only harder to notice.
            return Err(EngineError::Gated {
                reason: "the shadow branch moved after it was validated; validate it again".into(),
                diff: Box::new(diff),
            });
        }

        let merged = self.merge(record.shadow, record.target, author.clone(), now_ms)?;

        if matches!(merged, MergeOutcome::Merged { .. } | MergeOutcome::UpToDate) {
            self.record_audit(AuditEntry::for_promotion(
                &change_id.0,
                &self.branch_name(record.target),
                author,
                now_ms,
            ));
            // The branch has done its job. Its commits stay in the log — this
            // drops a pointer, never history.
            self.release_shadow(change_id, Settlement::Approved, now_ms);
        }
        Ok(merged)
    }

    /// Discard a branch by name.
    ///
    /// Refuses a protected branch, and refuses a shadow branch: a shadow's
    /// lifecycle belongs to the proposal it carries, so discarding one out from
    /// under a pending review would leave a proposal that can never be answered
    /// (reject it instead, which is the same act with a reason attached).
    ///
    /// The commits stay in the log — this drops a pointer, never history.
    pub fn discard_branch(&mut self, name: &str, author: Author, now_ms: i64) -> Result<()> {
        let branch = self
            .branches
            .by_name(name)
            .ok_or_else(|| EngineError::BadRequest {
                detail: format!("no branch named `{name}`"),
            })?;
        let (id, kind, protected) = (branch.id, branch.kind, branch.is_protected());

        if protected {
            return Err(EngineError::BadRequest {
                detail: format!("`{name}` is a protected branch and cannot be discarded"),
            });
        }
        if matches!(kind, BranchKind::Shadow) {
            return Err(EngineError::BadRequest {
                detail: format!(
                    "`{name}` is a shadow branch; reject or promote the change it carries \
                     rather than discarding it"
                ),
            });
        }

        self.record_audit(AuditEntry::for_branch_discard(name, author, now_ms));
        self.views.remove(&id);
        self.branches.discard(id)?;
        Ok(())
    }

    /// Abandon a proposal and reclaim its shadow branch.
    ///
    /// The counterpart to promotion, and the reason a reviewer can say no
    /// without leaving a branch behind. Rejecting a change that was never
    /// validated is fine — most rejections happen exactly there.
    pub fn reject_shadow(
        &mut self,
        change_id: &ChangeId,
        reason: &str,
        author: Author,
        now_ms: i64,
    ) -> Result<()> {
        let record = self
            .shadows
            .get(change_id)
            .ok_or_else(|| EngineError::UnknownChange(change_id.clone()))?
            .clone();

        self.record_audit(AuditEntry::for_rejection(
            &change_id.0,
            &self.branch_name(record.target),
            reason,
            author,
            now_ms,
        ));
        self.release_shadow(change_id, Settlement::Rejected, now_ms);
        Ok(())
    }

    /// Reclaim shadow branches and proposals whose deadline has passed.
    ///
    /// Returns the branches that were reclaimed, so a caller can log them
    /// rather than have them disappear silently. Expired proposals with no
    /// shadow branch are pruned quietly — nothing was created for them.
    ///
    /// This can only ever cost someone a re-proposal: an expired change is one
    /// that did *not* land. There is no direction in which collecting too
    /// eagerly lets an unreviewed change through, which is why it is safe to
    /// run unattended.
    pub fn collect_expired(&mut self, now_ms: i64) -> Vec<ReclaimedShadow> {
        let reclaimed = self.collect_expired_shadows(now_ms);

        // Proposals that never got an answer. Without this an agent proposing
        // steadily and confirming nothing grows `pending` for the life of the
        // process.
        //
        // Their review reservations are settled as expired, which refunds them.
        //
        // **One deadline, not two.** This used `shadow_ttl_ms` (a day) while the
        // review budget expires its own reservations at `proposal_ttl_ms` (four
        // hours), so the budget's deadline always fired first and this call was
        // dead code that logged a warning about a reservation the budget had
        // already released. Integration found it; a planted violation removing
        // this loop entirely changed nothing, which is what dead code looks like
        // from the outside.
        //
        // A proposal's lifetime is now one number. Taking the shorter of the two
        // rather than the engine's own, because the budget cannot hold a
        // reservation past its TTL and a proposal outliving its reservation is a
        // proposal nobody can answer.
        let ttl = (self.policy().shadow_ttl_ms as i64).min(self.review.policy().proposal_ttl_ms);
        let lapsed: Vec<ChangeId> = self
            .pending
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.proposed_at_ms) >= ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for change_id in lapsed {
            self.pending.remove(&change_id);
            self.settle_review(&change_id, Settlement::Expired, now_ms);
        }

        reclaimed
    }

    fn collect_expired_shadows(&mut self, now_ms: i64) -> Vec<ReclaimedShadow> {
        let ttl = self.policy().shadow_ttl_ms as i64;
        let expired: Vec<ChangeId> = self
            .shadows
            .values()
            .filter(|record| now_ms.saturating_sub(record.opened_at_ms) >= ttl)
            .map(|record| record.change_id.clone())
            .collect();

        expired
            .iter()
            .filter_map(|change_id| {
                let record = self.shadows.get(change_id)?.clone();
                let reclaimed = ReclaimedShadow {
                    change_id: change_id.clone(),
                    branch: record.shadow,
                    age_ms: now_ms.saturating_sub(record.opened_at_ms),
                    was_validated: record.outcome.is_some(),
                };
                self.record_audit(AuditEntry::for_expiry(
                    &change_id.0,
                    &self.branch_name(record.target),
                    reclaimed.age_ms,
                    now_ms,
                ));
                // Nobody reviewed it, so the budget it held comes back. This is
                // the case `specs/07` §6.1.4 refunds: attention that was never
                // spent should not be charged.
                self.release_shadow(change_id, Settlement::Expired, now_ms);
                Some(reclaimed)
            })
            .collect()
    }

    /// Shadow branches currently open, oldest first.
    pub fn open_shadows(&self) -> Vec<&ShadowValidation> {
        let mut open: Vec<&ShadowValidation> = self.shadows.values().collect();
        open.sort_by_key(|record| (record.opened_at_ms, record.shadow.0));
        open
    }

    /// Drop a shadow branch and everything tracking it.
    ///
    /// The pending proposal goes too. A proposal whose shadow branch is gone
    /// cannot be promoted, and leaving it behind would let a caller confirm
    /// against a change whose validation no longer exists.
    ///
    /// Takes the settlement rather than inferring it: promotion and rejection
    /// both land here, and they settle the review budget differently. A rejected
    /// proposal keeps its spend, because saying no takes as long as saying yes
    /// and refunding rejections would make bad proposals free to generate.
    fn release_shadow(&mut self, change_id: &ChangeId, settlement: Settlement, now_ms: i64) {
        let Some(record) = self.shadows.remove(change_id) else {
            return;
        };
        self.shadow_changes.remove(change_id);
        self.pending.remove(change_id);
        self.settle_review(change_id, settlement, now_ms);
        self.views.remove(&record.shadow);
        // Discarding the pointer leaves the commits in the log: the branch is
        // ephemeral, its history is not (`03-data-model-consistency.md` §2.1).
        let _ = self.branches.discard(record.shadow);
    }

    fn branch_name(&self, branch: BranchId) -> String {
        self.branches
            .get(branch)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| branch.0.to_string())
    }

    pub fn create_branch(
        &mut self,
        name: &str,
        from: BranchId,
        author: Author,
        now_ms: i64,
    ) -> Result<BranchId> {
        let head = self.log.head(from).unwrap_or(ContentHash::ZERO);
        let id = self.branches.create(name, head, BranchKind::Standard)?;
        self.log.set_head(id, head);
        // Logged, so the branch survives a restart and shows up in the audit
        // trail with its author like any other change.
        self.append(
            id,
            OpType::BranchCreate {
                name: name.to_string(),
                from: head,
            },
            author,
            now_ms,
        )?;
        Ok(id)
    }

    /// Merge `source` into `target`.
    ///
    /// A conflict is returned, never resolved: the outcome type carries no
    /// applicable operations in its conflicted variant
    /// (`03-data-model-consistency.md` §2.4).
    pub fn merge(
        &mut self,
        source: BranchId,
        target: BranchId,
        author: Author,
        now_ms: i64,
    ) -> Result<MergeOutcome> {
        let base = self.branches.get(source).and_then(|b| b.fork_point);
        let empty = theta_storage::MaterializedView::new();
        let target_view = self.views.get(&target).unwrap_or(&empty);
        let source_view = self.views.get(&source).unwrap_or(&empty);

        let outcome = merge::merge(&self.log, source, target, base, target_view, source_view)?;

        let MergeOutcome::Merged {
            schema,
            ops,
            crdt,
            converged,
        } = &outcome
        else {
            return Ok(outcome);
        };

        // Record the merge, then apply what it produced. Plain assignments go
        // through the log as ordinary ops; merged CRDT states are installed,
        // because replaying CRDT operations is not idempotent.
        let source_head = self.log.head(source).unwrap_or(ContentHash::ZERO);

        // Schema first, in the order the source made the changes. A migration
        // that travelled with the branch has to land before the values it
        // reshapes, exactly as it did on the branch it came from.
        for change in schema {
            self.append(
                target,
                OpType::Schema {
                    change: change.clone(),
                },
                author.clone(),
                now_ms,
            )?;
        }

        if !ops.is_empty() {
            self.append(
                target,
                OpType::Transaction { ops: ops.clone() },
                author.clone(),
                now_ms,
            )?;
        }
        self.append(
            target,
            OpType::Merge {
                source,
                source_head,
            },
            author,
            now_ms,
        )?;
        if let Some(view) = self.views.get_mut(&target) {
            // No `make_mut`: the map is persistent, so this shares its nodes
            // with whichever branch the view was forked from and path-copies
            // only what the extend touches (M10.6).
            view.crdts.extend(crdt.clone());
        }

        tracing::info!(?converged, "merge applied");
        Ok(outcome)
    }

    /// Delete a key. Gated by the breaker like any other write.
    pub fn delete(
        &mut self,
        branch: BranchId,
        key: &str,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        self.check_breaker(1, now_ms, &author)?;
        self.append(
            branch,
            OpType::Delete {
                key: key.to_string(),
            },
            author,
            now_ms,
        )
    }

    // ---- internals --------------------------------------------------------

    /// Reject a write whose value contradicts the field's declared type. Never
    /// coerce (`03-data-model-consistency.md` §2.2).
    /// Reject a write whose row contradicts the table's declared schema.
    ///
    /// Never coerce (`03-data-model-consistency.md` §2.2). Keys address rows as
    /// `<table>:<primary key>` and the value *is* the row, so the check is
    /// per-column against the table definition — see `theta_core::address`.
    fn check_type(&self, branch: BranchId, key: &str, value: &Value) -> Result<()> {
        let Some(view) = self.views.get(&branch) else {
            return Ok(());
        };
        // A key with no table addresses no row, so there is no schema to check
        // it against. That is legal: `get`/`put` do not require a schema.
        let Some(address) = RowAddress::parse(key) else {
            return Ok(());
        };

        address::check_row(&view.schema, address.table, value).map_err(|e| {
            EngineError::TypeMismatch {
                field: format!("{}.{}", e.table, e.column),
                declared: e.declared.to_string(),
                actual: e.actual.to_string(),
            }
        })
    }

    /// Refuse a write that would put two rows on one unique index.
    ///
    /// # Why this was missing, and why it matters
    ///
    /// `IndexDef::unique` has been in the schema since schemas existed and
    /// nothing on the write path ever read it. A caller who declared a unique
    /// index got a flag in a document and no enforcement — which is worse than
    /// having no such flag, because the natural reading of a declared
    /// constraint is that something is checking it. The failure it permits is
    /// the one nobody notices until an invoice goes out twice.
    ///
    /// # What it costs
    ///
    /// A scan of the table per write to a table that has a unique index, and
    /// nothing at all for a table that has none. That is honest rather than
    /// clever: there is no index structure behind `IndexDef` yet, so the only
    /// way to know whether a value is already present is to look.
    ///
    /// A table with a unique index and a large number of rows will therefore
    /// write slowly, and that is the right trade to make in this direction —
    /// silently accepting a duplicate is not a faster correct answer, it is a
    /// wrong one. When an index structure exists this becomes a lookup and the
    /// signature does not change.
    fn check_unique(&self, branch: BranchId, key: &str, value: &Value) -> Result<()> {
        let Some(view) = self.views.get(&branch) else {
            return Ok(());
        };
        let Some(address) = RowAddress::parse(key) else {
            return Ok(());
        };
        let Some(table) = view.schema.tables.get(address.table) else {
            return Ok(());
        };

        let unique: Vec<&theta_core::schema::IndexDef> =
            table.indexes.iter().filter(|i| i.unique).collect();
        if unique.is_empty() {
            return Ok(());
        }

        for index in unique {
            // The tuple this row would occupy. A column the row does not carry
            // makes the tuple incomplete, and an incomplete tuple collides with
            // nothing — the same rule SQL uses for a null in a unique index.
            let Some(incoming) = index
                .columns
                .iter()
                .map(|c| theta_core::address::column(value, address.primary_key, c))
                .collect::<Option<Vec<Value>>>()
            else {
                continue;
            };

            // `RowSource::scan` rather than the view's own iterator: it is the
            // one that knows how a table's rows are addressed.
            for (existing_key, existing) in theta_core::RowSource::scan(view, address.table) {
                // A row does not collide with itself. This is what makes an
                // update to an existing row legal rather than a violation of
                // the constraint it already satisfies.
                if existing_key.as_str() == address.primary_key {
                    continue;
                }
                let held: Option<Vec<Value>> = index
                    .columns
                    .iter()
                    .map(|c| theta_core::address::column(&existing, &existing_key, c))
                    .collect();
                if held.as_ref() == Some(&incoming) {
                    return Err(EngineError::UniqueViolation {
                        index: index.name.clone(),
                        table: address.table.to_string(),
                        columns: index.columns.join(", "),
                        conflicting_key: existing_key,
                    });
                }
            }
        }

        Ok(())
    }

    /// Write one entry to the trail.
    ///
    /// A failure here is logged and does not fail the operation. The trail
    /// matters, but losing the ability to *record* that a change was gated is
    /// not a reason to stop gating changes — and the log itself, which is the
    /// primary record, has already taken the entry
    /// (`04-threat-model-security.md` §5).
    fn record_audit(&mut self, entry: AuditEntry) {
        if let Err(e) = self.audit.append(entry) {
            tracing::error!(error = %e, "could not write the audit trail");
        }
    }

    fn check_breaker(&mut self, rows: u64, now_ms: i64, author: &Author) -> Result<()> {
        let decision = self.breaker.record(rows, now_ms.max(0) as u64);

        // Counted whether or not the breaker lets it through: a write the
        // breaker refused still cost something to evaluate, and a runaway loop
        // that shows as zero usage is a runaway loop nobody gets billed for
        // noticing.
        self.rows_written = self.rows_written.saturating_add(rows);

        if let BreakerDecision::Trip { ref reason, .. } = decision {
            if let Some(entry) = AuditEntry::for_breaker(&decision, author.clone(), now_ms) {
                self.record_audit(entry);
            }
            return Err(EngineError::BreakerOpen {
                reason: reason.clone(),
            });
        }
        Ok(())
    }

    fn append(
        &mut self,
        branch: BranchId,
        op: OpType,
        author: Author,
        now_ms: i64,
    ) -> Result<ContentHash> {
        let prev_hash = self.log.head(branch).unwrap_or(ContentHash::ZERO);
        let commit_id = self
            .branches
            .get(branch)
            .map(|b| b.next_commit)
            .unwrap_or(theta_core::CommitId(0));

        let entry = LogEntry {
            prev_hash,
            commit_id,
            branch_id: branch,
            op,
            author,
            timestamp_ms: now_ms,
        };
        // Durable and visible in one step. `append_and_apply` folds into the
        // entry's own branch view and seeds a new branch from its parent, so the
        // live path and the recovery path cannot drift apart.
        let hash = self.log.append_and_apply(entry, &mut self.views)?;
        self.branches.set_head(branch, hash)?;
        Ok(hash)
    }
}
