// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.
//
// Field names are exported (capitalised) here and camelCase on the wire; the
// JSON tag on each field is what keeps the wire name authoritative.

package thetabase

type Gate string

const (
	GateAutoApply      Gate = "autoApply"
	GateConfirm        Gate = "confirm"
	GateShadowValidate Gate = "shadowValidate"
)

type GateRule string

const (
	GateRuleIrreversibleOverShadowThreshold GateRule = "irreversibleOverShadowThreshold"
	GateRuleDestructive                     GateRule = "destructive"
	GateRuleOverRowImpactThreshold          GateRule = "overRowImpactThreshold"
	GateRuleAutoApprovedByPolicy            GateRule = "autoApprovedByPolicy"
	GateRuleWithinThresholds                GateRule = "withinThresholds"
)

type Remedy string

const (
	RemedyNone                   Remedy = "none"
	RemedyConfirm                Remedy = "confirm"
	RemedyValidateOnShadowBranch Remedy = "validateOnShadowBranch"
	RemedyReduceBlastRadius      Remedy = "reduceBlastRadius"
)

type Status string

const (
	StatusOk       Status = "ok"
	StatusConflict Status = "conflict"
	StatusBlocked  Status = "blocked"
	StatusUpToDate Status = "upToDate"
)

type ApplyRequest struct {
	ChangeId      string `json:"changeId"`
	RetiredChange string `json:"retiredChange"`
	Confirm       bool   `json:"confirm"`
}

type AuditEntryWire struct {
	Risk        int32  `json:"risk"`
	Summary     string `json:"summary"`
	Author      string `json:"author"`
	TimestampMs int64  `json:"timestampMs"`
	Detail      string `json:"detail"`
}

type AuditRequest struct {
	Limit   int32 `json:"limit"`
	MinRisk int32 `json:"minRisk"`
}

type AuditResponse struct {
	Entries []AuditEntryWire `json:"entries"`
}

type BranchInfo struct {
	BranchId   int64  `json:"branchId"`
	Name       string `json:"name"`
	Kind       int32  `json:"kind"`
	Head       string `json:"head"`
	Protected  bool   `json:"protected"`
	NextCommit int64  `json:"nextCommit"`
}

type BranchListResponse struct {
	Branches []BranchInfo `json:"branches"`
}

type BranchRequest struct {
	Name string `json:"name"`
	From int64  `json:"from"`
}

type BranchResponse struct {
	BranchId int64 `json:"branchId"`
}

type ChangeDiff struct {
	ChangeId        string   `json:"changeId"`
	Destructive     bool     `json:"destructive"`
	RowsAffected    int64    `json:"rowsAffected"`
	Reversible      bool     `json:"reversible"`
	EstimatedCostMs int32    `json:"estimatedCostMs"`
	RequiresConfirm bool     `json:"requiresConfirm"`
	ShadowBranchId  int64    `json:"shadowBranchId"`
	Reason          string   `json:"reason"`
	AffectedTable   string   `json:"affectedTable"`
	AffectedColumn  string   `json:"affectedColumn"`
	ChangeType      string   `json:"changeType"`
	Gate            Gate     `json:"gate"`
	Rule            GateRule `json:"rule"`
	Remedy          Remedy   `json:"remedy"`
	RuleThreshold   int64    `json:"ruleThreshold"`
}

type ChangeRequest struct {
	ChangeId string `json:"changeId"`
}

type ChangeStateResponse struct {
	Diff              ChangeDiff            `json:"diff"`
	HasShadow         bool                  `json:"hasShadow"`
	ShadowBranchId    int64                 `json:"shadowBranchId"`
	HasValidation     bool                  `json:"hasValidation"`
	ValidationPassed  bool                  `json:"validationPassed"`
	ValidationSummary string                `json:"validationSummary"`
	Checks            []ValidationCheckWire `json:"checks"`
}

type ColumnDescription struct {
	Name            string   `json:"name"`
	Type            string   `json:"type"`
	Nullable        bool     `json:"nullable"`
	Crdt            string   `json:"crdt"`
	DeclaredAt      string   `json:"declaredAt"`
	TouchedByAgent  bool     `json:"touchedByAgent"`
	Examples        []string `json:"examples"`
	NullBasisPoints int32    `json:"nullBasisPoints"`
}

