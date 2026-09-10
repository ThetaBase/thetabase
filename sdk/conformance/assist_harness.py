#!/usr/bin/env python3
"""Both SDKs against a live `theta-assist`, agreeing on the same answer.

The same argument as `harness.py` makes for the wire protocol: two clients that
each pass their own tests prove nothing about whether they agree with each
other. Here the shared thing is smaller — one HTTP endpoint — but the failure it
catches is the same one, and it is the failure that reaches a user as "it works
in Python".

The service is started with a scripted model, so this measures the SDKs rather
than a model provider, and needs no API key. A conformance run that depended on
a third party would be a conformance run that goes red when that third party has
a bad afternoon.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = {
    "tables": {
        "users": {
            "name": "users",
            "fields": {
                "email": {
                    "name": "email",
                    "ty": "text",
                    "nullable": False,
                    "crdt": None,
                    "declared_at": None,
                }
            },
            "indexes": [],
        }
    }
}
QUESTION = "every user's email address"


def wait_for(url: str, timeout_s: float = 30.0) -> None:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, OSError):
            time.sleep(0.1)
    raise SystemExit(f"assist did not become healthy at {url}")


def python_sdk(base_url: str) -> dict:
    sys.path.insert(0, str(ROOT / "sdk" / "python" / "src"))
    import thetabase  # noqa: E402

    theta = thetabase.Theta("conformance/assist", assist_url=base_url)
    return theta.assist.suggest_query(QUESTION, SCHEMA)


def typescript_sdk(base_url: str) -> dict:
    """Drive the TS SDK through Node, and return what it produced."""
    script = f"""
    import {{ Theta }} from "{(ROOT / 'sdk' / 'typescript' / 'dist' / 'index.js').as_posix()}";
    const theta = new Theta({{ project: "conformance/assist", assistUrl: {json.dumps(base_url)} }});
    const candidate = await theta.assist.suggestQuery(
      {json.dumps(QUESTION)},
      {json.dumps(SCHEMA)},
    );
    process.stdout.write(JSON.stringify(candidate));
    """
    result = subprocess.run(
        ["node", "--input-type=module", "-e", script],
        capture_output=True,
        text=True,
        cwd=ROOT / "sdk" / "typescript",
    )
    if result.returncode != 0:
        raise SystemExit(f"the TypeScript SDK failed:\n{result.stderr}")
    return json.loads(result.stdout)


def main() -> int:
    port = os.environ.get("THETA_ASSIST_TEST_PORT", "7811")
    base_url = f"http://127.0.0.1:{port}"

    service = subprocess.Popen(
        [
            str(ROOT / "target" / "debug" / "examples" / "assist_scripted"),
            "--listen",
            f"127.0.0.1:{port}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for(f"{base_url}/healthz")

        py = python_sdk(base_url)
        ts = typescript_sdk(base_url)

        # Neither SDK may report having run anything. This is the property the
        # whole milestone is about, checked at the surface a user actually
        # touches rather than only inside the service.
        for name, candidate in (("python", py), ("typescript", ts)):
            executed = candidate.get("executed")
            if executed is not False:
                print(f"{name} reported executed={executed!r}; it must be False")
                return 1
            if not candidate.get("sql"):
                print(f"{name} returned no SQL for a reviewer to read")
                return 1

        # The plan is the thing that would run, so it is the thing that has to
        # match. Field names differ between the two surfaces by design — the TS
        # facade renames `preview` to `planPreview` — so compare what is shared.
        py_plan = py["plan"]
        ts_plan = ts["plan"]
        if py_plan != ts_plan:
            print("the two SDKs produced different plans for one question:")
            print(f"  python:     {json.dumps(py_plan, sort_keys=True)}")
            print(f"  typescript: {json.dumps(ts_plan, sort_keys=True)}")
            return 1

        if py["sql"] != ts["sql"]:
            print(f"different SQL: {py['sql']!r} vs {ts['sql']!r}")
            return 1

        py_preview = py["preview"]
        ts_preview = ts["planPreview"]
        if py_preview["planHash"] != ts_preview["planHash"]:
            print("the previews disagree about the plan hash")
            return 1
        if py_preview["llmCalls"] != 0 or ts_preview["llmCalls"] != 0:
            print("a preview claims the plan makes a model call when it runs")
            return 1

        print(
            "assist conformance: both SDKs agree on one candidate plan, "
            "neither executed it, and both carry a reviewable preview"
        )
        return 0
    finally:
        service.terminate()
        service.wait(timeout=10)


if __name__ == "__main__":
    raise SystemExit(main())
