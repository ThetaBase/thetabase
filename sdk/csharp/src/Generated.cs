// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.

using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace ThetaBase;

[JsonConverter(typeof(JsonStringEnumConverter<Gate>))]
public enum Gate {
    [JsonStringEnumMemberName("autoApply")]
    AutoApply,
    [JsonStringEnumMemberName("confirm")]
    Confirm,
    [JsonStringEnumMemberName("shadowValidate")]
    ShadowValidate,
}

[JsonConverter(typeof(JsonStringEnumConverter<GateRule>))]
public enum GateRule {
    [JsonStringEnumMemberName("irreversibleOverShadowThreshold")]
    IrreversibleOverShadowThreshold,
    [JsonStringEnumMemberName("destructive")]
    Destructive,
    [JsonStringEnumMemberName("overRowImpactThreshold")]
    OverRowImpactThreshold,
    [JsonStringEnumMemberName("autoApprovedByPolicy")]
    AutoApprovedByPolicy,
    [JsonStringEnumMemberName("withinThresholds")]
    WithinThresholds,
}

[JsonConverter(typeof(JsonStringEnumConverter<Remedy>))]
public enum Remedy {
    [JsonStringEnumMemberName("none")]
    None,
    [JsonStringEnumMemberName("confirm")]
    Confirm,
    [JsonStringEnumMemberName("validateOnShadowBranch")]
    ValidateOnShadowBranch,
    [JsonStringEnumMemberName("reduceBlastRadius")]
    ReduceBlastRadius,
}

[JsonConverter(typeof(JsonStringEnumConverter<Status>))]
public enum Status {
    [JsonStringEnumMemberName("ok")]
    Ok,
    [JsonStringEnumMemberName("conflict")]
    Conflict,
    [JsonStringEnumMemberName("blocked")]
    Blocked,
    [JsonStringEnumMemberName("upToDate")]
    UpToDate,
}

public sealed record ApplyRequest(
    [property: JsonPropertyName("changeId")] string? ChangeId,
    [property: JsonPropertyName("retiredChange")] string? RetiredChange,
    [property: JsonPropertyName("confirm")] bool Confirm
);

public sealed record AuditEntryWire(
    [property: JsonPropertyName("risk")] int Risk,
    [property: JsonPropertyName("summary")] string? Summary,
    [property: JsonPropertyName("author")] string? Author,
    [property: JsonPropertyName("timestampMs")] long TimestampMs,
    [property: JsonPropertyName("detail")] string? Detail
);

public sealed record AuditRequest(
    [property: JsonPropertyName("limit")] int Limit,
    [property: JsonPropertyName("minRisk")] int MinRisk
);

public sealed record AuditResponse(
    [property: JsonPropertyName("entries")] IReadOnlyList<AuditEntryWire?>? Entries
);

public sealed record BranchInfo(
    [property: JsonPropertyName("branchId")] long BranchId,
    [property: JsonPropertyName("name")] string? Name,
    [property: JsonPropertyName("kind")] int Kind,
    [property: JsonPropertyName("head")] string? Head,
    [property: JsonPropertyName("protected")] bool Protected,
    [property: JsonPropertyName("nextCommit")] long NextCommit
);

public sealed record BranchListResponse(
    [property: JsonPropertyName("branches")] IReadOnlyList<BranchInfo?>? Branches
);

public sealed record BranchRequest(
    [property: JsonPropertyName("name")] string? Name,
    [property: JsonPropertyName("from")] long From
);

public sealed record BranchResponse(
    [property: JsonPropertyName("branchId")] long BranchId
);

