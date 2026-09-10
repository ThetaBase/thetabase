// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.
//
// Field names match the wire's, because Swift and the wire are both camelCase.
// A `CodingKeys` block appears only where a name had to be escaped.

import Foundation

public enum Gate: String, Codable, Sendable {
    case autoApply = "autoApply"
    case confirm = "confirm"
    case shadowValidate = "shadowValidate"
}

public enum GateRule: String, Codable, Sendable {
    case irreversibleOverShadowThreshold = "irreversibleOverShadowThreshold"
    case destructive = "destructive"
    case overRowImpactThreshold = "overRowImpactThreshold"
    case autoApprovedByPolicy = "autoApprovedByPolicy"
    case withinThresholds = "withinThresholds"
}

public enum Remedy: String, Codable, Sendable {
    case none = "none"
    case confirm = "confirm"
    case validateOnShadowBranch = "validateOnShadowBranch"
    case reduceBlastRadius = "reduceBlastRadius"
}

public enum Status: String, Codable, Sendable {
    case ok = "ok"
    case conflict = "conflict"
    case blocked = "blocked"
    case upToDate = "upToDate"
}

public struct ApplyRequest: Codable, Equatable, Sendable {
    public let changeId: String?
    public let retiredChange: String?
    public let confirm: Bool

    public init(changeId: String?, retiredChange: String?, confirm: Bool) {
        self.changeId = changeId
        self.retiredChange = retiredChange
        self.confirm = confirm
    }
}

public struct AuditEntryWire: Codable, Equatable, Sendable {
    public let risk: Int32
    public let summary: String?
    public let author: String?
    public let timestampMs: Int64
    public let detail: String?

    public init(risk: Int32, summary: String?, author: String?, timestampMs: Int64, detail: String?) {
        self.risk = risk
        self.summary = summary
        self.author = author
        self.timestampMs = timestampMs
        self.detail = detail
    }
}

public struct AuditRequest: Codable, Equatable, Sendable {
    public let limit: Int32
    public let minRisk: Int32

    public init(limit: Int32, minRisk: Int32) {
        self.limit = limit
        self.minRisk = minRisk
    }
}

public struct AuditResponse: Codable, Equatable, Sendable {
    public let entries: [AuditEntryWire]?

    public init(entries: [AuditEntryWire]?) {
        self.entries = entries
    }
}

public struct BranchInfo: Codable, Equatable, Sendable {
    public let branchId: Int64
    public let name: String?
    public let kind: Int32
    public let head: String?
    public let protected: Bool
    public let nextCommit: Int64

    public init(branchId: Int64, name: String?, kind: Int32, head: String?, protected: Bool, nextCommit: Int64) {
        self.branchId = branchId
        self.name = name
        self.kind = kind
        self.head = head
        self.protected = protected
        self.nextCommit = nextCommit
    }
}

public struct BranchListResponse: Codable, Equatable, Sendable {
    public let branches: [BranchInfo]?

    public init(branches: [BranchInfo]?) {
        self.branches = branches
    }
}

public struct BranchRequest: Codable, Equatable, Sendable {
    public let name: String?
    public let from: Int64

    public init(name: String?, from: Int64) {
        self.name = name
        self.from = from
    }
}

public struct BranchResponse: Codable, Equatable, Sendable {
    public let branchId: Int64

    public init(branchId: Int64) {
        self.branchId = branchId
    }
}

public struct ChangeDiff: Codable, Equatable, Sendable {
    public let changeId: String?
    public let destructive: Bool
    public let rowsAffected: Int64
    public let reversible: Bool
    public let estimatedCostMs: Int32
    public let requiresConfirm: Bool
    public let shadowBranchId: Int64
    public let reason: String?
    public let affectedTable: String?
    public let affectedColumn: String?
    public let changeType: String?
    public let gate: Gate
    public let rule: GateRule
    public let remedy: Remedy
    public let ruleThreshold: Int64

    public init(changeId: String?, destructive: Bool, rowsAffected: Int64, reversible: Bool, estimatedCostMs: Int32, requiresConfirm: Bool, shadowBranchId: Int64, reason: String?, affectedTable: String?, affectedColumn: String?, changeType: String?, gate: Gate, rule: GateRule, remedy: Remedy, ruleThreshold: Int64) {
        self.changeId = changeId
        self.destructive = destructive
        self.rowsAffected = rowsAffected
        self.reversible = reversible
        self.estimatedCostMs = estimatedCostMs
        self.requiresConfirm = requiresConfirm
        self.shadowBranchId = shadowBranchId
        self.reason = reason
        self.affectedTable = affectedTable
        self.affectedColumn = affectedColumn
        self.changeType = changeType
        self.gate = gate
        self.rule = rule
        self.remedy = remedy
        self.ruleThreshold = ruleThreshold
    }
}

