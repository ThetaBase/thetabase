// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.

package io.thetabase;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonValue;
import java.util.List;

/** Every wire type, generated from `theta.capnp`. */
public final class Generated {
    private Generated() {}

    public enum Gate {
        AUTO_APPLY("autoApply"),
        CONFIRM("confirm"),
        SHADOW_VALIDATE("shadowValidate");

        private final String wire;

        Gate(String wire) { this.wire = wire; }

        /** The name on the wire, which is not the Java constant. */
        @JsonValue
        public String wire() { return wire; }
    }

    public enum GateRule {
        IRREVERSIBLE_OVER_SHADOW_THRESHOLD("irreversibleOverShadowThreshold"),
        DESTRUCTIVE("destructive"),
        OVER_ROW_IMPACT_THRESHOLD("overRowImpactThreshold"),
        AUTO_APPROVED_BY_POLICY("autoApprovedByPolicy"),
        WITHIN_THRESHOLDS("withinThresholds");

        private final String wire;

        GateRule(String wire) { this.wire = wire; }

        /** The name on the wire, which is not the Java constant. */
        @JsonValue
        public String wire() { return wire; }
    }

    public enum Remedy {
        NONE("none"),
        CONFIRM("confirm"),
        VALIDATE_ON_SHADOW_BRANCH("validateOnShadowBranch"),
        REDUCE_BLAST_RADIUS("reduceBlastRadius");

        private final String wire;

        Remedy(String wire) { this.wire = wire; }

        /** The name on the wire, which is not the Java constant. */
        @JsonValue
        public String wire() { return wire; }
    }

    public enum Status {
        OK("ok"),
        CONFLICT("conflict"),
        BLOCKED("blocked"),
        UP_TO_DATE("upToDate");

        private final String wire;

        Status(String wire) { this.wire = wire; }

        /** The name on the wire, which is not the Java constant. */
        @JsonValue
        public String wire() { return wire; }
    }

    public record ApplyRequest(
            @JsonProperty("changeId") String changeId,
            @JsonProperty("retiredChange") String retiredChange,
            @JsonProperty("confirm") boolean confirm
    ) {}

    public record AuditEntryWire(
            @JsonProperty("risk") int risk,
            @JsonProperty("summary") String summary,
            @JsonProperty("author") String author,
            @JsonProperty("timestampMs") long timestampMs,
            @JsonProperty("detail") String detail
    ) {}

    public record AuditRequest(
            @JsonProperty("limit") int limit,
            @JsonProperty("minRisk") int minRisk
    ) {}

    public record AuditResponse(
            @JsonProperty("entries") List<AuditEntryWire> entries
    ) {}

    public record BranchInfo(
            @JsonProperty("branchId") long branchId,
            @JsonProperty("name") String name,
            @JsonProperty("kind") int kind,
            @JsonProperty("head") String head,
            @JsonProperty("protected") boolean protected_,
            @JsonProperty("nextCommit") long nextCommit
    ) {}

    public record BranchListResponse(
            @JsonProperty("branches") List<BranchInfo> branches
    ) {}

    public record BranchRequest(
            @JsonProperty("name") String name,
            @JsonProperty("from") long from
    ) {}

    public record BranchResponse(
            @JsonProperty("branchId") long branchId
    ) {}

    public record ChangeDiff(
            @JsonProperty("changeId") String changeId,
            @JsonProperty("destructive") boolean destructive,
            @JsonProperty("rowsAffected") long rowsAffected,
            @JsonProperty("reversible") boolean reversible,
            @JsonProperty("estimatedCostMs") int estimatedCostMs,
            @JsonProperty("requiresConfirm") boolean requiresConfirm,
            @JsonProperty("shadowBranchId") long shadowBranchId,
            @JsonProperty("reason") String reason,
            @JsonProperty("affectedTable") String affectedTable,
            @JsonProperty("affectedColumn") String affectedColumn,
            @JsonProperty("changeType") String changeType,
            @JsonProperty("gate") Gate gate,
            @JsonProperty("rule") GateRule rule,
            @JsonProperty("remedy") Remedy remedy,
            @JsonProperty("ruleThreshold") long ruleThreshold
    ) {}