public sealed record ChangeDiff(
    [property: JsonPropertyName("changeId")] string? ChangeId,
    [property: JsonPropertyName("destructive")] bool Destructive,
    [property: JsonPropertyName("rowsAffected")] long RowsAffected,
    [property: JsonPropertyName("reversible")] bool Reversible,
    [property: JsonPropertyName("estimatedCostMs")] int EstimatedCostMs,
    [property: JsonPropertyName("requiresConfirm")] bool RequiresConfirm,
    [property: JsonPropertyName("shadowBranchId")] long ShadowBranchId,
    [property: JsonPropertyName("reason")] string? Reason,
    [property: JsonPropertyName("affectedTable")] string? AffectedTable,
    [property: JsonPropertyName("affectedColumn")] string? AffectedColumn,
    [property: JsonPropertyName("changeType")] string? ChangeType,
    [property: JsonPropertyName("gate")] Gate Gate,
    [property: JsonPropertyName("rule")] GateRule Rule,
    [property: JsonPropertyName("remedy")] Remedy Remedy,
    [property: JsonPropertyName("ruleThreshold")] long RuleThreshold
);

public sealed record ChangeRequest(
    [property: JsonPropertyName("changeId")] string? ChangeId
);

public sealed record ChangeStateResponse(
    [property: JsonPropertyName("diff")] ChangeDiff? Diff,
    [property: JsonPropertyName("hasShadow")] bool HasShadow,
    [property: JsonPropertyName("shadowBranchId")] long ShadowBranchId,
    [property: JsonPropertyName("hasValidation")] bool HasValidation,
    [property: JsonPropertyName("validationPassed")] bool ValidationPassed,
    [property: JsonPropertyName("validationSummary")] string? ValidationSummary,
    [property: JsonPropertyName("checks")] IReadOnlyList<ValidationCheckWire?>? Checks
);

public sealed record ColumnDescription(
    [property: JsonPropertyName("name")] string? Name,
    [property: JsonPropertyName("type")] string? Type,
    [property: JsonPropertyName("nullable")] bool Nullable,
    [property: JsonPropertyName("crdt")] string? Crdt,
    [property: JsonPropertyName("declaredAt")] string? DeclaredAt,
    [property: JsonPropertyName("touchedByAgent")] bool TouchedByAgent,
    [property: JsonPropertyName("examples")] IReadOnlyList<string?>? Examples,
    [property: JsonPropertyName("nullBasisPoints")] int NullBasisPoints
);

public sealed record ConflictRef(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("ours")] string? Ours,
    [property: JsonPropertyName("theirs")] string? Theirs,
    [property: JsonPropertyName("reason")] string? Reason
);

/// <summary>`CrdtRequest`'s union: exactly one arm is present, and Kind says which.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<CrdtRequestBodyKind>))]
public enum CrdtRequestBodyKind {
    [JsonStringEnumMemberName("increment")]
    Increment,
    [JsonStringEnumMemberName("setRegister")]
    SetRegister,
    [JsonStringEnumMemberName("setAdd")]
    SetAdd,
    [JsonStringEnumMemberName("setRemove")]
    SetRemove,
    [JsonStringEnumMemberName("seqInsert")]
    SeqInsert,
    [JsonStringEnumMemberName("seqRemove")]
    SeqRemove,
}

public sealed record CrdtRequestBody(
    [property: JsonPropertyName("kind")] CrdtRequestBodyKind Kind,
    [property: JsonPropertyName("value")] object? Value
);

public sealed record CrdtRequest(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("mutation")] CrdtRequestBody Mutation
);

public sealed record DeleteRequest(
    [property: JsonPropertyName("key")] string? Key
);

public sealed record DescribeRequest(
    [property: JsonPropertyName("table")] string? Table,
    [property: JsonPropertyName("includeExamples")] bool IncludeExamples,
    [property: JsonPropertyName("exampleLimit")] int ExampleLimit
);

public sealed record DiscardBranchRequest(
    [property: JsonPropertyName("name")] string? Name
);

public sealed record ElemId(
    [property: JsonPropertyName("counter")] long Counter,
    [property: JsonPropertyName("replica")] long Replica
);

public sealed record ErrorResponse(
    [property: JsonPropertyName("code")] int Code,
    [property: JsonPropertyName("message")] string? Message,
    [property: JsonPropertyName("diff")] ChangeDiff? Diff,
    [property: JsonPropertyName("hasDiff")] bool HasDiff
);