public struct ChangeRequest: Codable, Equatable, Sendable {
    public let changeId: String?

    public init(changeId: String?) {
        self.changeId = changeId
    }
}

public struct ChangeStateResponse: Codable, Equatable, Sendable {
    public let diff: ChangeDiff?
    public let hasShadow: Bool
    public let shadowBranchId: Int64
    public let hasValidation: Bool
    public let validationPassed: Bool
    public let validationSummary: String?
    public let checks: [ValidationCheckWire]?

    public init(diff: ChangeDiff?, hasShadow: Bool, shadowBranchId: Int64, hasValidation: Bool, validationPassed: Bool, validationSummary: String?, checks: [ValidationCheckWire]?) {
        self.diff = diff
        self.hasShadow = hasShadow
        self.shadowBranchId = shadowBranchId
        self.hasValidation = hasValidation
        self.validationPassed = validationPassed
        self.validationSummary = validationSummary
        self.checks = checks
    }
}

public struct ColumnDescription: Codable, Equatable, Sendable {
    public let name: String?
    public let type: String?
    public let nullable: Bool
    public let crdt: String?
    public let declaredAt: String?
    public let touchedByAgent: Bool
    public let examples: [String]?
    public let nullBasisPoints: Int32

    public init(name: String?, type: String?, nullable: Bool, crdt: String?, declaredAt: String?, touchedByAgent: Bool, examples: [String]?, nullBasisPoints: Int32) {
        self.name = name
        self.type = type
        self.nullable = nullable
        self.crdt = crdt
        self.declaredAt = declaredAt
        self.touchedByAgent = touchedByAgent
        self.examples = examples
        self.nullBasisPoints = nullBasisPoints
    }
}

public struct ConflictRef: Codable, Equatable, Sendable {
    public let key: String?
    public let ours: String?
    public let theirs: String?
    public let reason: String?

    public init(key: String?, ours: String?, theirs: String?, reason: String?) {
        self.key = key
        self.ours = ours
        self.theirs = theirs
        self.reason = reason
    }
}

/// `CrdtRequest`'s union: exactly one arm is present, and `kind` says which.
public enum CrdtRequestBodyKind: String, Codable, Sendable {
    case increment = "increment"
    case setRegister = "setRegister"
    case setAdd = "setAdd"
    case setRemove = "setRemove"
    case seqInsert = "seqInsert"
    case seqRemove = "seqRemove"
}

public struct CrdtRequestBody: Codable, Equatable, Sendable {
    public let kind: CrdtRequestBodyKind
    /// The arm's payload. `JSONValue` rather than a per-arm associated
    /// value: the arms carry unrelated shapes, and every other binding
    /// sends this as an opaque document.
    public let value: JSONValue?

    public init(kind: CrdtRequestBodyKind, value: JSONValue? = nil) {
        self.kind = kind
        self.value = value
    }
}

public struct CrdtRequest: Codable, Equatable, Sendable {
    public let key: String?
    public let mutation: CrdtRequestBody

    public init(key: String?, mutation: CrdtRequestBody) {
        self.key = key
        self.mutation = mutation
    }
}

public struct DeleteRequest: Codable, Equatable, Sendable {
    public let key: String?

    public init(key: String?) {
        self.key = key
    }
}

public struct DescribeRequest: Codable, Equatable, Sendable {
    public let table: String?
    public let includeExamples: Bool
    public let exampleLimit: Int32

    public init(table: String?, includeExamples: Bool, exampleLimit: Int32) {
        self.table = table
        self.includeExamples = includeExamples
        self.exampleLimit = exampleLimit
    }
}

public struct DiscardBranchRequest: Codable, Equatable, Sendable {
    public let name: String?

    public init(name: String?) {
        self.name = name
    }
}

public struct ElemId: Codable, Equatable, Sendable {
    public let counter: Int64
    public let replica: Int64

    public init(counter: Int64, replica: Int64) {
        self.counter = counter
        self.replica = replica
    }
}

public struct ErrorResponse: Codable, Equatable, Sendable {
    public let code: Int32
    public let message: String?
    public let diff: ChangeDiff?
    public let hasDiff: Bool

    public init(code: Int32, message: String?, diff: ChangeDiff?, hasDiff: Bool) {
        self.code = code
        self.message = message
        self.diff = diff
        self.hasDiff = hasDiff
    }
}

public struct ExplainResponse: Codable, Equatable, Sendable {
    public let explanation: String?

