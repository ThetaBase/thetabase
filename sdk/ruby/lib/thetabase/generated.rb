# frozen_string_literal: true
#
# Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
#
# DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
# if this file and the schema disagree.
#
# The prose explaining each field lives in the schema, which is the one place it
# can be read without a stale copy to compare against.
#
# Field names are snake_case here and camelCase on the wire. Each type carries
# the mapping in its own `wire` method rather than deriving it, because deriving
# it would mean reversing snake_case and `writeVolumeMB` does not survive that.

module ThetaBase
  module Wire

    # One of "autoApply", "confirm", "shadowValidate".
    module Gate
      AUTO_APPLY = "autoApply"
      CONFIRM = "confirm"
      SHADOW_VALIDATE = "shadowValidate"

      ALL = ["autoApply", "confirm", "shadowValidate"].freeze
    end

    # One of "irreversibleOverShadowThreshold", "destructive", "overRowImpactThreshold", "autoApprovedByPolicy", "withinThresholds".
    module GateRule
      IRREVERSIBLE_OVER_SHADOW_THRESHOLD = "irreversibleOverShadowThreshold"
      DESTRUCTIVE = "destructive"
      OVER_ROW_IMPACT_THRESHOLD = "overRowImpactThreshold"
      AUTO_APPROVED_BY_POLICY = "autoApprovedByPolicy"
      WITHIN_THRESHOLDS = "withinThresholds"

      ALL = ["irreversibleOverShadowThreshold", "destructive", "overRowImpactThreshold", "autoApprovedByPolicy", "withinThresholds"].freeze
    end

    # One of "none", "confirm", "validateOnShadowBranch", "reduceBlastRadius".
    module Remedy
      NONE = "none"
      CONFIRM = "confirm"
      VALIDATE_ON_SHADOW_BRANCH = "validateOnShadowBranch"
      REDUCE_BLAST_RADIUS = "reduceBlastRadius"

      ALL = ["none", "confirm", "validateOnShadowBranch", "reduceBlastRadius"].freeze
    end

    # One of "ok", "conflict", "blocked", "upToDate".
    module Status
      OK = "ok"
      CONFLICT = "conflict"
      BLOCKED = "blocked"
      UP_TO_DATE = "upToDate"

      ALL = ["ok", "conflict", "blocked", "upToDate"].freeze
    end

    # Fields:
    #   change_id : String
    #   retired_change : String
    #   confirm : Boolean
    ApplyRequest = Data.define(:change_id, :retired_change, :confirm) do
      def self.wire
        {
          change_id: "changeId",
          retired_change: "retiredChange",
          confirm: "confirm",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   risk : Integer
    #   summary : String
    #   author : String
    #   timestamp_ms : Integer
    #   detail : String
    AuditEntryWire = Data.define(:risk, :summary, :author, :timestamp_ms, :detail) do
      def self.wire
        {
          risk: "risk",
          summary: "summary",
          author: "author",
          timestamp_ms: "timestampMs",
          detail: "detail",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   limit : Integer
    #   min_risk : Integer
    AuditRequest = Data.define(:limit, :min_risk) do
      def self.wire
        {
          limit: "limit",
          min_risk: "minRisk",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   entries : Array<AuditEntryWire>
    AuditResponse = Data.define(:entries) do
      def self.wire
        {
          entries: "entries",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   branch_id : Integer
    #   name : String
    #   kind : Integer
    #   head : String
    #   protected : Boolean
    #   next_commit : Integer
    BranchInfo = Data.define(:branch_id, :name, :kind, :head, :protected, :next_commit) do
      def self.wire
        {
          branch_id: "branchId",
          name: "name",
          kind: "kind",
          head: "head",
          protected: "protected",
          next_commit: "nextCommit",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   branches : Array<BranchInfo>
    BranchListResponse = Data.define(:branches) do
      def self.wire
        {
          branches: "branches",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   name : String
    #   from : Integer
    BranchRequest = Data.define(:name, :from) do
      def self.wire
        {
          name: "name",
          from: "from",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   branch_id : Integer
    BranchResponse = Data.define(:branch_id) do
      def self.wire
        {
          branch_id: "branchId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   change_id : String
    #   destructive : Boolean
    #   rows_affected : Integer
    #   reversible : Boolean
    #   estimated_cost_ms : Integer
    #   requires_confirm : Boolean
    #   shadow_branch_id : Integer
    #   reason : String
    #   affected_table : String
    #   affected_column : String
    #   change_type : String
    #   gate : Gate
    #   rule : GateRule
    #   remedy : Remedy
    #   rule_threshold : Integer
    ChangeDiff = Data.define(:change_id, :destructive, :rows_affected, :reversible, :estimated_cost_ms, :requires_confirm, :shadow_branch_id, :reason, :affected_table, :affected_column, :change_type, :gate, :rule, :remedy, :rule_threshold) do
      def self.wire
        {
          change_id: "changeId",
          destructive: "destructive",
          rows_affected: "rowsAffected",
          reversible: "reversible",
          estimated_cost_ms: "estimatedCostMs",
          requires_confirm: "requiresConfirm",
          shadow_branch_id: "shadowBranchId",
          reason: "reason",
          affected_table: "affectedTable",
          affected_column: "affectedColumn",
          change_type: "changeType",
          gate: "gate",
          rule: "rule",
          remedy: "remedy",
          rule_threshold: "ruleThreshold",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   change_id : String
    ChangeRequest = Data.define(:change_id) do
      def self.wire
        {
          change_id: "changeId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   diff : ChangeDiff
    #   has_shadow : Boolean
    #   shadow_branch_id : Integer
    #   has_validation : Boolean
    #   validation_passed : Boolean
    #   validation_summary : String
    #   checks : Array<ValidationCheckWire>
    ChangeStateResponse = Data.define(:diff, :has_shadow, :shadow_branch_id, :has_validation, :validation_passed, :validation_summary, :checks) do
      def self.wire
        {
          diff: "diff",
          has_shadow: "hasShadow",
          shadow_branch_id: "shadowBranchId",
          has_validation: "hasValidation",
          validation_passed: "validationPassed",
          validation_summary: "validationSummary",
          checks: "checks",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   name : String
    #   type : String
    #   nullable : Boolean
    #   crdt : String
    #   declared_at : String
    #   touched_by_agent : Boolean
    #   examples : Array<String>
    #   null_basis_points : Integer
    ColumnDescription = Data.define(:name, :type, :nullable, :crdt, :declared_at, :touched_by_agent, :examples, :null_basis_points) do
      def self.wire
        {
          name: "name",
          type: "type",
          nullable: "nullable",
          crdt: "crdt",
          declared_at: "declaredAt",
          touched_by_agent: "touchedByAgent",
          examples: "examples",
          null_basis_points: "nullBasisPoints",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    #   ours : String
    #   theirs : String
    #   reason : String
    ConflictRef = Data.define(:key, :ours, :theirs, :reason) do
      def self.wire
        {
          key: "key",
          ours: "ours",
          theirs: "theirs",
          reason: "reason",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # `CrdtRequest`'s union: exactly one arm is present, and `kind` says which.
    module CrdtRequestBodyKind
      INCREMENT = "increment"
      SET_REGISTER = "setRegister"
      SET_ADD = "setAdd"
      SET_REMOVE = "setRemove"
      SEQ_INSERT = "seqInsert"
      SEQ_REMOVE = "seqRemove"

      ALL = ["increment", "setRegister", "setAdd", "setRemove", "seqInsert", "seqRemove"].freeze
    end

    CrdtRequestBody = Data.define(:kind, :value) do
      def to_wire = { "kind" => kind, "value" => value }
      def self.from_wire(hash) = new(kind: hash["kind"], value: hash["value"])
    end

    # Fields:
    #   key : String
    #   mutation : CrdtRequestBody
    CrdtRequest = Data.define(:key, :mutation) do
      def self.wire
        {
          key: "key",
          mutation: "mutation",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    DeleteRequest = Data.define(:key) do
      def self.wire
        {
          key: "key",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   table : String
    #   include_examples : Boolean
    #   example_limit : Integer
    DescribeRequest = Data.define(:table, :include_examples, :example_limit) do
      def self.wire
        {
          table: "table",
          include_examples: "includeExamples",
          example_limit: "exampleLimit",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   name : String
    DiscardBranchRequest = Data.define(:name) do
      def self.wire
        {
          name: "name",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   counter : Integer
    #   replica : Integer
    ElemId = Data.define(:counter, :replica) do
      def self.wire
        {
          counter: "counter",
          replica: "replica",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   code : Integer
    #   message : String
    #   diff : ChangeDiff
    #   has_diff : Boolean
    ErrorResponse = Data.define(:code, :message, :diff, :has_diff) do
      def self.wire
        {
          code: "code",
          message: "message",
          diff: "diff",
          has_diff: "hasDiff",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   explanation : String
    ExplainResponse = Data.define(:explanation) do
      def self.wire
        {
          explanation: "explanation",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    GetRequest = Data.define(:key) do
      def self.wire
        {
          key: "key",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   found : Boolean
    #   value : String
    #   version_id : Integer
    GetResponse = Data.define(:found, :value, :version_id) do
      def self.wire
        {
          found: "found",
          value: "value",
          version_id: "versionId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   protocol_version : Integer
    #   session_token : String
    #   client_name : String
    Hello = Data.define(:protocol_version, :session_token, :client_name) do
      def self.wire
        {
          protocol_version: "protocolVersion",
          session_token: "sessionToken",
          client_name: "clientName",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    #   value : String (bytes)
    KeyValue = Data.define(:key, :value) do
      def self.wire
        {
          key: "key",
          value: "value",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # No fields.
    ListBranchesRequest = Data.define() do
      def self.wire
        {}.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   source_branch : Integer
    #   target_branch : Integer
    MergeRequest = Data.define(:source_branch, :target_branch) do
      def self.wire
        {
          source_branch: "sourceBranch",
          target_branch: "targetBranch",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   status : Status
    #   conflict_count : Integer
    #   conflicts : Array<ConflictRef>
    #   converged : Array<String>
    MergeResult = Data.define(:status, :conflict_count, :conflicts, :converged) do
      def self.wire
        {
          status: "status",
          conflict_count: "conflictCount",
          conflicts: "conflicts",
          converged: "converged",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # No fields.
    OkResponse = Data.define() do
      def self.wire
        {}.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   version : Integer
    #   accepted : Boolean
    PolicyAccepted = Data.define(:version, :accepted) do
      def self.wire
        {
          version: "version",
          accepted: "accepted",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    #   found : Boolean
    #   actual : Integer
    PreconditionFailed = Data.define(:key, :found, :actual) do
      def self.wire
        {
          key: "key",
          found: "found",
          actual: "actual",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   project_id : String
    #   branch : String
    #   write_volume_m_b : Float
    #   circuit_breaker_tripped : Boolean
    #   replica_regions : Array<String>
    #   breaker_window_rows : Integer
    #   protocol_version : Integer
    #   commits_applied : Integer
    #   storage_bytes : Integer
    #   has_storage_bytes : Boolean
    #   rows_written : Integer
    ProjectStatus = Data.define(:project_id, :branch, :write_volume_m_b, :circuit_breaker_tripped, :replica_regions, :breaker_window_rows, :protocol_version, :commits_applied, :storage_bytes, :has_storage_bytes, :rows_written) do
      def self.wire
        {
          project_id: "projectId",
          branch: "branch",
          write_volume_m_b: "writeVolumeMB",
          circuit_breaker_tripped: "circuitBreakerTripped",
          replica_regions: "replicaRegions",
          breaker_window_rows: "breakerWindowRows",
          protocol_version: "protocolVersion",
          commits_applied: "commitsApplied",
          storage_bytes: "storageBytes",
          has_storage_bytes: "hasStorageBytes",
          rows_written: "rowsWritten",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   change : String
    ProposeRequest = Data.define(:change) do
      def self.wire
        {
          change: "change",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   payload : String (bytes)
    #   signature : String (bytes)
    #   key_id : String
    PushPolicyRequest = Data.define(:payload, :signature, :key_id) do
      def self.wire
        {
          payload: "payload",
          signature: "signature",
          key_id: "keyId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   payload : String (bytes)
    #   signature : String (bytes)
    #   key_id : String
    PushRevocationsRequest = Data.define(:payload, :signature, :key_id) do
      def self.wire
        {
          payload: "payload",
          signature: "signature",
          key_id: "keyId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # `PutIfRequest`'s union: exactly one arm is present, and `kind` says which.
    module PutIfRequestBodyKind
      ABSENT = "absent"
      VERSION = "version"

      ALL = ["absent", "version"].freeze
    end

    PutIfRequestBody = Data.define(:kind, :value) do
      def to_wire = { "kind" => kind, "value" => value }
      def self.from_wire(hash) = new(kind: hash["kind"], value: hash["value"])
    end

    # Fields:
    #   key : String
    #   value : String
    #   ttl : Integer
    #   expect : PutIfRequestBody
    PutIfRequest = Data.define(:key, :value, :ttl, :expect) do
      def self.wire
        {
          key: "key",
          value: "value",
          ttl: "ttl",
          expect: "expect",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    #   value : String
    #   ttl : Integer
    PutRequest = Data.define(:key, :value, :ttl) do
      def self.wire
        {
          key: "key",
          value: "value",
          ttl: "ttl",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   commit_id : String
    PutResponse = Data.define(:commit_id) do
      def self.wire
        {
          commit_id: "commitId",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   plan_hash : Integer
    #   raw_query : String
    #   context_vars : Array<KeyValue>
    QueryPlan = Data.define(:plan_hash, :raw_query, :context_vars) do
      def self.wire
        {
          plan_hash: "planHash",
          raw_query: "rawQuery",
          context_vars: "contextVars",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   plan : QueryPlan
    QueryRequest = Data.define(:plan) do
      def self.wire
        {
          plan: "plan",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   result_set : String (bytes)
    #   plan_hash : Integer
    #   row_count : Integer
    QueryResponse = Data.define(:result_set, :plan_hash, :row_count) do
      def self.wire
        {
          result_set: "resultSet",
          plan_hash: "planHash",
          row_count: "rowCount",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   change_id : String
    #   reason : String
    RejectRequest = Data.define(:change_id, :reason) do
      def self.wire
        {
          change_id: "changeId",
          reason: "reason",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # `Request`'s union: exactly one arm is present, and `kind` says which.
    module RequestBodyKind
      GET = "get"
      PUT = "put"
      DELETE = "delete"
      QUERY = "query"
      EXPLAIN = "explain"
      PROPOSE_SCHEMA_CHANGE = "proposeSchemaChange"
      APPLY_SCHEMA_CHANGE = "applySchemaChange"
      CREATE_BRANCH = "createBranch"
      MERGE = "merge"
      STATUS = "status"
      PUSH_REVOCATIONS = "pushRevocations"
      AUDIT = "audit"
      LIST_BRANCHES = "listBranches"
      DISCARD_BRANCH = "discardBranch"
      SHOW_CHANGE = "showChange"
      PROMOTE_CHANGE = "promoteChange"
      REJECT_CHANGE = "rejectChange"
      PUSH_POLICY = "pushPolicy"
      PUT_IF = "putIf"
      DESCRIBE = "describe"
      CRDT = "crdt"
      SIGNED_WRITE = "signedWrite"
      REVIEW_QUEUE = "reviewQueue"

      ALL = ["get", "put", "delete", "query", "explain", "proposeSchemaChange", "applySchemaChange", "createBranch", "merge", "status", "pushRevocations", "audit", "listBranches", "discardBranch", "showChange", "promoteChange", "rejectChange", "pushPolicy", "putIf", "describe", "crdt", "signedWrite", "reviewQueue"].freeze
    end

    RequestBody = Data.define(:kind, :value) do
      def to_wire = { "kind" => kind, "value" => value }
      def self.from_wire(hash) = new(kind: hash["kind"], value: hash["value"])
    end

    # Fields:
    #   request_id : Integer
    #   branch_id : Integer
    #   body : RequestBody
    Request = Data.define(:request_id, :branch_id, :body) do
      def self.wire
        {
          request_id: "requestId",
          branch_id: "branchId",
          body: "body",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # `Response`'s union: exactly one arm is present, and `kind` says which.
    module ResponseBodyKind
      ERROR = "error"
      GET = "get"
      PUT = "put"
      DELETE = "delete"
      QUERY = "query"
      EXPLAIN = "explain"
      PROPOSE = "propose"
      APPLY = "apply"
      BRANCH = "branch"
      MERGE = "merge"
      STATUS = "status"
      REVOCATIONS = "revocations"
      AUDIT = "audit"
      BRANCHES = "branches"
      CHANGE = "change"
      OK = "ok"
      POLICY = "policy"
      PRECONDITION = "precondition"
      DESCRIPTION = "description"
      REVIEW_QUEUE = "reviewQueue"

      ALL = ["error", "get", "put", "delete", "query", "explain", "propose", "apply", "branch", "merge", "status", "revocations", "audit", "branches", "change", "ok", "policy", "precondition", "description", "reviewQueue"].freeze
    end

    ResponseBody = Data.define(:kind, :value) do
      def to_wire = { "kind" => kind, "value" => value }
      def self.from_wire(hash) = new(kind: hash["kind"], value: hash["value"])
    end

    # Fields:
    #   request_id : Integer
    #   body : ResponseBody
    Response = Data.define(:request_id, :body) do
      def self.wire
        {
          request_id: "requestId",
          body: "body",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   key : String
    #   gate : Integer
    #   changes : Array<ChangeDiff>
    #   rows_affected : Integer
    #   cost : Integer
    #   cost_if_unbatched : Integer
    #   reason : String
    ReviewBatchWire = Data.define(:key, :gate, :changes, :rows_affected, :cost, :cost_if_unbatched, :reason) do
      def self.wire
        {
          key: "key",
          gate: "gate",
          changes: "changes",
          rows_affected: "rowsAffected",
          cost: "cost",
          cost_if_unbatched: "costIfUnbatched",
          reason: "reason",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # No fields.
    ReviewQueueRequest = Data.define() do
      def self.wire
        {}.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   batches : Array<ReviewBatchWire>
    ReviewQueueResponse = Data.define(:batches) do
      def self.wire
        {
          batches: "batches",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   version : Integer
    #   accepted : Boolean
    RevocationsAccepted = Data.define(:version, :accepted) do
      def self.wire
        {
          version: "version",
          accepted: "accepted",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   tables : Array<TableDescription>
    #   examples_withheld : Boolean
    #   withheld_reason : String
    SchemaDescription = Data.define(:tables, :examples_withheld, :withheld_reason) do
      def self.wire
        {
          tables: "tables",
          examples_withheld: "examplesWithheld",
          withheld_reason: "withheldReason",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   after : ElemId
    #   value : String
    SeqInsertOp = Data.define(:after, :value) do
      def self.wire
        {
          after: "after",
          value: "value",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # `SignedWriteRequest`'s union: exactly one arm is present, and `kind` says which.
    module SignedWriteRequestBodyKind
      PUT = "put"
      DELETE = "delete"
      CRDT = "crdt"

      ALL = ["put", "delete", "crdt"].freeze
    end

    SignedWriteRequestBody = Data.define(:kind, :value) do
      def to_wire = { "kind" => kind, "value" => value }
      def self.from_wire(hash) = new(kind: hash["kind"], value: hash["value"])
    end

    # Fields:
    #   commit_id : Integer
    #   timestamp_ms : Integer
    #   signature : String (bytes)
    #   op : SignedWriteRequestBody
    SignedWriteRequest = Data.define(:commit_id, :timestamp_ms, :signature, :op) do
      def self.wire
        {
          commit_id: "commitId",
          timestamp_ms: "timestampMs",
          signature: "signature",
          op: "op",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # No fields.
    StatusRequest = Data.define() do
      def self.wire
        {}.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   name : String
    #   row_count : Integer
    #   columns : Array<ColumnDescription>
    TableDescription = Data.define(:name, :row_count, :columns) do
      def self.wire
        {
          name: "name",
          row_count: "rowCount",
          columns: "columns",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   name : String
    #   passed : Boolean
    #   detail : String
    #   samples : String
    ValidationCheckWire = Data.define(:name, :passed, :detail, :samples) do
      def self.wire
        {
          name: "name",
          passed: "passed",
          detail: "detail",
          samples: "samples",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end

    # Fields:
    #   protocol_version : Integer
    #   project_id : String
    #   server_name : String
    Welcome = Data.define(:protocol_version, :project_id, :server_name) do
      def self.wire
        {
          protocol_version: "protocolVersion",
          project_id: "projectId",
          server_name: "serverName",
        }.freeze
      end

      def to_wire
        self.class.wire.each_with_object({}) { |(name, on_wire), out| out[on_wire] = public_send(name) }
      end

      def self.from_wire(hash)
        new(**wire.each_with_object({}) { |(name, on_wire), out| out[name] = hash[on_wire] })
      end
    end
  end
end