    public record ChangeRequest(
            @JsonProperty("changeId") String changeId
    ) {}

    public record ChangeStateResponse(
            @JsonProperty("diff") ChangeDiff diff,
            @JsonProperty("hasShadow") boolean hasShadow,
            @JsonProperty("shadowBranchId") long shadowBranchId,
            @JsonProperty("hasValidation") boolean hasValidation,
            @JsonProperty("validationPassed") boolean validationPassed,
            @JsonProperty("validationSummary") String validationSummary,
            @JsonProperty("checks") List<ValidationCheckWire> checks
    ) {}

    public record ColumnDescription(
            @JsonProperty("name") String name,
            @JsonProperty("type") String type,
            @JsonProperty("nullable") boolean nullable,
            @JsonProperty("crdt") String crdt,
            @JsonProperty("declaredAt") String declaredAt,
            @JsonProperty("touchedByAgent") boolean touchedByAgent,
            @JsonProperty("examples") List<String> examples,
            @JsonProperty("nullBasisPoints") int nullBasisPoints
    ) {}

    public record ConflictRef(
            @JsonProperty("key") String key,
            @JsonProperty("ours") String ours,
            @JsonProperty("theirs") String theirs,
            @JsonProperty("reason") String reason
    ) {}

    /** `CrdtRequest`'s union: exactly one arm is present, and kind says which. */
    public enum CrdtRequestBodyKind {
            INCREMENT("increment"),
            SET_REGISTER("setRegister"),
            SET_ADD("setAdd"),
            SET_REMOVE("setRemove"),
            SEQ_INSERT("seqInsert"),
            SEQ_REMOVE("seqRemove")
;

        private final String wire;

        CrdtRequestBodyKind(String wire) { this.wire = wire; }

        @JsonValue
        public String wire() { return wire; }
    }

    public record CrdtRequestBody(
            @JsonProperty("kind") CrdtRequestBodyKind kind,
            @JsonProperty("value") Object value
    ) {}

    public record CrdtRequest(
            @JsonProperty("key") String key,
            @JsonProperty("mutation") CrdtRequestBody mutation
    ) {}

    public record DeleteRequest(
            @JsonProperty("key") String key
    ) {}

    public record DescribeRequest(
            @JsonProperty("table") String table,
            @JsonProperty("includeExamples") boolean includeExamples,
            @JsonProperty("exampleLimit") int exampleLimit
    ) {}

    public record DiscardBranchRequest(
            @JsonProperty("name") String name
    ) {}

    public record ElemId(
            @JsonProperty("counter") long counter,
            @JsonProperty("replica") long replica
    ) {}

    public record ErrorResponse(
            @JsonProperty("code") int code,
            @JsonProperty("message") String message,
            @JsonProperty("diff") ChangeDiff diff,
            @JsonProperty("hasDiff") boolean hasDiff
    ) {}

    public record ExplainResponse(
            @JsonProperty("explanation") String explanation
    ) {}

    public record GetRequest(
            @JsonProperty("key") String key
    ) {}

    public record GetResponse(
            @JsonProperty("found") boolean found,
            @JsonProperty("value") String value,
            @JsonProperty("versionId") long versionId
    ) {}

    public record Hello(
            @JsonProperty("protocolVersion") int protocolVersion,
            @JsonProperty("sessionToken") String sessionToken,
            @JsonProperty("clientName") String clientName
    ) {}

    public record KeyValue(
            @JsonProperty("key") String key,
            @JsonProperty("value") byte[] value
    ) {}

    public record ListBranchesRequest(

    ) {}

    public record MergeRequest(
            @JsonProperty("sourceBranch") long sourceBranch,
            @JsonProperty("targetBranch") long targetBranch
    ) {}

    public record MergeResult(
            @JsonProperty("status") Status status,
            @JsonProperty("conflictCount") int conflictCount,
            @JsonProperty("conflicts") List<ConflictRef> conflicts,
            @JsonProperty("converged") List<String> converged
    ) {}

    public record OkResponse(

    ) {}

    public record PolicyAccepted(
            @JsonProperty("version") long version,
            @JsonProperty("accepted") boolean accepted
    ) {}