    public init(explanation: String?) {
        self.explanation = explanation
    }
}

public struct GetRequest: Codable, Equatable, Sendable {
    public let key: String?

    public init(key: String?) {
        self.key = key
    }
}

public struct GetResponse: Codable, Equatable, Sendable {
    public let found: Bool
    public let value: String?
    public let versionId: Int64

    public init(found: Bool, value: String?, versionId: Int64) {
        self.found = found
        self.value = value
        self.versionId = versionId
    }
}

public struct Hello: Codable, Equatable, Sendable {
    public let protocolVersion: Int32
    public let sessionToken: String?
    public let clientName: String?

    public init(protocolVersion: Int32, sessionToken: String?, clientName: String?) {
        self.protocolVersion = protocolVersion
        self.sessionToken = sessionToken
        self.clientName = clientName
    }
}

public struct KeyValue: Codable, Equatable, Sendable {
    public let key: String?
    public let value: Data?

    public init(key: String?, value: Data?) {
        self.key = key
        self.value = value
    }
}

public struct ListBranchesRequest: Codable, Equatable, Sendable {

    public init() {
    }
}

public struct MergeRequest: Codable, Equatable, Sendable {
    public let sourceBranch: Int64
    public let targetBranch: Int64

    public init(sourceBranch: Int64, targetBranch: Int64) {
        self.sourceBranch = sourceBranch
        self.targetBranch = targetBranch
    }
}

public struct MergeResult: Codable, Equatable, Sendable {
    public let status: Status
    public let conflictCount: Int32
    public let conflicts: [ConflictRef]?
    public let converged: [String]?

    public init(status: Status, conflictCount: Int32, conflicts: [ConflictRef]?, converged: [String]?) {
        self.status = status
        self.conflictCount = conflictCount
        self.conflicts = conflicts
        self.converged = converged
    }
}

public struct OkResponse: Codable, Equatable, Sendable {

    public init() {
    }
}

public struct PolicyAccepted: Codable, Equatable, Sendable {
    public let version: Int64
    public let accepted: Bool

    public init(version: Int64, accepted: Bool) {
        self.version = version
        self.accepted = accepted
    }
}

public struct PreconditionFailed: Codable, Equatable, Sendable {
    public let key: String?
    public let found: Bool
    public let actual: Int64

    public init(key: String?, found: Bool, actual: Int64) {
        self.key = key
        self.found = found
        self.actual = actual
    }
}

public struct ProjectStatus: Codable, Equatable, Sendable {
    public let projectId: String?
    public let branch: String?
    public let writeVolumeMB: Double
    public let circuitBreakerTripped: Bool
    public let replicaRegions: [String]?
    public let breakerWindowRows: Int64
    public let protocolVersion: Int32
    public let commitsApplied: Int64
    public let storageBytes: Int64
    public let hasStorageBytes: Bool
    public let rowsWritten: Int64

    public init(projectId: String?, branch: String?, writeVolumeMB: Double, circuitBreakerTripped: Bool, replicaRegions: [String]?, breakerWindowRows: Int64, protocolVersion: Int32, commitsApplied: Int64, storageBytes: Int64, hasStorageBytes: Bool, rowsWritten: Int64) {
        self.projectId = projectId
        self.branch = branch
        self.writeVolumeMB = writeVolumeMB
        self.circuitBreakerTripped = circuitBreakerTripped
        self.replicaRegions = replicaRegions
        self.breakerWindowRows = breakerWindowRows
        self.protocolVersion = protocolVersion
        self.commitsApplied = commitsApplied
        self.storageBytes = storageBytes
        self.hasStorageBytes = hasStorageBytes
        self.rowsWritten = rowsWritten
    }
}

public struct ProposeRequest: Codable, Equatable, Sendable {
    public let change: String?

    public init(change: String?) {
        self.change = change
    }
}

public struct PushPolicyRequest: Codable, Equatable, Sendable {
    public let payload: Data?
    public let signature: Data?
    public let keyId: String?

    public init(payload: Data?, signature: Data?, keyId: String?) {
        self.payload = payload
        self.signature = signature
        self.keyId = keyId
    }
}

public struct PushRevocationsRequest: Codable, Equatable, Sendable {
    public let payload: Data?
    public let signature: Data?
    public let keyId: String?

    public init(payload: Data?, signature: Data?, keyId: String?) {
        self.payload = payload
        self.signature = signature
        self.keyId = keyId
    }
}

/// `PutIfRequest`'s union: exactly one arm is present, and `kind` says which.
public enum PutIfRequestBodyKind: String, Codable, Sendable {
    case absent = "absent"
    case version = "version"
}

