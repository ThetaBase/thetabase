# ThetaBase wire protocol.
#
# Source of truth for every generated SDK binding (02-api-wire-protocol.md §3),
# so a change here is a change to all language bindings at once. Follow the
# additive-by-default rule in §5: new fields are appended with new ordinals and
# made optional, existing ordinals are never reused or reordered.
#
# Regenerate bindings: `make proto`.
#
# ## Why requests are structs, not a capnp interface
#
# Spec §2 sketched this surface as `interface ThetaRpc`, and spec §1 fixes the
# framing as `[u32 length][message]`. Those cannot both hold: a capnp `interface`
# is served by capnp-rpc, which carries its own multi-segment framing, a
# four-way handshake and a capability table. Implementing the interface would
# mean abandoning the stated framing.
#
# The framing wins, because it is the load-bearing half. It is explicit,
# testable, and what every SDK binding has to agree on byte-for-byte; the
# `interface` form was shorthand for the shape of the surface, and that shape is
# preserved exactly below. Spec §2 has been updated to match. Nothing is lost:
# capnp-rpc's promise pipelining is of no use here, because no ThetaBase call
# takes a capability returned by another.

@0xb4c3d2e1f0a99887;

struct KeyValue {
  key   @0 :Text;
  value @1 :Data;
}

struct QueryPlan {
  # Cache key if the plan is already compiled; 0 means "not precompiled".
  planHash    @0 :UInt64;
  # Typed query language / SQL-subset source, used only when planHash misses.
  rawQuery    @1 :Text;
  # Bound parameters. Never interpolated into rawQuery — see
  # 04-threat-model-security.md §4.
  contextVars @2 :List(KeyValue);
}

struct ChangeDiff {
  changeId        @0 :Text;
  destructive     @1 :Bool;
  rowsAffected    @2 :UInt64;
  reversible      @3 :Bool;
  estimatedCostMs @4 :UInt32;
  requiresConfirm @5 :Bool;
  # Populated once the change has been applied to a shadow branch. Zero means
  # no shadow branch exists yet.
  shadowBranchId  @6 :UInt64;
  # Plain-language explanation of the gate decision.
  reason          @7 :Text;
  affectedTable   @8 :Text;
  affectedColumn  @9 :Text;
  changeType      @10 :Text;
  # What must actually happen before this change may land.
  #
  # requiresConfirm cannot express the difference between "confirm and it
  # lands" and "confirmation is not sufficient", which is the distinction
  # 07-agent-safety-layer.md §4 turns on. A client that only saw the bool would
  # offer a confirmation the server is going to refuse.
  gate            @11 :Gate;
  # Why, as data rather than as prose (M19).
  #
  # `reason` above is rendered *from* this, not written beside it: structured
  # fields maintained alongside a sentence are two representations of one
  # decision, free to disagree, which is the mistake §4 already records about
  # `gate` and `requiresConfirm`.
  #
  # An agent reads these; a person reads `reason`. Parsing `reason` to recover
  # any of this is parsing English to get back something that was structured a
  # moment earlier.
  rule            @12 :GateRule;
  # The action that would let this change proceed.
  #
  # The field that makes a refusal actionable. "Blocked on row count" and
  # "blocked on irreversibility" arrive identically as `requiresConfirm: true`
  # and imply opposite next moves, and an agent that guesses wrong retries the
  # same rejected thing forever.
  remedy          @13 :Remedy;
  # The threshold the rule compared against, in rows. Zero when the rule read no
  # threshold. Sent because "you are over the limit" without the limit is a
  # refusal a caller cannot act on.
  ruleThreshold   @14 :UInt64;
}

