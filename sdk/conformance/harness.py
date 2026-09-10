"""The M6 gate: every SDK, a live thetad, identical output.

Runs the shared suite through each binding and compares what they produced. Two
bindings that each pass their own suite prove nothing about whether they agree;
bindings that produce the same document from the same cases, from the same
starting state, have actually been shown to speak one protocol.

A difference is printed as a diff and fails the build, because a divergence
between bindings is the failure this milestone exists to prevent.

Every binding is compared against the *first* one rather than pairwise. With
three it makes no difference to what passes; it makes a large difference to what
a failure reads like — "Go disagrees with TypeScript" names the new binding,
where a pairwise sweep would report the same disagreement twice and leave the
reader to work out which one moved.

Agreement is not enough, and this harness learned that the hard way. Three
bindings that are identically broken agree perfectly. The case named *"a SQL
literal is bound and never becomes syntax"* passed for as long as it existed
while the server refused every parameterised query — the core forwarded the
host's plain JSON where the wire encoding is `{"kind":"text","value":...}`, all
three bindings sent the same wrong bytes, and nothing here asked whether the
operation had worked. So each case in `cases.json` now carries an `expect`, and
a case's name is its claim: if it says a literal is bound and never becomes
syntax, an error response is a failure however unanimous it is.
"""

from __future__ import annotations

import difflib
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# Windows names an executable with `.exe`, and the gate is developed on both.
EXE = ".exe" if sys.platform == "win32" else ""
SERVER = ROOT / f"target/debug/examples/conformance_server{EXE}"
WASM = ROOT / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"


def missing(path: Path, how: str) -> None:
    print(f"{path} is not built — run `{how}` first", file=sys.stderr)
    raise SystemExit(2)


