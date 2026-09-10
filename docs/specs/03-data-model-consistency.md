# Data Model & Consistency Specification

ThetaBase v1

---

## 1. Purpose

This doc states, precisely and defensibly, what guarantees ThetaBase offers — so the claims survive adversarial questioning from an engineer evaluating whether to trust it with real state.

---

## 2. Data Model

### 2.1 Log as ground truth
All state is derived from an append-only, content-addressed event log (Merkle DAG). There is no "current row" that isn't a deterministic fold over log entries up to some commit. This is what makes every guarantee below provable rather than asserted.

### 2.2 Schema-typed fields
Every field has a canonical type, tracked with provenance. Dynamic/schemaless writes are allowed during prototyping, but the engine never silently reinterprets a value's type after the fact (e.g., no auto-coercing a malformed value into the field's declared type without surfacing that as a rejected/flagged write — see Agent-Safety Layer Spec).

### 2.3 CRDT primitives for concurrent-safe types
| Type | Backing CRDT | Guarantee |
|---|---|---|
| Counter | PN-Counter | Merges are commutative, associative, idempotent — order of merge doesn't matter |
| Flag/last-write field | LWW-Register w/ explicit tie-break (commit timestamp + branch id) | Deterministic winner on concurrent write, no ambiguity |
| Collection (unordered) | OR-Set | Add/remove converge correctly even with concurrent adds/removes of the same element |
| Ordered sequence | RGA/Yjs-style sequence CRDT | Concurrent inserts converge to a consistent order |

### 2.4 Non-CRDT structured data
Arbitrary nested documents/objects without a declared CRDT shape are treated as **conflict-eligible**: concurrent branch modifications to the same object produce an explicit conflict record on merge, never an automatic or model-guessed resolution.

---

## 3. Consistency Guarantees

### 3.1 Within a branch
- **Read-your-writes**, always, for the client that performed the write (session-consistent).
- **Strong eventual consistency** for other readers of the same branch: given the same set of applied operations, all replicas converge to the same state (standard SEC definition — this is provable given the CRDT/log design, not just claimed).
- **Writes to a branch are totally ordered.** One project is one log served by one process, so concurrent writes to the same key do not interleave: every one of them succeeds, and the surviving value is always one somebody wrote — never a blend, never a missing key. Asserted by `concurrent_writers_to_one_key_serialize_to_one_of_their_values`.
- **Read-modify-write needs a precondition**, and has one. An unconditional `put` succeeds regardless of what the row held, so two clients doing get-then-put would both win and one update would be lost. `putIf` carries a precondition — the row is absent, or it is at exactly the version a `get` returned — checked in the same call that appends, and refused with the row's *current* version so a retry costs no extra round trip. `crates/thetad/tests/conditional_write.rs` runs 24 clients at one key and requires every increment to survive; the same loop without a precondition provably loses updates, which is what makes the first test evidence rather than decoration.

  This was previously written as "no global linearizability guarantee across concurrent writers to the same key". That understated it in both directions: the order *is* global within a branch, and what was missing was atomic read-modify-write, which is a narrower and more useful thing to say.

### 3.2 Across branches
- Each branch is fully isolated until merge — no partial visibility of another branch's uncommitted writes.
- Merge is deterministic for CRDT-typed fields; non-deterministic (human-arbitrated) for conflict-eligible fields, and the system never proceeds past a conflict without resolution.

### 3.3 Transactions
- Single-key operations are atomic by construction (log append is atomic).
- Conditional single-key writes (`putIf`) are compare-and-set: the precondition is evaluated and the append performed under one exclusive borrow of the engine, so nothing can write to the branch in between. A precondition that could be raced is not a precondition.
- Multi-key transactions: supported via a bounded transaction log entry (all-or-nothing batch commit) — no distributed two-phase commit across branches; a transaction is scoped to one branch.

---

## 4. What ThetaBase Does Not Claim