# Which rule produced the gate. One arm per branch of the classifier's decision
# table, so a client can distinguish cases that share a gate.
enum GateRule {
  # Irreversible, destructive, and over the shadow threshold. The strongest
  # gate, and the one a confirmation cannot clear.
  irreversibleOverShadowThreshold @0;
  # Destructive by kind. One confirmation clears it.
  destructive                     @1;
  # Not destructive, but wide enough that the blast-radius rule applies
  # independently of type (07-agent-safety-layer.md §7).
  overRowImpactThreshold          @2;
  # Matched a narrowly-scoped project auto-approval.
  autoApprovedByPolicy            @3;
  # Nothing applied. Non-destructive and inside every threshold.
  withinThresholds                @4;
}

enum Remedy {
  # Nothing to do: the change applied.
  none                   @0;
  confirm                @1;
  # Confirmation is not sufficient. Retrying with one will fail again.
  validateOnShadowBranch @2;
  # Narrow the change so it touches fewer rows. Never sent for an irreversible
  # change, where a smaller drop is still a drop and an agent following the
  # advice loops forever.
  reduceBlastRadius      @3;
}

enum Gate {
  autoApply      @0;
  confirm        @1;
  shadowValidate @2;
}

struct ConflictRef {
  key    @0 :Text;
  # Both sides of the conflict, as canonical JSON, for human resolution.
  # Never auto-resolved.
  ours   @1 :Text;
  theirs @2 :Text;
  reason @3 :Text;
}

struct MergeResult {
  enum Status {
    ok       @0;
    conflict @1;
    blocked  @2;
    upToDate @3;
  }
  status        @0 :Status;
  conflictCount @1 :UInt32;
  conflicts     @2 :List(ConflictRef);
  # Fields reconciled by CRDT convergence rather than by taking a side.
  converged     @3 :List(Text);
}

struct ProjectStatus {
  projectId             @0 :Text;
  branch                @1 :Text;
  writeVolumeMB         @2 :Float32;
  circuitBreakerTripped @3 :Bool;
  replicaRegions        @4 :List(Text);
  # Rows accumulated in the breaker's current rolling window.
  breakerWindowRows     @5 :UInt64;
  protocolVersion       @6 :UInt32;
  commitsApplied        @7 :UInt64;
  # Bytes this project's log occupies on disk, and whether that was measurable.
  #
  # The flag is not decoration: a billing surface has to tell "nobody measured"
  # from "zero", because rendering an unmeasured figure as zero tells a customer
  # they are using nothing (06-provisioning-identity-flow.md §5).
  storageBytes          @8 :UInt64;
  hasStorageBytes       @9 :Bool;
  # Rows written since this instance started, which is what a usage period is
  # metered on.
  rowsWritten           @10 :UInt64;
  # Milliseconds since this instance last served a request.
  #
  # Reported as a *duration* rather than a timestamp so the Control Plane never
  # has to reconcile its clock with the instance's. An absolute time would make
  # hibernation depend on two machines agreeing, and a few seconds of skew
  # decides whether a project stays up.
  #
  # On an instance that has served nothing, this runs from when it started, not
  # from zero. An instance woken by a request that then went away is exactly the
  # thing hibernation exists to stop paying for, and reporting a permanent zero
  # would keep it running forever.
  idleMs                @11 :UInt64;
}

# ---- handshake --------------------------------------------------------------

struct Hello {
  # Highest protocol version the client speaks.
  protocolVersion @0 :UInt32;
  # Scoped session token. Never a raw project credential
  # (04-threat-model-security.md §2).
  sessionToken    @1 :Text;
  clientName      @2 :Text;
}

struct Welcome {
  # The version both sides agreed on. An incompatible pairing is refused with
  # an error, never negotiated down to a guess (02-api-wire-protocol.md §5).
  protocolVersion @0 :UInt32;
  projectId       @1 :Text;
  serverName      @2 :Text;
}

# ---- request / response -----------------------------------------------------

struct Request {
  # Correlates a response with its request. Responses may arrive out of order,
  # so a client can have several calls in flight on one connection.
  requestId @0 :UInt64;
  # Branch to operate on. Zero is `main`.
  branchId  @1 :UInt64;

