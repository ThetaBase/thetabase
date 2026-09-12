"""ThetaBase Python SDK.

The surface mirrors the TypeScript SDK exactly, because both are generated from
one protocol definition — that is what keeps an agent's generated code in
lockstep with the live schema (docs/specs/02, §3).

This docstring said, until now, "types and surface only — every method raises".
That stopped being true when M6 landed and nobody updated it, and a stale
status note on the first line of an SDK is worse than none: it is the first
thing a reader sees, it is load-bearing for whether they use the library at
all, and one of them sent a build plan down a two-service detour to avoid an
SDK that had worked for weeks. Every method here calls the wire.
"""

from __future__ import annotations

import os
import socket as _socket
import ssl as _ssl
from dataclasses import dataclass, field
from typing import Any, Literal, cast

from .scribe import Connection, ScribeCore

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


def _wants_tls(address: str) -> bool:
    """Whether this address should be dialled with TLS.

    ``THETA_TLS`` wins when it is set, so a self-hosted instance behind a
    terminating proxy on some other port can say so -- and so can a developer
    tunnelling 443 to a local plaintext process.

    Otherwise: port 443 means TLS. That is the port Fly's proxy listens on and
    the port the Control Plane hands out, and it is the only port in this
    product's vocabulary that implies a terminator in front of ``thetad``.
    Local instances are on 7700 and upwards.

    Inferred from the address rather than carried beside it, deliberately, and
    identically to the Rust client. The address travels through
    ``THETA_ADDRESS`` into every SDK; a second variable that had to agree with
    it would be a second thing to get wrong, and the symptom of getting it
    wrong is a *hang* rather than an error -- a plaintext frame sent to a TLS
    listener is not rejected, it is read as a ClientHello, found malformed, and
    the connection dropped or left open.
    """
    override = os.environ.get("THETA_TLS")
    if override is not None:
        if override.lower() in ("1", "true", "require", "yes"):
            return True
        if override.lower() in ("0", "false", "off", "no"):
            return False

    _, _, port = address.rpartition(":")
    return port.strip() == "443"


def _tls_context() -> _ssl.SSLContext:
    """A verifying TLS context, with a deployment's own CA if it named one.

    ``THETA_TLS_CA`` is added to the default roots rather than replacing them.
    A customer running ``thetad`` behind their own terminating proxy signs with
    their own CA, and a client that trusts only the public set cannot reach it
    -- and "turn TLS off instead" is not an answer for a database.

    Said plainly because the difference matters: this widens what the client
    will accept, it does not pin. A deployment that wants only its own CA to be
    acceptable is asking for something this does not provide.

    Verification is never disabled and there is no option to disable it. A flag
    that turns off certificate checking is a flag that ends up set in
    production, and the whole reason this transport exists is that a session
    token was about to cross the open internet.
    """
    context = _ssl.create_default_context()
    ca = os.environ.get("THETA_TLS_CA")
    if ca:
        # Failures are refusals, never warnings. An operator who set this has
        # said "trust this CA"; continuing without it would silently connect
        # under a laxer trust policy than the one they chose.
        context.load_verify_locations(cafile=ca)
    return context


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


def _expect(response: dict[str, Any], kind: str) -> dict[str, Any]:
    """Unwrap a response of the expected kind, or raise something actionable.

    The three named failures stay distinct. A gate refusal carries the diff the
    caller has to act on and is an *answer* rather than a fault; an open breaker
    is a limit this project set rather than a bad request; everything else is an
    error with a message. Collapsing them into one exception would make the
    first two unactionable, which is the reason the wire separates them.
    """
    if response.get("kind") == kind:
        return response.get("value") or {}

    if response.get("kind") == "error":
        value = response.get("value") or {}
        message = str(value.get("message", "the server refused the request"))
        code = str(value.get("code", ""))
        if code == "ConfirmationRequired":
            raise SafetyGateError(message, value.get("diff"))  # type: ignore[arg-type]
        if code == "BreakerOpen":
            raise CircuitBreakerError(
                message, int(value.get("windowRows", 0)), int(value.get("ceiling", 0))
            )
        raise RuntimeError(message)

    # What the core produces for a response this build does not know. Reported
    # as a version problem rather than a parse failure, because that is what it
    # is and upgrading is the remedy.
    if response.get("kind") == "raw":
        raise RuntimeError(
            "the server sent a response this SDK does not understand; it is "
            "newer than this client. Upgrade the SDK."
        )

    raise RuntimeError(f"expected a `{kind}` response, got `{response.get('kind')}`")


