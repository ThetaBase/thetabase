// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place
// it can be read without a stale copy to compare against.
//
// 64-bit integers are `bigint`. Above 2^53 a JavaScript `number` silently
// rounds, and a commit id or row impact that quietly changes value would be
// worse than the inconvenience of a distinct type.

export type Gate =
  | "autoApply"
  | "confirm"
  | "shadowValidate";

export type GateRule =
  | "irreversibleOverShadowThreshold"
  | "destructive"
  | "overRowImpactThreshold"
  | "autoApprovedByPolicy"
  | "withinThresholds";

export type Remedy =
  | "none"
  | "confirm"
  | "validateOnShadowBranch"
  | "reduceBlastRadius";

export type Status =
  | "ok"
  | "conflict"
  | "blocked"
  | "upToDate";

export interface ApplyRequest {
  changeId: string;
  retiredChange: string;
  confirm: boolean;
}

export interface AuditEntryWire {
  risk: number;
  summary: string;
  author: string;
  timestampMs: bigint;
  detail: string;
}

export interface AuditRequest {
  limit: number;
  minRisk: number;
}

export interface AuditResponse {
  entries: AuditEntryWire[];
}

export interface BranchInfo {
  branchId: bigint;
  name: string;
  kind: number;
  head: string;
  protected: boolean;
  nextCommit: bigint;
}

export interface BranchListResponse {
  branches: BranchInfo[];
}

export interface BranchRequest {
  name: string;
  from: bigint;
}

export interface BranchResponse {
  branchId: bigint;
}

export interface ChangeDiff {
  changeId: string;
  destructive: boolean;
  rowsAffected: bigint;
  reversible: boolean;
  estimatedCostMs: number;
  requiresConfirm: boolean;
  shadowBranchId: bigint;
  reason: string;
  affectedTable: string;
  affectedColumn: string;
  changeType: string;
  gate: Gate;
  rule: GateRule;
  remedy: Remedy;
  ruleThreshold: bigint;
}

export interface ChangeRequest {
  changeId: string;
}

export interface ChangeStateResponse {
  diff: ChangeDiff;
  hasShadow: boolean;
  shadowBranchId: bigint;
  hasValidation: boolean;
  validationPassed: boolean;
  validationSummary: string;
  checks: ValidationCheckWire[];
}

export interface ColumnDescription {
  name: string;
  type: string;
  nullable: boolean;
  crdt: string;
  declaredAt: string;
  touchedByAgent: boolean;
  examples: string[];
  nullBasisPoints: number;
}

export interface ConflictRef {
  key: string;
  ours: string;
  theirs: string;
  reason: string;
}

/** `CrdtRequest.mutation`: exactly one arm is present. */
export type CrdtRequestBody =
  | { kind: "increment"; value: bigint }
  | { kind: "setRegister"; value: string }
  | { kind: "setAdd"; value: string }
  | { kind: "setRemove"; value: string }
  | { kind: "seqInsert"; value: SeqInsertOp }
  | { kind: "seqRemove"; value: ElemId };

export interface CrdtRequest {
  key: string;
  mutation: CrdtRequestBody;
}

export interface DeleteRequest {
  key: string;
}

export interface DescribeRequest {
  table: string;
  includeExamples: boolean;
  exampleLimit: number;
}

export interface DiscardBranchRequest {
  name: string;
}

export interface ElemId {
  counter: bigint;
  replica: bigint;
}

export interface ErrorResponse {
  code: number;
  message: string;
  diff: ChangeDiff;
  hasDiff: boolean;
}

export interface ExplainResponse {
  explanation: string;
}

export interface GetRequest {
  key: string;
}

export interface GetResponse {
  found: boolean;
  value: string;
  versionId: bigint;
}

export interface Hello {
  protocolVersion: number;
  sessionToken: string;
  clientName: string;
}

export interface KeyValue {
  key: string;
  value: Uint8Array;
}

export interface ListBranchesRequest {
}

export interface MergeRequest {
  sourceBranch: bigint;
  targetBranch: bigint;
}

export interface MergeResult {
  status: Status;
  conflictCount: number;
  conflicts: ConflictRef[];
  converged: string[];
}

export interface OkResponse {
}

export interface PolicyAccepted {
  version: bigint;
  accepted: boolean;
}

export interface PreconditionFailed {
  key: string;
  found: boolean;
  actual: bigint;
}

export interface ProjectStatus {
  projectId: string;
  branch: string;
  writeVolumeMB: number;
  circuitBreakerTripped: boolean;
  replicaRegions: string[];
  breakerWindowRows: bigint;
  protocolVersion: number;
  commitsApplied: bigint;
  storageBytes: bigint;
  hasStorageBytes: boolean;
  rowsWritten: bigint;
}

export interface ProposeRequest {
  change: string;
}

export interface PushPolicyRequest {
  payload: Uint8Array;
  signature: Uint8Array;
  keyId: string;
}

export interface PushRevocationsRequest {
  payload: Uint8Array;
  signature: Uint8Array;
  keyId: string;
}

