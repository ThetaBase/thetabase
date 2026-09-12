"""The host half of Scribe: sockets, and nothing else.

Every decision about the protocol — how a request is encoded, what a response
means, when a cached read must be dropped — lives in the WebAssembly core
(``crates/theta-scribe-wasm``). This module moves bytes.

The split is deliberate. A Python SDK that built wire messages in Python would
be a second implementation of the protocol and the TypeScript one a third, and
the first time the schema moved two of the three would be wrong. What must be
identical across languages is the protocol; what must differ is I/O, because
WebAssembly has no sockets and the host runtimes do not agree on what one is.
"""

from __future__ import annotations

import json
import socket as _socket
from pathlib import Path
from typing import Any

# ``[u32 length][u8 tag][bytes]``, as ``abi.rs`` lays it out.
_RESULT_HEADER = 5
LENGTH_PREFIX_BYTES = 4


def _compact(value: Any) -> bytes:
    """JSON with no incidental whitespace.

    ``json.dumps`` pads with spaces and ``JSON.stringify`` does not, so without
    this the Python and TypeScript hosts hand the core different bytes for the
    same request. Nothing on the wire changes — the core re-encodes into Cap'n
    Proto either way — but two hosts that disagree about what they send are two
    hosts whose behaviour can diverge somewhere it does matter.
    """
    return json.dumps(value, separators=(",", ":")).encode("utf-8")


class ProtocolError(Exception):
    """A protocol error the core rejected, distinct from a transport failure."""


class ScribeCore:
    """The WebAssembly core, loaded once per process."""

    def __init__(self, wasm_path: str | Path | None = None) -> None:
        # Imported here rather than at module scope: the type surface in
        # ``__init__`` is usable without a WebAssembly runtime installed, and a
        # caller who only wants the types should not be made to install one.
        import wasmtime

        path = Path(wasm_path) if wasm_path else _default_module()
        self._store = wasmtime.Store()
        module = wasmtime.Module.from_file(self._store.engine, str(path))
        # No imports: the core is pure computation, which is why the same module
        # loads in Python, Node, a browser and an edge runtime.
        self._instance = wasmtime.Instance(self._store, module, [])
        self._memory = self._instance.exports(self._store)["memory"]

    def _fn(self, name: str) -> Any:
        return self._instance.exports(self._store)[name]

    def _write(self, data: bytes) -> int:
        ptr = self._fn("theta_alloc")(self._store, len(data))
        self._memory.write(self._store, data, ptr)
        return ptr

    def _take(self, ptr: int) -> bytes:
        header = self._memory.read(self._store, ptr, ptr + _RESULT_HEADER)
        length = int.from_bytes(header[:4], "little")
        tag = header[4]
        body = bytes(
            self._memory.read(
                self._store, ptr + _RESULT_HEADER, ptr + _RESULT_HEADER + length
            )
        )
        self._fn("theta_free_result")(self._store, ptr)

        if tag != 0:
            raise ProtocolError(body.decode("utf-8", "replace"))
        return body

    def _call(self, name: str, data: bytes, *args: Any) -> bytes:
        ptr = self._write(data)
        try:
            return self._take(self._fn(name)(self._store, ptr, len(data), *args))
        finally:
            self._fn("theta_free")(self._store, ptr, len(data))

    def encode(self, request: dict[str, Any], request_id: int, branch_id: int) -> bytes:
        """A host request as a framed wire message."""
        return self._call("theta_encode", _compact(request), request_id, branch_id)

    def decode(self, body: bytes) -> dict[str, Any]:
        """A wire response as JSON."""
        return json.loads(self._call("theta_decode", body).decode("utf-8"))

    def body_length(self, prefix: bytes) -> int:
        """How many bytes of body follow a length prefix."""
        ptr = self._write(prefix)
        try:
            length = self._fn("theta_body_length")(self._store, ptr)
        finally:
            self._fn("theta_free")(self._store, ptr, len(prefix))

        if length == 0xFFFFFFFF:
            raise ProtocolError("the peer sent a length this protocol does not allow")
        return int(length)

    def encode_hello(self, token: str, client_name: str) -> bytes:
        """Encode the handshake.

        The protocol version is the core's to state, not the host's: a host that
        could name its own version could claim a compatibility it does not have,
        and the negotiation that refuses mismatched versions rather than
        guessing would be negotiating with itself.
        """
        return self._call(
            "theta_encode_hello", _compact({"token": token, "clientName": client_name})
        )

    def decode_welcome(self, body: bytes) -> dict[str, Any]:
        """Decode the server's reply to a handshake. A refusal raises."""
        return json.loads(self._call("theta_decode_welcome", body).decode("utf-8"))

    def render_query(self, ast: dict[str, Any]) -> dict[str, Any]:
        """Render a typed query AST to SQL-subset source and bound parameters.

        In the core rather than here, so a query built the same way in
        TypeScript renders to the same bytes — and so the rule that a value
        never becomes query text has one implementation.
        """
        return json.loads(self._call("theta_render_query", _compact(ast)).decode("utf-8"))

    def invalidation(self, request: dict[str, Any]) -> dict[str, Any]:
        """What this request invalidates: one key, everything, or nothing."""
        return json.loads(self._call("theta_invalidation", _compact(request)).decode("utf-8"))


def _default_module() -> Path:
    """Find the core next to the package, then in the build tree."""
    # `here` is `sdk/python/src/thetabase`, so the repository root is
    # `parents[3]`. It was `parents[4]` — one level above the checkout — which
    # meant the build-tree fallback never resolved and the only way to load the
    # core from a checkout was to pass its path by hand. Nothing caught it
    # because the conformance runners do exactly that.
    here = Path(__file__).resolve().parent
    candidates = [
        # Beside the package: how it is laid out once installed.
        here / "theta_scribe_wasm.wasm",
        here.parents[2] / "theta_scribe_wasm.wasm",
        # In the build tree: how it is laid out in a checkout.
        here.parents[3] / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise FileNotFoundError(
        "the Scribe core was not found — run `make wasm` to build it, or pass its "
        "path to ScribeCore()"
    )


class Connection:
    """One framed connection to ``thetad``.

    Sequential by construction: a caller that needs concurrency opens more
    connections. Multiplexing one socket would need request-id correlation on
    the read side, and correlating a reply to the wrong request is a
    data-corruption bug rather than a slow one.
    """

    def __init__(self, core: ScribeCore, sock: _socket.socket) -> None:
        self._core = core
        self._sock = sock
        self._next_id = 1

    def _read_exactly(self, count: int) -> bytes:
        chunks = []
        remaining = count
        while remaining:
            chunk = self._sock.recv(remaining)
            if not chunk:
                raise ConnectionError(
                    f"the peer closed with {remaining} of {count} bytes still to come"
                )
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)

    def send_raw(self, framed: bytes) -> bytes:
        """Write a framed message and read one framed reply back."""
        self._sock.sendall(framed)
        prefix = self._read_exactly(LENGTH_PREFIX_BYTES)
        return self._read_exactly(self._core.body_length(prefix))

    def call(self, request: dict[str, Any], branch_id: int = 0) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        body = self.send_raw(self._core.encode(request, request_id, branch_id))
        return self._core.decode(body)

    def close(self) -> None:
        self._sock.close()