class _Schema:
    def __init__(self, theta: "Theta") -> None:
        # A back-reference rather than a copy of the connection: `Theta.connect`
        # attaches the connection after these are built, and a copy taken at
        # construction would be `None` forever.
        self._theta = theta

    def propose(self, change: dict[str, Any]) -> ChangeDiff:
        """Submit a change. Always returns a diff; never applies anything to the
        target branch.

        A change the rules put at the shadow gate is applied to an ephemeral
        shadow branch and validated there as part of this call, so the diff
        comes back already saying what the checks found. Landing it still takes
        an explicit :meth:`promote`.
        """
        response = self._theta._call({"op": "proposeSchemaChange", "change": change})
        return cast(ChangeDiff, _expect(response, "propose"))

    def show(self, change_id: str) -> ChangeDiff:
        """A proposal's diff, and what validating it found."""
        response = self._theta._call({"op": "showChange", "changeId": change_id})
        return cast(ChangeDiff, _expect(response, "change"))

    def apply(self, change_id: str, confirm: bool) -> None:
        """Confirm a proposed change, by id.

        Deliberately takes no change body and no branch: the server applies what
        it classified under this id, on the branch that proposal targeted. A
        caller that could supply either could confirm one change and execute
        another (docs/specs/07, §5.1).

        Confirmation is not a path at all for a change at the shadow gate — that
        one lands through :meth:`promote`.
        """
        response = self._theta._call(
            {"op": "applySchemaChange", "changeId": change_id, "confirm": confirm}
        )
        _expect(response, "commit")

    def validate(self, change_id: str) -> dict[str, Any]:
        """Re-run validation against a change's shadow branch.

        Not a required step — :meth:`propose` already ran it. This is for a
        shadow branch that moved afterwards, which makes the earlier result
        stale and blocks promotion until it is re-checked.
        """
        change = _expect(
            self._theta._call({"op": "showChange", "changeId": change_id}), "change"
        )
        return {
            "shadowBranch": str(change.get("shadowBranchId", "")),
            "passed": change.get("validationPassed") is True,
        }

    def promote(self, change_id: str) -> None:
        """Merge a validated shadow branch onto its target. Never re-executes."""
        _expect(self._theta._call({"op": "promoteChange", "changeId": change_id}), "commit")

    def reject(self, change_id: str, reason: str) -> None:
        """Refuse a change and reclaim its shadow branch."""
        _expect(
            self._theta._call(
                {"op": "rejectChange", "changeId": change_id, "reason": reason}
            ),
            "ok",
        )