    public record PreconditionFailed(
            @JsonProperty("key") String key,
            @JsonProperty("found") boolean found,
            @JsonProperty("actual") long actual
    ) {}

    public record ProjectStatus(
            @JsonProperty("projectId") String projectId,
            @JsonProperty("branch") String branch,
            @JsonProperty("writeVolumeMB") double writeVolumeMB,
            @JsonProperty("circuitBreakerTripped") boolean circuitBreakerTripped,
            @JsonProperty("replicaRegions") List<String> replicaRegions,
            @JsonProperty("breakerWindowRows") long breakerWindowRows,
            @JsonProperty("protocolVersion") int protocolVersion,
            @JsonProperty("commitsApplied") long commitsApplied,
            @JsonProperty("storageBytes") long storageBytes,
            @JsonProperty("hasStorageBytes") boolean hasStorageBytes,
            @JsonProperty("rowsWritten") long rowsWritten
    ) {}

    public record ProposeRequest(
            @JsonProperty("change") String change
    ) {}

    public record PushPolicyRequest(
            @JsonProperty("payload") byte[] payload,
            @JsonProperty("signature") byte[] signature,
            @JsonProperty("keyId") String keyId
    ) {}

    public record PushRevocationsRequest(
            @JsonProperty("payload") byte[] payload,
            @JsonProperty("signature") byte[] signature,
            @JsonProperty("keyId") String keyId
    ) {}

    /** `PutIfRequest`'s union: exactly one arm is present, and kind says which. */
    public enum PutIfRequestBodyKind {
            ABSENT("absent"),
            VERSION("version")
;

        private final String wire;

        PutIfRequestBodyKind(String wire) { this.wire = wire; }

        @JsonValue
        public String wire() { return wire; }
    }

    public record PutIfRequestBody(
            @JsonProperty("kind") PutIfRequestBodyKind kind,
            @JsonProperty("value") Object value
    ) {}

    public record PutIfRequest(
            @JsonProperty("key") String key,
            @JsonProperty("value") String value,
            @JsonProperty("ttl") long ttl,
            @JsonProperty("expect") PutIfRequestBody expect
    ) {}

    public record PutRequest(
            @JsonProperty("key") String key,
            @JsonProperty("value") String value,
            @JsonProperty("ttl") long ttl
    ) {}

    public record PutResponse(
            @JsonProperty("commitId") String commitId
    ) {}

    public record QueryPlan(
            @JsonProperty("planHash") long planHash,
            @JsonProperty("rawQuery") String rawQuery,
            @JsonProperty("contextVars") List<KeyValue> contextVars
    ) {}

    public record QueryRequest(
            @JsonProperty("plan") QueryPlan plan
    ) {}

    public record QueryResponse(
            @JsonProperty("resultSet") byte[] resultSet,
            @JsonProperty("planHash") long planHash,
            @JsonProperty("rowCount") long rowCount
    ) {}

    public record RejectRequest(
            @JsonProperty("changeId") String changeId,
            @JsonProperty("reason") String reason
    ) {}

    /** `Request`'s union: exactly one arm is present, and kind says which. */
    public enum RequestBodyKind {
            GET("get"),
            PUT("put"),
            DELETE("delete"),
            QUERY("query"),
            EXPLAIN("explain"),
            PROPOSE_SCHEMA_CHANGE("proposeSchemaChange"),
            APPLY_SCHEMA_CHANGE("applySchemaChange"),
            CREATE_BRANCH("createBranch"),
            MERGE("merge"),
            STATUS("status"),
            PUSH_REVOCATIONS("pushRevocations"),
            AUDIT("audit"),
            LIST_BRANCHES("listBranches"),
            DISCARD_BRANCH("discardBranch"),
            SHOW_CHANGE("showChange"),
            PROMOTE_CHANGE("promoteChange"),
            REJECT_CHANGE("rejectChange"),
            PUSH_POLICY("pushPolicy"),
            PUT_IF("putIf"),
            DESCRIBE("describe"),
            CRDT("crdt"),
            SIGNED_WRITE("signedWrite"),
            REVIEW_QUEUE("reviewQueue")
;

