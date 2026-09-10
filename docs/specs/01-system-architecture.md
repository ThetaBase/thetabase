# System Architecture Doc

ThetaBase v1 — Technical Architecture Specification

---

## 1. Overview

ThetaBase is a log-structured, branchable database engine designed around one operating assumption: **most writes and schema changes will be authored by an AI agent, not typed by a human.** Every architectural decision below optimizes for that: safe by construction, cheap to branch, deterministic to query, and reversible by default.

---

## 2. Component Map

| Component | Runtime | Responsibility |
|---|---|---|
| Control Plane | Rust (Axum), horizontally scaled | Org/project graph, identity resolution, token minting, provisioning orchestration, billing hooks |
| Storage Engine (`thetad`) | Rust, Tokio, one process per project (what separates them today: §8.1) | Append-only log, materialized views, branching, CRDT merge, query execution |
| Query Planner | Embedded in `thetad` | Parses typed query language / SQL-subset, produces deterministic execution plan, cost estimation |
| Safety Layer | Embedded in `thetad`, pre-commit hook | Diff generation, destructive-change classification, blast-radius estimation, circuit breaker |
| Scribe (Edge SDK) | WASM, runs in user's app runtime (edge or origin) | Connection pooling, local read cache, request batching, token refresh |
| Cold Archive | Object storage (S3-compatible) | Periodic log snapshots, point-in-time restore source |
| AI Query-Assist (optional layer) | Separate service, called only on request | Translates NL → typed query candidate; never executes directly |

---

## 3. Storage Engine

### 3.1 Log Structure
- Append-only, content-addressed event log per project, structured as a Merkle DAG (git-like commit graph).
- Each entry: `{ prev_hash, op_type, payload, author (user|agent session id), timestamp, branch_id }`.
- Materialized state (the "current table view") is a fold over the log up to a given commit, cached and incrementally updated — not recomputed from genesis on every read.
- **The chain is verified on replay, not merely written** (SEC-8). Every entry's hash covers its contents, so altering an entry changes its hash and orphans its successor; recovery refuses a log whose links do not resolve rather than replaying it as intact. Two limits, asserted by tests rather than assumed: startup resumes from a checkpoint and so cannot speak for entries the view snapshot already covers — `DurableLogStore::verify_chain` is the full-log pass for that — and an edit to the *last* entry of a branch breaks no link, because nothing points at its hash. Detecting that needs an anchor outside the log (a signed, published head), which is v2 and is not claimed today.

### 3.2 Branching
- A branch is a pointer to a commit in the DAG plus a private write-ahead segment.
- Branch creation is O(1) — copy a pointer, not the data (copy-on-write).
- Every agent session, PR, and preview environment gets its own branch by default.
- Branches can be merged, discarded, or promoted to become the new `main` pointer.

### 3.3 Merge Semantics
- **CRDT types are the default schema primitives** for anything mutable under concurrent branches: counters (G-Counter/PN-Counter), registers (LWW-Register with explicit tie-break rule), sets (OR-Set), sequences where ordering matters.
- Merges of CRDT-typed fields are automatic and provably convergent — no human or model judgment involved.
- Any field/table not expressible as a CRDT (arbitrary structured data with semantic meaning, e.g. a hand-edited document) produces an **explicit conflict object** on merge attempt — surfaced to a human, never auto-resolved by an LLM.

#### 3.3.1 A converging field is written by operation, never by value

A CRDT-typed row has its own write request carrying a *mutation*, separate from
`put`. This is not an ergonomic choice. A CRDT converges because replaying its
operations in any order gives the same answer, and two absolute values cannot be
reconciled without choosing one — so a caller that could reach a converging field
through `put` would be writing exactly the thing the field exists to avoid, and
the convergence guarantee would hold only for callers who happened not to.

**A mutation that disagrees with the field's declared kind is refused before it
reaches the log.** The fold records a mismatched operation as rejected and moves
on, which is right for a fold — it must never reinterpret what it is replaying —
and wrong as an answer to a caller: the entry would be in the log, the write
would have been reported as succeeding, and the value would not have changed.
The declared kind and the kind the row already is are both checked, because a
branch can hold state written under a schema that has since changed.

