# SLA & Performance Specification

ThetaBase v1

---

## 1. Principle

The hot path (reads, writes, typed queries) must never depend on an LLM call — that's what makes these targets achievable and is itself a testable claim (see Test & Validation Plan, Section 4).

---

## 2. Target Latencies (Pro tier, single-region)

| Operation | p50 | p99 | Notes |
|---|---|---|---|
| `get()` (point lookup) | 5ms | 25ms | Served from Scribe edge cache where possible |
| `put()` (write) | 8ms | 40ms | Includes log append + WAL durability |
| `query()` (precompiled/cached plan) | 15ms | 60ms | Bytecode plan reuse, no parse/plan step |
| `query()` (first-run, uncached plan) | 40ms | 150ms | Includes parse + plan generation, still zero LLM calls |
| Schema change (non-destructive) | 100ms | 400ms | Auto-applied path |
| Schema change (destructive, shadow-validated) | N/A — async | N/A — async | Explicitly not latency-bound; correctness over speed for this path by design |
| Branch create | 50ms | 200ms | **Only to ~120k realistic rows in the parent.** Not a pointer — see §2.1 |
| Merge (CRDT-only fields) | 100ms | 500ms | Scales with number of divergent operations, not data size |
| Provisioning (new project, first write) | 500ms | 1.5s | **Only up to ~100k rows in the project.** Recovery is ~1.3µs/row — see §2.3 |
| AI Query-Assist — cached suggestion | 200ms | 600ms | Met: measured ~0.6ms. Not part of any hot-path SLA — separate, opt-in service |
| AI Query-Assist — cold suggestion | **not met** | **not met** | Bounded by the model provider, not by ThetaBase. See below |

### 2.1 Branch create is constant time, and the copy is deferred

This row read *"copy-on-write pointer, not data copy"* until it was measured. It
was a data copy, at about 1.5µs per realistic row — a million-row branch took a
second and a half and missed the p99 by eight times.

It is now what the row always claimed. `MaterializedView` holds its rows,
versions, CRDT states and indexes behind `Arc`, so cloning a view for a fork
bumps four refcounts and copies nothing (M10.6). Measured by
`crates/theta-storage/tests/branch_cost.rs`:

```
fork, 200 bare rows              ~550 µs
fork, 20,000 bare rows           ~730 µs
fork, 20,000 five-field rows     ~660 µs   with two indexes declared
```

All three are one fsync. **Branch creation no longer scales with anything.**

**And there is no deferred copy either.** An intermediate version of this shared
the maps behind `Arc` and copied on first write, which moved ~3,800µs from the
fork to the first write on a branch. The rows now live in structurally-shared
persistent maps, so a write path-copies O(log n) nodes and shares the rest:

```
first write to a freshly forked branch    ~614 µs
second write                              ~594 µs
difference                                 ~20 µs   ≈ 0.001 µs/row
```

Both are one fsync. **Forking a branch and writing to it costs what writing
costs.**

**What it cost to get there, stated because it is real.** A persistent map's
lookup is about 1.9× a `BTreeMap`'s — 45ns against 85ns in the view, 59ns
against 110ns measured in isolation. Writes are ~2.5×. Both are absorbed
entirely by the operations they sit inside: `get` measures **104µs p50**
end-to-end against a 5ms target, so 40ns is 0.04% of the budget, and a `put` is
dominated by its fsync. The trade is 40 nanoseconds of lookup for the removal of
a copy that scaled with the whole dataset.

### 2.2 Reads do not degrade with branch depth

A branch is a view, not a chain to walk. Once the fork has paid for its copy,
reading at depth 500 costs what reading at depth 0 costs — measured at 1.10×,
which is host noise. Nothing about the branch topology reaches the read path:
no ancestor stack is consulted, no chain is walked, no copy-on-write layers are
resolved in order.

This is stated as a guarantee rather than an observation because it is a
structural property of the design, and because Columbia's BranchBench reports
5–4000× read degradation on this axis across PostgreSQL, DoltgreSQL, Neon,
TigerData and Xata. Asserted by
`a_read_does_not_get_slower_as_branches_deepen`, which fails if depth ever
becomes something a read has to account for.

Switching branches is a map lookup — about 15ns. There is no connection to
reopen and nothing to check out, so it is reported for completeness rather than
targeted.

---

### 2.3 Cold start is bounded by a project's size, not its history

This row was written before either half of a cold start had been measured, and
the number was a budget rather than an observation. Both halves are now measured
and the row is qualified above.

**Our half.** `Engine::open` costs about **1.3µs per row** — 13ms at ten
thousand rows, 131ms at a hundred thousand. It is linear in the size of the
materialised view, because opening a project means building an in-memory
`OrdMap` of every row it holds, and there is no way to have the map without
paying to construct it.

Two hypotheses were tested and both were wrong, which is why they are recorded
rather than the conclusion alone:

* **It is not log replay.** The checkpoint interval is 1,000 entries and `open`
  resumes from the view snapshot, which is what the snapshot is for.
* **It is not the snapshot's encoding.** The snapshot is `serde_json`, and the
  obvious fix was a binary format. Measured against MessagePack on the real
  7.1MB snapshot: **1.1× faster, 1.3× smaller.** That would have been a refactor
  across the storage layer for five per cent. `Value` is adjacently tagged
  (`tag = "kind", content = "value"`), which also rules out the non-self-describing
  formats that would have been faster still.

