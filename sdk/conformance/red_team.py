"""Adversarial probes against a live `thetad`.

`docs/specs/04-threat-model-security.md` §7 requires a penetration test of the
token minting and scoping system before any production launch, and this is a
first pass at it. It is not a substitute for the independent review that section
also asks for — it was written by the same hands that wrote the thing it
attacks, which is exactly the weakness an outside reviewer does not have.

# What it does

Every probe is a *specific attack with an expected outcome*, and a probe passes
when the server refuses in the way the threat model says it should. A probe that
merely "gets an error" is not a pass: an oversized frame that is refused because
the JSON underneath was malformed would tell us nothing about whether the frame
limit works.

So each probe names the property it is testing, and a failure prints what the
server did instead. The list is meant to grow; anything found by hand belongs
here afterwards so it cannot come back.

# What it deliberately does not do

No fuzzing. A fuzzer finds crashes, and crashes are the least interesting
failure here — the claims this product makes are about *authorisation* and
*isolation*, and those fail silently rather than loudly. A server that accepted
a token for another project would pass every crash test ever written.
"""

from __future__ import annotations

import base64
import json
import socket
import struct
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXE = ".exe" if sys.platform == "win32" else ""
SERVER = ROOT / f"target/debug/examples/conformance_server{EXE}"
WASM = ROOT / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"

sys.path.insert(0, str(ROOT / "sdk/python/src"))


@dataclass
class Findings:
    passed: list[str] = field(default_factory=list)
    failed: list[str] = field(default_factory=list)
    broken_controls: list[str] = field(default_factory=list)

    def check(self, name: str, held: bool, detail: str = "") -> None:
        if held:
            self.passed.append(name)
            print(f"  refused  {name}")
        else:
            self.failed.append(f"{name}: {detail}")
            print(f"  ACCEPTED {name}  <-- {detail}")

    def control(self, name: str, accepted: bool, detail: str = "") -> None:
        """A thing that must be *accepted*.

        Every probe reports "refused" when something goes wrong, so a probe that
        reported it unconditionally would score perfectly against a server with
        no authentication at all. If a control reads as refused, nothing else in
        the run means anything, and the run says so instead of passing.
        """
        if accepted:
            print(f"  accepted {name}")
        else:
            self.broken_controls.append(f"{name}: {detail}")
            print(f"  BROKEN   {name}  <-- {detail}")