  body :union {
    get                 @2 :GetRequest;
    put                 @3 :PutRequest;
    delete              @4 :DeleteRequest;
    query               @5 :QueryRequest;
    explain             @6 :QueryRequest;
    proposeSchemaChange @7 :ProposeRequest;
    applySchemaChange   @8 :ApplyRequest;
    createBranch        @9 :BranchRequest;
    merge               @10 :MergeRequest;
    status              @11 :StatusRequest;
    # Control Plane only: push a signed revocation list. Signed with the
    # project key, so it needs no separate authentication and an instance can
    # verify it with the public key it already holds.
    pushRevocations     @12 :PushRevocationsRequest;
    # ---- review surface (07-agent-safety-layer.md §5, §7) ----
    audit               @13 :AuditRequest;
    listBranches        @14 :ListBranchesRequest;
    discardBranch       @15 :DiscardBranchRequest;
    # A proposal's current state: the diff, and what validating it found.
    showChange          @16 :ChangeRequest;
    # Land a validated change by merging its shadow branch. Distinct from
    # applySchemaChange, which confirmation drives and which refuses a change
    # at the shadowValidate gate outright.
    promoteChange       @17 :ChangeRequest;
    rejectChange        @18 :RejectRequest;
    # Control Plane only. Carries a policy signed with the project key.
    #
    # The policy decides what the Safety Layer lets through, so it arrives the
    # same way a revocation list does: as signed bytes an instance verifies with
    # the public keyset it already holds. A session token cannot produce that
    # signature, so no agent request can raise its own ceiling
    # (07-agent-safety-layer.md §7).
    pushPolicy          @19 :PushPolicyRequest;
    # A write with a precondition on the row's current version (M10.5).
    #
    # A separate request rather than optional fields on `put`, so a server that
    # predates it refuses the call outright instead of accepting the write and
    # silently ignoring the condition. A precondition that can be dropped in
    # transit is worse than none: the caller believes they have protection they
    # do not.
    putIf               @20 :PutIfRequest;
    # What is in here, and where it came from (M19).
    #
    # A first-class call rather than something a caller assembles from `query`
    # and `audit`: an agent arriving at an unfamiliar database asks this first,
    # and every step it has to take before it can ask is a step it can get
    # wrong.
    describe            @21 :DescribeRequest;
    # A mutation of a CRDT-typed row (01-system-architecture.md §3.3).
    #
    # A separate request from `put`, not a field on it. A CRDT-typed row
    # converges by replaying *operations*; a caller reaching it through `put`
    # would be writing an absolute value, and two absolute values cannot be
    # reconciled without choosing one — which is the thing the field exists not
    # to do.
    crdt                @22 :CrdtRequest;
    # A write the caller signed with its session key
    # (04-threat-model-security.md 7.2).
    #
    # A separate request rather than optional fields on `put`, for the same
    # reason `putIf` is separate: a signature that can be dropped in transit is
    # worse than none, because the caller believes the record is checkable and
    # it is not. A server that predates this refuses the call outright.
    signedWrite         @23 :SignedWriteRequest;
    # What is waiting for a human, grouped so it can be answered together
    # (07-agent-safety-layer.md 6.2).
    #
    # Distinct from `audit`, which is a record of what happened. This is a list
    # of what has not happened yet, and reading one for the other is how a
    # reviewer concludes the queue is empty because nothing was logged.
    reviewQueue         @24 :ReviewQueueRequest;
    # Several writes that land as one commit, or not at all (M10.5 follow-on).
    #
    # A separate request rather than a flag on a batched put, for the reason
    # `putIf` is separate from `put`: a server that predates this refuses the
    # call instead of applying the writes one at a time and leaving the caller
    # believing they were atomic. Losing atomicity in transit is worse than not
    # having it, because a partial result looks like a whole one.
    #
    # Each operation may carry its own precondition. The server checks every one
    # of them against the branch before applying any operation, so a transaction
    # that would violate a condition changes nothing at all. That is what makes
    # "record the payment and update the invoice, or neither" expressible in one
    # call.
    transaction         @25 :TransactionRequest;
  }
}