def run_binding(name: str, argv: list[str]) -> str:
    """Run one binding's suite against a server of its own.

    A fresh server per binding, not one shared between them. Sharing looked
    tidier and was wrong: the suite writes rows, so whichever binding ran second
    started from the other's state and reported different version ids for the
    same case. The comparison only means anything from identical initial state.
    """
    # The server's stdin stays open for its lifetime — closing it is how it is
    # told to stop, so a runner that let it close would race its own suite.
    server = subprocess.Popen(
        [str(SERVER)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        assert server.stdout is not None
        line = server.stdout.readline()
        if not line:
            raise SystemExit(f"the conformance server exited before the {name} run")
        details = json.loads(line)

        result = subprocess.run(
            [*argv, details["address"], details["token"]],
            capture_output=True,
            text=True,
            # UTF-8 both ways, explicitly. Windows decodes a pipe with the
            # locale codepage otherwise, and the refusal message for an
            # unusable identifier contains an em-dash — so the harness reported
            # the Python and TypeScript SDKs disagreeing about a character that
            # never left either of them intact.
            encoding="utf-8",
            env={
                **os.environ,
                "PYTHONIOENCODING": "utf-8",
                # A user-profile .NET install needs DOTNET_ROOT; the muxer
                # otherwise resolves the machine-wide runtime, which may be an
                # older major version than the SDK that built the runner.
                "DOTNET_ROOT": os.environ.get(
                    "DOTNET_ROOT", str(Path(os.path.expanduser("~")) / ".dotnet")
                ),
                **swift_environment(),
            },
            cwd=ROOT,
        )
        if result.returncode != 0:
            print(f"the {name} runner failed:\n{result.stderr}", file=sys.stderr)
            raise SystemExit(1)
        return result.stdout
    finally:
        if server.stdin:
            server.stdin.close()
        server.wait(timeout=10)


GO_RUNNER = ROOT / f"target/conformance-go{EXE}"
RUST_RUNNER = ROOT / f"target/debug/examples/conformance{EXE}"
JAVA_CLASSES = ROOT / "sdk/java/target/classes"
JAVA_CLASSPATH = ROOT / "sdk/java/target/classpath.txt"
CSHARP_DLL = ROOT / "sdk/csharp/conformance/bin/Debug/net9.0/ThetaBase.Conformance.dll"
RUBY_RUNNER = ROOT / "sdk/ruby/conformance.rb"
SWIFT_RUNNER = ROOT / f"sdk/swift/.build/debug/thetabase-conformance{EXE}"


def swift_environment() -> dict[str, str]:
    """Swift's runtime DLLs, for a per-user toolchain on Windows.

    A machine-wide Swift install puts them on the system PATH; a per-user one
    does not, and the runner then exits with STATUS_DLL_NOT_FOUND before main.
    Empty everywhere else, where the runtime is linked or already found.
    """
    runtime = (
        Path(os.path.expanduser("~"))
        / "AppData/Local/Programs/Swift/Runtimes/6.3.3/usr/bin"
    )
    if sys.platform == "win32" and runtime.exists():
        return {"PATH": str(runtime) + os.pathsep + os.environ["PATH"]}
    return {}


def ruby() -> str:
    """Where `ruby` is.

    `RUBY` first, then a user-profile install, then PATH — the same reasoning as
    `dotnet()`. RubyInstaller without administrator rights lands in the user
    profile and puts nothing on the machine PATH.
    """
    if "RUBY" in os.environ:
        return os.environ["RUBY"]
    local = Path(os.path.expanduser("~")) / "tools" / "Ruby34" / "bin" / f"ruby{EXE}"
    return str(local) if local.exists() else "ruby"


def dotnet() -> str:
    """Where `dotnet` is.

    `DOTNET` first, then a user-profile install, then whatever is on PATH. The
    middle case is not exotic: installing the SDK without administrator rights
    puts it in `~/.dotnet`, and the `dotnet` already on PATH may be an older
    machine-wide runtime that cannot launch this build.
    """
    if "DOTNET" in os.environ:
        return os.environ["DOTNET"]
    local = Path(os.path.expanduser("~")) / ".dotnet" / f"dotnet{EXE}"
    return str(local) if local.exists() else "dotnet"


def java_command() -> list[str]:
    """`java -cp <classes><sep><deps> io.thetabase.Conformance`.

    The dependency list is written by `mvn dependency:build-classpath` during
    `make sdk`, rather than assembled here: Maven knows where its own local
    repository is and which transitive jars the SDK actually resolved to, and a
    hand-built path would be a second answer to that question.
    """
    separator = ";" if sys.platform == "win32" else ":"
    classpath = separator.join([str(JAVA_CLASSES), JAVA_CLASSPATH.read_text().strip()])
    return ["java", "-cp", classpath, "io.thetabase.Conformance"]


def bindings() -> list[tuple[str, list[str]]]:
    """Each binding, and how to run it.

    The Go runner is a built binary rather than `go run`, for the reason the
    Node runner imports `dist/` rather than `src/`: a suite that passed against
    sources and failed against the published artifact would be testing the wrong
    thing.
    """
    return [
        ("typescript", ["node", str(ROOT / "sdk/conformance/run.mjs")]),
        ("python", [sys.executable, str(ROOT / "sdk/conformance/run.py")]),
        ("go", [str(GO_RUNNER)]),
        ("rust", [str(RUST_RUNNER)]),
        ("java", java_command()),
        ("csharp", [dotnet(), str(CSHARP_DLL)]),
        ("ruby", [ruby(), str(RUBY_RUNNER)]),
        ("swift", [str(SWIFT_RUNNER)]),
    ]


def main() -> int:
    if not SERVER.exists():
        missing(SERVER, "cargo build -p thetad --example conformance_server")
    if not WASM.exists():
        missing(WASM, "make wasm")
    if not GO_RUNNER.exists():
        missing(GO_RUNNER, "make sdk")
    if not RUST_RUNNER.exists():
        missing(RUST_RUNNER, "cargo build -p thetabase --example conformance")
    if not JAVA_CLASSPATH.exists():
        missing(JAVA_CLASSPATH, "make sdk")
    if not CSHARP_DLL.exists():
        missing(CSHARP_DLL, "make sdk")
    if not RUBY_RUNNER.exists():
        missing(RUBY_RUNNER, "git checkout sdk/ruby")
    if not SWIFT_RUNNER.exists():
        missing(SWIFT_RUNNER, "make sdk")

    outputs = {name: run_binding(name, argv).strip() for name, argv in bindings()}

    reference, expected = next(iter(outputs.items()))
    disagreed = False
    for name, actual in outputs.items():
        if name == reference or actual == expected:
            continue
        disagreed = True
        print(f"the {reference} and {name} SDKs disagree:\n", file=sys.stderr)
        sys.stderr.writelines(
            difflib.unified_diff(
                expected.splitlines(keepends=True),
                actual.splitlines(keepends=True),
                fromfile=reference,
                tofile=name,
            )
        )

    if disagreed:
        print(
            "\nEvery binding ran the same cases from the same starting state. A "
            "difference here means one of them is speaking a protocol the others are not.",
            file=sys.stderr,
        )
        return 1

    report = json.loads(expected)
    unmet = expectations(report)
    if unmet:
        print(
            "every binding agreed, and they agreed on the wrong answer:\n",
            file=sys.stderr,
        )
        for line in unmet:
            print(f"  {line}\n", file=sys.stderr)
        print(
            "A case's name is its claim. Agreement between bindings is not the "
            "same as the operation working — see the note at the top of this file.",
            file=sys.stderr,
        )
        return 1

    wire = len(report["results"])
    queries = len(report.get("queries", []))
    print(
        f"conformance: {len(outputs)} SDKs agree across {wire} wire cases against a "
        f"live thetad and {queries} typed-query cases, and every case did what its "
        f"name says"
    )
    return 0


def at(document: object, dotted: str) -> object:
    """Follow a dotted path, or return a marker if it is not there."""
    node = document
    for part in dotted.split("."):
        if not isinstance(node, dict) or part not in node:
            return MISSING
        node = node[part]
    return node


MISSING = object()


def check(entry: dict, expect: object) -> str | None:
    """Compare one case's outcome against what its name claims.

    `expect` is either a word or an object:

    - ``"ok"``           — any response the server was willing to give
    - ``"clientError"``  — the core refused to send at all, before the wire
    - ``{"response.kind": "propose", "response.value.gate": "Confirm"}`` — any
      number of dotted paths into the entry that must hold

    The object form exists because the interesting cases are not "did it work".
    *"dropping a populated column is gated rather than applied"* is a perfectly
    successful `propose` whose diff says confirmation is required — reporting it
    as "ok" would pass just as happily if the gate field said `autoApply`, which
    is the failure the case is named after.
    """
    status = entry.get("status")
    if isinstance(expect, str):
        if expect == "clientError":
            return None if status == "clientError" else f"expected the core to refuse it, got {status}"
        if status != "ok":
            return f"expected {expect}, got {status}: {entry.get('message', '')}"
        return None

    if status != "ok":
        return f"expected a response, got {status}: {entry.get('message', '')}"

    for path, wanted in expect.items():
        actual = at(entry, path)
        if actual is MISSING:
            return f"{path} is not in the response"
        if actual != wanted:
            return f"{path} is {actual!r}, expected {wanted!r}"
    return None


def expectations(report: dict) -> list[str]:
    """Check each case against what `cases.json` says it should do.

    Without this the harness could only answer "do the bindings agree", and
    three bindings that are identically broken agree perfectly. That is not
    hypothetical — it is why this function exists. Three separate bugs were
    passing here:

    - every `put` sent the host's plain JSON where the server reads a tagged
      `Value`, so no write from any SDK worked;
    - every parameterised query did the same, under a case named *"a SQL literal
      is bound and never becomes syntax"*;
    - `audit` could not be called at all, because the core's request enum
      renamed its variants and not their fields.

    All three were unanimous, and unanimity was the only thing being measured.
    """
    suite = json.loads((Path(__file__).parent / "cases.json").read_text(encoding="utf-8"))
    wanted = {
        case["name"]: case["expect"]
        for case in suite["cases"] + suite["queries"]
        if "expect" in case
    }

    unmet = []
    for entry in report["results"] + report.get("queries", []):
        name = entry.get("name")
        if name not in wanted:
            unmet.append(f"{name!r}: no `expect` in cases.json — say what it should do")
            continue
        problem = check(entry, wanted[name])
        if problem:
            unmet.append(f"{name!r}: {problem}")
    return unmet


if __name__ == "__main__":
    raise SystemExit(main())