type ConflictRef struct {
	Key    string `json:"key"`
	Ours   string `json:"ours"`
	Theirs string `json:"theirs"`
	Reason string `json:"reason"`
}

// CrdtRequestBody is `CrdtRequest`'s union: exactly one arm is present, and
// Kind says which.
type CrdtRequestBodyKind string

const (
	CrdtRequestBodyKindIncrement   CrdtRequestBodyKind = "increment"
	CrdtRequestBodyKindSetRegister CrdtRequestBodyKind = "setRegister"
	CrdtRequestBodyKindSetAdd      CrdtRequestBodyKind = "setAdd"
	CrdtRequestBodyKindSetRemove   CrdtRequestBodyKind = "setRemove"
	CrdtRequestBodyKindSeqInsert   CrdtRequestBodyKind = "seqInsert"
	CrdtRequestBodyKindSeqRemove   CrdtRequestBodyKind = "seqRemove"
)

type CrdtRequestBody struct {
	Kind  CrdtRequestBodyKind `json:"kind"`
	Value any                 `json:"value,omitempty"`
}

type CrdtRequest struct {
	Key      string          `json:"key"`
	Mutation CrdtRequestBody `json:"mutation"`
}

type DeleteRequest struct {
	Key string `json:"key"`
}

type DescribeRequest struct {
	Table           string `json:"table"`
	IncludeExamples bool   `json:"includeExamples"`
	ExampleLimit    int32  `json:"exampleLimit"`
}

type DiscardBranchRequest struct {
	Name string `json:"name"`
}

type ElemId struct {
	Counter int64 `json:"counter"`
	Replica int64 `json:"replica"`
}

type ErrorResponse struct {
	Code    int32      `json:"code"`
	Message string     `json:"message"`
	Diff    ChangeDiff `json:"diff"`
	HasDiff bool       `json:"hasDiff"`
}

type ExplainResponse struct {
	Explanation string `json:"explanation"`
}

type GetRequest struct {
	Key string `json:"key"`
}

type GetResponse struct {
	Found     bool   `json:"found"`
	Value     string `json:"value"`
	VersionId int64  `json:"versionId"`
}

type Hello struct {
	ProtocolVersion int32  `json:"protocolVersion"`
	SessionToken    string `json:"sessionToken"`
	ClientName      string `json:"clientName"`
}

type KeyValue struct {
	Key   string `json:"key"`
	Value []byte `json:"value"`
}

type ListBranchesRequest struct {
}

type MergeRequest struct {
	SourceBranch int64 `json:"sourceBranch"`
	TargetBranch int64 `json:"targetBranch"`
}

type MergeResult struct {
	Status        Status        `json:"status"`
	ConflictCount int32         `json:"conflictCount"`
	Conflicts     []ConflictRef `json:"conflicts"`
	Converged     []string      `json:"converged"`
}

type OkResponse struct {
}

type PolicyAccepted struct {
	Version  int64 `json:"version"`
	Accepted bool  `json:"accepted"`
}

type PreconditionFailed struct {
	Key    string `json:"key"`
	Found  bool   `json:"found"`
	Actual int64  `json:"actual"`
}

type ProjectStatus struct {
	ProjectId             string   `json:"projectId"`
	Branch                string   `json:"branch"`
	WriteVolumeMB         float64  `json:"writeVolumeMB"`
	CircuitBreakerTripped bool     `json:"circuitBreakerTripped"`
	ReplicaRegions        []string `json:"replicaRegions"`
	BreakerWindowRows     int64    `json:"breakerWindowRows"`
	ProtocolVersion       int32    `json:"protocolVersion"`
	CommitsApplied        int64    `json:"commitsApplied"`
	StorageBytes          int64    `json:"storageBytes"`
	HasStorageBytes       bool     `json:"hasStorageBytes"`
	RowsWritten           int64    `json:"rowsWritten"`
}

type ProposeRequest struct {
	Change string `json:"change"`
}

