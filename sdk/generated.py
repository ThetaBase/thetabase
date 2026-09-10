# Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
#
# DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
# if this file and the schema disagree.
#
# The prose explaining each field lives in the schema, which is the one place it
# can be read without a stale copy to compare against.
#
# Field names are snake_case here and camelCase on the wire. The mapping is
# mechanical and total, so a name can always be recovered in either direction.

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal, Union

Gate = Literal["autoApply", "confirm", "shadowValidate"]

Status = Literal["ok", "conflict", "blocked", "upToDate"]

@dataclass(frozen=True)
class ApplyRequest:
    change_id: str
    retired_change: str
    confirm: bool

@dataclass(frozen=True)
class AuditEntryWire:
    risk: int
    summary: str
    author: str
    timestamp_ms: int
    detail: str

@dataclass(frozen=True)
class AuditRequest:
    limit: int
    min_risk: int

@dataclass(frozen=True)
class AuditResponse:
    entries: list["AuditEntryWire"]

@dataclass(frozen=True)
class BranchInfo:
    branch_id: int
    name: str
    kind: int
    head: str
    protected: bool

@dataclass(frozen=True)
class BranchListResponse:
    branches: list["BranchInfo"]

@dataclass(frozen=True)
class BranchRequest:
    name: str
    from_: int

@dataclass(frozen=True)
class BranchResponse:
    branch_id: int

@dataclass(frozen=True)
class ChangeDiff:
    change_id: str
    destructive: bool
    rows_affected: int
    reversible: bool
    estimated_cost_ms: int
    requires_confirm: bool
    shadow_branch_id: int
    reason: str
    affected_table: str
    affected_column: str
    change_type: str
    gate: "Gate"

@dataclass(frozen=True)
class ChangeRequest:
    change_id: str

@dataclass(frozen=True)
class ChangeStateResponse:
    diff: "ChangeDiff"
    has_shadow: bool
    shadow_branch_id: int
    has_validation: bool
    validation_passed: bool
    validation_summary: str
    checks: list["ValidationCheckWire"]

@dataclass(frozen=True)
class ConflictRef:
    key: str
    ours: str
    theirs: str
    reason: str

@dataclass(frozen=True)
class DeleteRequest:
    key: str

@dataclass(frozen=True)
class DiscardBranchRequest:
    name: str

@dataclass(frozen=True)
class ErrorResponse:
    code: int
    message: str
    diff: "ChangeDiff"
    has_diff: bool

@dataclass(frozen=True)
class ExplainResponse:
    explanation: str

@dataclass(frozen=True)
class GetRequest:
    key: str

@dataclass(frozen=True)
class GetResponse:
    found: bool
    value: str
    version_id: int

@dataclass(frozen=True)
class Hello:
    protocol_version: int
    session_token: str
    client_name: str

@dataclass(frozen=True)
class KeyValue:
    key: str
    value: bytes

@dataclass(frozen=True)
class ListBranchesRequest:
    pass

@dataclass(frozen=True)
class MergeRequest:
    source_branch: int
    target_branch: int

@dataclass(frozen=True)
class MergeResult:
    status: "Status"
    conflict_count: int
    conflicts: list["ConflictRef"]
    converged: list[str]

@dataclass(frozen=True)
class OkResponse:
    pass

@dataclass(frozen=True)
class PolicyAccepted:
    version: int
    accepted: bool

@dataclass(frozen=True)
class PreconditionFailed:
    key: str
    found: bool
    actual: int

@dataclass(frozen=True)
class ProjectStatus:
    project_id: str
    branch: str
    write_volume_m_b: float
    circuit_breaker_tripped: bool
    replica_regions: list[str]
    breaker_window_rows: int
    protocol_version: int
    commits_applied: int
    storage_bytes: int
    has_storage_bytes: bool
    rows_written: int

@dataclass(frozen=True)
class ProposeRequest:
    change: str

@dataclass(frozen=True)
class PushPolicyRequest:
    payload: bytes
    signature: bytes
    key_id: str

@dataclass(frozen=True)
class PushRevocationsRequest:
    payload: bytes
    signature: bytes
    key_id: str

# Exactly one arm is present; `expect_kind` says which.
PutIfRequestKind = Literal["absent", "version"]

@dataclass(frozen=True)
class PutIfRequest:
    key: str
    value: str
    ttl: int
    expect_kind: PutIfRequestKind
    expect_value: object | None = None

@dataclass(frozen=True)
class PutRequest:
    key: str
    value: str
    ttl: int

@dataclass(frozen=True)
class PutResponse:
    commit_id: str

@dataclass(frozen=True)
class QueryPlan:
    plan_hash: int
    raw_query: str
    context_vars: list["KeyValue"]

@dataclass(frozen=True)
class QueryRequest:
    plan: "QueryPlan"

@dataclass(frozen=True)
class QueryResponse:
    result_set: bytes
    plan_hash: int
    row_count: int

@dataclass(frozen=True)
class RejectRequest:
    change_id: str
    reason: str

# Exactly one arm is present; `body_kind` says which.
RequestKind = Literal["get", "put", "delete", "query", "explain", "proposeSchemaChange", "applySchemaChange", "createBranch", "merge", "status", "pushRevocations", "audit", "listBranches", "discardBranch", "showChange", "promoteChange", "rejectChange", "pushPolicy", "putIf"]

@dataclass(frozen=True)
class Request:
    request_id: int
    branch_id: int
    body_kind: RequestKind
    body_value: object | None = None

# Exactly one arm is present; `body_kind` says which.
ResponseKind = Literal["error", "get", "put", "delete", "query", "explain", "propose", "apply", "branch", "merge", "status", "revocations", "audit", "branches", "change", "ok", "policy", "precondition"]

@dataclass(frozen=True)
class Response:
    request_id: int
    body_kind: ResponseKind
    body_value: object | None = None

@dataclass(frozen=True)
class RevocationsAccepted:
    version: int
    accepted: bool

@dataclass(frozen=True)
class StatusRequest:
    pass

@dataclass(frozen=True)
class ValidationCheckWire:
    name: str
    passed: bool
    detail: str
    samples: str

@dataclass(frozen=True)
class Welcome:
    protocol_version: int
    project_id: str
    server_name: str
