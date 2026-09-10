# Working in this repository

## What this is

ThetaBase: a log-structured, branchable database built for workloads where the
writer is an autonomous agent rather than a person. Read `README.md` for the
shape of it and `docs/ROADMAP.md` for what is built and what is next.

`docs/specs/` holds nine design specs and is the **source of truth**. Code that
contradicts a spec is a bug in one of the two — decide which deliberately, and
update the other. Do not let them drift.

## Invariants — do not break these without changing the spec first

These are not style preferences. Each one is load-bearing for a claim the
product makes, and several are enforced by tests that will fail loudly.

1. **No LLM call on the hot path.** `get`, `put`, and typed `query` must never
   reach a model. `crates/thetad/tests/no_llm_on_hot_path.rs` enforces this by
   checking that no HTTP or model client exists in the dependency closure of the
   hot-path crates. Adding `reqwest` to `theta-core` will fail CI, and that is
   the point.

2. **The Safety Layer is rule-based.** No inference, no heuristics that could be
   argued with, anywhere in `theta-safety`. Classification is a pure function of
   (change kind, row impact, reversibility, branch protection). This is what
   makes it impossible to prompt-inject into misclassifying a drop as safe.

3. **Prevent, don't correct.** Never rewrite a proposal or coerce a value to
   make it work. A rejected write is rejected; the caller submits a corrected
   one. Silent coercion corrupts data invisibly, which is worse than the problem
   it solves.

4. **No raw strings in the execution path.** The query IR has no `Raw(String)`
   variant and must never gain one. The SQL front end parses *into* typed plan
   nodes; parameters are bound, never interpolated.

5. **Non-CRDT conflicts go to a human.** Never auto-resolve, never guess, never
   ask a model.

6. **The log is the only source of truth.** No state that is not a deterministic
   fold over log entries. No side effects outside the log.

## Conventions

- Every module that is not fully implemented starts its doc comment with
  `STATUS:` and names the ROADMAP milestone that delivers it. Unimplemented
  functions return a clear error naming that milestone — they never silently
  return a default or degrade to something unsafe.
- Comments explain *why*, especially which spec section a decision comes from.
  Cite them as `docs/specs/07-agent-safety-layer.md §4`.
- Test names are sentences describing the property under test, not
  `test_foo_2`. A failing test should read like a bug report.
- Safety-relevant behavior gets a test that would fail if the behavior
  regressed. "It's obviously correct" is how the two bugs already found in this
  repo got written.

## Before you push

```sh
make check   # fmt, clippy (warnings are errors), all tests
make gates   # the three live validation gates
```

Both must be green. If a gate fails, fix the code — do not weaken the gate. The
adversarial corpus in particular only ever grows: entries are never deleted to
make the suite pass.