public sealed record ExplainResponse(
    [property: JsonPropertyName("explanation")] string? Explanation
);

public sealed record GetRequest(
    [property: JsonPropertyName("key")] string? Key
);

public sealed record GetResponse(
    [property: JsonPropertyName("found")] bool Found,
    [property: JsonPropertyName("value")] string? Value,
    [property: JsonPropertyName("versionId")] long VersionId
);

public sealed record Hello(
    [property: JsonPropertyName("protocolVersion")] int ProtocolVersion,
    [property: JsonPropertyName("sessionToken")] string? SessionToken,
    [property: JsonPropertyName("clientName")] string? ClientName
);

public sealed record KeyValue(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("value")] byte[]? Value
);

public sealed record ListBranchesRequest(

);

public sealed record MergeRequest(
    [property: JsonPropertyName("sourceBranch")] long SourceBranch,
    [property: JsonPropertyName("targetBranch")] long TargetBranch
);

public sealed record MergeResult(
    [property: JsonPropertyName("status")] Status Status,
    [property: JsonPropertyName("conflictCount")] int ConflictCount,
    [property: JsonPropertyName("conflicts")] IReadOnlyList<ConflictRef?>? Conflicts,
    [property: JsonPropertyName("converged")] IReadOnlyList<string?>? Converged
);

public sealed record OkResponse(

);

public sealed record PolicyAccepted(
    [property: JsonPropertyName("version")] long Version,
    [property: JsonPropertyName("accepted")] bool Accepted
);

public sealed record PreconditionFailed(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("found")] bool Found,
    [property: JsonPropertyName("actual")] long Actual
);

public sealed record ProjectStatus(
    [property: JsonPropertyName("projectId")] string? ProjectId,
    [property: JsonPropertyName("branch")] string? Branch,
    [property: JsonPropertyName("writeVolumeMB")] double WriteVolumeMB,
    [property: JsonPropertyName("circuitBreakerTripped")] bool CircuitBreakerTripped,
    [property: JsonPropertyName("replicaRegions")] IReadOnlyList<string?>? ReplicaRegions,
    [property: JsonPropertyName("breakerWindowRows")] long BreakerWindowRows,
    [property: JsonPropertyName("protocolVersion")] int ProtocolVersion,
    [property: JsonPropertyName("commitsApplied")] long CommitsApplied,
    [property: JsonPropertyName("storageBytes")] long StorageBytes,
    [property: JsonPropertyName("hasStorageBytes")] bool HasStorageBytes,
    [property: JsonPropertyName("rowsWritten")] long RowsWritten
);

public sealed record ProposeRequest(
    [property: JsonPropertyName("change")] string? Change
);

public sealed record PushPolicyRequest(
    [property: JsonPropertyName("payload")] byte[]? Payload,
    [property: JsonPropertyName("signature")] byte[]? Signature,
    [property: JsonPropertyName("keyId")] string? KeyId
);

public sealed record PushRevocationsRequest(
    [property: JsonPropertyName("payload")] byte[]? Payload,
    [property: JsonPropertyName("signature")] byte[]? Signature,
    [property: JsonPropertyName("keyId")] string? KeyId
);

/// <summary>`PutIfRequest`'s union: exactly one arm is present, and Kind says which.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<PutIfRequestBodyKind>))]
public enum PutIfRequestBodyKind {
    [JsonStringEnumMemberName("absent")]
    Absent,
    [JsonStringEnumMemberName("version")]
    Version,
}

public sealed record PutIfRequestBody(
    [property: JsonPropertyName("kind")] PutIfRequestBodyKind Kind,
    [property: JsonPropertyName("value")] object? Value
);

public sealed record PutIfRequest(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("value")] string? Value,
    [property: JsonPropertyName("ttl")] long Ttl,
    [property: JsonPropertyName("expect")] PutIfRequestBody Expect
);

public sealed record PutRequest(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("value")] string? Value,
    [property: JsonPropertyName("ttl")] long Ttl
);