struct TransactionRequest {
  # Applied in order, as a single commit. An empty list is refused rather than
  # committed: an empty transaction is a caller bug, and committing nothing
  # while reporting success hides it.
  ops @0 :List(TransactionOp);
}

struct TransactionOp {
  key @0 :Text;
  # The precondition this operation carries, checked before *any* operation in
  # the transaction is applied.
  expect :union {
    # No condition on this row.
    any     @1 :Void;
    # The row must not exist. Create-only — this is how a caller makes a
    # uniqueness rule the storage layer enforces, by putting the unique tuple
    # in the key.
    absent  @2 :Void;
    # The row must be at exactly this version.
    version @3 :UInt64;
  }
  action :union {
    put    @4 :PutAction;
    delete @5 :Void;
  }
}

struct PutAction {
  # Canonical JSON encoding of the value.
  value @0 :Text;
  ttl   @1 :UInt64;
}

struct ReviewQueueRequest {}

struct ReviewBatchWire {
  # The grouping key, which is the table the changes are about.
  key             @0 :Text;
  # The gate the batch carries: the strongest of its members. A batch is never
  # gated below its worst change, which is what stops batching from becoming a
  # discount on risk.
  gate            @1 :UInt8;
  changes         @2 :List(ChangeDiff);
  rowsAffected    @3 :UInt64;
  cost            @4 :UInt32;
  costIfUnbatched @5 :UInt32;
  # Why the batch carries the gate it does, naming the member responsible - a
  # reviewer's next question after "this needs shadow validation" is always
  # *which one*.
  reason          @6 :Text;
}

struct ReviewQueueResponse { batches @0 :List(ReviewBatchWire); }

struct SignedWriteRequest {
  # The entry the caller built, and the server appends verbatim.
  #
  # The server does not assemble an entry on the caller's behalf and then attach
  # their signature - an entry the server composed is one the signature cannot
  # be about. So the two fields the server would otherwise choose are chosen by
  # the caller and *validated* here: `commitId` must be the branch's next, and
  # `timestampMs` must be within the accepted skew. A caller that loses the race
  # is refused and retries, exactly as `putIf` does.
  #
  # `author` is not on the wire. It is derived from the token on both sides, so
  # sending it would let a caller sign one author and be recorded as another.
  commitId    @0 :UInt64;
  timestampMs @1 :Int64;
  # Ed25519 over the entry's content hash, by the session key the token carries.
  signature   @2 :Data;

  op :union {
    put    @3 :PutRequest;
    delete @4 :DeleteRequest;
    crdt   @5 :CrdtRequest;
  }
}

struct ElemId {
  # Identity of one element of a sequence. Ordered so that concurrent inserts
  # at the same position have a deterministic tie-break.
  counter @0 :UInt64;
  replica @1 :UInt64;
}

struct SeqInsertOp {
  # The element to insert after. Absent inserts at the head of the list.
  after @0 :ElemId;
  # Canonical JSON of the value, encoded as `put` encodes one.
  value @1 :Text;
}

struct CrdtRequest {
  # The row whose value is the CRDT.
  key @0 :Text;

  # **No field here carries an element id for an insert**, and that is the
  # point rather than an omission. An RGA element's id decides both its
  # identity and its position among concurrent siblings, so a client that chose
  # its own could collide with another writer's element or displace one. The
  # server assigns it from (branch, commit) — unique by construction, and the
  # reason replaying an entry twice is idempotent. A wire type that cannot
  # express a client-chosen id cannot carry one that was not checked.
  #
  # `seqRemove` does name an id, and must: it names an element the caller read.
  mutation :union {
    # PN-Counter. Negative values decrement.
    increment   @1 :Int64;
    # LWW-Register, tie-broken by (timestamp, branch, commit).
    setRegister @2 :Text;
    setAdd      @3 :Text;
    setRemove   @4 :Text;
    seqInsert   @5 :SeqInsertOp;
    seqRemove   @6 :ElemId;
  }
}