public struct PutIfRequestBody: Codable, Equatable, Sendable {
    public let kind: PutIfRequestBodyKind
    /// The arm's payload. `JSONValue` rather than a per-arm associated
    /// value: the arms carry unrelated shapes, and every other binding
    /// sends this as an opaque document.
    public let value: JSONValue?

    public init(kind: PutIfRequestBodyKind, value: JSONValue? = nil) {
        self.kind = kind
        self.value = value
    }
}

public struct PutIfRequest: Codable, Equatable, Sendable {
    public let key: String?
    public let value: String?
    public let ttl: Int64
    public let expect: PutIfRequestBody

    public init(key: String?, value: String?, ttl: Int64, expect: PutIfRequestBody) {
        self.key = key
        self.value = value
        self.ttl = ttl
        self.expect = expect
    }
}

public struct PutRequest: Codable, Equatable, Sendable {
    public let key: String?
    public let value: String?
    public let ttl: Int64

    public init(key: String?, value: String?, ttl: Int64) {
        self.key = key
        self.value = value
        self.ttl = ttl
    }
}

public struct PutResponse: Codable, Equatable, Sendable {
    public let commitId: String?

    public init(commitId: String?) {
        self.commitId = commitId
    }
}

public struct QueryPlan: Codable, Equatable, Sendable {
    public let planHash: Int64
    public let rawQuery: String?
    public let contextVars: [KeyValue]?

    public init(planHash: Int64, rawQuery: String?, contextVars: [KeyValue]?) {
        self.planHash = planHash
        self.rawQuery = rawQuery
        self.contextVars = contextVars
    }
}

public struct QueryRequest: Codable, Equatable, Sendable {
    public let plan: QueryPlan?

    public init(plan: QueryPlan?) {
        self.plan = plan
    }
}

public struct QueryResponse: Codable, Equatable, Sendable {
    public let resultSet: Data?
    public let planHash: Int64
    public let rowCount: Int64

    public init(resultSet: Data?, planHash: Int64, rowCount: Int64) {
        self.resultSet = resultSet
        self.planHash = planHash
        self.rowCount = rowCount
    }
}

public struct RejectRequest: Codable, Equatable, Sendable {
    public let changeId: String?
    public let reason: String?

    public init(changeId: String?, reason: String?) {
        self.changeId = changeId
        self.reason = reason
    }
}

/// `Request`'s union: exactly one arm is present, and `kind` says which.
public enum RequestBodyKind: String, Codable, Sendable {
    case get = "get"
    case put = "put"
    case delete = "delete"
    case query = "query"
    case explain = "explain"
    case proposeSchemaChange = "proposeSchemaChange"
    case applySchemaChange = "applySchemaChange"
    case createBranch = "createBranch"
    case merge = "merge"
    case status = "status"
    case pushRevocations = "pushRevocations"
    case audit = "audit"
    case listBranches = "listBranches"
    case discardBranch = "discardBranch"
    case showChange = "showChange"
    case promoteChange = "promoteChange"
    case rejectChange = "rejectChange"
    case pushPolicy = "pushPolicy"
    case putIf = "putIf"
    case describe = "describe"
    case crdt = "crdt"
    case signedWrite = "signedWrite"
    case reviewQueue = "reviewQueue"
}

public struct RequestBody: Codable, Equatable, Sendable {
    public let kind: RequestBodyKind
    /// The arm's payload. `JSONValue` rather than a per-arm associated
    /// value: the arms carry unrelated shapes, and every other binding
    /// sends this as an opaque document.
    public let value: JSONValue?

    public init(kind: RequestBodyKind, value: JSONValue? = nil) {
        self.kind = kind
        self.value = value
    }
}

public struct Request: Codable, Equatable, Sendable {
    public let requestId: Int64
    public let branchId: Int64
    public let body: RequestBody

    public init(requestId: Int64, branchId: Int64, body: RequestBody) {
        self.requestId = requestId
        self.branchId = branchId
        self.body = body
    }
}

/// `Response`'s union: exactly one arm is present, and `kind` says which.
public enum ResponseBodyKind: String, Codable, Sendable {
    case error = "error"
    case get = "get"
    case put = "put"
    case delete = "delete"
    case query = "query"
    case explain = "explain"
    case propose = "propose"
    case apply = "apply"
    case branch = "branch"
    case merge = "merge"
    case status = "status"
    case revocations = "revocations"
    case audit = "audit"
    case branches = "branches"
    case change = "change"
    case ok = "ok"
    case policy = "policy"
    case precondition = "precondition"
    case description = "description"
    case reviewQueue = "reviewQueue"
}