public sealed record PutResponse(
    [property: JsonPropertyName("commitId")] string? CommitId
);

public sealed record QueryPlan(
    [property: JsonPropertyName("planHash")] long PlanHash,
    [property: JsonPropertyName("rawQuery")] string? RawQuery,
    [property: JsonPropertyName("contextVars")] IReadOnlyList<KeyValue?>? ContextVars
);

public sealed record QueryRequest(
    [property: JsonPropertyName("plan")] QueryPlan? Plan
);

public sealed record QueryResponse(
    [property: JsonPropertyName("resultSet")] byte[]? ResultSet,
    [property: JsonPropertyName("planHash")] long PlanHash,
    [property: JsonPropertyName("rowCount")] long RowCount
);

public sealed record RejectRequest(
    [property: JsonPropertyName("changeId")] string? ChangeId,
    [property: JsonPropertyName("reason")] string? Reason
);

/// <summary>`Request`'s union: exactly one arm is present, and Kind says which.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<RequestBodyKind>))]
public enum RequestBodyKind {
    [JsonStringEnumMemberName("get")]
    Get,
    [JsonStringEnumMemberName("put")]
    Put,
    [JsonStringEnumMemberName("delete")]
    Delete,
    [JsonStringEnumMemberName("query")]
    Query,
    [JsonStringEnumMemberName("explain")]
    Explain,
    [JsonStringEnumMemberName("proposeSchemaChange")]
    ProposeSchemaChange,
    [JsonStringEnumMemberName("applySchemaChange")]
    ApplySchemaChange,
    [JsonStringEnumMemberName("createBranch")]
    CreateBranch,
    [JsonStringEnumMemberName("merge")]
    Merge,
    [JsonStringEnumMemberName("status")]
    Status,
    [JsonStringEnumMemberName("pushRevocations")]
    PushRevocations,
    [JsonStringEnumMemberName("audit")]
    Audit,
    [JsonStringEnumMemberName("listBranches")]
    ListBranches,
    [JsonStringEnumMemberName("discardBranch")]
    DiscardBranch,
    [JsonStringEnumMemberName("showChange")]
    ShowChange,
    [JsonStringEnumMemberName("promoteChange")]
    PromoteChange,
    [JsonStringEnumMemberName("rejectChange")]
    RejectChange,
    [JsonStringEnumMemberName("pushPolicy")]
    PushPolicy,
    [JsonStringEnumMemberName("putIf")]
    PutIf,
    [JsonStringEnumMemberName("describe")]
    Describe,
    [JsonStringEnumMemberName("crdt")]
    Crdt,
    [JsonStringEnumMemberName("signedWrite")]
    SignedWrite,
    [JsonStringEnumMemberName("reviewQueue")]
    ReviewQueue,
}

public sealed record RequestBody(
    [property: JsonPropertyName("kind")] RequestBodyKind Kind,
    [property: JsonPropertyName("value")] object? Value
);

public sealed record Request(
    [property: JsonPropertyName("requestId")] long RequestId,
    [property: JsonPropertyName("branchId")] long BranchId,
    [property: JsonPropertyName("body")] RequestBody Body
);

/// <summary>`Response`'s union: exactly one arm is present, and Kind says which.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<ResponseBodyKind>))]
public enum ResponseBodyKind {
    [JsonStringEnumMemberName("error")]
    Error,
    [JsonStringEnumMemberName("get")]
    Get,
    [JsonStringEnumMemberName("put")]
    Put,
    [JsonStringEnumMemberName("delete")]
    Delete,
    [JsonStringEnumMemberName("query")]
    Query,
    [JsonStringEnumMemberName("explain")]
    Explain,
    [JsonStringEnumMemberName("propose")]
    Propose,
    [JsonStringEnumMemberName("apply")]
    Apply,
    [JsonStringEnumMemberName("branch")]
    Branch,
    [JsonStringEnumMemberName("merge")]
    Merge,
    [JsonStringEnumMemberName("status")]
    Status,
    [JsonStringEnumMemberName("revocations")]
    Revocations,
    [JsonStringEnumMemberName("audit")]
    Audit,
    [JsonStringEnumMemberName("branches")]
    Branches,
    [JsonStringEnumMemberName("change")]
    Change,
    [JsonStringEnumMemberName("ok")]
    Ok,
    [JsonStringEnumMemberName("policy")]
    Policy,
    [JsonStringEnumMemberName("precondition")]
    Precondition,
    [JsonStringEnumMemberName("description")]
    Description,
    [JsonStringEnumMemberName("reviewQueue")]
    ReviewQueue,
}

