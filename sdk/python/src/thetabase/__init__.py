"""ThetaBase Python SDK.

STATUS: types and surface only — every method raises. The transport exists
(ROADMAP M2, and ``thetad`` serves the full surface), but nothing here is wired
to it, because from M6 this module is generated from
``crates/theta-proto/schema/theta.capnp`` and hand-written bodies would be
overwritten. M6 is what delivers a working SDK.

The surface mirrors the TypeScript SDK exactly, because both are generated from
one protocol definition — that is what keeps an agent's generated code in
lockstep with the live schema (docs/specs/02, §3).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Literal

__all__ = [
    "Theta",
    "AuditEntry",
    "BranchInfo",
    "ChangeDiff",
    "ChangeState",
    "CircuitBreakerError",
    "ConflictRef",
    "Explain",
    "Gate",
    "MergeResult",
    "ProjectStatus",
    "SafetyGateError",
    "ValidationCheck",
]

Value = Any
Environment = Literal["dev", "preview", "prod"]


# The wire types are not restated here.
#
# They used to be, and they had drifted: this module's ``ChangeDiff`` had no
# ``gate`` — the field that says whether confirmation is even a route — its
# ``ConflictRef`` carried a table and column the wire does not send, and its
# ``MergeResult`` status was missing ``up_to_date``. Code written against those
# types was written against a protocol that does not exist.
#
# One definition, generated from the schema, re-exported here so the import path
# stays ``thetabase``.
from .generated import (  # noqa: F401
    AuditEntryWire as AuditEntry,
    BranchInfo,
    ChangeDiff,
    ChangeStateResponse as ChangeState,
    ConflictRef,
    Gate,
    MergeResult,
    ProjectStatus,
    ValidationCheckWire as ValidationCheck,
)

@dataclass(frozen=True)
class Explain:
    plan_hash: str
    steps: list[dict[str, Any]]
    estimated_rows: int
    estimated_cost_ms: int
    indexes_used: list[str]
    #: Always 0 on the typed path. Asserted in CI, not assumed.
    llm_calls: int


class SafetyGateError(Exception):
    """Raised when the Safety Layer refuses a change. Carries the diff to act on."""

    def __init__(self, message: str, diff: ChangeDiff) -> None:
        super().__init__(message)
        self.diff = diff


class CircuitBreakerError(Exception):
    """Raised when the blast-radius breaker is open."""

    def __init__(self, message: str, window_rows: int, ceiling: int) -> None:
        super().__init__(message)
        self.window_rows = window_rows
        self.ceiling = ceiling


def _not_implemented(what: str, milestone: str) -> Any:
    raise NotImplementedError(
        f"{what} is not implemented yet — lands in {milestone} (see docs/ROADMAP.md)"
    )


class _Schema:
    def propose(self, change: dict[str, Any]) -> ChangeDiff:
        """Submit a change. Always returns a diff; never applies anything to the
        target branch.

        A change the rules put at the shadow gate is applied to an ephemeral
        shadow branch and validated there as part of this call, so the diff
        comes back already saying what the checks found. Landing it still takes
        an explicit :meth:`promote`.
        """
        return _not_implemented("Theta.schema.propose", "M6 (Generated SDKs)")

    def show(self, change_id: str) -> ChangeDiff:
        """A proposal's diff, and what validating it found."""
        return _not_implemented("Theta.schema.show", "M6 (Generated SDKs)")

    def apply(self, change_id: str, confirm: bool) -> None:
        """Confirm a proposed change, by id.

        Deliberately takes no change body and no branch: the server applies what
        it classified under this id, on the branch that proposal targeted. A
        caller that could supply either could confirm one change and execute
        another (docs/specs/07, §5.1).

        Confirmation is not a path at all for a change at the shadow gate — that
        one lands through :meth:`promote`.
        """
        return _not_implemented("Theta.schema.apply", "M6 (Generated SDKs)")

    def validate(self, change_id: str) -> dict[str, Any]:
        """Re-run validation against a change's shadow branch.

        Not a required step — :meth:`propose` already ran it. This is for a
        shadow branch that moved afterwards, which makes the earlier result
        stale and blocks promotion until it is re-checked.
        """
        return _not_implemented("Theta.schema.validate", "M6 (Generated SDKs)")

    def promote(self, change_id: str) -> None:
        """Merge a validated shadow branch onto its target. Never re-executes."""
        return _not_implemented("Theta.schema.promote", "M6 (Generated SDKs)")

    def reject(self, change_id: str, reason: str) -> None:
        """Refuse a change and reclaim its shadow branch."""
        return _not_implemented("Theta.schema.reject", "M6 (Generated SDKs)")