**The platform's half**, which the budget has to leave room for: resuming a
stopped Firecracker microVM is 200–500ms. An ECS `RunTask` is 10–90s, because a
stopped task does not exist and has to be re-provisioned and its image re-pulled
— which is why hibernation is only viable on a platform that can resume a
stopped VM, and why the deployment target changed.

**What follows, and it is the reason this is a qualification and not a defect.**
Recovery tracks a project's *current size*, not everything it has ever done: the
same ten thousand rows rewritten five times recovers no slower than written once
(measured at 0.67×, because the view is identical and the snapshot is the same
size). So a project does not get slower to wake every month, which is the
failure that would have made hibernation untenable.

Small projects wake fast, and small projects are the ones that hibernate —
development instances, where the cost model needs the saving. A large project is
a production instance, which never hibernates, so its recovery is paid once at
deploy rather than on somebody's first query of the day.

`crates/thetad/tests/cold_start.rs` holds the per-row rate, the budget for a
hibernating project, and the size-not-history property.

### A note on the Assist row

The 200ms/600ms figure was written before Assist existed and is not achievable
for a cold call. A model round trip costs hundreds of milliseconds before its
first token, and nothing on ThetaBase's side changes that. Rather than quietly
miss the number or quietly relax it, the row is split into the two things it
could have meant:

- **Assist's own work** is ~0.6ms p50 and ~1ms p99, measured against a scripted
  model so the number describes this service rather than a third party
  (`crates/theta-assist/tests/service.rs`, run by `make assist`). The budget is
  therefore spent essentially entirely on the model.
- **A cached suggestion** — same question, same schema — is served without
  reaching a model at all, and meets the budget with three orders of magnitude
  to spare.
- **A cold suggestion** is the model's latency plus that ~0.6ms. It will not be
  under 200ms, and no roadmap item proposes to make it so.

This is tolerable precisely because of what Assist is: optional, explicitly
invoked, and outside every hot-path SLA. A deployment that never starts it loses
suggestions and nothing else. If that were ever untrue — if anything on the
read/write path came to depend on a suggestion — this row would become a real
problem, and `no_llm_on_hot_path` exists to make that impossible rather than
unlikely.

---

## 3. Guardrail-Related Behavior (not latency, but part of the SLA conversation)

- Blast-radius circuit breaker trip: must surface a clear, immediate rejection (target: <50ms decision time) rather than allowing a runaway operation to continue accumulating cost while a slower check catches up.
- Shadow-branch validation for destructive changes is explicitly async and excluded from latency SLAs — correctness is prioritized over speed for this specific path, and that trade-off should be stated plainly to users rather than papered over with an aggressive target that pressures the Safety Layer to cut corners.

---

## 4. Availability

- Target uptime for Pro/Team tiers: define after initial production data is available (do not commit to a specific number pre-launch based on guesswork — this should be revisited once the Chaos & Recovery testing in the Validation Plan produces real numbers).
- Auto-pause (idle projects) and auto-scale (volume growth) behavior inherited conceptually from the original tiering model, re-specified once actual usage data exists.

---

## 5. Measurement & Enforcement

- All SLA metrics instrumented in CI performance benchmarks (Test & Validation Plan Section 4) before any target is published externally.
- Real-world production metrics tracked per-project and exposed via `status` RPC (see API/Wire Protocol Spec) — no SLA claim ships without a corresponding internal dashboard to verify it's actually being met in production, not just in benchmark conditions.

---

### 5.1 What a dashboard must be able to say

The requirement above is easy to satisfy in appearance and hard in substance. A
dashboard that cannot say what it measured is how an SLA claim gets made on a
figure nobody checked, so **a number and its provenance are the same value**:
what instrument produced it, over how many samples, across what window. There is
no representation of a bare figure, because a field that can be filled in later
is one that is not.

**A statistic with too little behind it is reported as absent, not as a number.**
A p99 computed from eleven samples is the largest of eleven samples — a maximum
wearing a percentile's clothes, and lower than the true p99 essentially always,
biased in the direction that makes this SLA look met. Below a sample floor the
answer is *unmeasured*, with the count and the floor stated.

**Absent is not zero.** A project reporting `0ms p99` because nothing was
collected looks like the best project in the fleet, and a project reporting `0`
breaker trips because its collector is broken looks like the healthiest. This
follows `06-provisioning-identity-flow.md` §5, which already distinguishes a
measured usage figure from an unswept one.

**A fleet figure is the worst project, not the mean, and it states its coverage.**
This SLA is a promise to each customer rather than to the average customer: a
fleet where one project sits at 400ms and ninety-nine sit at 4ms has a customer
whose SLA is being missed, and the mean says 8ms. Coverage matters for the same
reason — averaging over the projects that reported and calling it fleet health
hides a missing third, and the missing third is disproportionately the broken one.

**Percentiles are values that were actually observed.** Nearest-rank, not
interpolated: a figure that is the basis of an SLA should not be a latency nobody
experienced.

---

## 6. Explicit Caution

Do not publish external SLA guarantees (e.g., financial refund commitments) until at least one full production quarter of real telemetry exists. Committing to specific numbers before that data exists is exactly the kind of unvalidated claim this build process is designed to avoid.