class Server:
    """A live instance, with a token minted for it."""

    def __enter__(self) -> "Server":
        # stdin held open: closing it is the server's shutdown signal.
        self.process = subprocess.Popen(
            [str(SERVER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        line = self.process.stdout.readline()
        if not line:
            raise SystemExit("the server exited before it was ready")
        details = json.loads(line)
        self.address = details["address"]
        self.token = details["token"]
        self.project = details["projectId"]
        return self

    def __exit__(self, *_: object) -> None:
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()

    def socket(self) -> socket.socket:
        host, _, port = self.address.rpartition(":")
        s = socket.create_connection((host, int(port)), timeout=5)
        s.settimeout(5)
        return s


def raw_send(sock: socket.socket, payload: bytes) -> bytes | None:
    """Write a framed payload and read whatever comes back, or None on close."""
    sock.sendall(struct.pack("<I", len(payload)) + payload)
    try:
        prefix = sock.recv(4)
        if len(prefix) < 4:
            return None
        (length,) = struct.unpack("<I", prefix)
        return sock.recv(length)
    except (socket.timeout, ConnectionResetError, OSError):
        return None


def tamper(token: str, mutate) -> str:
    """Re-encode a token with its claims changed, leaving the signature alone.

    The whole security of the scheme rests on the signature covering the claims,
    so this is the attack that matters: if a modified payload is ever accepted,
    every other control is decoration.
    """
    version, payload_b64, signature_b64 = token.split(".", 2)
    pad = "=" * (-len(payload_b64) % 4)
    claims = json.loads(base64.urlsafe_b64decode(payload_b64 + pad))
    mutate(claims)
    repacked = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    return f"{version}.{repacked}.{signature_b64}"


def handshake(core, sock: socket.socket, token: str) -> bytes | None:
    """Present a token. Returns the welcome body, or None if refused."""
    return raw_send(sock, core.encode_hello(token, "red-team")[4:])


def probe_controls(server: Server, core, out: Findings) -> None:
    """The things that must be accepted, so the refusals above mean something."""
    print("\ncontrols (these must be ACCEPTED)")

    from thetabase.scribe import Connection

    sock = server.socket()
    try:
        body = handshake(core, sock, server.token)
        welcomed = body is not None and core.decode_welcome(body).get("projectId") is not None
        out.control("the real token is accepted", welcomed, "a valid handshake was refused")
    except Exception as e:  # noqa: BLE001
        out.control("the real token is accepted", False, str(e)[:120])
    finally:
        sock.close()

    sock = server.socket()
    try:
        conn = Connection(core, sock)
        conn.send_raw(core.encode_hello(server.token, "red-team"))
        written = conn.call({"op": "put", "key": "control", "value": 1})
        out.control(
            "an ordinary write succeeds",
            written.get("kind") == "commit",
            f"a valid write was refused: {str(written)[:120]}",
        )
    except Exception as e:  # noqa: BLE001
        out.control("an ordinary write succeeds", False, str(e)[:120])
    finally:
        sock.close()


def probe_authentication(server: Server, core, out: Findings) -> None:
    print("\nauthentication and token scoping")

    def refused_with(token: str) -> tuple[bool, str]:
        """True when the server refused this token.

        # The bug this shape exists to avoid

        The first version decoded the welcome frame with `core.decode`, which is
        for *responses* — a welcome is `decode_welcome`. So every handshake threw,
        the `except` treated a throw as a refusal, and all eleven token probes
        reported success against a server whose signature check had been
        deliberately removed. The suite scored 23/23 while accepting forged
        tokens.

        So: a welcome that parses is an **acceptance**, and nothing else counts
        as one. Exceptions are narrowed to the socket, because an exception from
        the decoder is a bug in this file rather than evidence about the server.
        """
        sock = server.socket()
        try:
            body = handshake(core, sock, token)
            if body is None:
                return True, ""
            try:
                welcome = core.decode_welcome(body)
            except Exception:  # noqa: BLE001 - not a welcome, so not an acceptance
                return True, ""
            if welcome.get("projectId"):
                return False, f"handshake succeeded: {str(welcome)[:100]}"
            return True, ""
        except (OSError, socket.timeout) as e:
            # The connection went away, which is how `thetad` refuses.
            return True, str(e)[:80]
        finally:
            sock.close()

    for name, token in [
        ("an empty token", ""),
        ("a token that is not a token", "not-a-token"),
        ("a token with no signature", server.token.rsplit(".", 1)[0]),
        ("a token with an empty signature", server.token.rsplit(".", 1)[0] + "."),
        ("a token from a future format version", "v9" + server.token[2:]),
    ]:
        held, detail = refused_with(token)
        out.check(name, held, detail)

    # The one that matters most: the claims are inside the signature, so editing
    # any of them must invalidate it.
    for name, mutate in [
        ("a token whose project was swapped", lambda c: c.update(project_id="someone-else")),
        ("a token whose expiry was extended", lambda c: c.update(expires_at_ms=c["expires_at_ms"] + 10**12)),
        ("a token whose user was swapped", lambda c: c.update(user_id="root")),
        ("a token whose environment was raised", lambda c: c.update(environment="prod")),
        ("a token whose org was swapped", lambda c: c.update(org_id="org_other")),
        ("a token whose id was changed to dodge revocation", lambda c: c.update(token_id="tok_fresh")),
    ]:
        held, detail = refused_with(tamper(server.token, mutate))
        out.check(name, held, detail)


def probe_framing(server: Server, core, out: Findings) -> None:
    print("\nframing and protocol")

    def survives(payload: bytes, *, raw_prefix: bytes | None = None) -> tuple[bool, str]:
        """True if the server refused without becoming unusable."""
        sock = server.socket()
        try:
            if raw_prefix is not None:
                sock.sendall(raw_prefix)
            else:
                sock.sendall(struct.pack("<I", len(payload)) + payload)
            try:
                sock.recv(4)
            except (socket.timeout, ConnectionResetError, OSError):
                pass
        finally:
            sock.close()

        # The real question is whether the *server* is still there afterwards.
        try:
            probe = server.socket()
            probe.close()
            return True, ""
        except OSError as e:
            return False, f"the server stopped accepting connections: {e}"

    # A four-gigabyte length on a four-byte frame. The limit exists so a caller
    # cannot make the server allocate by lying about what follows.
    held, detail = survives(b"", raw_prefix=struct.pack("<I", 0xFFFF_FFFF) + b"\x00")
    out.check("a frame claiming four gigabytes", held, detail)

    held, detail = survives(b"", raw_prefix=struct.pack("<I", 1_000_000) + b"\x00" * 10)
    out.check("a frame that ends early", held, detail)

    held, detail = survives(b"")
    out.check("a zero-length frame", held, detail)

    held, detail = survives(b"\xff" * 512)
    out.check("garbage where a message should be", held, detail)

    # Requests before the handshake must not be served.
    sock = server.socket()
    try:
        body = raw_send(sock, core.encode({"op": "get", "key": "k"}, 1, 0)[4:])
        served = False
        if body:
            try:
                served = core.decode(body).get("kind") == "get"
            except Exception:  # noqa: BLE001
                served = False
        out.check(
            "a read before the handshake",
            not served,
            "the server answered a request from an unauthenticated connection",
        )
    finally:
        sock.close()


def probe_isolation(server: Server, core, out: Findings) -> None:
    """Keys are addressed within a project; a caller must not escape it."""
    print("\ntenant isolation and key addressing")

    from thetabase.scribe import Connection

    sock = server.socket()
    try:
        conn = Connection(core, sock)
        conn.send_raw(core.encode_hello(server.token, "red-team"))

        # Path-traversal shapes in a key. These are not a filesystem path, but a
        # key that escaped its project would be the whole ballgame, and the
        # instance-directory namer has had to defend against exactly this.
        for hostile in [
            "../other-project/secret",
            "..\\..\\windows\\system32",
            "a/../../b",
            "\x00truncated",
            "k\nInjected: header",
        ]:
            written = conn.call({"op": "put", "key": hostile, "value": 1})
            if written.get("kind") == "error":
                out.check(f"a hostile key {hostile!r}", True)
                continue
            # Accepted is not automatically wrong — a key is an opaque string.
            # What matters is that reading it back gives *that* row and not
            # another, so the traversal did not resolve anywhere.
            read = conn.call({"op": "get", "key": hostile})
            value = (read.get("value") or {}).get("value")
            out.check(
                f"a hostile key {hostile!r} stays an opaque key",
                value == 1,
                f"stored 1 and read back {value!r}",
            )
    finally:
        sock.close()


def probe_injection(server: Server, core, out: Findings) -> None:
    """The claim is that a value never becomes query syntax."""
    print("\ninjection")

    from thetabase.scribe import Connection

    sock = server.socket()
    try:
        conn = Connection(core, sock)
        conn.send_raw(core.encode_hello(server.token, "red-team"))

        # A value that is a SQL statement. If parameters are bound rather than
        # interpolated, this is data and nothing happens.
        conn.call({"op": "put", "key": "victim", "value": "safe"})
        payload = "'; DROP TABLE rows; --"
        conn.call({"op": "put", "key": "attack", "value": payload})

        read = conn.call({"op": "get", "key": "attack"})
        stored = (read.get("value") or {}).get("value")
        out.check(
            "a SQL statement stored as a value stays a value",
            stored == payload,
            f"round-tripped as {stored!r}",
        )

        survivor = conn.call({"op": "get", "key": "victim"})
        out.check(
            "the rest of the data is still there afterwards",
            (survivor.get("value") or {}).get("found") is True,
            "the victim row disappeared",
        )
    finally:
        sock.close()


def main() -> int:
    for path, how in [
        (SERVER, "cargo build -p thetad --example conformance_server"),
        (WASM, "make wasm"),
    ]:
        if not path.exists():
            print(f"{path} is not built — run `{how}` first", file=sys.stderr)
            return 1

    from thetabase.scribe import ScribeCore

    core = ScribeCore(str(WASM))
    out = Findings()

    # Controls first. If the probes cannot recognise acceptance, nothing they
    # report afterwards is evidence of anything.
    with Server() as server:
        probe_controls(server, core, out)
    with Server() as server:
        probe_authentication(server, core, out)
    with Server() as server:
        probe_framing(server, core, out)
    with Server() as server:
        probe_isolation(server, core, out)
    with Server() as server:
        probe_injection(server, core, out)

    if out.broken_controls:
        print("\nCONTROLS BROKEN - every refusal above is meaningless:")
        for broken in out.broken_controls:
            print(f"  - {broken}")
        return 1

    print(f"\n{len(out.passed)} probes refused as intended, {len(out.failed)} accepted")
    if out.failed:
        print("\nFINDINGS:")
        for finding in out.failed:
            print(f"  - {finding}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