- **Not a distributed, strictly serializable system across projects.** Within a project there are no shards to be ACID across: one project is one log, one process, one total order. Across projects there is deliberately nothing — `04-threat-model-security.md` §3 makes cross-project queries architecturally impossible rather than access-controlled, and that isolation is a security property the product sells rather than a consistency shortfall it apologises for.

  What *is* eventual is convergence **across branches**: a branch is a divergent timeline, and merging is deterministic for CRDT-typed fields and human-arbitrated otherwise. That is the design, not a weakness of it.
- Not suitable, in v1, for workloads requiring cross-branch distributed transactions (e.g., simultaneously debiting one branch's balance and crediting another's within one atomic operation).
- Time-travel/restore is bounded by log retention policy, not infinite — and the policy is now real rather than aspirational. `Retention::Forever` is the default and a supportable tier, because the archive compresses log-shaped segments about 53× with a verified restore; `Retention::For { ms }` expires history beyond a window.

  Expiry removes a **contiguous prefix and never a hole**. The log is a fold, so a missing segment in the middle produces a state that never existed and looks entirely normal (`docs/INVARIANTS.md` invariant 6) — so expiry stops at the first segment still inside the window even if later ones have aged out, refuses to empty the archive entirely, and `Manifest::forget` rejects anything that is not currently the oldest. `Manifest::horizon` reports the earliest restorable point, so a caller asking to restore below it is told before the restore runs rather than after it fails.

---

## 5. Validation Requirement

Every guarantee in Section 3 must have a corresponding property-based or model-checked test (e.g., via a Jepsen-style consistency test suite) before this document can be cited in customer-facing material — see Test & Validation Plan, Section on Consistency Verification.

---

## Merge queues, and what "clean" is checked against

Branch-per-agent works until fifty agents branch from one head. Forty-nine of
their merges then conflict, and each finds out at merge time — minutes or hours
after the change was made, by which point the agent has moved on and the context
that would let it fix the conflict is gone.

**An agent that will conflict is told at enqueue.** A merge that would not land
is refused there, with its conflicts and with the ticket it disagrees with,
rather than queued to fail later.

### Speculation is against the target as it *will* be

Checking against the target as it *is* is the wrong question: by the time a
merge reaches the front, the target is the target plus everything ahead of it.
A queue that validated against the old target would clear a merge and then land
it into a branch nobody checked it against.

Speculation therefore happens on the **log**, not on the materialised view.
Merging is three-way — each side's changes are derived from its history since
the base, and the view supplies CRDT state and types rather than deciding what
the target already changed. Overlaying a view has no effect on conflict
detection at all.

### A queued merge can stop being clean

Speculation is a prediction and predictions expire: something ahead may be
withdrawn, or the target may move underneath the queue. Re-checking can find a
merge that no longer lands, and that merge is **evicted with its conflicts**.

Landing something because it was clean when it was queued is the same failure
this section exists to prevent, arriving by a different route.

### Order is first-in, first-out

Letting a clean merge overtake a slower one sounds efficient and changes what
the slower one merges against, which is exactly the honesty speculation is for.
Tickets are never reused: a ticket is how an agent is told what it is behind, so
reissuing one after a withdrawal would point a second agent at a merge that is
not the one it was told about.

### What is not a conflict

Two agents adding **the same column with the same definition** have not
disagreed about anything. It is one change proposed twice, and landing it once
is correct rather than something a human should adjudicate. Two agents adding
one column name with *different* definitions is the disagreement, and it goes to
a human like every other (§3.2).

---

## Multi-region: regional branches and follower reads

ThetaBase is multi-region and is **not** multi-master, and the difference is
stated rather than blurred. See `DECISION-single-branch-multi-master.md` for
what the strict version would cost and why it is not built.

### Regional branches

Each region writes to its own branch and the branches merge explicitly. A caller
in one region writes locally with no cross-region round trip, which is the
property people want from multi-master.

It costs nothing conceptually new: the conflicts are branch-merge conflicts this
document already resolves — deterministic for CRDT fields, human-arbitrated
otherwise (§3.2).