public sealed record ResponseBody(
    [property: JsonPropertyName("kind")] ResponseBodyKind Kind,
    [property: JsonPropertyName("value")] object? Value
);

public sealed record Response(
    [property: JsonPropertyName("requestId")] long RequestId,
    [property: JsonPropertyName("body")] ResponseBody Body
);

public sealed record ReviewBatchWire(
    [property: JsonPropertyName("key")] string? Key,
    [property: JsonPropertyName("gate")] int Gate,
    [property: JsonPropertyName("changes")] IReadOnlyList<ChangeDiff?>? Changes,
    [property: JsonPropertyName("rowsAffected")] long RowsAffected,
    [property: JsonPropertyName("cost")] int Cost,
    [property: JsonPropertyName("costIfUnbatched")] int CostIfUnbatched,
    [property: JsonPropertyName("reason")] string? Reason
);

public sealed record ReviewQueueRequest(

);

public sealed record ReviewQueueResponse(
    [property: JsonPropertyName("batches")] IReadOnlyList<ReviewBatchWire?>? Batches
);

public sealed record RevocationsAccepted(
    [property: JsonPropertyName("version")] long Version,
    [property: JsonPropertyName("accepted")] bool Accepted
);

public sealed record SchemaDescription(
    [property: JsonPropertyName("tables")] IReadOnlyList<TableDescription?>? Tables,
    [property: JsonPropertyName("examplesWithheld")] bool ExamplesWithheld,
    [property: JsonPropertyName("withheldReason")] string? WithheldReason
);

public sealed record SeqInsertOp(
    [property: JsonPropertyName("after")] ElemId? After,
    [property: JsonPropertyName("value")] string? Value
);

/// <summary>`SignedWriteRequest`'s union: exactly one arm is present, and Kind says which.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<SignedWriteRequestBodyKind>))]
public enum SignedWriteRequestBodyKind {
    [JsonStringEnumMemberName("put")]
    Put,
    [JsonStringEnumMemberName("delete")]
    Delete,
    [JsonStringEnumMemberName("crdt")]
    Crdt,
}

public sealed record SignedWriteRequestBody(
    [property: JsonPropertyName("kind")] SignedWriteRequestBodyKind Kind,
    [property: JsonPropertyName("value")] object? Value
);

public sealed record SignedWriteRequest(
    [property: JsonPropertyName("commitId")] long CommitId,
    [property: JsonPropertyName("timestampMs")] long TimestampMs,
    [property: JsonPropertyName("signature")] byte[]? Signature,
    [property: JsonPropertyName("op")] SignedWriteRequestBody Op
);

public sealed record StatusRequest(

);

public sealed record TableDescription(
    [property: JsonPropertyName("name")] string? Name,
    [property: JsonPropertyName("rowCount")] long RowCount,
    [property: JsonPropertyName("columns")] IReadOnlyList<ColumnDescription?>? Columns
);

public sealed record ValidationCheckWire(
    [property: JsonPropertyName("name")] string? Name,
    [property: JsonPropertyName("passed")] bool Passed,
    [property: JsonPropertyName("detail")] string? Detail,
    [property: JsonPropertyName("samples")] string? Samples
);

public sealed record Welcome(
    [property: JsonPropertyName("protocolVersion")] int ProtocolVersion,
    [property: JsonPropertyName("projectId")] string? ProjectId,
    [property: JsonPropertyName("serverName")] string? ServerName
);