type PushPolicyRequest struct {
	Payload   []byte `json:"payload"`
	Signature []byte `json:"signature"`
	KeyId     string `json:"keyId"`
}

type PushRevocationsRequest struct {
	Payload   []byte `json:"payload"`
	Signature []byte `json:"signature"`
	KeyId     string `json:"keyId"`
}

// PutIfRequestBody is `PutIfRequest`'s union: exactly one arm is present, and
// Kind says which.
type PutIfRequestBodyKind string

const (
	PutIfRequestBodyKindAbsent  PutIfRequestBodyKind = "absent"
	PutIfRequestBodyKindVersion PutIfRequestBodyKind = "version"
)

type PutIfRequestBody struct {
	Kind  PutIfRequestBodyKind `json:"kind"`
	Value any                  `json:"value,omitempty"`
}

type PutIfRequest struct {
	Key    string           `json:"key"`
	Value  string           `json:"value"`
	Ttl    int64            `json:"ttl"`
	Expect PutIfRequestBody `json:"expect"`
}

type PutRequest struct {
	Key   string `json:"key"`
	Value string `json:"value"`
	Ttl   int64  `json:"ttl"`
}

type PutResponse struct {
	CommitId string `json:"commitId"`
}

type QueryPlan struct {
	PlanHash    int64      `json:"planHash"`
	RawQuery    string     `json:"rawQuery"`
	ContextVars []KeyValue `json:"contextVars"`
}

type QueryRequest struct {
	Plan QueryPlan `json:"plan"`
}

type QueryResponse struct {
	ResultSet []byte `json:"resultSet"`
	PlanHash  int64  `json:"planHash"`
	RowCount  int64  `json:"rowCount"`
}

type RejectRequest struct {
	ChangeId string `json:"changeId"`
	Reason   string `json:"reason"`
}

// RequestBody is `Request`'s union: exactly one arm is present, and
// Kind says which.
type RequestBodyKind string

const (
	RequestBodyKindGet                 RequestBodyKind = "get"
	RequestBodyKindPut                 RequestBodyKind = "put"
	RequestBodyKindDelete              RequestBodyKind = "delete"
	RequestBodyKindQuery               RequestBodyKind = "query"
	RequestBodyKindExplain             RequestBodyKind = "explain"
	RequestBodyKindProposeSchemaChange RequestBodyKind = "proposeSchemaChange"
	RequestBodyKindApplySchemaChange   RequestBodyKind = "applySchemaChange"
	RequestBodyKindCreateBranch        RequestBodyKind = "createBranch"
	RequestBodyKindMerge               RequestBodyKind = "merge"
	RequestBodyKindStatus              RequestBodyKind = "status"
	RequestBodyKindPushRevocations     RequestBodyKind = "pushRevocations"
	RequestBodyKindAudit               RequestBodyKind = "audit"
	RequestBodyKindListBranches        RequestBodyKind = "listBranches"
	RequestBodyKindDiscardBranch       RequestBodyKind = "discardBranch"
	RequestBodyKindShowChange          RequestBodyKind = "showChange"
	RequestBodyKindPromoteChange       RequestBodyKind = "promoteChange"
	RequestBodyKindRejectChange        RequestBodyKind = "rejectChange"
	RequestBodyKindPushPolicy          RequestBodyKind = "pushPolicy"
	RequestBodyKindPutIf               RequestBodyKind = "putIf"
	RequestBodyKindDescribe            RequestBodyKind = "describe"
	RequestBodyKindCrdt                RequestBodyKind = "crdt"
	RequestBodyKindSignedWrite         RequestBodyKind = "signedWrite"
	RequestBodyKindReviewQueue         RequestBodyKind = "reviewQueue"
)

type RequestBody struct {
	Kind  RequestBodyKind `json:"kind"`
	Value any             `json:"value,omitempty"`
}

type Request struct {
	RequestId int64       `json:"requestId"`
	BranchId  int64       `json:"branchId"`
	Body      RequestBody `json:"body"`
}

// ResponseBody is `Response`'s union: exactly one arm is present, and
// Kind says which.
type ResponseBodyKind string