struct DescribeRequest {
  # Empty describes every table; naming one narrows it.
  table           @0 :Text;
  # Draw example values from the branch.
  #
  # **Off by default, and that is a decision rather than an oversight.** An
  # example is customer data. A `describe` is the call an agent makes to orient
  # itself, often automatically, and one that returns rows by default pulls
  # customer data into a model's context and into whatever logs that context —
  # for a call the agent made to learn the *shape* of the data.
  #
  # It is not an authorisation boundary: a caller who can describe a table can
  # already query it, so this reveals nothing they could not fetch. It is a
  # blast-radius decision about the default.
  includeExamples @1 :Bool;
  # How many examples per column, capped by the server. Zero means the server's
  # default.
  exampleLimit    @2 :UInt32;
}

struct SchemaDescription {
  tables @0 :List(TableDescription);
  # True when the caller asked for examples and the server withheld them.
  #
  # Reported rather than silently omitted: a client that asked for examples and
  # got none should be able to tell "there were none" from "we would not give
  # them to you".
  examplesWithheld @1 :Bool;
  # Why they were withheld, when they were. Empty otherwise.
  withheldReason   @2 :Text;
}

struct TableDescription {
  name     @0 :Text;
  # Rows in this table on this branch.
  rowCount @1 :UInt64;
  columns  @2 :List(ColumnDescription);
}

struct ColumnDescription {
  name     @0 :Text;
  # The declared type: int, float, text, bool, bytes, timestamp, json.
  type     @1 :Text;
  nullable @2 :Bool;
  # The CRDT kind, when this field has one. Empty when it does not — which is
  # the difference between a field that converges on concurrent modification and
  # one that becomes a conflict a human resolves (docs/INVARIANTS.md invariant 5), and
  # therefore the single most useful thing on this struct for an agent deciding
  # whether two writes can race.
  crdt     @3 :Text;
  # The commit that established this column's canonical type, hex-encoded.
  #
  # Empty when the establishing commit is outside the visible log — a branch
  # forked after the column existed, or a segment expired by retention. Absent
  # rather than guessed: inventing one would be a confident answer about a
  # period nobody can see.
  declaredAt   @4 :Text;
  # Whether an agent has ever changed this column. The first question a person
  # asks about a column they do not recognise.
  touchedByAgent @5 :Bool;
  # Example values, as canonical JSON. Empty unless asked for.
  examples       @6 :List(Text);
  # How many of the sampled rows had no value here, in basis points. Sent even
  # without examples, because "this column is 98% null" is schema-shaped
  # information an agent needs and no individual value discloses it.
  nullBasisPoints @7 :UInt32;
}

struct PushPolicyRequest {
  # Canonical JSON of the versioned policy, signed as-is.
  payload   @0 :Data;
  signature @1 :Data;
  keyId     @2 :Text;
}

struct AuditRequest {
  limit   @0 :UInt32;
  # Entries below this risk are omitted. Mirrors theta_safety RiskLevel:
  # 0 info, 1 low, 2 medium, 3 high.
  minRisk @1 :UInt8;
}

struct ListBranchesRequest {}
struct DiscardBranchRequest { name @0 :Text; }
struct ChangeRequest { changeId @0 :Text; }
struct RejectRequest {
  changeId @0 :Text;
  reason   @1 :Text;
}

struct PushRevocationsRequest {
  # Canonical JSON of the revocation list, signed as-is.
  payload   @0 :Data;
  signature @1 :Data;
  keyId     @2 :Text;
}

struct GetRequest    { key @0 :Text; }
struct DeleteRequest { key @0 :Text; }

struct PutRequest {
  key   @0 :Text;
  # Canonical JSON encoding of the value.
  value @1 :Text;
  ttl   @2 :UInt64;
}