**The server assigns a sequence element's id; the request cannot carry one.** An
RGA element's id fixes both its identity and its order among concurrent
siblings, so a client-chosen id could collide with another writer's element or
displace one. It is `(commit, branch)` — unique by construction, and the reason
replaying an entry twice is idempotent. The wire type has no field for it, so
this is a shape the protocol cannot express rather than a value the server
remembers to ignore.

Removal is the one place an id travels inward, and must be: it names an element
the caller read. That makes the live element ids a readable property of a
sequence — without them the removal request would be one nobody could issue.

### 3.4 Schema
- Schema is versioned in the same log (schema changes are just another op type), giving branch-and-merge semantics to schema, not just data.
- Type inference is optional for prototyping (dynamic mode) but every field has a canonical type once written; the engine tracks type provenance so agents/tools can query "what type is this, really" deterministically — no silent re-coercion of existing data.

---

## 4. Query Execution Path

- Primary interface: typed query builder / SQL-subset compiled to a deterministic execution plan (bytecode), cached by plan hash.
- No LLM inference sits in the execution path for `get`/`put`/typed `query()` calls — these are the hot path and must hit single-digit-ms latencies.
- AI Query-Assist is a **separate, explicitly-invoked** service: given an NL prompt, it returns a candidate typed query (with an EXPLAIN-style preview) for a human or agent to accept, edit, or reject — never runs against the live database on its own authority.

---

## 5. Safety Layer (pre-commit)

Runs on every proposed schema change and every write batch above a configurable size threshold, before it lands on the branch it targets:

1. **Classify**: non-destructive (add column/index, widen type, new table) vs. destructive (drop, narrow, non-null backfill, bulk delete/update above threshold).
2. **Non-destructive** → auto-applies, logged.
3. **Destructive** → generates a structured diff (rows affected, reversibility, estimated cost) and either:
   - requires explicit confirmation (human or a declared policy rule), or
   - is auto-redirected to run on an ephemeral shadow branch first, with results surfaced before any merge to a protected branch (`main`, `prod`) is allowed.
4. **Blast-radius guardrail**: every write path carries a cost/row-impact estimate; operations exceeding a per-project configurable ceiling trip a circuit breaker and require confirmation regardless of destructive/non-destructive classification (catches runaway loops, not just single bad commands).

---

## 6. Provisioning & Multi-Tenancy