class _Branch:
    def __init__(self, theta: "Theta") -> None:
        self._theta = theta

    def create(self, name: str, from_: str | None = None) -> str:
        response = self._theta._call({"op": "createBranch", "name": name, "from": from_})
        return str(_expect(response, "branch")["branchId"])

    def merge(self, source: str, into: str = "main") -> MergeResult:
        response = self._theta._call(
            {"op": "merge", "sourceBranch": source, "targetBranch": into}
        )
        return cast(MergeResult, _expect(response, "merge"))

    def discard(self, name: str) -> None:
        _expect(self._theta._call({"op": "discardBranch", "name": name}), "ok")


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
        self._connection: Connection | None = None
        self._core: ScribeCore | None = None
        self.schema = _Schema(self)
        self.branch = _Branch(self)
        self.assist = _Assist(assist_url)

    @classmethod
    def connect(
        cls,
        project: str | None = None,
        environment: Environment = "dev",
        branch: str | None = None,
        assist_url: str | None = None,
    ) -> "Theta":
        """Open a connection, taking the address and token from the environment.

        There is deliberately no address or token parameter. ``theta exec``
        resolves a scoped token for the project and injects both; an SDK that
        accepted a connection string would undo the property that exists to
        remove one from application code.
        """
        address = os.environ.get("THETA_ADDRESS")
        token = os.environ.get("THETA_TOKEN")
        if not address or not token:
            raise RuntimeError(
                "THETA_ADDRESS and THETA_TOKEN are not both set. Run this "
                "process under `theta exec`, which resolves a scoped token for "
                "the project and injects both."
            )

        core = ScribeCore()
        host, _, port = address.rpartition(":")
        sock: _socket.socket = _socket.create_connection((host, int(port)))

        # Nagle off. Every exchange is one small frame and then a wait for the
        # answer, which is the exact shape Nagle delays.
        sock.setsockopt(_socket.IPPROTO_TCP, _socket.TCP_NODELAY, 1)

        if _wants_tls(address):
            # ``server_hostname`` is the SNI name, and on a shared address it is
            # what the proxy routes on -- so getting it wrong does not produce a
            # certificate error, it produces a connection to the wrong instance
            # or to none.
            sock = _tls_context().wrap_socket(sock, server_hostname=host)

        connection = Connection(core, sock)
        # The handshake, before anything else travels. `thetad` also
        # re-authorises on every request, so this is the introduction rather
        # than the whole of the authentication.
        welcome_body = connection.send_raw(core.encode_hello(token, "thetabase-python"))
        core.decode_welcome(welcome_body)

        theta = cls(
            project or os.environ.get("THETA_PROJECT", ""),
            environment,
            branch,
            assist_url,
        )
        theta._connection = connection
        theta._core = core
        return theta

    def close(self) -> None:
        """Release the connection. Further calls will fail."""
        if self._connection is not None:
            self._connection.close()
            self._connection = None

    def __enter__(self) -> "Theta":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()

    def _call(self, request: dict[str, Any]) -> dict[str, Any]:
        if self._connection is None:
            raise RuntimeError(
                "this Theta is not connected. Build it with `Theta.connect(...)` "
                "rather than by calling the constructor, which exists for tests "
                "that supply their own connection."
            )
        return self._connection.call(request)

    def get(self, key: str) -> Value:
        """Point lookup. Hot path: p50 5ms, and no model call, ever."""
        value = _expect(self._call({"op": "get", "key": key}), "get")
        # `found: False` is a successful answer meaning the row is not there,
        # and it is distinct from a null value that is.
        return value.get("value") if value.get("found") else None

    def put(self, key: str, value: Value) -> dict[str, Any]:
        response = self._call({"op": "put", "key": key, "value": value})
        return {"commitId": _expect(response, "commit")["commitId"]}

    def delete(self, key: str) -> dict[str, Any]:
        response = self._call({"op": "delete", "key": key})
        return {"commitId": _expect(response, "commit")["commitId"]}

    def put_if(self, key: str, value: Value, expect: dict[str, Any]) -> dict[str, Any] | None:
        """A write conditional on the row's current state.

        Returns ``None`` when the condition was not met. That is not an error:
        the request was well formed and the server did what it was asked, and a
        lost-update retry that raised would make a contended key look like a
        fault.
        """
        response = self._call({"op": "putIf", "key": key, "value": value, "expect": expect})
        if response.get("kind") == "preconditionFailed":
            return None
        return {"commitId": _expect(response, "commit")["commitId"]}

    def transaction(self, ops: list[dict[str, Any]]) -> dict[str, Any] | None:
        """Several writes that land as one commit, or none of them.

        Each operation may carry its own ``expect``, and all of them are checked
        before any write is applied — so a transaction that would violate one
        changes nothing. Returns ``None`` when a precondition was not met, for
        the reason :meth:`put_if` gives.
        """
        response = self._call({"op": "transaction", "ops": ops})
        if response.get("kind") == "preconditionFailed":
            return None
        return {"commitId": _expect(response, "commit")["commitId"]}

    def query(self, plan: Any) -> list[Value]:
        """Run a typed plan.

        The plan is rendered to SQL *by the core*, not here — which is why there
        is nothing in this file to inject into.
        """
        assert self._core is not None
        rendered = self._core.render_query(plan)
        response = self._call({"op": "query", **rendered})
        return _expect(response, "query").get("rows", [])

    def explain(self, plan: Any) -> Explain:
        """EXPLAIN without executing — what a reviewer reads before approving."""
        assert self._core is not None
        rendered = self._core.render_query(plan)
        response = self._call({"op": "explain", **rendered})
        return cast(Explain, _expect(response, "explain"))

    def status(self) -> ProjectStatus:
        return cast(ProjectStatus, _expect(self._call({"op": "status"}), "status"))

    def describe(self) -> dict[str, Any]:
        """What is in here, and where it came from."""
        return _expect(self._call({"op": "describe"}), "description")