struct PutIfRequest {
  key   @0 :Text;
  # Canonical JSON encoding of the value.
  value @1 :Text;
  ttl   @2 :UInt64;
  expect :union {
    # The row must not exist. Create-only.
    absent  @3 :Void;
    # The row must be at exactly this version — the `versionId` a `get`
    # returned.
    version @4 :UInt64;
  }
}

# Refused because the row was not in the state the caller required.
#
# Carries what the version *actually* is, so a retry needs no second round trip
# to find out. A bare "no" would make every conflict cost two calls, and the
# contended case is exactly where the extra call hurts.
struct PreconditionFailed {
  key      @0 :Text;
  # True when the row exists but is at a different version.
  found    @1 :Bool;
  # The row's current version. Meaningless when `found` is false.
  actual   @2 :UInt64;
}

struct QueryRequest { plan @0 :QueryPlan; }

struct ProposeRequest {
  # Canonical JSON encoding of theta_core::schema::SchemaChange. Kept opaque at
  # the wire level so schema evolution does not require a protocol bump.
  change          @0 :Text;
  # There is deliberately no impact field. It used to carry the caller's row
  # count and the server used it to classify the change, so a client could name
  # zero and walk a drop past the gate that reads it. The server measures the
  # impact against the branch's own view and the answer comes back in the diff.
  # Ordinals 1 and 2 are retired and must not be reused.
}

struct ApplyRequest {
  changeId @0 :Text;
  # Retired. The ordinal stays declared so the layout is stable and an older
  # client that still sets it is ignored rather than misread; the server never
  # reads this field.
  retiredChange @1 :Text;
  confirm  @2 :Bool;
  #
  # The server applies the change it classified under this id. When the body
  # travelled with the confirmation a caller could propose `drop column`, be
  # gated on 500 rows, then confirm with a `drop table` body and drop the
  # table — the gate described one change and another one ran
  # (07-agent-safety-layer.md §4). The branch is not on this request either,
  # and for the same reason: proposing against a branch where the table was
  # empty and confirming against `main` applied main's rows under the empty
  # branch's classification.
}

struct BranchRequest {
  name @0 :Text;
  from @1 :UInt64;
}

struct MergeRequest {
  sourceBranch @0 :UInt64;
  targetBranch @1 :UInt64;
}

struct StatusRequest {}

struct Response {
  requestId @0 :UInt64;

  body :union {
    error       @1 :ErrorResponse;
    get         @2 :GetResponse;
    put         @3 :PutResponse;
    delete      @4 :PutResponse;
    query       @5 :QueryResponse;
    explain     @6 :ExplainResponse;
    propose     @7 :ChangeDiff;
    apply       @8 :PutResponse;
    branch      @9 :BranchResponse;
    merge       @10 :MergeResult;
    status      @11 :ProjectStatus;
    revocations @12 :RevocationsAccepted;
    audit       @13 :AuditResponse;
    branches    @14 :BranchListResponse;
    change      @15 :ChangeStateResponse;
    ok          @16 :OkResponse;
    policy      @17 :PolicyAccepted;
    # A conditional write whose precondition did not hold (M10.5). Not an
    # `error`: the request was well-formed and the server did exactly what was
    # asked. Conflating the two would make an ordinary lost-update retry
    # indistinguishable from a fault, in logs and in error budgets alike.
    precondition @18 :PreconditionFailed;
    description  @19 :SchemaDescription;
    reviewQueue  @20 :ReviewQueueResponse;
    # One commit id for the whole transaction. Singular deliberately: a
    # response carrying an id per operation would be describing something that
    # did not happen.
    transaction @21 :TransactionResponse;
  }
}

struct TransactionResponse { commitId @0 :Text; }

struct PolicyAccepted {
  # The version the instance now holds, so a caller can poll until every
  # instance reports the version it wrote — the same way revocation propagation
  # is made measurable.
  version  @0 :UInt64;
  accepted @1 :Bool;
}

struct OkResponse {}