**What it does to the guarantees, exactly.** `read-your-writes` holds within a
region, because a region is a branch and a branch has a total order. Across
regions you see your own writes immediately and another region's after a merge,
which is the branch model's existing behaviour rather than a new weakening.
Total order *within a branch* is untouched.

**An unplaced region is refused, never sent to the home branch.** A fallback
would route a write across an ocean silently, which is the exact latency the
arrangement exists to avoid, and the caller would have no way to tell. A region's
placement also cannot be silently replaced: doing so would move every subsequent
write to a different branch while the writes already on the old one stopped
being visible to callers still reading where they were told to.

**A region that has never merged is lagging, not current.** Treating "no merge
recorded" as zero lag would make a new region that is silently failing to merge
look like the healthiest one in the fleet.

### Follower reads with a version floor

A read replica that lags breaks §3.1 for a caller routed to it after writing.
There are two fixes and only one is any good: pinning a session to the primary
throws away the replica for exactly the callers who most need it, and it is a
session-level answer to a per-key problem.

So the caller names the version it last saw and the replica **refuses** below it.
A read that would be stale becomes a **redirect**, never a wrong answer.

**The floor is per key.** A global floor redirects every read on a replica that
is behind on any key, which is a replica nobody can use.

**Absent is not version zero.** A key the caller has seen and the replica has not
is the clearest case of a lagging replica; serving it as "not found" would look
like data loss to a caller who wrote it a moment ago. A read that finds nothing
reports *absent*, and an absent read sets no floor — otherwise a caller who read
a key that does not exist would record a floor of zero for it and be served by a
replica that still did not have it.

**A caller cannot lower its own floor.** A client that had seen version 9 asking
to be served version 4 is read-your-writes broken by the client's own request.

---

## Time as a query dimension

The log already holds every version of every row and the view is a fold over it,
so reading a branch as it was is the same fold stopped earlier. Not temporal-table
machinery bolted on, and not something a relational engine can do cheaply because
it threw the history away.

**The boundary that stays.** No distributed execution, no cost-based optimiser
for star schemas, no competing with a columnar engine on its own ground. What is
claimed is the temporal dimension, which is free here and expensive elsewhere.

### `AS OF` refuses below the retention horizon

§4 already bounds time travel by retention. A request below the horizon is
**refused, with the horizon** — never answered from the earliest state available.

Answering would be the worst option on offer: the caller asked for Tuesday, got
Thursday, and has no way to tell. Every conclusion they draw is about a different
day than the one they think.

### A timestamp query says which commit it got

`timestamp_ms` is advisory for ordering; clocks skew and the log's order is
authoritative where they disagree. So `AS OF` a timestamp resolves to the last
commit at or before it, and **the resolved commit is returned with the answer**.
A caller needing an exact point asks by commit; one asking by time is told what
it actually got rather than left to assume they are the same thing.

A timestamp before the branch's first entry is refused rather than answered with
an empty view. An empty view is a plausible-looking answer to a question with no
answer, and it is indistinguishable from a branch that really was empty then.

### A diff carries both sides

What changed between two points, as rows. A change carries the old value as well
as the new: reporting only the new one answers "what does it say now", which the
caller could already read, and a diff is for what it *stopped* saying.

A removal carries the value that is gone. Reporting a bare key name makes the
value unrecoverable from the diff, which is exactly when somebody needs it.

### Change data capture is the same fold, streamed

The log is already the shape a CDC pipeline wants and usually has to reconstruct
from a write-ahead log nobody documented.

**A consumer that was away longer than retention is told, never silently
resumed.** Resuming from wherever history now begins leaves a gap the consumer
cannot see, and every downstream aggregate built from that feed is quietly wrong.
Being told means it can re-seed from a snapshot instead.

Changes are folded from state rather than read off the operations. An operation
says what was written; a change says what the state did, and the two differ
whenever a write sets a key to the value it already held. A consumer that saw
that as a change would act on a no-op.