- One `thetad` logical instance per project (can share physical VM/host with other low-traffic projects behind isolation boundaries; dedicated resources above usage thresholds — same tiering logic as the original concept's autoscaling rules).
- No cross-project data path exists at any layer, including the Safety Layer's audit store. That is checked in code; what separates two running projects from each other is a deployment property and is stated in §8.1.
- Provisioning is orchestrated by the Control Plane using org-graph resolution (see Provisioning & Identity Flow Spec) — no manual dashboard step in the request path.

---

## 7. Failure Modes & Recovery

Operator procedures for each row are in [`docs/RUNBOOKS.md`](../RUNBOOKS.md),
which also records, per failure mode, whether the behaviour below has actually
been *observed* or only implemented. That distinction is what M10's gate turns
on: a runbook nobody has followed is a hypothesis.


| Scenario | Behavior |
|---|---|
| Storage node crash | Auto-restart, replay WAL from last durable checkpoint, resume in seconds |
| Archive/object storage unavailable | Writes continue locally, snapshots queue and retry; extended outage pages the team and falls back to a secondary local snapshot target |
| Query planner degraded/slow | Falls back to a simpler, non-optimized plan rather than blocking; never falls back to unchecked raw execution |
| Merge conflict (non-CRDT field) | Surfaced explicitly, blocked from auto-merge, never resolved silently |
| Destructive change attempted | Blocked pre-commit unless confirmed or run through shadow-branch validation first |

---

## 8. What Is Explicitly Out of Scope for v1

### 8.1 What separates two projects, as built

This section exists because the two lines above used to say "process/VM
boundary", and an external review measured that against the code and found
something narrower.

**As built, two projects on one host are separated by an OS user, not by a
process/VM sandbox.** Each project runs as its own `thetad` process under its own
account, and file permissions are what stop one reading another's data
directory. That is a real boundary and it is not the one the phrase "process/VM
isolation" leads a reader to expect: it does not survive a local privilege
escalation, and there is no per-project sandbox, container or VM enforcing it.
The key-injection path that would give each project a distinct data key without
the host ever holding all of them is **not implemented**.

`claims.toml` has said the honest version all along — the entry
`in-process-isolation-is-checked-in-code-not-in-deployment` records that a
deployment really running two projects as two processes "is not checkable from
inside the repository" and that "a per-project sandbox that made the boundary
compiler-enforced is not built". The specs had not caught up. They have now.

What *is* checked, and is worth keeping separate from the above: no code path in
any single-project crate accepts two project identifiers, and a test reads every
such crate and fails if one does. Cross-project access is impossible in the
engine regardless of what the deployment does. The gap is that "impossible in the
engine" and "isolated at the process boundary" are different claims, and only the
first is currently earned.



- Cross-region multi-master writes (single active region per project at launch; replicas are read-only).
- Arbitrary stored procedures / triggers with side effects outside the log (keeps the log as a complete, replayable source of truth).
- LLM-mediated conflict resolution of any kind.

---

## Embedded deployment

The same engine, in the caller's process. Not a subset: a subset diverges, which
is the argument the Scribe core makes about the protocol applied to the engine.
The embedded facade links `thetad`'s engine and reimplements none of its
decisions.

### The gate comes along; the caller answers it

An embedded ThetaBase with no gate is a different product, so the gate is not
optional. A destructive change is classified exactly as it would be on a server
and refused exactly as hard, and shadow validation works unchanged because an
ephemeral branch is a data structure rather than a deployment.

What changes is who reviews. In a hosted deployment a human does; in-process
there may be no human and no queue, so the caller is the reviewer and says so by
calling `confirm`. There is deliberately **no `auto_confirm` flag, no `force`,
and no configuration that disables the gate**. A caller who wants a drop applied
writes the confirmation, which is a line somebody can find in review.
Confirmation is still not sufficient at the shadow gate (`07-agent-safety-layer.md`
§4) — offering an embedded caller a way past it would make the embedded build
the weaker product this design exists to avoid.

The safety policy is a required argument to `open` rather than a default,
because the hosted product's policy is signed by the Control Plane and embedded
there is nobody to sign one. A default would be a gate somebody did not know
they had.

### What embedding does not give you

Stated as a value in code rather than as prose, so a caller can assert on it and
so removing one is a diff:

- **No cross-project isolation.** The hosted guarantee is one project per process
  (`04-threat-model-security.md` §3). Embedded, the process is the caller's: two
  projects opened in it share an address space. That is the absence of the
  guarantee, not a weaker version of it.
- **No token scoping or revocation**, because there is no credential.
- **No managed archive, anchoring or restore drills** unless the caller runs
  them.
- **An audit trail the caller can edit**, because the process holding it is
  theirs.

### Sync is not merge

Merging branches is three-way over one log: both sides share a base and the
chain links them. An embedded instance and a hosted one share no chain at all
— two logs about the same data, neither holding a hash the other has seen.

So sync carries entries across and **re-appends** them, which necessarily gives
them new chain hashes. Two consequences follow, and both are load-bearing:

**Digests are built from content hashes, not chain hashes.** Two instances
holding the same write have the same content hash and different chain hashes, so
a digest of chain hashes would report everything as missing on both sides and
carry the whole log on every sync.

**Signatures cover content, not chain position.** This is why
`LogEntry::content_hash` exists separately from `LogEntry::hash`. A signature
over the chain hash would die on any legitimate re-append, so signed commits and
sync would be mutually exclusive — and nobody would find out until they used
both. Reordering is still caught, by the chain, which is its job.

**Sync resolves nothing.** Two sides that wrote the same key independently are a
conflict and it goes to a human (`03-data-model-consistency.md` §3.2). A sync
that picked a side would be auto-resolving a non-CRDT conflict at the largest
scale available. Concurrent CRDT mutations are excluded, because converging is
what those are for.

**Two synced instances still have two chains.** They agree about content and do
not share an identity, which is why each still needs its own anchor.