/** `PutIfRequest.expect`: exactly one arm is present. */
export type PutIfRequestBody =
  | { kind: "absent" }
  | { kind: "version"; value: bigint };

export interface PutIfRequest {
  key: string;
  value: string;
  ttl: bigint;
  expect: PutIfRequestBody;
}

export interface PutRequest {
  key: string;
  value: string;
  ttl: bigint;
}

export interface PutResponse {
  commitId: string;
}

export interface QueryPlan {
  planHash: bigint;
  rawQuery: string;
  contextVars: KeyValue[];
}

export interface QueryRequest {
  plan: QueryPlan;
}

export interface QueryResponse {
  resultSet: Uint8Array;
  planHash: bigint;
  rowCount: bigint;
}

export interface RejectRequest {
  changeId: string;
  reason: string;
}

/** `Request.body`: exactly one arm is present. */
export type RequestBody =
  | { kind: "get"; value: GetRequest }
  | { kind: "put"; value: PutRequest }
  | { kind: "delete"; value: DeleteRequest }
  | { kind: "query"; value: QueryRequest }
  | { kind: "explain"; value: QueryRequest }
  | { kind: "proposeSchemaChange"; value: ProposeRequest }
  | { kind: "applySchemaChange"; value: ApplyRequest }
  | { kind: "createBranch"; value: BranchRequest }
  | { kind: "merge"; value: MergeRequest }
  | { kind: "status"; value: StatusRequest }
  | { kind: "pushRevocations"; value: PushRevocationsRequest }
  | { kind: "audit"; value: AuditRequest }
  | { kind: "listBranches"; value: ListBranchesRequest }
  | { kind: "discardBranch"; value: DiscardBranchRequest }
  | { kind: "showChange"; value: ChangeRequest }
  | { kind: "promoteChange"; value: ChangeRequest }
  | { kind: "rejectChange"; value: RejectRequest }
  | { kind: "pushPolicy"; value: PushPolicyRequest }
  | { kind: "putIf"; value: PutIfRequest }
  | { kind: "describe"; value: DescribeRequest }
  | { kind: "crdt"; value: CrdtRequest }
  | { kind: "signedWrite"; value: SignedWriteRequest }
  | { kind: "reviewQueue"; value: ReviewQueueRequest };

export interface Request {
  requestId: bigint;
  branchId: bigint;
  body: RequestBody;
}

/** `Response.body`: exactly one arm is present. */
export type ResponseBody =
  | { kind: "error"; value: ErrorResponse }
  | { kind: "get"; value: GetResponse }
  | { kind: "put"; value: PutResponse }
  | { kind: "delete"; value: PutResponse }
  | { kind: "query"; value: QueryResponse }
  | { kind: "explain"; value: ExplainResponse }
  | { kind: "propose"; value: ChangeDiff }
  | { kind: "apply"; value: PutResponse }
  | { kind: "branch"; value: BranchResponse }
  | { kind: "merge"; value: MergeResult }
  | { kind: "status"; value: ProjectStatus }
  | { kind: "revocations"; value: RevocationsAccepted }
  | { kind: "audit"; value: AuditResponse }
  | { kind: "branches"; value: BranchListResponse }
  | { kind: "change"; value: ChangeStateResponse }
  | { kind: "ok"; value: OkResponse }
  | { kind: "policy"; value: PolicyAccepted }
  | { kind: "precondition"; value: PreconditionFailed }
  | { kind: "description"; value: SchemaDescription }
  | { kind: "reviewQueue"; value: ReviewQueueResponse };

export interface Response {
  requestId: bigint;
  body: ResponseBody;
}

export interface ReviewBatchWire {
  key: string;
  gate: number;
  changes: ChangeDiff[];
  rowsAffected: bigint;
  cost: number;
  costIfUnbatched: number;
  reason: string;
}

export interface ReviewQueueRequest {
}

export interface ReviewQueueResponse {
  batches: ReviewBatchWire[];
}

export interface RevocationsAccepted {
  version: bigint;
  accepted: boolean;
}

export interface SchemaDescription {
  tables: TableDescription[];
  examplesWithheld: boolean;
  withheldReason: string;
}

export interface SeqInsertOp {
  after: ElemId;
  value: string;
}

/** `SignedWriteRequest.op`: exactly one arm is present. */
export type SignedWriteRequestBody =
  | { kind: "put"; value: PutRequest }
  | { kind: "delete"; value: DeleteRequest }
  | { kind: "crdt"; value: CrdtRequest };

export interface SignedWriteRequest {
  commitId: bigint;
  timestampMs: bigint;
  signature: Uint8Array;
  op: SignedWriteRequestBody;
}

export interface StatusRequest {
}

export interface TableDescription {
  name: string;
  rowCount: bigint;
  columns: ColumnDescription[];
}

export interface ValidationCheckWire {
  name: string;
  passed: boolean;
  detail: string;
  samples: string;
}

export interface Welcome {
  protocolVersion: number;
  projectId: string;
  serverName: string;
}