class _Branch:
    def create(self, name: str, from_: str | None = None) -> str:
        return _not_implemented("Theta.branch.create", "M6 (Generated SDKs)")

    def merge(self, source: str, into: str = "main") -> MergeResult:
        return _not_implemented("Theta.branch.merge", "M6 (Generated SDKs)")

    def discard(self, name: str) -> None:
        return _not_implemented("Theta.branch.discard", "M6 (Generated SDKs)")


class _Assist:
    """Client for the AI Query-Assist service.

    Plain HTTP rather than the Cap'n Proto transport the rest of this SDK uses,
    because Assist is a separate service that ``thetad`` does not speak to.
    Routing it through the wire protocol would put a model call inside the
    protocol the hot path speaks, which is exactly what ``no_llm_on_hot_path``
    exists to prevent.
    """

    def __init__(self, base_url: str | None) -> None:
        self._base_url = base_url

    def suggest_query(self, prompt: str, schema: dict[str, Any]) -> dict[str, Any]:
        """Translate natural language into a *candidate* typed plan.

        Explicitly invoked, never on the hot path, and never authoritative: the
        candidate must be passed to :meth:`Theta.query` deliberately before
        anything runs. This method cannot execute anything — it returns a
        proposal and stops.

        Raises if no ``assist_url`` was configured. That is not a degraded mode
        to paper over: a caller who did not deploy Assist should hear so rather
        than receive an empty result where a query was expected.
        """
        if not self._base_url:
            raise RuntimeError(
                "AI Query-Assist is not configured. Pass assist_url= to Theta() "
                "with the address of the theta-assist service. It is optional — "
                "everything on the read/write path works without it."
            )

        import json
        import urllib.error
        import urllib.request

        body = json.dumps({"question": prompt, "schema": schema}).encode()
        request = urllib.request.Request(
            self._base_url.rstrip("/") + "/v1/suggest",
            data=body,
            headers={"content-type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request) as response:
                return json.loads(response.read())
        except urllib.error.HTTPError as e:
            # The service says what the caller can do about a refusal; dropping
            # that here would turn actionable advice into a status code.
            detail = json.loads(e.read() or b"{}")
            raise RuntimeError(
                f"assist declined ({e.code}): {detail.get('error', 'no detail')}"
                + (f"\n{detail['remedy']}" if detail.get("remedy") else "")
            ) from None


class Theta:
    """A connection to one ThetaBase project.

    No connection string, no API key, no ``.env``: the project is named and the
    scoped token is resolved and injected by the toolchain.
    """

    def __init__(
        self,
        project: str,
        environment: Environment = "dev",
        branch: str | None = None,
        assist_url: str | None = None,
    ) -> None:
        """
        ``assist_url`` is where AI Query-Assist is running, if it is. ``None``
        is the normal case: Assist is a separate, optional service
        (``specs/01`` §3), and a deployment that never starts it loses
        suggestions and nothing else. Naming it here rather than deriving it
        from the project keeps that separation visible at the call site.
        """
        self.project = project
        self.environment = environment
        self.branch_name = branch
        self.schema = _Schema()
        self.branch = _Branch()
        self.assist = _Assist(assist_url)

    def get(self, key: str) -> Value:
        """Point lookup. Hot path: p50 5ms, and no model call, ever."""
        return _not_implemented("Theta.get", "M6 (Generated SDKs)")

    def put(self, key: str, value: Value) -> dict[str, Any]:
        return _not_implemented("Theta.put", "M6 (Generated SDKs)")

    def query(self, plan: Any) -> list[Value]:
        return _not_implemented("Theta.query", "M6 (Generated SDKs)")

    def explain(self, plan: Any) -> Explain:
        """EXPLAIN without executing — what a reviewer reads before approving."""
        return _not_implemented("Theta.explain", "M6 (Generated SDKs)")

    def status(self) -> ProjectStatus:
        return _not_implemented("Theta.status", "M6 (Generated SDKs)")