        private final String wire;

        RequestBodyKind(String wire) { this.wire = wire; }

        @JsonValue
        public String wire() { return wire; }
    }

    public record RequestBody(
            @JsonProperty("kind") RequestBodyKind kind,
            @JsonProperty("value") Object value
    ) {}

    public record Request(
            @JsonProperty("requestId") long requestId,
            @JsonProperty("branchId") long branchId,
            @JsonProperty("body") RequestBody body
    ) {}

    /** `Response`'s union: exactly one arm is present, and kind says which. */
    public enum ResponseBodyKind {
            ERROR("error"),
            GET("get"),
            PUT("put"),
            DELETE("delete"),
            QUERY("query"),
            EXPLAIN("explain"),
            PROPOSE("propose"),
            APPLY("apply"),
            BRANCH("branch"),
            MERGE("merge"),
            STATUS("status"),
            REVOCATIONS("revocations"),
            AUDIT("audit"),
            BRANCHES("branches"),
            CHANGE("change"),
            OK("ok"),
            POLICY("policy"),
            PRECONDITION("precondition"),
            DESCRIPTION("description"),
            REVIEW_QUEUE("reviewQueue")
;

        private final String wire;

        ResponseBodyKind(String wire) { this.wire = wire; }

        @JsonValue
        public String wire() { return wire; }
    }

    public record ResponseBody(
            @JsonProperty("kind") ResponseBodyKind kind,
            @JsonProperty("value") Object value
    ) {}

    public record Response(
            @JsonProperty("requestId") long requestId,
            @JsonProperty("body") ResponseBody body
    ) {}

    public record ReviewBatchWire(
            @JsonProperty("key") String key,
            @JsonProperty("gate") int gate,
            @JsonProperty("changes") List<ChangeDiff> changes,
            @JsonProperty("rowsAffected") long rowsAffected,
            @JsonProperty("cost") int cost,
            @JsonProperty("costIfUnbatched") int costIfUnbatched,
            @JsonProperty("reason") String reason
    ) {}

    public record ReviewQueueRequest(

    ) {}

    public record ReviewQueueResponse(
            @JsonProperty("batches") List<ReviewBatchWire> batches
    ) {}

    public record RevocationsAccepted(
            @JsonProperty("version") long version,
            @JsonProperty("accepted") boolean accepted
    ) {}

    public record SchemaDescription(
            @JsonProperty("tables") List<TableDescription> tables,
            @JsonProperty("examplesWithheld") boolean examplesWithheld,
            @JsonProperty("withheldReason") String withheldReason
    ) {}

    public record SeqInsertOp(
            @JsonProperty("after") ElemId after,
            @JsonProperty("value") String value
    ) {}

    /** `SignedWriteRequest`'s union: exactly one arm is present, and kind says which. */
    public enum SignedWriteRequestBodyKind {
            PUT("put"),
            DELETE("delete"),
            CRDT("crdt")
;

        private final String wire;

        SignedWriteRequestBodyKind(String wire) { this.wire = wire; }

        @JsonValue
        public String wire() { return wire; }
    }

    public record SignedWriteRequestBody(
            @JsonProperty("kind") SignedWriteRequestBodyKind kind,
            @JsonProperty("value") Object value
    ) {}

    public record SignedWriteRequest(
            @JsonProperty("commitId") long commitId,
            @JsonProperty("timestampMs") long timestampMs,
            @JsonProperty("signature") byte[] signature,
            @JsonProperty("op") SignedWriteRequestBody op
    ) {}

    public record StatusRequest(

    ) {}

    public record TableDescription(
            @JsonProperty("name") String name,
            @JsonProperty("rowCount") long rowCount,
            @JsonProperty("columns") List<ColumnDescription> columns
    ) {}

    public record ValidationCheckWire(
            @JsonProperty("name") String name,
            @JsonProperty("passed") boolean passed,
            @JsonProperty("detail") String detail,
            @JsonProperty("samples") String samples
    ) {}

    public record Welcome(
            @JsonProperty("protocolVersion") int protocolVersion,
            @JsonProperty("projectId") String projectId,
            @JsonProperty("serverName") String serverName
    ) {}
}
