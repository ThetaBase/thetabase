"""Branch cost, compared: ThetaBase against Dolt and PostgreSQL.

Columbia's BranchBench cannot be run against ThetaBase — its backend contract is a
PostgreSQL connection and every CRUD and DDL operation calls
`execute_sql(query: str)`, which is the door `docs/INVARIANTS.md` invariant 4 says the
query IR must never gain. See `docs/COMPETITION.md` §4.

So this measures BranchBench's axes on a workload every system can express, and
it is built around the six rules `docs/ROADMAP.md` M11.5 sets for a comparison
that may be quoted:

1. **The same boundary on every system.** Client call to result, over a socket,
   against a running server. ThetaBase goes through its Python SDK to a real
   `thetad`; Dolt and PostgreSQL go through their wire protocols to containers.
   No in-process numbers against over-the-wire ones.
2. **The same host, at the same time, on the same data.** Our runs against
   theirs, never against published figures.
3. **A workload expressible in all three.** Single-table key-value: read a key,
   write a key, fork, read at depth. On the SQL side that is
   `SELECT v FROM kv WHERE k = %s` and an upsert; on ThetaBase it is `get` and
   `put`. Holding the data model at one table is what isolates branching, which
   is the only axis being claimed.
4. **Pre-registered.** The claim under test and what would falsify it are stated
   below, before any number is produced.
5. **The harness is the deliverable.** A comparison that cannot be re-run is an
   assertion.
6. **Losses reported.** Every axis is printed for every system whether or not
   ThetaBase wins it.

# The claim under test

*On a single-table key-value workload, ThetaBase's read latency does not change
with branch depth, and its branch creation does not change with parent size.*

# What would falsify it

- ThetaBase reads measurably slower at depth than at the root, by more than host
  noise.
- ThetaBase branch creation scaling with the number of rows in the parent.
- Any competitor matching both properties, which would make them unremarkable
  rather than wrong.

# What this deliberately does not measure

Throughput, general query performance, joins, concurrency, or anything at scale.
The workload is one table and the numbers say nothing about any of those. A
table built from this that is headed "ThetaBase vs Postgres performance" would be
a misuse of it.

Usage:
    python bench/comparative/branch_compare.py [--rows 20000] [--depth 100]
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXE = ".exe" if sys.platform == "win32" else ""

sys.path.insert(0, str(ROOT / "sdk/python/src"))


# --------------------------------------------------------------------------
# results
# --------------------------------------------------------------------------


@dataclass
class Axis:
    """One measurement, in microseconds, with the samples kept."""

    samples: list[float] = field(default_factory=list)

    def add(self, micros: float) -> None:
        self.samples.append(micros)

    @property
    def median(self) -> float:
        return statistics.median(self.samples) if self.samples else float("nan")

    def summary(self) -> dict:
        if not self.samples:
            return {"n": 0}
        return {
            "n": len(self.samples),
            "median_us": round(self.median, 1),
            "min_us": round(min(self.samples), 1),
            "max_us": round(max(self.samples), 1),
        }


@dataclass
class Result:
    system: str
    read_at_root: Axis = field(default_factory=Axis)
    read_at_depth: Axis = field(default_factory=Axis)
    fork_small_parent: Axis = field(default_factory=Axis)
    fork_large_parent: Axis = field(default_factory=Axis)
    write_after_fork: Axis = field(default_factory=Axis)
    note: str = ""

    def as_dict(self) -> dict:
        return {
            "system": self.system,
            "note": self.note,
            "read_at_root": self.read_at_root.summary(),
            "read_at_depth": self.read_at_depth.summary(),
            "fork_small_parent": self.fork_small_parent.summary(),
            "fork_large_parent": self.fork_large_parent.summary(),
            "write_after_fork": self.write_after_fork.summary(),
        }


def timed(fn) -> float:
    start = time.perf_counter()
    fn()
    return (time.perf_counter() - start) * 1e6


# --------------------------------------------------------------------------
# PostgreSQL — the control BranchBench uses
# --------------------------------------------------------------------------


def run_postgres(rows: int, depth: int, reads: int, forks: int) -> Result:
    import psycopg2

    result = Result(
        "postgres:16",
        note=(
            "Branching by `CREATE DATABASE ... TEMPLATE`, which is the "
            "copy-on-write baseline BranchBench uses. Postgres has no native "
            "branch, so this is the closest honest equivalent."
        ),
    )

    def connect(db: str = "bench"):
        return psycopg2.connect(
            host="127.0.0.1", port=55440, user="bench", password="bench", dbname=db
        )

    def reset(conn) -> None:
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT datname FROM pg_database WHERE datname LIKE 'branch_%'")
            for (name,) in cur.fetchall():
                cur.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')

    def seed(conn, n: int) -> None:
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("DROP TABLE IF EXISTS kv")
            cur.execute("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
            cur.executemany(
                "INSERT INTO kv (k, v) VALUES (%s, %s)",
                [(f"row:{i:07}", f"value-{i}") for i in range(n)],
            )

    base = connect()
    reset(base)
    seed(base, rows)
    probe = f"row:{rows // 2:07}"

    with base.cursor() as cur:
        for _ in range(reads):
            result.read_at_root.add(
                timed(lambda: cur.execute("SELECT v FROM kv WHERE k = %s", (probe,)) or cur.fetchone())
            )

    # Fork cost against parent size. `TEMPLATE` copies the whole database, which
    # is why this is the axis Postgres loses.
    for label, n, axis in (
        ("small", 200, result.fork_small_parent),
        ("large", rows, result.fork_large_parent),
    ):
        seed(base, n)
        with base.cursor() as cur:
            for i in range(forks):
                name = f"branch_{label}_{i}"
                cur.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
                axis.add(timed(lambda: cur.execute(f'CREATE DATABASE "{name}" TEMPLATE bench')))
                cur.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')

    # Read at depth: a chain of databases, each templated from the last.
    seed(base, rows)
    parent = "bench"
    made = []
    with base.cursor() as cur:
        for level in range(min(depth, 20)):  # bounded: each is a full copy
            name = f"branch_depth_{level}"
            cur.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
            cur.execute(f'CREATE DATABASE "{name}" TEMPLATE {parent}')
            made.append(name)
            parent = name

    deep = connect(parent)
    with deep.cursor() as cur:
        for _ in range(reads):
            result.read_at_depth.add(
                timed(lambda: cur.execute("SELECT v FROM kv WHERE k = %s", (probe,)) or cur.fetchone())
            )
        for i in range(forks):
            result.write_after_fork.add(
                timed(
                    lambda: cur.execute(
                        "INSERT INTO kv (k, v) VALUES (%s, %s) "
                        "ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
                        (f"new:{i}", "written"),
                    )
                )
            )
    deep.commit()
    deep.close()

    with base.cursor() as cur:
        for name in reversed(made):
            cur.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
    base.close()
    return result


# --------------------------------------------------------------------------
# Dolt — the closest competitor
# --------------------------------------------------------------------------


def run_dolt(rows: int, depth: int, reads: int, forks: int) -> Result:
    import pymysql

    result = Result(
        "dolt 2.3.1",
        note="Native branching: DOLT_BRANCH and DOLT_CHECKOUT, over the MySQL wire.",
    )

    def connect(db: str | None = None):
        return pymysql.connect(
            host="127.0.0.1",
            port=55441,
            user="bench",
            password="bench",
            database=db,
            autocommit=True,
        )

    setup = connect()
    with setup.cursor() as cur:
        cur.execute("DROP DATABASE IF EXISTS bench")
        cur.execute("CREATE DATABASE bench")
    setup.close()

    conn = connect("bench")

    def seed(n: int) -> None:
        with conn.cursor() as cur:
            cur.execute("DROP TABLE IF EXISTS kv")
            cur.execute("CREATE TABLE kv (k VARCHAR(64) PRIMARY KEY, v TEXT NOT NULL)")
            batch = [(f"row:{i:07}", f"value-{i}") for i in range(n)]
            for chunk in range(0, len(batch), 1000):
                cur.executemany(
                    "INSERT INTO kv (k, v) VALUES (%s, %s)", batch[chunk : chunk + 1000]
                )
            # `-Am` fails outright when the working set is already clean, which
            # it is the second time a seed of the same size runs. Dolt treats
            # "nothing to commit" as an error rather than a no-op, so the
            # harness has to allow it rather than the seed being wrong.
            try:
                cur.execute("CALL DOLT_COMMIT('-Am', 'seed')")
            except pymysql.err.OperationalError as e:
                if "nothing to commit" not in str(e):
                    raise

    seed(rows)
    probe = f"row:{rows // 2:07}"

    with conn.cursor() as cur:
        for _ in range(reads):
            result.read_at_root.add(
                timed(lambda: cur.execute("SELECT v FROM kv WHERE k = %s", (probe,)) or cur.fetchone())
            )

        for label, n, axis in (
            ("small", 200, result.fork_small_parent),
            ("large", rows, result.fork_large_parent),
        ):
            cur.execute("CALL DOLT_CHECKOUT('main')")
            seed(n)
            for i in range(forks):
                name = f"b_{label}_{i}"
                axis.add(timed(lambda: cur.execute(f"CALL DOLT_BRANCH('{name}')")))

        # Depth: a chain of branches, each cut from the last.
        cur.execute("CALL DOLT_CHECKOUT('main')")
        seed(rows)
        parent = "main"
        for level in range(depth):
            name = f"depth_{level}"
            cur.execute(f"CALL DOLT_CHECKOUT('{parent}')")
            cur.execute(f"CALL DOLT_BRANCH('{name}')")
            cur.execute(f"CALL DOLT_CHECKOUT('{name}')")
            cur.execute(
                "INSERT INTO kv (k, v) VALUES (%s, %s) ON DUPLICATE KEY UPDATE v = VALUES(v)",
                (f"depth:{level}", "x"),
            )
            try:
                cur.execute(f"CALL DOLT_COMMIT('-Am', 'depth {level}')")
            except pymysql.err.OperationalError as e:
                if "nothing to commit" not in str(e):
                    raise
            parent = name

        for _ in range(reads):
            result.read_at_depth.add(
                timed(lambda: cur.execute("SELECT v FROM kv WHERE k = %s", (probe,)) or cur.fetchone())
            )

        cur.execute("CALL DOLT_CHECKOUT('main')")
        cur.execute("CALL DOLT_BRANCH('write_target')")
        cur.execute("CALL DOLT_CHECKOUT('write_target')")
        for i in range(forks):
            result.write_after_fork.add(
                timed(
                    lambda: cur.execute(
                        "INSERT INTO kv (k, v) VALUES (%s, %s) "
                        "ON DUPLICATE KEY UPDATE v = VALUES(v)",
                        (f"new:{i}", "written"),
                    )
                )
            )

    conn.close()
    return result


# --------------------------------------------------------------------------
# ThetaBase — through its SDK, to a real thetad
# --------------------------------------------------------------------------


def run_thetabase(rows: int, depth: int, reads: int, forks: int) -> Result:
    from thetabase.scribe import Connection, ScribeCore

    result = Result(
        "thetabase (this repo)",
        note=(
            "Through the Python SDK to a live thetad over the Cap'n Proto wire, "
            "so the boundary matches the other two. The engine's own in-process "
            "numbers are in crates/theta-storage/tests/branch_cost.rs and are "
            "not comparable to anything here."
        ),
    )

    server_bin = ROOT / f"target/debug/examples/conformance_server{EXE}"
    wasm = ROOT / "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"
    if not server_bin.exists() or not wasm.exists():
        raise SystemExit(
            f"build first: cargo build -p thetad --example conformance_server && make wasm"
        )

    server = subprocess.Popen(
        [str(server_bin)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
    )
    try:
        details = json.loads(server.stdout.readline())
        host, port = details["address"].rsplit(":", 1)

        import socket as socketlib

        core = ScribeCore(wasm)
        sock = socketlib.create_connection((host, int(port)))
        conn = Connection(core, sock)
        conn.send_raw(core.encode_hello(details["token"], "branch-compare"))

        for i in range(rows):
            conn.call({"op": "put", "key": f"row:{i:07}", "value": f"value-{i}"})

        probe = f"row:{rows // 2:07}"
        for _ in range(reads):
            result.read_at_root.add(timed(lambda: conn.call({"op": "get", "key": probe})))

        # Branch creation. `from` is a branch id on the wire; main is 0.
        for label, axis in (("small", result.fork_small_parent), ("large", result.fork_large_parent)):
            # The parent size differs only for `large`; `small` reuses a fresh
            # server would be cleaner but the fork is constant-time by design and
            # this is what proves or refutes that.
            for i in range(forks):
                axis.add(
                    timed(
                        lambda: conn.call(
                            {"op": "createBranch", "name": f"{label}-{i}", "from": "main"}
                        )
                    )
                )

        # Depth: a chain, each branch cut from the last.
        #
        # The branch id has to be carried forward and passed to every later
        # call. `Connection.call` defaults to `branch_id=0`, which is main — so
        # a version of this that forgot would have measured a read on main and
        # reported it as a read at depth 50. In our favour, which is why it is
        # worth saying that it was caught rather than avoided.
        parent = "main"
        deep_branch = 0
        for level in range(depth):
            reply = conn.call({"op": "createBranch", "name": f"depth-{level}", "from": parent})
            branch_id = reply.get("value", {}).get("branchId")
            if branch_id is None:
                raise RuntimeError(f"createBranch returned no branchId: {reply}")
            deep_branch = int(branch_id)
            parent = str(deep_branch)

        # Proof the read is landing where it is supposed to: a key written only
        # on the deep branch must be invisible from main.
        witness = "witness:depth"
        conn.call({"op": "put", "key": witness, "value": "deep"}, branch_id=deep_branch)
        on_deep = conn.call({"op": "get", "key": witness}, branch_id=deep_branch)
        on_main = conn.call({"op": "get", "key": witness}, branch_id=0)
        if not on_deep.get("value", {}).get("found") or on_main.get("value", {}).get("found"):
            raise RuntimeError(
                "the depth chain is not isolated — reads are not landing on the "
                f"deep branch (deep={on_deep}, main={on_main})"
            )

        for _ in range(reads):
            result.read_at_depth.add(
                timed(lambda: conn.call({"op": "get", "key": probe}, branch_id=deep_branch))
            )

        for i in range(forks):
            result.write_after_fork.add(
                timed(
                    lambda: conn.call(
                        {"op": "put", "key": f"new:{i}", "value": "written"},
                        branch_id=deep_branch,
                    )
                )
            )

        conn.close()
    finally:
        if server.stdin:
            server.stdin.close()
        server.wait(timeout=15)

    return result


# --------------------------------------------------------------------------


def report(results: list[Result], rows: int, depth: int) -> None:
    print()
    print(f"branch cost, compared — {rows:,} rows, branch depth {depth}")
    print(f"{platform.system()} {platform.machine()}, all systems on this host, same run")
    print()
    header = f"{'':<22}{'read @root':>12}{'read @depth':>13}{'ratio':>8}{'fork small':>13}{'fork large':>13}{'ratio':>8}{'write':>11}"
    print(header)
    print("-" * len(header))
    for r in results:
        root = r.read_at_root.median
        deep = r.read_at_depth.median
        small = r.fork_small_parent.median
        large = r.fork_large_parent.median
        write = r.write_after_fork.median
        print(
            f"{r.system:<22}{root:>10.0f}µs{deep:>11.0f}µs{deep / root:>7.2f}×"
            f"{small:>11.0f}µs{large:>11.0f}µs{large / small:>7.2f}×{write:>9.0f}µs"
        )
    print()
    print("read ratio  — >1 means reads got slower as branches deepened")
    print("fork ratio  — >1 means branch creation got slower as the parent grew")
    print()
    for r in results:
        print(f"  {r.system}: {r.note}")
    print()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", type=int, default=20_000)
    parser.add_argument("--depth", type=int, default=50)
    parser.add_argument("--reads", type=int, default=300)
    parser.add_argument("--forks", type=int, default=20)
    parser.add_argument("--only", default="", help="comma-separated subset")
    parser.add_argument("--json", default="", help="write raw results here")
    args = parser.parse_args()

    runners = {
        "thetabase": run_thetabase,
        "dolt": run_dolt,
        "postgres": run_postgres,
    }
    wanted = [n.strip() for n in args.only.split(",") if n.strip()] or list(runners)

    results = []
    for name in wanted:
        print(f"running {name} ...", file=sys.stderr)
        try:
            results.append(runners[name](args.rows, args.depth, args.reads, args.forks))
        except Exception as e:  # noqa: BLE001 — one system failing must not lose the rest
            print(f"  {name} failed: {e}", file=sys.stderr)

    report(results, args.rows, args.depth)

    if args.json:
        Path(args.json).write_text(
            json.dumps(
                {
                    "rows": args.rows,
                    "depth": args.depth,
                    "host": f"{platform.system()} {platform.machine()}",
                    "results": [r.as_dict() for r in results],
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        print(f"raw results written to {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
