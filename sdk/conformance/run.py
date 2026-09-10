"""Runs the shared conformance suite from Python, against a live thetad.

Prints one JSON document on stdout. The Node runner prints the same document
from the same cases, and ``make conformance`` diffs them: two SDKs that each
pass their own suite prove nothing about whether they agree.
"""

from __future__ import annotations

import copy
import json
import socket
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "sdk/python/src"))

from thetabase import query as qb  # noqa: E402
from thetabase.scribe import Connection, ProtocolError, ScribeCore  # noqa: E402

WASM = ROOT / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"


def redact(response: dict, paths: list[str]) -> dict:
    """Blank out values that legitimately differ between runs."""
    out = copy.deepcopy(response)
    for dotted in paths:
        node = out
        parts = dotted.split(".")
        for part in parts[:-1]:
            node = node.get(part) if isinstance(node, dict) else None
            if node is None:
                break
        if isinstance(node, dict) and parts[-1] in node:
            node[parts[-1]] = "<redacted>"
    return out


def build_query(spec: dict) -> qb.Query:
    q = qb.table(spec["table"])
    for column in spec.get("select", []):
        q = q.select(column)
    for op, column, value in spec.get("where", []):
        q = q.where(getattr(qb, op)(column, value))
    for column, descending in spec.get("orderBy", []):
        q = q.order_by(column, descending)
    if "limit" in spec:
        q = q.limit(spec["limit"])
    if "offset" in spec:
        q = q.offset(spec["offset"])
    return q


def main() -> int:
    if len(sys.argv) < 3:
        print("usage: run.py <host:port> <token>", file=sys.stderr)
        return 2

    host, port = sys.argv[1].rsplit(":", 1)
    token = sys.argv[2]

    core = ScribeCore(WASM)
    suite = json.loads((Path(__file__).parent / "cases.json").read_text())

    sock = socket.create_connection((host, int(port)))
    conn = Connection(core, sock)

    # Handshake first: the server refuses anything else until it has one.
    welcome_body = conn.send_raw(core.encode_hello(token, "conformance-python"))
    welcome = core.decode_welcome(welcome_body)

    results = []
    for case in suite["cases"]:
        try:
            response = conn.call(case["request"])
            outcome = {
                "name": case["name"],
                "status": "ok",
                "response": redact(response, case.get("redact", [])),
            }
        except ProtocolError as e:
            outcome = {
                "name": case["name"],
                "status": "clientError",
                "message": str(e).split("\n")[0],
            }
        except (ConnectionError, OSError) as e:
            outcome = {
                "name": case["name"],
                "status": "transportError",
                "message": str(e).split("\n")[0],
            }
        results.append(outcome)

    conn.close()

    # The typed builder renders rather than calls, so these cases need no
    # server. Each binding builds with its own fluent API; the rendered output
    # is compared.
    queries = []
    for case in suite["queries"]:
        try:
            queries.append(
                {
                    "name": case["name"],
                    "status": "ok",
                    "rendered": build_query(case["build"]).render(core),
                }
            )
        except ProtocolError as e:
            queries.append(
                {"name": case["name"], "status": "clientError", "message": str(e).split("\n")[0]}
            )

    print(
        json.dumps(
            {
                "protocolVersion": welcome["protocolVersion"],
                "results": results,
                "queries": queries,
            },
            indent=2,
            # Python escapes non-ASCII by default and JavaScript does not, so
            # without this the two runners print the same string differently and
            # the comparison fails on its own formatting rather than on the SDKs.
            ensure_ascii=False,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