public struct ResponseBody: Codable, Equatable, Sendable {
    public let kind: ResponseBodyKind
    /// The arm's payload. `JSONValue` rather than a per-arm associated
    /// value: the arms carry unrelated shapes, and every other binding
    /// sends this as an opaque document.
    public let value: JSONValue?

    public init(kind: ResponseBodyKind, value: JSONValue? = nil) {
        self.kind = kind
        self.value = value
    }
}

public struct Response: Codable, Equatable, Sendable {
    public let requestId: Int64
    public let body: ResponseBody

    public init(requestId: Int64, body: ResponseBody) {
        self.requestId = requestId
        self.body = body
    }
}

public struct ReviewBatchWire: Codable, Equatable, Sendable {
    public let key: String?
    public let gate: Int32
    public let changes: [ChangeDiff]?
    public let rowsAffected: Int64
    public let cost: Int32
    public let costIfUnbatched: Int32
    public let reason: String?

    public init(key: String?, gate: Int32, changes: [ChangeDiff]?, rowsAffected: Int64, cost: Int32, costIfUnbatched: Int32, reason: String?) {
        self.key = key
        self.gate = gate
        self.changes = changes
        self.rowsAffected = rowsAffected
        self.cost = cost
        self.costIfUnbatched = costIfUnbatched
        self.reason = reason
    }
}

public struct ReviewQueueRequest: Codable, Equatable, Sendable {

    public init() {
    }
}

public struct ReviewQueueResponse: Codable, Equatable, Sendable {
    public let batches: [ReviewBatchWire]?

    public init(batches: [ReviewBatchWire]?) {
        self.batches = batches
    }
}

public struct RevocationsAccepted: Codable, Equatable, Sendable {
    public let version: Int64
    public let accepted: Bool

    public init(version: Int64, accepted: Bool) {
        self.version = version
        self.accepted = accepted
    }
}

public struct SchemaDescription: Codable, Equatable, Sendable {
    public let tables: [TableDescription]?
    public let examplesWithheld: Bool
    public let withheldReason: String?

    public init(tables: [TableDescription]?, examplesWithheld: Bool, withheldReason: String?) {
        self.tables = tables
        self.examplesWithheld = examplesWithheld
        self.withheldReason = withheldReason
    }
}

public struct SeqInsertOp: Codable, Equatable, Sendable {
    public let after: ElemId?
    public let value: String?

    public init(after: ElemId?, value: String?) {
        self.after = after
        self.value = value
    }
}

/// `SignedWriteRequest`'s union: exactly one arm is present, and `kind` says which.
public enum SignedWriteRequestBodyKind: String, Codable, Sendable {
    case put = "put"
    case delete = "delete"
    case crdt = "crdt"
}

public struct SignedWriteRequestBody: Codable, Equatable, Sendable {
    public let kind: SignedWriteRequestBodyKind
    /// The arm's payload. `JSONValue` rather than a per-arm associated
    /// value: the arms carry unrelated shapes, and every other binding
    /// sends this as an opaque document.
    public let value: JSONValue?

    public init(kind: SignedWriteRequestBodyKind, value: JSONValue? = nil) {
        self.kind = kind
        self.value = value
    }
}

public struct SignedWriteRequest: Codable, Equatable, Sendable {
    public let commitId: Int64
    public let timestampMs: Int64
    public let signature: Data?
    public let op: SignedWriteRequestBody

    public init(commitId: Int64, timestampMs: Int64, signature: Data?, op: SignedWriteRequestBody) {
        self.commitId = commitId
        self.timestampMs = timestampMs
        self.signature = signature
        self.op = op
    }
}

public struct StatusRequest: Codable, Equatable, Sendable {

    public init() {
    }
}

public struct TableDescription: Codable, Equatable, Sendable {
    public let name: String?
    public let rowCount: Int64
    public let columns: [ColumnDescription]?

    public init(name: String?, rowCount: Int64, columns: [ColumnDescription]?) {
        self.name = name
        self.rowCount = rowCount
        self.columns = columns
    }
}

public struct ValidationCheckWire: Codable, Equatable, Sendable {
    public let name: String?
    public let passed: Bool
    public let detail: String?
    public let samples: String?

    public init(name: String?, passed: Bool, detail: String?, samples: String?) {
        self.name = name
        self.passed = passed
        self.detail = detail
        self.samples = samples
    }
}

public struct Welcome: Codable, Equatable, Sendable {
    public let protocolVersion: Int32
    public let projectId: String?
    public let serverName: String?

    public init(protocolVersion: Int32, projectId: String?, serverName: String?) {
        self.protocolVersion = protocolVersion
        self.projectId = projectId
        self.serverName = serverName
    }
}
