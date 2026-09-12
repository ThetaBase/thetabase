"""The high-level clients, against a live `thetad`.

# Why this exists next to `harness.py` rather than inside it

`harness.py` drives the *protocol core* through each host shim: every runner
builds requests as plain dictionaries and hands them to `ScribeCore`. That
proves eight bindings speak one protocol, which is what it was written for, and
it says nothing at all about the `Theta` classes a customer actually holds —
those were written afterwards and no runner touches them.

So the capability matrix could honestly say "wired" and not "works". This is
what closes that gap: it connects with the real client, performs the real
handshake, and exercises the methods a caller would call.

# Why it is a smoke test and not a second conformance suite

`harness.py` earns its complexity by *comparing* bindings — a difference between
two of them is a protocol bug and there is no other way to find it. There is no
equivalent question here. Each client wraps the same core, so what can go wrong
is the wrapping: a request built with the wrong field names, a response unwrapped
from the wrong key, a handshake never performed. Every one of those fails on the
first call, which is why a handful of calls is enough and a matrix of them would
only be slower.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXE = ".exe" if sys.platform == "win32" else ""
SERVER = ROOT / f"target/debug/examples/conformance_server{EXE}"
WASM = ROOT / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"

sys.path.insert(0, str(ROOT / "sdk/python/src"))


class Server:
    """A live instance on an ephemeral port, with a token minted for it."""

    def __enter__(self) -> "Server":
        # `stdin=PIPE` is not optional, and its absence is not obvious.
        #
        # The server treats stdin closing as its shutdown signal, so that it
        # cannot outlive the runner that started it. Inheriting the parent's
        # stdin means inheriting whatever state that is in — under a
        # non-interactive shell it is already at EOF, so the server prints its
        # address and shuts down in the same breath. The symptom is a live
        # process that refuses every connection, which reads like a firewall
        # problem and is not one.
        self.process = subprocess.Popen(
            [str(SERVER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        # The server prints one line of JSON once it is listening. Reading it is
        # also how we wait for it — a sleep would be a race that passes on a
        # fast machine.
        line = self.process.stdout.readline()
        if not line:
            raise SystemExit("the conformance server exited before it was ready")
        details = json.loads(line)
        if self.process.poll() is not None:
            raise SystemExit(
                "the server printed its address and then exited:\n"
                + (self.process.stderr.read() or "(no stderr)")
            )
        self.address = details["address"]
        self.token = details["token"]
        return self

    def __exit__(self, *_: object) -> None:
        # Closing stdin is the polite shutdown; terminate is the fallback for a
        # server that has stopped reading it.
        if self.process.stdin:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=10)


def check(name: str, got: object, want: object) -> None:
    if got != want:
        raise SystemExit(f"FAIL {name}: got {got!r}, wanted {want!r}")
    print(f"  ok  {name}")


def exercise_python(server: Server) -> None:
    from thetabase import Theta

    # The client reads both from the environment and refuses to take them as
    # parameters, so the test supplies them the way `theta exec` would.
    os.environ["THETA_ADDRESS"] = server.address
    os.environ["THETA_TOKEN"] = server.token

    with Theta.connect(project="conformance") as theta:
        check("get of an absent row is None", theta.get("smoke/absent"), None)

        theta.put("smoke/a", 1)
        check("put then get round-trips", theta.get("smoke/a"), 1)

        # The condition holds: no row at this key yet.
        created = theta.put_if("smoke/b", 2, {"kind": "absent"})
        check("a create-only write lands when the row is absent", created is not None, True)

        # And now it does not, so the same call must report the failure rather
        # than overwrite.
        again = theta.put_if("smoke/b", 3, {"kind": "absent"})
        check("the same write is refused once the row exists", again, None)
        check("the refused write changed nothing", theta.get("smoke/b"), 2)

        # The one worth having. Both operations land, or neither does.
        committed = theta.transaction(
            [
                {"key": "smoke/tx1", "action": "put", "value": 10},
                {"key": "smoke/tx2", "action": "put", "value": 20},
            ]
        )
        check("a transaction commits", committed is not None, True)
        check("both of its writes are visible", theta.get("smoke/tx2"), 20)

        # A precondition that cannot hold, on the *second* operation. The first
        # must not survive it.
        refused = theta.transaction(
            [
                {"key": "smoke/tx3", "action": "put", "value": 30},
                {"key": "smoke/b", "action": "put", "value": 99, "expect": {"kind": "absent"}},
            ]
        )
        check("a transaction with a failing precondition is refused", refused, None)
        check("its other write did not land", theta.get("smoke/tx3"), None)

        theta.delete("smoke/a")
        check("delete removes the row", theta.get("smoke/a"), None)


def exercise_typescript(server: Server) -> None:
    """The TypeScript client, in its own process.

    A subprocess rather than an import: the client is ESM built to `dist/`, and
    driving it from Python would mean a second Node entry point that could drift
    from the one a customer runs.
    """
    result = subprocess.run(
        ["node", str(ROOT / "sdk/conformance/client_smoke.mjs")],
        capture_output=True,
        text=True,
        encoding="utf-8",
        env={
            **os.environ,
            "THETA_ADDRESS": server.address,
            "THETA_TOKEN": server.token,
            "THETA_PROJECT": "conformance",
        },
        cwd=ROOT,
    )
    print(result.stdout, end="")
    if result.returncode != 0:
        raise SystemExit("the TypeScript client failed:\n" + result.stderr)


def exercise_subprocess(server: Server, name: str, argv: list[str], extra_env: dict) -> None:
    """A client that lives in another runtime, driven through its own binary.

    C# and Swift reuse their conformance runners with a `--smoke` flag rather
    than shipping a second executable each: the conformance build is already in
    the gate, and another artefact is another thing to keep built — which is
    exactly how the Go runner went two weeks stale.
    """
    result = subprocess.run(
        [*argv, server.address, server.token, "--smoke"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        env={**os.environ, **extra_env},
        cwd=ROOT,
    )
    print(result.stdout, end="")
    if result.returncode != 0:
        raise SystemExit(f"the {name} client failed:\n{result.stderr}")


def dotnet() -> str:
    """The user-profile .NET, not the machine-wide runtime.

    A machine-wide runtime-only install shadows a user-profile SDK on PATH and
    the muxer resolves there, so the binary is named directly.
    """
    candidate = Path(os.path.expanduser("~")) / ".dotnet" / ("dotnet.exe" if EXE else "dotnet")
    return str(candidate) if candidate.exists() else "dotnet"


def swift_environment() -> dict:
    """A built Swift executable needs its runtime beside it.

    Without those DLLs on PATH it exits before `main` with a status code and no
    message worth reading.
    """
    if not EXE:
        return {}
    runtime = Path(os.path.expandvars("%LOCALAPPDATA%")) / "Programs/Swift/Runtimes/6.3.3/usr/bin"
    if not runtime.exists():
        return {}
    return {"PATH": f"{runtime};{os.environ.get('PATH', '')}"}


def main() -> int:
    for path, how in [
        (SERVER, "cargo build -p thetad --example conformance_server"),
        (WASM, "make wasm"),
    ]:
        if not path.exists():
            print(f"{path} is not built — run `{how}` first", file=sys.stderr)
            return 1

    # A server each, so one client's rows cannot make another's assertions pass.
    with Server() as server:
        print("python client:")
        exercise_python(server)

    with Server() as server:
        print("\ntypescript client:")
        exercise_typescript(server)

    # Skipped when the runner is not built rather than failed, because these two
    # need toolchains a contributor may not have. `make conformance` builds both
    # first, so the gate never takes this branch.
    csharp = ROOT / "sdk/csharp/conformance/bin/Debug/net9.0/ThetaBase.Conformance.dll"
    if csharp.exists():
        with Server() as server:
            print("\nc# client:")
            exercise_subprocess(
                server,
                "C#",
                [dotnet(), str(csharp)],
                {"DOTNET_ROOT": str(Path(os.path.expanduser("~")) / ".dotnet")},
            )
    else:
        print("\nc# client: skipped, runner not built")

    swift = ROOT / f"sdk/swift/.build/debug/thetabase-conformance{EXE}"
    if swift.exists():
        with Server() as server:
            print("\nswift client:")
            exercise_subprocess(server, "Swift", [str(swift)], swift_environment())
    else:
        print("\nswift client: skipped, runner not built")

    print("\nclient smoke: every client connected, wrote, and was refused correctly")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