const (
	ResponseBodyKindError        ResponseBodyKind = "error"
	ResponseBodyKindGet          ResponseBodyKind = "get"
	ResponseBodyKindPut          ResponseBodyKind = "put"
	ResponseBodyKindDelete       ResponseBodyKind = "delete"
	ResponseBodyKindQuery        ResponseBodyKind = "query"
	ResponseBodyKindExplain      ResponseBodyKind = "explain"
	ResponseBodyKindPropose      ResponseBodyKind = "propose"
	ResponseBodyKindApply        ResponseBodyKind = "apply"
	ResponseBodyKindBranch       ResponseBodyKind = "branch"
	ResponseBodyKindMerge        ResponseBodyKind = "merge"
	ResponseBodyKindStatus       ResponseBodyKind = "status"
	ResponseBodyKindRevocations  ResponseBodyKind = "revocations"
	ResponseBodyKindAudit        ResponseBodyKind = "audit"
	ResponseBodyKindBranches     ResponseBodyKind = "branches"
	ResponseBodyKindChange       ResponseBodyKind = "change"
	ResponseBodyKindOk           ResponseBodyKind = "ok"
	ResponseBodyKindPolicy       ResponseBodyKind = "policy"
	ResponseBodyKindPrecondition ResponseBodyKind = "precondition"
	ResponseBodyKindDescription  ResponseBodyKind = "description"
	ResponseBodyKindReviewQueue  ResponseBodyKind = "reviewQueue"
)

type ResponseBody struct {
	Kind  ResponseBodyKind `json:"kind"`
	Value any              `json:"value,omitempty"`
}

type Response struct {
	RequestId int64        `json:"requestId"`
	Body      ResponseBody `json:"body"`
}

type ReviewBatchWire struct {
	Key             string       `json:"key"`
	Gate            int32        `json:"gate"`
	Changes         []ChangeDiff `json:"changes"`
	RowsAffected    int64        `json:"rowsAffected"`
	Cost            int32        `json:"cost"`
	CostIfUnbatched int32        `json:"costIfUnbatched"`
	Reason          string       `json:"reason"`
}

type ReviewQueueRequest struct {
}

type ReviewQueueResponse struct {
	Batches []ReviewBatchWire `json:"batches"`
}

type RevocationsAccepted struct {
	Version  int64 `json:"version"`
	Accepted bool  `json:"accepted"`
}

type SchemaDescription struct {
	Tables           []TableDescription `json:"tables"`
	ExamplesWithheld bool               `json:"examplesWithheld"`
	WithheldReason   string             `json:"withheldReason"`
}

type SeqInsertOp struct {
	After ElemId `json:"after"`
	Value string `json:"value"`
}

// SignedWriteRequestBody is `SignedWriteRequest`'s union: exactly one arm is present, and
// Kind says which.
type SignedWriteRequestBodyKind string

const (
	SignedWriteRequestBodyKindPut    SignedWriteRequestBodyKind = "put"
	SignedWriteRequestBodyKindDelete SignedWriteRequestBodyKind = "delete"
	SignedWriteRequestBodyKindCrdt   SignedWriteRequestBodyKind = "crdt"
)

type SignedWriteRequestBody struct {
	Kind  SignedWriteRequestBodyKind `json:"kind"`
	Value any                        `json:"value,omitempty"`
}

type SignedWriteRequest struct {
	CommitId    int64                  `json:"commitId"`
	TimestampMs int64                  `json:"timestampMs"`
	Signature   []byte                 `json:"signature"`
	Op          SignedWriteRequestBody `json:"op"`
}

type StatusRequest struct {
}

type TableDescription struct {
	Name     string              `json:"name"`
	RowCount int64               `json:"rowCount"`
	Columns  []ColumnDescription `json:"columns"`
}

type ValidationCheckWire struct {
	Name    string `json:"name"`
	Passed  bool   `json:"passed"`
	Detail  string `json:"detail"`
	Samples string `json:"samples"`
}

type Welcome struct {
	ProtocolVersion int32  `json:"protocolVersion"`
	ProjectId       string `json:"projectId"`
	ServerName      string `json:"serverName"`
}