struct AuditEntryWire {
  # 0 info, 1 low, 2 medium, 3 high.
  risk        @0 :UInt8;
  # The plain-language line. This is the field a human reads; `detail` exists
  # for tooling (07-agent-safety-layer.md §8).
  summary     @1 :Text;
  # Canonical JSON of theta_core::Author.
  author      @2 :Text;
  timestampMs @3 :Int64;
  # Canonical JSON of the structured detail.
  detail      @4 :Text;
}

struct AuditResponse { entries @0 :List(AuditEntryWire); }

struct BranchInfo {
  branchId  @0 :UInt64;
  name      @1 :Text;
  # Mirrors theta_core::BranchKind: 0 protected, 1 standard, 2 shadow.
  kind      @2 :UInt8;
  head      @3 :Text;
  protected @4 :Bool;
  # The commit id the next entry on this branch will carry.
  #
  # Needed to sign a write: a signed entry commits to its own position, so a
  # caller has to know the position before it can sign for it. Published rather
  # than left to be derived - a client counting the entries it has seen would be
  # wrong the moment anybody else wrote, and would sign for a position it did not
  # hold.
  #
  # It can be stale by the time the write arrives. That is handled where every
  # other lost race is: the write is refused with a conflict and the caller
  # re-signs.
  nextCommit @5 :UInt64;
}

struct BranchListResponse { branches @0 :List(BranchInfo); }

struct ValidationCheckWire {
  name    @0 :Text;
  passed  @1 :Bool;
  detail  @2 :Text;
  # Canonical JSON of the sampled rows, so a reviewer sees what changed rather
  # than only a count.
  samples @3 :Text;
}

# A proposal's current state: the diff, plus whatever validating it found.
struct ChangeStateResponse {
  diff           @0 :ChangeDiff;
  # False when this change has no shadow branch — an ordinary confirm-gated
  # proposal, or one not yet prepared.
  hasShadow      @1 :Bool;
  shadowBranchId @2 :UInt64;
  # False when nothing has been validated yet. A client must not read
  # `validationPassed` unless this is true, or it will read "not yet run" as
  # "failed" — or worse, a default `true` as a pass.
  hasValidation  @3 :Bool;
  validationPassed @4 :Bool;
  validationSummary @5 :Text;
  checks         @6 :List(ValidationCheckWire);
}

struct RevocationsAccepted {
  # The version the instance now holds. A caller polls until instances report
  # the version it revoked at, which is what makes propagation measurable.
  version  @0 :UInt64;
  accepted @1 :Bool;
}

struct ErrorResponse {
  # Mirrors theta_proto::StatusCode.
  code    @0 :UInt8;
  message @1 :Text;
  # Present when the Safety Layer refused a change, so the caller can act on
  # the diff without a second round trip.
  diff    @2 :ChangeDiff;
  hasDiff @3 :Bool;
}

struct GetResponse {
  found     @0 :Bool;
  # Canonical JSON encoding of the value.
  value     @1 :Text;
  # The commit that last wrote *this row* (M10.5).
  #
  # It used to be the branch's commit counter, which moved whenever any other
  # key was written — a field named for the row, carrying a number about the
  # branch. Anything comparing it across two reads of one key was comparing the
  # wrong thing.
  #
  # Zero when the row is absent, and `found` is what distinguishes that from a
  # row genuinely written by commit zero. Pass this to `putIf` to make a write
  # conditional on nothing having changed underneath it.
  versionId @2 :UInt64;
}

struct PutResponse {
  # Content hash of the commit, hex-encoded.
  commitId @0 :Text;
}

struct QueryResponse {
  # Arrow IPC stream format, zero-copy on the client side.
  resultSet @0 :Data;
  planHash  @1 :UInt64;
  rowCount  @2 :UInt64;
}

struct ExplainResponse {
  # Canonical JSON encoding of theta_query::Explain.
  explanation @0 :Text;
}

struct BranchResponse {
  branchId @0 :UInt64;
}
