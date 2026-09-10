# ThetaBase — Build-Out to Production

The task list from where the repo is now to a production launch.

Two rules govern this document, both inherited from
[`specs/08-test-validation-plan.md`](specs/08-test-validation-plan.md):

1. **A milestone is not complete until its gate passes.** Not "mostly passes",
   not "passes except for a known issue". The gate is a build blocker.
2. **If a gate cannot be met on schedule, the schedule moves — not the gate.**

Gates marked `CI` run on every pull request and are wired up in
`.github/workflows/ci.yml`. Gates marked `manual` require a human or an external
party and are checked at the milestone boundary.

---

## Where the repo is now

**M0 is complete. M1–M4 are substantially complete** — see their sections for
what remains. What exists and is tested:

| Area | State |
|---|---|
| Content-addressed log entries, Merkle chaining | Implemented, tested |
| CRDT primitives (PN-Counter, LWW-Register, OR-Set, RGA sequence) | Implemented, 2000-case property suite |
| Strict value/type lattice (no silent coercion) | Implemented, tested |
| Versioned schema ops + materialized fold | Implemented, tested |
| Safety Layer classification, gates, circuit breaker, audit summaries | Implemented, adversarial corpus green |
| Durable segment log: CRC'd records, crash recovery, torn-write truncation | Implemented, property-tested at every crash offset |
| Checkpointing + materialized view snapshots | Implemented, tested |
| Branch merge: CRDT convergence, explicit conflicts | Implemented, property-tested |
| Log store (CAS append), branch registry, O(1) branching | In-memory and durable implementations, tested |
| Typed plan IR, plan cache, EXPLAIN with real estimates | Implemented, tested |
| Query executor, SQL-subset parser, typed builder | Implemented, tested |
| Statistics, cost estimation, optimizer passes | Implemented, tested |
| Engine wiring (propose → gate → apply, shadow branches, breaker) | Implemented, end-to-end tests |
| Org graph, context resolution, token lifecycle | Implemented, tested |
| Cap'n Proto wire schema, codegen, framed transport | Implemented, every variant round-trip tested |
| TCP server: handshake, dispatch, backpressure, connection limits | Implemented, tested end to end |
| Concurrent engine (actor), concurrent-writer safety | Implemented, tested |
| Scribe edge runtime: pooling, read cache, batching, token refresh | Implemented, tested against a live server |
| Arrow IPC result-set encoding | Implemented, tested |
| Per-project token signing, verification, revocation | Implemented, isolation proven pairwise |
| Control plane: resolve, provision, mint, revoke | Implemented, tested |
| `theta` CLI: login, use, contexts, exec, token | Implemented, verified end to end |
| OAuth login (Google, GitHub) with PKCE, loopback redirect | Implemented, tested against a mock provider |
| Verified identity + org auto-discovery in the control plane | Implemented, tested |
| Credential storage in the OS keychain, with an honest fallback | Implemented, tested against an in-memory store |
| Keyset delivery at provisioning time | Implemented, tested |
| Billing: plans, proration, subscriptions, invoices, dashboard | Implemented, tested |
| SDK surfaces (TS + Python) | Types and signatures only, typechecked |

What is deliberately **not** implemented, each marked `STATUS:` in its module
with the milestone that delivers it: the Safety Layer's operator surface
(`theta status`, `audit`, `branch`, `schema`), the generated SDK bodies, storage
and write-volume metering, and the `eject` migration flow.

Run everything: `make check && make gates`

---

## M1 — Durable log and branching engine

The in-memory store becomes a real one. Everything downstream depends on this
being correct, so it comes first and gets the heaviest gate.

- [x] Segment file format: length-prefixed records, per-record CRC32, segment
      header with format version.
- [x] WAL implementing the durability contract: an append that returned `Ok` is
      fsynced before it returns, under both sync policies.
- [x] Torn-write detection: a partial trailing record is truncated on recovery,
      never interpreted, and the truncation is reported rather than silently
      repaired.
- [x] Checkpointing (temp-file-then-rename, so a crash leaves the previous
      checkpoint intact), and segment release once checkpointed.
- [x] Crash recovery: replay from last checkpoint, idempotent under repeated
      replay. Recovery is mandatory — `open` returns a `RecoveringWal` whose only
      operation is `recover`, so no caller can append to an unreplayed log.
- [x] Materialized view persistence + incremental catch-up. A missing or
      inconsistent snapshot falls back to a full replay: the snapshot is a cache
      over the log, never a second source of truth.
- [x] Branch merge: CRDT-typed fields converge automatically, everything else
      that diverged produces an explicit `ConflictRef`.
- [x] Merge never auto-resolves a non-CRDT conflict — enforced by the return
      type, which carries no applicable operations in its conflicted variant.
- [x] Multi-key transactions: an all-or-nothing batch is one log record, so a
      crash loses all of it or none of it.
- [x] Fault-injection harness: crash at any byte offset, with the invariant that
      recovery always yields a *prefix* of what was written.
- [x] Read-your-writes property tests for the writing session, before and across
      a restart.
- [x] Convergence suite extended to run against the durable store — log
      encoding, materialized fold, merge, and restart, not just the CRDT types.

**Still open before the gate is fully met:**

- [x] Concurrent writers — closed in M2, once there was a server to drive them
      through. Sixteen concurrent connections lose no writes, and concurrent
      writes to one key serialize to one of the written values.
- [x] Network partitions, live as `cargo test -p thetad --test
      network_partitions`. The faults are injected *in front of* the server
      rather than inside it: a partition is a property of the network, and
      modelling it in the transport would put test-only code on a hot path
      whose dependency closure is itself asserted. The proxy distinguishes the
      three cases that behave differently — a blackhole that holds bytes
      without closing (silence, which a client can only time out on, unlike a
      close it can react to), a sever, and an asymmetric drop where the request
      lands and the acknowledgement never returns. That last one is what
      decides whether an unacknowledged write was applied, and it is the case
      most likely to be got wrong.
- [x] Cross-branch schema merge. Changes to different declarations travel;
      two answers to one declaration go to a human. Two cases that a
      declaration-keyed comparison cannot see are caught explicitly, because
      both merged cleanly and should not have: a whole-table change against a
      column-scoped one on that table (dropping `users` while the other branch
      adds `users.nickname` — merging both applies a change to a table the
      other side removed), and a rename racing the other branch for the name it
      produces (`declaration_of` keys a rename on the name it consumes, which
      leaves the name it creates invisible). Identical migrations on both
      branches remain a duplicate rather than a divergence.
- [ ] Segment release to cold archive is enumerated (`archivable_segments`) but
      nothing ships them anywhere yet.

      **The pieces exist on both sides and nothing joins them.**
      `only_segments_fully_behind_the_checkpoint_are_archivable` proves the
      engine can say which segments are safe to release, and `theta-archive`'s
      custodian can upload, verify and expire them against AT-1 or S3. What is
      missing is the thing that calls one with the other on a schedule.

      **Why it is not just a loop.** Releasing a segment is the one operation
      that makes data unreadable locally, so the ordering has to be
      archive-then-verify-then-release, and a crash between any two steps must
      leave the log readable. `Manifest::forget` already refuses anything that
      is not the oldest segment, and `horizon()` already reports the earliest
      restorable point — so the wiring is: upload, verify the SHA-256 the
      manifest recorded, *then* call `release_segment`, and never in another
      order.

      Delivered by M10 alongside the custodian schedule, which is where the
      clock to hang it on already lives.

**Gate (`CI` + `manual`)** — `specs/08` §2. Jepsen-style suite: thousands of
randomized interleaving scenarios, 100% pass. Read-your-writes never violated
for the writing session; CRDT fields converge regardless of merge order;
non-CRDT conflicts always surfaced. **Met**, and live as `make consistency` and
`make wire`. The partition half runs 12 independent storms concurrently rather
than in sequence — the storm is bound by waiting for acknowledgements a
partition is swallowing, not by the CPU, so concurrency buys the interleaving
count the gate asks for at a wall clock a gate run on every change can afford.
A representative run verifies over a thousand acknowledged writes across ~1,400
randomized rounds, roughly a quarter of which are partitioned hard enough to
lose the acknowledgement. Seeds are derived from one constant so any failure
replays.

Only acknowledged writes are asserted to survive. A write whose acknowledgement
a partition swallowed may or may not have landed, and the suite deliberately
claims nothing about it — claiming more would be asserting a promise the
protocol does not make.

**Blocks:** everything.

## M2 — Wire protocol and RPC surface

- [x] Cap'n Proto codegen. Bindings are generated in-process at build time
      (`capnpc-embedded` parses, `capnpc` emits), so building ThetaBase needs no
      Cap'n Proto toolchain installed; CI still validates the schema against the
      reference compiler.
- [x] Framed transport: `[u32 length][message]`, refusing an oversized length
      prefix before allocating.
- [x] Every RPC implemented against the engine, except `query` execution, which
      needs the M3 executor and reports that rather than failing opaquely.
- [x] Protocol version negotiation on connect; both too-old and too-new clients
      are refused with a reason rather than downgraded.
- [x] Arrow IPC encoding for result sets, including the empty case, which still
      carries its schema so a client can learn a query's shape from a result
      that matched nothing.
- [x] Scribe edge runtime: connection pooling, local read cache with
      write-invalidation, request batching, token refresh.
- [x] `status` reporting real breaker state, write volume, branch, and commit
      count.
- [x] Backpressure and connection limits: a connection past the limit is refused
      at accept rather than accepted and starved; work past the queue depth gets
      a `Busy` response.
- [x] The engine runs on the durable log from M1, as an actor with one owning
      task, which is what makes concurrent connections safe.

**Resolved along the way:** spec §2 sketched the surface as a capnp `interface`,
which cannot coexist with the framing spec §1 fixes. The framing won and
`docs/specs/02` §2 records the resolution and its reasoning.

**Still open:**

- [x] Token signing — landed with the Control Plane in M4, as this entry
      anticipated. `session::Authorizer::authorize` verifies the signature
      against the instance's public keyset *before* reading anything else out
      of the token, because a token that does not verify has told us nothing;
      `crates/thetad/tests/server.rs` pins that ordering rather than trusting
      it. The instance holds public keys only, so it can verify and never mint.
- [ ] Zero-copy on the read side. Results are encoded as Arrow IPC, but the
      server still copies them into the response frame; making that a true
      zero-copy hand-off is worth doing once the executor produces real volume.
- [x] Network partition testing, the other half of the M1 gate. The
      fault-injecting transport lives in `crates/thetad/tests/network_partitions.rs`
      and runs under `make consistency`, where the rest of that gate already is.

**Gate (`CI`)** — Round-trip conformance for every RPC, plus a negotiation
matrix asserting refusal rather than a guess. **Met**, and live as `make wire`:
every request and response variant round-trips, the negotiation matrix holds
across a version range, garbage decodes to an error rather than a
partially-defaulted message, and the RPC surface plus the edge runtime are
tested end to end over real sockets.

**Depends on:** M1.

## M3 — Query planner and SQL subset

- [x] Typed query builder as the canonical construction API. A value passed in
      is a `Value` and reaches the executor as a literal or a bound parameter, so
      an injection cannot be constructed even deliberately — there is no text to
      inject into.
- [x] SQL-subset parser producing typed plan nodes only. Unsupported clauses are
      refused rather than dropped, and writes do not parse at all: the subset has
      no statement form for one.
- [x] Query executor: scan, point lookup, filter, project, sort, limit,
      aggregate, with null and missing-column semantics matching Postgres.
- [x] Table statistics and cardinality estimation, recomputed exactly before
      each plan.
- [x] Index selection and predicate pushdown. Every pass is written to be
      obviously meaning-preserving.
- [x] Plan cache keyed by a hand-written canonical hash, so the key is stable
      across releases rather than dependent on a derive's field order.
- [x] Real `estimatedRows` / `estimatedCostMs` in EXPLAIN, plus
      `estimatesFromStatistics` so a reader can tell a measurement from a
      default.
- [x] Degraded mode: a plan the optimizer cannot improve comes back unchanged —
      correct but unoptimized, never a fallback to unchecked raw execution.
- [x] Latency gate over a real socket (`make sla`), asserting the SLA targets
      with CI slack and reporting the headroom on every run.

**Measured on CI hardware** (p50, against the published Pro-tier targets):
`get` 17µs / 5ms · `put` 412µs / 8ms (fsync included) · cached query 4.0ms /
15ms · uncached query 4.0ms / 40ms · branch create 7.4ms / 50ms.

**Still open:**

- [ ] Bytecode plan compilation. Plans are interpreted from the IR today. The
      cache is keyed and hit; what it stores is a tree rather than compiled
      bytecode, which the latency numbers do not currently justify changing.
- [x] Real index structures. `IndexScan` now asks the source for candidates
      and only scans when nothing can answer. The index is deliberately allowed
      to over-approximate and never to under-approximate: it returns candidates
      and the filter still runs, which is what lets the value encoding be lossy
      where being exact would be delicate — `Int` and `Float` share a key so
      `x = 5` finds a stored `5.0`, and a large `i64` losing precision as `f64`
      costs a row the filter discards rather than a wrong answer. `Null`, `Map`
      and `List` are unindexable, because the executor's comparison returns
      nothing for them. `AND` may use any one conjunct; `OR` is served only when
      every branch can be, since serving some would return a subset and lose
      rows silently. Indexes are not snapshotted — they are a fold over rows and
      schema, and invariant 6 admits no second copy — so they are rebuilt on
      load and when one is declared over a table that already has rows.
- [x] Cost-model calibration, live as part of `make sla`. Two claims are
      checked: that the estimator orders plans the way the clock does — which is
      what the optimizer consumes, and getting it backwards picks the worse plan
      every time — and that each per-row constant is within an order of
      magnitude of measurement. Measured by slope rather than by differencing
      whole-plan timings, which cancels every cost that does not scale with the
      variable under test.

      It found one. `PER_ROW_SCAN_US` was 0.05 and measures ~0.96: twenty times
      low, and in the direction that matters, since it had a scan costing 2.5x a
      predicate evaluation when it really costs about thirty times as much. The
      gap is where the cost is — a scan clones a row map per row, a predicate
      reads one field. The band is wide on purpose: the failure worth catching
      is a constant wrong by two orders of magnitude, not one that drifts with
      the machine.
- [x] Streaming operators. `LIMIT` now pushes a row budget down the plan, and
      the estimator claims the saving because the engine makes it: `LIMIT 10`
      over ten thousand rows reads ten. The budget travels only through
      operators that cannot change which rows come out on top — a `Sort` or an
      `Aggregate` has to see everything before it knows its own first row, so
      both refuse it, and `COUNT(*)` under a `LIMIT` still counts the table. A
      `Filter` stops once it has enough but hands its input no budget, because
      how far it must read to find that many is exactly what the predicate
      decides.
- [ ] Join ordering — deferred deliberately, and this records when it stops
      being deferrable. There are no joins to order yet: the SQL subset in
      `specs/03` has no `JOIN`, and the executor has nothing to reorder.

      **The trigger is the subset growing, not the optimiser being weak.** When
      `JOIN` lands, ordering becomes load-bearing immediately — a two-table join
      picked backwards is the difference between an index seek and a cross
      product, and the cost model in `theta-query` already has the statistics it
      would need (`analyze` populates row counts and index selectivity).

      Until then this line exists so that whoever adds `JOIN` finds a note
      saying "and now do this too" rather than discovering it in a latency
      graph.

**Gate (`CI`)** — `specs/08` §4. p50/p99 targets met with zero LLM calls on the
typed path. **Met**, and live as `make query` and `make sla`. The structural
half of the no-LLM assertion (dependency closure) and the behavioural half
(EXPLAIN reporting zero calls for the plan that runs) are both asserted. The
CI gate is deliberately looser than the published SLA: a shared runner is not
Pro-tier hardware, and a test that fails on a noisy neighbour teaches people to
re-run CI until it passes. The published numbers remain a claim about
production, verified against production telemetry per `specs/09` §5.

**Depends on:** M2.

## M4 — Identity, provisioning, and the control plane

- [x] Token signing with per-project Ed25519 keys. A token minted for project A
      is cryptographically incapable of verifying against project B — forging
      the `project_id` does not help, because the signature is checked with the
      verifier's key. An instance holds public keys only, so it can verify and
      never mint.
- [x] `resolveContext` endpoint: resolve, provision, mint, return, in one round
      trip. An ambiguous hint asks one question; an unknown project asks one
      confirmation; an inaccessible one is indistinguishable from a missing one.
- [x] Revocation by signed push, propagating well inside the 5s budget. Four
      granularities — token, session, user, org — so revoking a membership
      withdraws every token for every project of that org without enumerating
      them, including projects created afterwards.
- [x] Provisioning orchestration with instance placement, idempotent so a
      retried request cannot double-provision, and preserving an instance's
      address across a pause.
- [x] Token injection into a child process's environment via `theta exec` —
      never a file the user manages, never this process's stdout. `theta token
      print` remains as an explicit, logged escape hatch.
- [x] `theta login`, `use`, `contexts`, `exec`, `token` command bodies, plus
      credential storage that refuses a world-readable file rather than using it.

- [x] **OAuth**, Google and GitHub, Authorization Code + PKCE (S256 only) over a
      loopback redirect (RFC 8252, RFC 7636). The whole flow is exercised
      against a mock provider whose SHA-256 is implemented independently of the
      one under test, so a bug cannot hide behind itself.
- [x] Verified identity in the Control Plane. `/v1/login` exchanges a provider
      access token for a signed identity token — the Control Plane calls the
      provider itself, so the identity is asserted to this service rather than
      claimed by the client. `/v1/resolve` requires that token and reads the
      user id from it; there is no field left for naming a user.
- [x] Org auto-discovery from Workspace domains and GitHub org memberships. Only
      provider-vouched orgs reach the graph, and a non-member gets the same
      "not found or no access" as a caller naming an org that does not exist.
- [x] **OS keychain**, probed at runtime rather than assumed from the target
      platform. The backend actually used is what gets reported, and a downgrade
      to a mode-0600 file carries its reason.
- [x] Public keysets delivered at provisioning time rather than fetched by the
      instance, which resolves the bootstrap circularity: authenticating a
      keyset fetch would need a keyset. A rotated key reaches a paused instance
      when it next resumes.
- [x] Billing surface and the minimal dashboard — plan and usage, upgrade,
      downgrade, cancel, invoices, and nothing else `06-provisioning-identity-flow.md`
      §5 does not allow. Money is integer cents throughout; the payment provider
      commits before the local subscription changes; a downgrade takes effect at
      the period boundary the customer already paid for.

- [x] Storage and write-volume metering. Each instance measures the bytes its
      log occupies and counts the rows it has written; the courier collects both
      and the dashboard reports measured figures. A project no sweep has reached
      still says "not collected" rather than rendering as zero, because those are
      different facts. Full per-instance telemetry and dashboards remain M10.
- [x] The instance courier. Revocation lists, safety policies, and usage were
      three mechanisms without a driver — the RPCs existed and the tests
      exercised them, and nothing in a running Control Plane ever called them. A
      revocation that reaches no instance is not a revocation. The Control Plane
      dials outward over the same protocol everything else uses, because an
      instance that polled would need an HTTP client and `thetad` is held free of
      one.
- [x] Roles model for multi-user orgs (`specs/06` §7). Owner, admin, member,
      with the capability §7 names explicitly — who may create projects versus
      who may only work in the ones that exist. Roles live in the org graph
      rather than the identity token, so a demotion takes effect on the next
      request instead of at the token's expiry. The first person into an org owns
      it, which is what keeps solo v1 working with nothing to configure.
- [x] `/v1/revoke` requires a credential. It previously took none: revoking is
      the safe direction for a *credential*, but anyone who could reach the
      endpoint could revoke an entire org's tokens and take it offline.

**Gate (`CI` + `manual`)** — **Met**, live as `make identity`. Cross-project
token rejection is proven against real signed tokens, and revocation propagation
is measured over a real connection rather than assumed. The independent
penetration test of the identity flow (`specs/04` §7) remains M11.

**Depends on:** M2. Parallelizable with M3.

## M5 — Safety Layer productionization

The classifier and breaker are implemented. This milestone makes them
operable and grows the corpus to gate strength.

- [x] Real row-impact estimation from the storage engine, replacing
      caller-supplied `Impact`.
- [x] Shadow-branch validation flow end to end: apply to shadow, run the
      project's verification (test suite or sampled query comparison), surface
      results, promote by merge — never blind re-execution.
- [x] Ephemeral shadow branch lifecycle and garbage collection.
- [x] Persistent audit store, per-project, with no cross-project data path.
- [x] `theta audit`, `theta schema propose/show/confirm/promote` command bodies.
- [x] Project policy configuration surface — writable by a project owner, never
      by the agent whose changes are being gated.
- [x] Breaker calibration under realistic runaway-agent load; false positives
      here directly cost the product its speed, so this needs measurement, not a
      guessed default.
- [x] Grow the adversarial corpus. The three categories that were missing are
      covered: prompt-injected identifiers in
      `crates/theta-safety/tests/adversarial_corpus.rs`, and sustained agent
      loops plus benign-in-isolation migration sequences in
      `crates/thetad/tests/adversarial_sequences.rs` — both need real rows and a
      real branch to mean anything.

**Gate (`CI`)** — `specs/08` §3. Zero unreviewed destructive changes reach a
protected branch across the full corpus; every entry has a recorded expected
outcome; the suite re-runs on every Safety Layer change. **Met**, and live as
`make adversarial`: the classifier corpus, the gate and shadow-validation flow,
policy authority, the engine-level sequence corpus, and the breaker calibration
that pins both shipped ceilings from above and below.

Five bypasses were found and closed building this milestone, all the same shape
— a value the caller controlled deciding what a gate applied to. The impact
estimate arrived in the proposal; opening a shadow branch was itself sufficient
to clear the strongest gate; a confirmation could carry a different change body
than the one classified, or name a different branch. The fifth was in the other
direction: a drop removed only the schema declaration and left every row
readable, so the gates were guarding an operation that destroyed nothing. The
corpus only ever grows, and entries are never deleted to make it pass.

**Depends on:** M1, M2.

---

## M6 — Generated SDKs

- [x] Code generation from `theta.capnp` for JS/TS and Python. Written in-tree
      (`crates/theta-codegen`) rather than bought: Stainless generates clients
      from OpenAPI, and this protocol is Cap'n Proto. The generator reads the IR
      the *same* embedded compiler produces for the Rust bindings, so there is
      one compilation and one reading of the schema — a second parser would be
      the drift it exists to prevent.
- [x] The wire types are generated; the hand-written surfaces re-export them
      instead of restating them. They had already drifted: `ChangeDiff` had no
      `gate`, `ConflictRef` carried fields the wire does not send, and
      `MergeResult` was missing a status.
- [x] Typed query builder in both languages. Fluent in each — that is what
      makes a query pleasant to write — but what a builder produces is an AST,
      and the AST is rendered to SQL-subset source and bound parameters by the
      WASM core. One renderer, so the same query built in TypeScript and Python
      reaches the server as the same bytes, and the rule that a value never
      becomes query text has one implementation rather than three.
- [x] Transport. Scribe's protocol core compiles to WebAssembly
      (`crates/theta-scribe-wasm`, ~380KB), as `specs/01` §1 specifies, and each
      SDK supplies only sockets. WebAssembly has no networking and the host
      runtimes disagree about what a socket is, so that is the seam: what must be
      identical across languages is the protocol and the cache invariants, what
      must differ is I/O.
- [x] Generation in CI, with drift detection — `make sdk-check`, in `make
      gates`. A schema change that was not regenerated fails the build.

**Gate (`CI`)** — **Met**, live as `make sdk-check` and `make conformance`.
Both SDKs run one shared case file against a live `thetad` and their output is
compared: two bindings that each pass their own suite prove nothing about
whether they agree, so the gate is that they produce the same document from the
same cases. It has already earned its keep — it caught the Python and TypeScript
hosts serialising the same request with different whitespace on its first run.

**Still open:** nothing in this milestone. The remaining languages (Go, Rust,
Java/Kotlin, Swift, C#, Ruby) have moved to **M11.6**, which is a launch
item rather than a post-launch one.

`specs/02` §3 conditions them on the first two SDKs being validated "in real
use", which cannot mean waiting for users in a language that has no SDK — that
condition can never fire on its own. What M6 had to prove is the shape, and it
did: one generator, one AST, one renderer, and a check that fails on drift.
Expanding the set is then a question of launch coverage rather than of
readiness, so it lives next to the other launch work.

**Depends on:** M2, M3.

---

## M7 — Branch-per-PR GitHub integration

- [x] Webhook receiver mapping pull-request events to branch actions. The
      signature is the whole security model here: this endpoint creates and
      destroys branches on a customer's database and is reachable from the
      internet by construction, so verification runs over the raw body before
      anything parses it, the comparison is constant-time, and an instance with
      no secret configured refuses every delivery rather than accepting unsigned
      ones. GitHub's retries are deduplicated by delivery id.
- [x] Environment auto-detection from git branch context. Deliberately
      asymmetric: `prod` is only ever chosen from an explicit signal — the
      default branch git reports, or an operator saying so — and everything
      else, including "no idea", lands on a non-production environment. A
      preview run that should have been production is an inconvenience;
      production written by a pull request is an incident. Every resolution
      reports why, because one a user cannot explain is one they cannot correct.
- [x] PR comment surfacing the Safety Layer's diff. Answers the three questions
      a reviewer actually has — what will this destroy, can it be undone, what
      has to happen before it lands — because a diff showing `DROP COLUMN email`
      cannot say whether that is 12 rows or 14,000. Identifiers are escaped,
      bounded and rendered inside widened code spans: a PR comment is Markdown,
      which gives an injected identifier more to work with than the audit
      trail's plain text does.
- [x] Branch discarded on PR close; merged on PR merge, into the base it was cut
      from, through the same gates as any other merge. The preview branch is
      named from the PR number rather than its head ref, because a head ref is
      attacker-controlled in a fork PR and can name anything git allows.

**Gate (`CI`)** — **Met**, live as `cargo test -p theta-control --test
github_webhook`: the full lifecycle — open, push, merge, close — driven through
the real router with real signatures, plus the perimeter cases that matter
(unsigned, wrong secret, body altered after signing, no secret configured,
redelivery).

The decision and the doing are separate on purpose — deciding is pure and
testable without a server, doing needs a socket — and both halves now exist.
`preview_lifecycle.rs` runs the actions against a real `thetad`: a preview cut
from the base that a PR's code can write to, a merge that lands those writes and
only then reclaims the branch, and a conflicting merge that goes to a human with
the branch left standing. `github_rest.rs` runs the REST client against a local
HTTP server, which is the only thing that can check the URLs, headers and JSON
shapes a fake cannot.

A repository is bound to a project explicitly, through
`PUT /v1/projects/<project>/github`, and the capability required is
`CreateProjects` — because that is effectively what the binding grants. Inferring
it from names would mean a stranger's pull request creating branches on
somebody's database.

- [x] GitHub App authentication (`github_app.rs`). A personal token carries one
      *person's* permissions across everything they can reach, dies when they
      leave, and would make a customer hand over access to their whole account.
      An App is installed on the repositories a customer chooses, its permissions
      are declared and visible, and the credential ThetaBase holds is an
      installation token — one customer, one hour, minted on demand from the
      private key and never stored.

      The App needs only `pull_requests: write` and `metadata: read`. No
      `contents` permission at all: branches are created in ThetaBase, not in git,
      so write access to source code would be asking for something the feature
      does not use.

      `docs/github-app-setup.md` covers creating it, including a manifest that
      turns the twenty-field form into one approval and is the only way GitHub
      hands back the private key programmatically.

**Still open:** the App has to be registered by a human — GitHub has no API that
creates one without a browser session and consent, which is correct for
something that can be granted write access to repositories. The setup guide is
what closes that gap.

**Depends on:** M4, M5.

---

## M8 — `eject`: migration from Postgres/Supabase

- [x] Postgres schema reflection (`crates/theta-eject/src/reflect.rs`). Read
      from `information_schema` and `pg_catalog` rather than from a dump,
      because what matters is what the database currently believes and a dump
      is a statement about a moment. Key *order* comes from `pg_index.indkey`,
      since a composite key read out of order silently re-addresses every row.
      Views are skipped: migrating a derivation as though it were data produces
      two copies of one truth, free to disagree. Nothing writes to the source.
- [x] Type mapping to the lattice, with CRDT suggestions where concurrency
      matters. Two rules: nothing is silently narrowed, and nothing is guessed.
      `numeric` is arbitrary-precision and `Float` is not, so it maps and is
      flagged every time — that is the classic silent money bug. A `timestamp`
      with no zone is read as UTC, which is a choice rather than a conversion.
      A type with no honest home keeps its Postgres text rendering rather than
      being coerced into a shape that merely parses. CRDT kinds are *suggested
      and never applied*: choosing one changes how concurrent writes resolve,
      which is the application's decision, not a migration tool's.
- [x] Streaming, resumable import. Keyset pagination, so peak memory is a
      batch rather than a table, and so a resume does not re-walk everything
      before it the way `OFFSET` would. The cursor is committed *after* the
      batch it describes; the other order loses rows on a crash between the
      two. That makes delivery at-least-once, which is safe only because a row
      is addressed by its primary key and rewriting the same key with the same
      value is one row — the test interrupts a migration, replays the boundary
      batch deliberately, and asserts the count is unchanged.
- [x] Schema-semantics verification. Findings are `Expected` or `Unexpected`,
      and the distinction is the whole design: a migration is *allowed* to
      change meaning and is not allowed to change it quietly, so a predicted
      loss is reported without failing and an unpredicted one fails. Row counts,
      value-by-value comparison against a **second read of the source** — a
      comparison against the importer's own input would only prove it agrees
      with itself — plus the constraints that stop being enforced: UNIQUE,
      length bounds, defaults.
- [x] `theta eject` command body, including `--run`. Dry run by default,
      because the interesting failures of a migration are decisions and a
      decision is cheap to change before the run and expensive after it. Only
      `--run` needs a logged-in context: reading somebody's Postgres schema is
      not something ThetaBase should demand a credential for. Blockers are listed
      individually, not counted — each needs a different answer, and "3
      blockers" tells nobody which three — and `--exclude` exists because the
      blocker for a keyless table told the operator to exclude it while there
      was no way to.

      The write path proposes every schema change rather than applying it, so
      the Safety Layer sees them. An `eject` that installed its own schema
      behind the gate would be the one caller allowed to skip the review
      everything else submits to, and a migration is exactly when a destructive
      change is most likely to be accidental. A gated change stops the run
      before any row is written.

      The ordering lives in `theta_eject::migrate` rather than in the CLI, so
      it can be tested: schema before rows, the cursor after the batch it
      describes, verification against a re-read of the source. The CLI's target
      is a `thetad` over the wire and a test's is an in-process view, and both
      drive the same code — an ordering that only held on one path would be a
      bug on the path nobody tested.

      Run end to end against a provisioned instance: Control Plane resolves and
      provisions, `thetad` serves the placed address, and `theta eject --run`
      migrates a real Postgres into it and reports no unexpected mismatches.
- [x] **Comparative benchmark against the source Postgres.** Deliberately here
      rather than in M3, where the SLA gate lives, because this is the first
      point at which a fair comparison exists: the same data, the same queries,
      one migrated from the other. A synthetic head-to-head before that would be
      measuring two different workloads and calling the difference a result.

      What to publish and what not to. `get`/`put`/cached `query` latency
      against the Postgres a user actually left is the number they care about —
      "is this slower than what I had" — and it is fair. Raw analytical
      throughput against a mature planner is not a comparison ThetaBase wins or
      should claim; `specs/09` states absolute targets for good reason. Branch
      create and merge have no Postgres equivalent at all, so they are reported
      as capabilities rather than as a race.

      No comparative number ships without the harness that produced it, the
      hardware it ran on, and the version of both systems — so the harness
      prints all three above the numbers, and refuses to be read as a
      performance claim: it asserts that both systems answered the same
      question with the same answer, and asserts nothing about which was
      faster. A latency assertion here would pin a claim to whichever machine
      happened to run it.

**Gate (`CI`)** — `specs/08` §6. **Met**, and live as `make eject-live`
against a real Postgres, with `make eject` covering the half that needs no
database. A real project is migrated end to end with zero data-meaning
mismatches undetected by the verification pass, and the comparative benchmark
runs on that same migration, so the numbers describe one real workload rather
than a benchmark written to be won.

The sign-off is a suite rather than a person, and the suite is adversarial
because the gate's wording demands it: "zero *undetected* mismatches" is a
claim about the detector, and a migration that comes back clean would satisfy a
detector that returned "clean" unconditionally. So six tests damage the migrated
data one way at a time — a row removed, a row invented, a value altered, a
timestamp rounded to the second, a column dropped — and each asserts the pass
finds it. A seventh plants a drift in a column the plan already called lossy
and asserts it is reported *without* failing, since a migration is allowed to
change meaning and only forbidden to do it quietly.

The source schema is built from the cases that actually go wrong rather than
the ones that are easy to write: a composite primary key, a table with no key
at all, `numeric`, arrays holding a comma and the literal text `NULL`, `jsonb`,
`bytea`, and timestamps before the epoch and past 2038.

**Depends on:** M3.

---

## M8.5 — Production hardening

Everything in `SECURITY-REVIEW.md`, plus the Control Plane durability work that
was sitting in M12 and should never have been that late.

Placed here rather than folded into M11 because M11 is the *external* review,
and sending someone a system whose Control Plane forgets its tenants on restart
wastes the budget you are paying them for. These are also the items that block
production testing, which is a different and earlier bar than GA.

- [x] **SEC-1 — persist the Control Plane's state.** Org graph, memberships,
      token records, revocation list, and the per-project signing keys are all
      in process memory. Revocation is the documented containment for a leaked
      token (`specs/04` §2) and does not survive a restart; every deploy is
      also a total outage, because regenerated keys invalidate every session
      and lost memberships make `/v1/resolve` answer `not_found` for everyone.

      The two failures interact badly: key regeneration *happens* to make a
      leaked token fail closed, which is luck rather than design, and it
      disappears the moment someone fixes key persistence without also fixing
      revocation persistence.

      Not on ThetaBase. The Dogfooding section below is right that anything
      needed to bring ThetaBase back up cannot live in ThetaBase, and this is the
      clearest case: an outage that takes the org graph with it takes away the
      console needed to fix the outage. Signing keys want a KMS or a sealed
      secret rather than a row.

      *Was M12's "Persist the Control Plane's own state". Moved because it
      blocks production testing rather than GA.*

      **Landed for the security half**, live as `make durability` against a
      real Postgres: signing keys, the revocation list and the issued-token
      records now survive a restart, and six tests assert what the gate asks —
      a token revoked before a restart is still revoked after one, a token
      minted before a restart still verifies after one, and an older revocation
      list cannot overwrite a newer one.

      Key material is wrapped before it reaches storage (`secrets.rs`), so a
      database compromise is not a forgery compromise; a test asserts the
      private half does not appear in the bytes Postgres hands back. The
      counter that names token ids is derived from the highest issued id rather
      than persisted separately, because a counter stored as its own row can
      disagree with the tokens it named — and two tokens sharing an id means
      revoking one revokes the other.

      **Both halves landed**, live as `make durability`. The availability half
      — org graph, memberships, provisioned instances, policies and repo
      bindings — is persisted too, and `ControlPlane::restore_from` rebuilds a
      plane from the store before the listener binds. A plane that served a
      request before rehydrating would answer it from an empty graph, which
      reads to the caller as "your project does not exist".

      Writes go through the store at the point of the decision, not after it: a
      revocation is persisted *before* the 200, because the other order tells
      an operator a credential is withdrawn and then forgets it — the exact
      failure, with an operator who has no reason to check.

      Running without a database is still allowed and no longer quiet: it warns
      on loopback and is **refused** anywhere else, on the same reasoning as
      `--dev-seed`.

      One bug worth recording, because it is the kind only a real process
      shows: `restore_from` originally borrowed the key wrapper to unwrap with
      and left the plane holding its default *ephemeral* one, so it read the
      stored keys correctly and then wrapped every subsequent write with a key
      that died with the process. The in-process test did its own wrapping and
      could not see it; restarting an actual binary failed on the second start.

      **KMS landed**, live as `make kms`. AWS chosen on one criterion that
      outweighs the rest: BYOK. When an enterprise asks for customer-managed
      keys the request is almost always a KMS key ARN, and AWS has the largest
      BYOC footprint, so the tier that runs instances in a customer's account
      most often lands there. Behind a `kms` cargo feature, so a self-hosted or
      air-gapped build does not carry an AWS SDK it will never call, and behind
      the existing `KeyWrapper` seam, so a second provider is an implementation
      rather than a refactor.

      `Encrypt`/`Decrypt` directly rather than `GenerateDataKey`: envelope
      encryption exists because KMS refuses payloads over 4 KB, and a project's
      signing keys are a few hundred bytes — a test asserts that, so the day a
      keyset grows past it fails here rather than in production. Data *at rest*
      (SEC-2) is the opposite shape and does want a data key; that is a
      different wrapper rather than this one growing a mode.

      `decrypt` names the key explicitly. Left to the ciphertext's own metadata,
      KMS decrypts with whichever key the blob names — so a blob swapped by
      someone who can write the database decrypts happily under a key they
      control, and the Control Plane starts signing with material an attacker
      chose. A test wraps under a second key and asserts the first refuses it.

      Configuring a KMS key on a binary built without the feature is refused
      rather than downgraded to `LocalKeyWrapper`: an operator who asked for KMS
      and silently got an environment variable would have exactly the protection
      they were trying to stop relying on, and no way to tell.

- [x] **SEC-2 — encryption at rest, per project.** `specs/04` §3 described this
      as present when segments, the WAL and the view snapshot were all written
      in clear. Now they are sealed with XChaCha20-Poly1305 under a key
      belonging to one project, and `specs/04` describes what exists.

      XChaCha20 rather than AES-GCM, and the reason is only the nonce. GCM's
      96-bit nonce makes random selection a birthday problem, and a repeat in
      GCM does not degrade the ciphertext — it hands over the authentication
      key. Avoiding that needs a counter, which a crash-and-truncate cycle can
      replay, or a rotation deadline nobody remembers. A 192-bit nonce needs
      neither, and is fast without hardware AES, which M12's edge targets will
      want. AES-GCM stays where it fits: the Control Plane, wrapping a few small
      keys, rarely.

      The checksum covers the ciphertext, so recovery can still find a torn
      record without the key — and a record that will not decrypt is reported as
      a wrong key rather than as corruption. That distinction is the whole
      safety argument: recovery answers corruption by truncating, so conflating
      the two would mean starting with the wrong key silently destroyed the
      database.

      Each segment records whether it is sealed, so encryption can be turned on
      for a store that already holds data. Building that surfaced a real bug —
      sealed records were being appended into a segment whose header said
      plaintext, which the next genuine recovery would have truncated away. The
      first version of the test missed it because the view snapshot
      short-circuited the replay; deleting the snapshot is now part of the test,
      and the comment says why.

      The Control Plane mints the key at provision time and holds the wrapped
      copy in the same blob as the project's signing keys, so no restart can
      find one without the other. It reaches the instance through the process
      environment and not through a file in the data directory: a key beside the
      ciphertext is copied by everything that copies the volume, which is most
      of what this defends against. `thetad` cannot fetch it itself — invariant
      1 leaves a hot-path crate with no HTTP client, so it cannot reach a KMS,
      and the Control Plane is the only party that can.

      Costs ~2.6µs per record against a `put` p50 of ~3.6ms dominated by an
      fsync. Too small for the end-to-end SLA to see, which is why `seal_cost`
      measures the cipher directly and now runs in the `sla` gate.

      One gap, named rather than left to be found: `audit.jsonl` is still
      written in clear. It holds change summaries and schema identifiers, not
      row values, so it is a smaller exposure than the log — and still one.
      Sealing it means getting the key into `theta-safety`; that is the next
      increment, and it is written down here so nobody reads "encryption at
      rest" and assumes the whole directory.

      Customer-managed keys are deliberately not here. The `KeyWrapper` seam
      from SEC-1 is where they plug in, so it is an implementation rather than a
      redesign — but it needs a rotation story for data already sealed, and that
      is M13.

- [x] **SEC-3 — a provisioning quota.** `POST /v1/resolve` with
      `confirm_create` provisions an instance, with no ceiling and no cost
      attribution at the point of creation. The blast-radius breaker bounds what
      an agent does *inside* a project; nothing bounded how many projects it
      created, so the retry loop `specs/07` was written for could exhaust a
      region rather than a table.

      A per-org ceiling from the billing plan, checked before anything is
      created rather than after. Counted from the org graph rather than from
      metering: metering is collected by the courier and lags, and a ceiling
      enforced against a stale number can be walked past by creating faster
      than the sweep. An org with no subscription is treated as being on the
      default tier rather than as unlimited, because "unknown means unbounded"
      makes the most expensive case the one nobody signed up for.

      Refused with 402, not 403. This is not a permission boundary, and telling
      someone they are "forbidden" from creating a project they are entitled to
      create sends them to the wrong person; the message names both fixes.

- [x] **SEC-4 — rate limiting.** None anywhere in `theta-control`. Signed
      tokens make credential brute force pointless, so the exposure is resource
      exhaustion: one caller can occupy the login path's upstream verification
      budget and take the Control Plane down for every tenant.

      A token bucket rather than a fixed window: a window lets a caller spend
      the whole allowance in its last millisecond and the whole of the next in
      its first, so the real burst is twice the configured one at the boundary.
      Keyed by identity where there is one and by address otherwise — an
      attacker rotates addresses far more easily than identity tokens, so
      keying on the credential is both fairer to callers behind a NAT and
      harder to evade. The identity key is a hash, because a long-lived map of
      live credentials is a richer target than a map of fingerprints.

      `/healthz` is exempt: a limiter that can convince a load balancer the
      process is unhealthy is a limiter that causes outages.

      No distributed state, deliberately. Two Control Planes mean two buckets
      and twice the rate, and fixing that needs a round trip to the store whose
      availability this exists to protect.

      The tests found a real bug: the first implementation used `insert_entry`
      rather than `or_insert`, so every request found a full bucket and nothing
      was ever limited. It read correctly and did nothing.

- [x] **SEC-5 — stop `/v1/revocations` publishing a census.** It returns every
      revoked token, session, user and org id to anyone who asks. The reason it
      is unauthenticated is sound — an instance whose own credentials lapsed
      must still be able to learn about revocations — so keep it reachable
      without a token and require *evidence* instead - which it now does: the
      caller names a project and gets only what an instance serving it needs,
      derived from the issued-token records rather than guessed. A token is
      recorded when it is minted, so every credential that could reach the
      project is in that table, and a user with no token for it cannot present
      one.

      Signing is worth more than the scoping. An unsigned list from an
      unauthenticated endpoint is one an attacker on the path can replace with
      an empty list, and the instance cannot tell; signing makes un-revoking
      require the signing key, which lives only in the Control Plane. An
      unknown project gets the same answer as a known one with nothing revoked,
      because distinguishing them would put project *existence* back on an
      unauthenticated endpoint.

      The version stays the global one rather than becoming per-project. An
      instance compares versions to decide whether an update is newer than what
      it holds, and a per-project counter would let a project that saw no
      changes accept a stale list.

- [x] **SEC-6 — keep the dev token out of the log pipeline.** `--dev-seed`
      logged a working identity token at `WARN`, labelled "never use this
      anywhere real"; log aggregators do not read labels. It is now written to
      `<instance-root>/dev-identity-token`, created `0600` rather than
      chmod'd afterwards — a credential that is briefly world-readable was
      world-readable.

      `--dev-seed` is also now *refused* when the listen address is not
      loopback, rather than warned about: the warning would be in the log
      nobody reads until afterwards. A hostname that would need resolving
      counts as not-loopback, because the safe answer to "I cannot tell" is to
      refuse — being wrong the other way puts a minted credential on a
      reachable port.

- [x] **SEC-7 — `eject`'s identifier interpolation.** Reviewed and sound:
      quote-doubling is correct for a quoted identifier and Postgres
      identifiers cannot contain a null byte. What was missing was the stated
      trust boundary, now in `specs/04` §5b — `eject` trusts the source
      database's catalog, and an unstated assumption is one a later change can
      violate without noticing.

- [x] **SEC-8 — keep the positives true.** The review records eleven properties
      an external test should try hardest to break rather than rediscover. Each
      was audited by planting the violation and confirming a test goes red — a
      recorded positive whose test passes either way is not a defended property,
      it is a comment, and a section that makes that distinction has to be held
      to it.

      Eight were already defended. Three needed work.

      **The chain was written and never read.** "The log is hash-chained, so
      tampering with history is detectable" described a data structure, not a
      control: `prev_hash` was set on every entry and checked on *append*, and
      recovery never looked at it. An attacker with write access to a segment
      could edit a record, recompute the CRC — arithmetic, not a forgery — and
      the log replayed as though the edit had always been there. The SEC-2
      pattern exactly, one directory over.

      Now verified on replay, plus `verify_chain` for the full-log pass that
      ordinary startup cannot do because it resumes from a checkpoint. Both
      limits are asserted by tests rather than described: an edit below the
      checkpoint is invisible to startup, and an edit to the *last* entry of a
      branch is invisible to everything, because nothing points at its hash.
      Closing that needs an anchor outside the log — a signed head published
      where whoever holds the disk cannot reach it. That is M13, and it is not
      claimed now.

      **Three claims cannot be defended by behaviour** and now have source-level
      guards, the same technique `no_llm_on_hot_path.rs` uses for invariant 1.
      No behavioural test tells a constant-time comparison from `==` — both
      accept the same deliveries, and a timing assertion in CI is a flake
      generator. "This variant does not exist" is a claim about absence, which a
      running program cannot observe. Two of the three turned out to be enforced
      by the compiler too; the guards stay because they name the property.

      One flaky gate found and fixed along the way, since a gate that fails
      two runs in three is a gate that gets ignored: `cost_calibration` built
      its fixture with `n` ascending, so sorting by it was O(n) — Rust detects
      the existing run — and "a scan is cheaper than sorting the same rows"
      held by a margin smaller than the measurement noise. The fixture now
      permutes `n` deterministically, which gives the sort real work while
      keeping the value *set* identical, so every range predicate in the file
      still matches the same rows.

      **One sentence in the review was wrong.** It said `session.rs` "checks the
      signature before reading anything else out of the token". `session.rs`
      applies none of its own checks first — that ordering is real and tested —
      but `verify` must parse the unverified payload before checking anything,
      because `key_id` selects the key to verify with. No multi-key scheme can
      do otherwise. The true and stronger property, now stated and tested:
      nothing read from an unverified payload can cause a token to be accepted;
      it can only select a candidate key, or reject.

**Gate (`CI` + `manual`)** — a restart test: kill the Control Plane at every
point in a resolve/revoke/provision sequence and assert that no revoked token is
ever usable afterwards and no tenant loses its identity. Plus the rate-limit and
quota paths asserted rather than configured.

**Depends on:** M4.

---

## M9 — AI Query-Assist (optional layer)

Deliberately last among the feature milestones: it is the only component that
calls a model, and it must be impossible for it to become load-bearing.

- [x] Separate service, separate deployment, called only on explicit request.
      `crates/theta-assist`, its own binary on its own port, one route. No
      background loop and no subscription — a service with a timer is one that
      can become load-bearing without anyone deciding to make it so.
- [x] NL → candidate typed plan, returned with an EXPLAIN-style preview.
- [x] Never executes on its own authority; the candidate goes back through
      `query()` like anything else.
- [x] Verify the hot-path guard still passes — Assist must not enter the
      `thetad` dependency closure.

**The design is one idea.** The model's output is untrusted text, and the only
door it can come through is `theta_query::sql::compile`, whose sole legal output
is a typed `Plan` and whose subset has no statement form for a write. A model
that is jailbroken, prompt-injected, or simply wrong still cannot produce
anything but a read-only plan over real tables, or an error. Structural, not a
filter: nobody maintains a list of bad SQL, and the verb nobody thought of is
refused too.

What survives the parser is well-formed but may name tables that do not exist,
so it is checked against the caller's schema and **refused rather than
repaired**. Correcting `usrs` to `users` is the most tempting feature this
service could have and the one most likely to read the wrong data
(invariant 3).

**"Never executes" is stronger than a promise.** Executing needs a `RowSource`,
and nothing in the crate has one — `suggest` takes a `Schema`, which describes
data without containing any. There is no connection here to run against.
`tests/never_executes.rs` asserts that behaviourally, in the source, and in the
manifest.

**Prompt injection is handled by not depending on the model resisting it.** The
schema reaches the prompt as text and a schema is data — someone who can add a
column can name it. That steers the model; it cannot widen what the answer is
allowed to be. `tests/injection.rs` includes the honest worst case: an injection
that *succeeds* still yields a read-only plan, unexecuted, with a preview the
caller reviews.

- [x] Both SDKs reach it. `assist.suggestQuery` / `assist.suggest_query` were
      stubs naming M9 as the milestone that delivers them, so leaving them
      stubbed would have closed M9 with the SDKs contradicting it. They talk
      plain HTTP to the Assist service rather than going over the Cap'n Proto
      transport — routing a suggestion through the protocol the hot path speaks
      is exactly what M9 is about not doing. An unconfigured `assistUrl` raises
      rather than degrading: a caller who did not deploy Assist should hear so,
      not receive an empty result where a query was expected.

**Gate (`CI`, `make assist`)** — `no_llm_on_hot_path` now names `theta-assist`
as a forbidden dependency of every hot-path crate, so wiring Assist into
`thetad` fails CI rather than review. Confirmed by planting it: the guard
reports `thetad → theta-assist`.

The gate also runs both SDKs against a live Assist with a scripted model and
compares what they produce, on the same argument `make conformance` makes for
the wire protocol: two clients that each pass their own tests prove nothing
about whether they agree, and "it works in Python" is how that reaches a user.

**It earned its keep on the first run.** The two SDKs disagreed about the plan
hash. `PlanHash` is a `u64` and was serialized as a JSON number, so
`17778716309262963995` came back from `JSON.parse` as `17778716309262965000` —
wrong by about a thousand, and entirely plausible-looking. A plan hash is what a
reviewer compares to confirm the plan they approved is the plan that ran, so a
value that silently changes in JavaScript makes that check worthless while
appearing to work.

`PlanHash` now serializes as a string. The generated SDK bindings had already
solved this for every 64-bit wire field by rendering them as `bigint`, with a
comment saying why; JSON has no `bigint`, so a string is the equivalent. The
reader still accepts a number, so nothing written before this stops loading —
lenient in the reader, never in the writer.

**On the latency budget, which is not met and says so.** `specs/09` §2 asked for
200ms p50 / 600ms p99. A cold call cannot meet that — a model round trip costs
hundreds of milliseconds before its first token — and no engineering on this
side changes it. Rather than miss it quietly or relax it quietly, the spec row
is now split into what it could have meant:

- Assist's own work: ~0.6ms p50, measured with a scripted model so the number
  describes this service and not a provider. The budget is spent essentially
  entirely on the model.
- A cached suggestion (same question, same schema): meets it with three orders
  of magnitude to spare. The schema is part of the cache key, so a schema change
  invalidates exactly the entries it should and there is no invalidation logic
  to get wrong.
- A cold suggestion: will not be under 200ms, and nothing here proposes to make
  it so.

That is tolerable only because of what Assist is — optional, explicitly invoked,
outside every hot-path SLA. A deployment that never starts it loses suggestions
and nothing else.

**Depends on:** M3, M6.

---

## M9.5 — Platform administration

ThetaBase's own operators, as distinct from its customers' owners. Everything
built so far is *tenant* authority: an org owner administers their org and can
reach nothing outside it, which is the property `04-threat-model-security.md` §3
is about. A platform admin is a different kind of principal entirely, and giving
one to a tenant role would collapse the isolation the product sells.

The reason this is its own milestone rather than a role added to `theta-control`:
the interesting work is not the permission bit, it is making an authority that
can cross tenants safe to hold.

- [x] **A `PlatformAdmin` principal, outside the org graph.** Not a fourth org
      role — and the reason is sharper than "different concern": `Role` is
      compared with `>=`, so a `Platform` variant above `Owner` would silently
      admit a platform admin to every existing `role.can(...)` check, including
      the ones whose whole job is to answer "not yours" identically to "no such
      org". The moment those two diverge, that endpoint enumerates tenants.

      So platform authority is a different type, from a different credential,
      checked in a different function. There is no value of `Role` that produces
      a `PlatformPrincipal`, and `PlatformRole` deliberately has no `PartialOrd`
      either: tenant roles nest, these do not. The person debugging an instance
      has no business moving money.
- [x] **Separate authentication, and hardware-key 2FA required.** Separate in
      three independent ways: a different signing keyset, a different token
      prefix (`vp1.` against `v1.` and `vi1.`, so a tenant token fails on its
      shape before any key is consulted), and a second factor that is required
      by the only constructor rather than by a flag someone can pass `false` to.
      Sessions last fifteen minutes, and the role is re-read from the directory
      on every request — a suspension takes effect at once rather than when the
      session expires.

      **Open, and stated rather than implied:** this is not a full WebAuthn
      verifier. The assertion is checked as an origin-bound, single-use ed25519
      signature, which gets the properties that matter — unreplayable, and not
      readable aloud to an attacker the way a TOTP code is. Attestation-format
      parsing, the user-verification and user-presence flags, and the
      signature-counter regression check that detects a cloned authenticator all
      belong in a real implementation and are not there. Written down because a
      control described more strongly than it is built is exactly SEC-2.
- [x] **Every action logged to an audit trail the admin cannot write to.**
      Append-only structurally: there is no `delete`, no `update`, and no
      `retain` on the trail or on the store trait behind it, and no
      `PlatformCapability` variant for redaction — so there is nothing to grant
      by mistake. A test sweeps `DELETE`, `PUT` and `PATCH` against the audit
      routes as a superuser and requires 404.

      Also hash-chained, and — per SEC-8 — the chain is actually *read*.
      `verify` walks every link and recomputes every entry's hash, so editing an
      entry is caught, and re-hashing it to cover the edit breaks its
      successor's link instead. `Trail::restore` verifies at startup: the moment
      to learn the log was tampered with is not mid-incident, when somebody is
      asking whether it can be trusted.

      The same honest limit as the log: an edit to the *last* entry breaks no
      link, because nothing points at its hash. `Trail::head` exists to be
      published where we cannot reach it, which is what would close that. Until
      it is, this detects alteration of history but not truncation of its tail,
      and a test pins that rather than leaving it to be discovered.

      Every action is recorded *before* it runs and resolved after, because one
      logged only on success goes unrecorded whenever it panics or times out —
      precisely the set somebody would most want unrecorded. Refusals are
      recorded too: an admin repeatedly being told no is the most interesting
      pattern this trail can hold.
- [x] **No default read access to tenant data.** A `DataAccessGrant` is about
      one admin, one project, one window, one stated reason — never a permission
      bit on the admin, because an admin who "has data access" in general is the
      principal this milestone exists to avoid creating. Every grant expires;
      four hours is the ceiling, and a longer ask is refused rather than quietly
      shortened (invariant 3).

      Two paths that differ in what they cost the customer, not in what they
      reveal: an approved grant is asked for and waited on, and break-glass is
      taken now and is loud. Break-glass records `approved_by: None`, because
      nobody approved it — a phantom approver would blur the one distinction
      that matters. It also needs a role ordinary support does not hold.

      The customer's half is on the *tenant* surface, exercised with their own
      credential and gated on a new `ApproveDataAccess` capability at owner
      level: consenting to let someone outside the org read the rows is closer
      to signing something than to operating a database. They can withdraw
      mid-session, and break-glass appears in their list immediately — they did
      not get a say beforehand, and they do get to see it straight after.
- [x] Operational surface: list and search projects across orgs, inspect
      instance health and placement, force-pause a runaway project, re-drive a
      stuck provisioning, rotate a project's keys. The fleet view is shaped so
      it *cannot* carry a row, a key, or a column name — a test asserts the
      response contains none of them, because the way this leaks is somebody
      adding a helpful field in a hurry.
- [x] Support surface: impersonation *with the customer's recorded consent*,
      scoped and time-boxed, never silent. — **built.**
      `crates/theta-control/src/impersonation.rs`, `specs/04` §3.4, claim
      `support-never-acts-as-you-invisibly`.

      Three decisions, because the obvious implementation gets all three wrong:

      **A session never claims to be the customer.** Minting a token that looks
      like theirs would make every record produced during the session say the
      customer did it — support reproduces a bug, a row changes, and six weeks
      later the customer's own audit log attributes it to them, with nothing
      inside the tenant indicating anyone else was ever there. That is an audit
      trail that is confidently wrong, which is worse than one with a gap,
      because a gap is visible. Both identities travel together, always.

      **Read-only, and there is no flag that changes it.** A grant obtained for
      debugging must not confer the ability to change the data. When a write is
      genuinely needed the customer makes it — slower, and correct: the person
      accountable for the data is the person who changes it.

      **Consent is re-read on every request, not at open.** A customer who
      withdraws expects it to stop now, and checking once would leave a
      four-hour window in which withdrawal does nothing — precisely the window
      they would care about.

      Ending a session does not revoke the grant. Collapsing the two would make
      an operator's cleanup look, in the record, like the customer changed their
      mind.
- [x] Billing administration: refunds, credits, plan overrides, and dunning
      state. — **built.** `crates/theta-control/src/billing_admin.rs`,
      `specs/06` §8.

      The separation finally has something to guard. Four objects rather than
      one signed "adjustment", because they differ in what bounds them, and the
      way a single validation path fails is that the loosest bound wins.

      A refund is bounded by **what remains refundable**, not merely by the
      charge: refunding twice is far likelier than refunding too much once,
      because two people working one support ticket is an ordinary Tuesday.

      No dunning state makes a customer's data unretrievable, written-off
      included. Non-payment is a commercial dispute, and a commercial dispute is
      not settled by deleting somebody's database — asserted over every state,
      so a new one has to answer the question.
- [x] **Design-partner terms: coupons, credits, and waived tiers.** —
      **built.** Claim `free-usage-is-an-object-with-an-owner-and-an-end`.

      Each grant carries who authorised it, what it is worth, what it applies
      to, and when it ends. **Expiry is required**, and an over-long term is
      refused rather than quietly shortened (invariant 3): capping a two-year
      ask to one year produces a customer who believes they have something they
      do not, and the first anyone hears of it is a renewal conversation that
      goes badly.

      **Redemption cannot create a coupon** — there is no upsert, and that is
      the property rather than the convention. If redeeming and issuing shared
      an entry point, whatever validates "may this caller create value" would be
      the same code running for an anonymous form post, one missing branch away
      from letting them.

      One thing the positional API allowed and the named one does not:
      `value_cents` and `term_ms` were adjacent `i64` parameters. Swapping them
      yields a coupon worth a few milliseconds that runs for several centuries —
      compiles, passes every bound check, discovered in a revenue report.
- [x] **Reporting.** — **built**, and it is the same module as M10's
      dashboards, because both rest on one requirement.
      `crates/theta-control/src/reporting.rs`, `specs/09` §5.1, claim
      `no-number-without-its-provenance`. See M10 for what it does and why.
- [x] Roles within platform administration: support, operator, billing,
      superuser. Non-nesting, as above. Everyone can read the audit trail — an
      audit log readable only by the people it audits is not much of a control.

**Durability (`make durability`, needs a Postgres).** The trail, the admin
directory and the grant book all survive a restart, and the Control Plane
**refuses to start** on a trail that does not chain — failing closed, because a
process that started anyway would be one whose audit log nobody could believe,
running as though nothing were wrong.

That suite found a real gap on its first run: the three tables were created and
written correctly, and `PostgresStore::load` never read them back. Everything
persisted and everything came back empty, with no error anywhere — the writes
succeeded, so nothing complained. It is also why the tamper test reaches around
the store's own methods with raw SQL: the store cannot edit an entry, so an
attacker would need database access, and that is the threat the chain is for.

**Gate (`CI`, `make platform`)** — **Met in CI.** All three properties are
swept over the HTTP surface in `crates/theta-control/tests/platform_isolation.rs`:
every tenant role against every platform route, no row data without a recorded
grant, and every action — including every refusal — in a trail with no route
that could edit it.

One deliberate deviation from the wording above: the escalation property is
asserted there rather than in the adversarial corpus. That corpus is the Safety
Layer's, and every entry in it is a `SchemaChange` classified into a `Gate`;
a token-authority test filed there would be filed where it does not fit and
where nobody looking for it would find it. Sweeping every route with every
tenant role is the same assertion in the place it belongs.

**Still open, and not claimed:**

- The **manual support walkthrough** of the three most common escalations. It
  needs an escalation to walk through, and nothing is in production yet.
- **Collection and retention for the reporting module.** The shapes are built
  and tested; nothing yet sweeps the `status` RPC into them on a schedule. That
  is wiring, and it wants a staging environment to be worth running against.
Everything else in this milestone is built. The three items that were open here
— impersonation, billing administration, reporting — were the ones the roadmap
described as having their *separation* enforced with nothing behind it, which is
the least useful state for a control to be in: it looks like a policy and
defends nothing.

The billing surface is the clearest case. `AdministerBilling` belonged to the
billing and superuser roles alone and there were no billing operations, so the
rule guarded an empty room. `an_operator_cannot_touch_money` sweeps all five
routes and is the first test in this repository that can fail if that separation
stops holding. It is paired with `a_billing_admin_can_do_the_billing`, because
without a positive case both refusal tests are satisfied by a route that refuses
everyone — a way of passing a security test that nobody notices until a
customer needs a refund.

**A finding from building them.** Adding the impersonation routes turned
`platform_isolation.rs` red, and it was right. Its sweep sent one fixed body
that happened to satisfy every existing request type, so the property it asserts
— an unauthenticated caller reaches nothing — was only ever proven for callers
*whose body parsed*. A framework runs body extractors before the handler, so a
stranger sending nonsense got `422` rather than `401`, which confirms the route
exists and says the body was the only thing wrong.

Fixed by making platform routes **authenticate, parse, authorise, act** in that
order (`specs/04` §3.5), and pinned by sweeping several malformed shapes rather
than one well-formed one. `tests/route_coverage.rs` now also fails if a route is
added to `platform_api.rs` without being added to the sweep — the drift that
made this possible, and the sort of thing that only ever bites the newest route,
which is also the one whose authorisation has been thought about least.

**Depends on:** M4, M5. Should land before any paid tier has real customers on
it — the first time support needs to look at a live project is too late to
design this.

---

## The queue — what is actually left, in order

Written 2026-08-27, after M10.6. Everything above M11 is done or listed here.
The ordering is not preference: each block is blocked by the one above it, or
would be wasted work if done out of order.

**1. Ship before anything is public** — these three gate every other item,
because each of the others is a public disclosure.

| | |
|---|---|
| Trademark clearance on "ThetaBase" | M11.5, and `DISTRIBUTION.md` §0 |
| Decide and file IP protection | M11.5 |
| Choose the licence, flip the release state | M11.5 |

**2. Then the comparative benchmark** — M11.5. M10.6 just moved the numbers by
45×, so running it before now would have measured a system that no longer
exists. Docker is the only dependency for the local half.

**3. Then the site, in the order the gate can check it** — M11.5. Reference
docs and guides are generated or CI-checked; marketing copy comes last because
`theta-claims copy` can only check it once the claims it cites are settled.

**4. In parallel, the operational half** — M10 and M9.5. Neither blocks the
site. Dashboards (M10) are the item that unblocks M12's SLA, and nothing else
does, so they should start early even though they finish late: `specs/09` §6
requires a full production quarter of telemetry, and the quarter cannot start
before the instrument exists.

**5. External reviews** — M11's remaining three are all somebody else's
calendar. Book them now; they are the longest lead time in the project and the
only items that cannot be compressed by working harder.

**6. Engine work that is deferred rather than pending** — M1's segment release
(needs M10's custodian schedule), M2's zero-copy reads, M3's bytecode plans and
join ordering. Each records the condition that makes it worth doing; none of
them is on the critical path to GA.

**7. GA** — M12, which is mostly the assertion that everything above is
simultaneously green on one commit.

---

## M10 — Operations, observability, and recovery

- [x] Cold archive: log segments to object storage, point-in-time restore.
      `crates/theta-archive`, live as `make archive` and `make archive-live`.

      Two suites, because they answer different questions. `archive` runs
      against a fake backend that corrupts silently, returns success without
      storing anything, lies about its own integrity, and goes unreachable on
      demand — the failure modes a real archive cannot be asked to produce.
      `archive-live` runs the same flows against the real AT-1 service, which is
      the only thing that can tell us the commands this crate issues still do
      what the CLI's documentation says.

      Measured on log-shaped segments: **561,670 bytes to 10,576, a ratio of
      0.019** — 53× — with a verified byte-identical restore. Framed binary
      records survive as well as line-delimited JSON does, which matters because
      `auto` picks a codec by structural fingerprint and segments are not all
      one shape.

      **The invariant, which is the whole crate:** a local segment is never
      released until the archive has been *proved* to return it byte for byte.
      Not the exit code, not the container's checksum, not the compressor's
      documented losslessness — a round trip against a digest taken before the
      compressor saw the data. One decompression per segment, against the
      alternative of finding out during a restore.

      A gap refuses the restore rather than skipping it: the log is a fold, so
      applying segment 5 after a missing 4 gives a state that never existed and
      looks entirely normal (`CLAUDE.md` invariant 6). `Manifest::gaps()` runs
      on a schedule rather than at restore time, because an archive with a hole
      is broken from the moment the hole appears.

      **AT-1** (`tinyfiles.io`, `npm install -g @tinyfiles/cli`) is the
      container format. Three of its properties match what a log archive needs
      rather than what is merely nice: byte-identical decompression, SHA-256 on
      the container so rot is detectable without a full restore, and queryable
      in place over HTTP Range so locating segments for a point-in-time restore
      does not mean pulling everything first. Its `worm` command verifies
      append-only journals, which is exactly what a ThetaBase log is — the closest
      fit of all, and still to be taken up.

      Encoding requires a connected account; decoding, querying and verifying
      never do. That asymmetry is worth more than the free tier: a restore has
      to work when the billing relationship does not, and an expired card must
      never be why a customer cannot get their data back.

      It lives outside `thetad` so the no-LLM dependency guard stays true by
      construction, reads segments the engine already released through
      `archivable_segments()`, and drives the CLI as a subprocess — so no HTTP
      client is linked anywhere near the hot path.
- [x] Drive sweeps on a schedule, and release local segments once proved.
      `Custodian::tick` archives, proves, releases and reports; `schedule::run`
      is the loop around it. The property the tests assert is the unglamorous
      one: **local disk goes down**, measured in bytes before and after, rather
      than "release was called".

      **Releasing is two decisions, kept apart.** `ArchiveOutcome::releasable`
      answers "has the archive proved it can return these bytes";
      `Wal::release_segment` answers "can this process still recover without
      them". Neither answers the other's question, and deleting needs both.
      Keeping them separate makes a bug in the archiver a wasted sweep rather
      than a lost segment: the storage layer re-checks the sequence against its
      own checkpoint and refuses anything at or above it, whatever the archiver
      believes. Releasing twice is a no-op rather than an error, because a sweep
      interrupted between deleting and recording will run again.

      The scheduler is deliberately thin — everything that decides anything is
      in `tick`, which is synchronous and takes its clock as an argument. A
      scheduler that also decided what to archive would be one whose logic could
      only be tested by waiting; as it is, its behaviour over an hour is
      asserted in microseconds on a paused clock.

      A deferred archive is **not** an incident and does not raise anything: a
      database whose writes stop when a *backup* is unreachable has made its
      backup a dependency of being up, and paging on it is how the alert gets
      muted before the failure that matters. A failed *proof* is loud — that
      means the archive returned something other than what went in.
- [~] Archive-unavailable path: writes continue locally, snapshots queue and
      retry. Implemented as `ArchiveOutcome::Deferred`, now driven by the
      scheduler so the retry actually happens, and tested end to end against a
      log that keeps every segment while the archive is down.

      **The extended-outage page and the secondary target are still not built**,
      and that is the largest gap in `docs/RUNBOOKS.md`: today a five-day outage
      looks exactly like a five-minute one until somebody notices the disk. The
      resource an outage consumes is local headroom, and nothing watches it.
- [x] **Bring your own object storage.** An S3-compatible `ArchiveBackend`
      alongside AT-1, so a customer can keep their archive in a bucket they
      control. S3-compatible rather than S3: the same API is spoken by R2, B2,
      MinIO, Ceph and every on-premises appliance worth having, so one
      implementation covers self-hosted and cloud both.

      **This is what makes the AT-1 recommendation honest.** Today AT-1 is the
      only real backend, so recommending it is not a recommendation — it is the
      absence of a choice, and `DISTRIBUTION.md` says as much. With a second
      backend that a customer can actually run, "AT-1 is recommended" becomes a
      claim that rests on measured differences instead of on there being nothing
      else.

      **The round-trip proof does not change, and that is the whole point.** A
      segment is released only once the archive has been *proved* to return it
      byte for byte, against a digest taken before any compressor saw it. That
      invariant belongs to `theta-archive`, not to a backend, so it holds for a
      bucket exactly as it holds for AT-1. Making it hold is not extra work —
      it is what `ArchiveBackend` is for.

      **The suite becomes generic over the trait.** `round_trip.rs` currently
      drives a fake that corrupts, lies about integrity, goes unreachable, and
      accepts without storing. Those are the cases a real service cannot be
      asked to produce, so the fake stays — but the *invariant* tests should run
      against every backend, so adding one means proving it satisfies the same
      contract rather than hoping it does. A backend that passes its own tests
      and not the shared ones is the shape this failure takes.

      **`check_integrity` must not lie**, and S3 makes that easy to get wrong.
      An ETag is an MD5 for a single-part upload and *not* a content hash for a
      multipart one, so treating it as a checksum reports "intact" for an object
      it has not checked. Use `x-amz-checksum-sha256` where the store provides
      it, and report *unknown* rather than *intact* where it does not — a weaker
      answer is fine, and a confident wrong one is the failure this method
      exists to prevent.

      **Compressed with zstd** rather than stored raw. Uncompressed log segments
      in a customer's bucket is a bill we would be handing them, and zstd is
      boring, ubiquitous and independently audited. It also keeps the comparison
      with AT-1 specific rather than rhetorical: the honest pitch is not "we
      compress and they do not", it is the ratio, plus WORM verification of
      append-only journals, plus query-in-place over HTTP Range.

      **Feature-gated**, like the KMS wrapper, so a build that will never touch
      a bucket does not carry an S3 SDK. Credentials are read the way the data
      key is — from the environment, never from the data directory — and
      `theta-archive` is already outside `thetad`'s dependency closure, so none
      of this comes near the hot path.

      **Built and proved against a real store.** `crates/theta-archive/src/s3.rs`,
      behind the `s3` feature, exercised by `tests/live_s3.rs` against MinIO —
      chosen over AWS for the reason `make kms` chooses LocalStack, and because
      passing against an *S3-compatible* store is evidence for the claim
      actually being made where passing against AWS alone would only be evidence
      for AWS.

      The contract moved to `tests/contract/mod.rs` and both backends run it.
      That is the structural half: a backend is not "supported" because it has
      tests, it is supported because it passes *the same* tests.

      **The measurement changed the pitch.** zstd gets 0.020 on log-shaped
      segments where AT-1 gets 0.019 — effectively identical. The compression
      argument for AT-1 does not survive being measured, so it is dropped rather
      than repeated. What remains is real and worth stating: `worm` verifies
      append-only journals, which is exactly the shape a ThetaBase log is, and
      query-in-place over HTTP Range means a point-in-time restore does not
      begin by pulling everything. Plus the first 100 GB free, which is the part
      a new user actually feels.

      The site may now say the archive is pluggable, because it is.
- [ ] **A staging environment.** Blocked on the IP filing only; the bring-up
      is written and ready in `docs/STAGING.md`, in an order where each step is
      verifiable before the next depends on it.

      Three gates need it and none can be closed without it: `RUNBOOKS.md`'s
      manual half has never been performed against a deployment, external review
      2 is reviewing a diagram until it exists, and M12's SLA quarter cannot
      start before the instrument that measures it is proven.

- [x] Per-project metrics and internal dashboards. — **built.**
      `crates/theta-control/src/reporting.rs`, `specs/09` §5.1.

      **This is what unblocks M12's SLA, and nothing else does.** `specs/09` §5
      is explicit: no SLA claim ships without a corresponding internal dashboard
      verifying it in production. So this is not reporting — it is the
      instrument that starts the quarter running, which is why it is built to
      refuse rather than to encourage.

      **A number and its provenance are one value.** There is no representation
      of a bare figure: nowhere to put a value while leaving where-it-came-from
      for later, which is the only way that question reliably gets answered.

      **A percentile from fewer than 100 samples is refused.** A p99 over eleven
      samples is the largest of eleven samples — a maximum wearing a
      percentile's clothes, and lower than the true p99 essentially always,
      biased in the direction that makes an SLA look met. This is the mechanism
      by which §6's "a full production quarter" gets enforced by something other
      than somebody remembering.

      **Absent is never zero.** A project reporting `0ms p99` because nothing
      was collected looks like the best in the fleet; one reporting `0` breaker
      trips because its collector broke looks like the healthiest. `swept`
      exists precisely to separate "we looked and found none" from "nothing has
      told us".

      **A fleet figure is the worst project, not the mean, and states its
      coverage.** An SLA is a promise to each customer, not to the average one:
      one project at 400ms behind ninety-nine at 4ms is a customer whose SLA is
      being missed, and the mean says 8ms. Coverage is carried because averaging
      over the projects that reported and calling it fleet health hides a
      missing third — and the missing third is disproportionately the broken
      one.

      Rows cover the five latency claims in §2 plus branch creation and first
      write, which became claims at M10.6.

      **Still open:** collection and retention. The `status` RPC already carries
      most of the numbers (`specs/02`) and nothing yet sweeps them into this on a
      schedule. That is wiring, and it wants the staging environment to be worth
      running against.
- [ ] Auto-pause for idle projects; auto-scale on volume growth.

      **The economics only work with it.** One project is one `thetad` process
      (`specs/04` §3), so a thousand trial projects that nobody has touched in a
      week are a thousand processes holding memory. Pausing them is what makes
      a free tier affordable, and `specs/09` §4 already says the auto-pause
      behaviour is "re-specified once actual usage data exists".

      **The hard part is not stopping, it is starting.** A paused project's
      first request has to fit the cold-start budget in `specs/09` §2 —
      500ms p50, 1.5s p99 for provisioning — and a paused project resuming is
      the same problem with a warm disk. The log is the only source of truth
      (invariant 6), so resume is: open the segments, load the view snapshot,
      replay whatever came after it. **That is exactly the path
      `a_lost_view_snapshot_costs_replay_time_not_correctness` already
      measures**, which means the resume budget is a question about snapshot
      frequency rather than a new mechanism.

      Auto-scale is the smaller half and can follow: it is a placement
      decision in the Control Plane, not an engine change.
- [ ] Read replicas — read-only. Cross-region multi-master is explicitly out of
      scope for v1 and is a stated non-claim (`specs/03` §4).

      **The log makes this unusually cheap, and that is worth checking rather
      than assuming.** A replica is a process that replays the same log and
      serves reads from its own view; it needs no consensus, because it is never
      a writer. The segments are already content-addressed and hash-chained, so
      a replica can verify what it received rather than trusting the link.

      **What it changes is a guarantee, not an implementation.** `specs/03` §3.1
      promises read-your-writes for the client that performed the write, and a
      replica that lags breaks that for a caller routed to it after writing to
      the primary. Either the SDK pins a session to the primary after a write,
      or the replica refuses reads below the version the caller names — and
      M10.5's per-key versions are exactly the mechanism for the second. **Pick
      one deliberately and write it into `specs/03` before building**, because
      discovering it afterwards means either a silent weakening of a claim in
      `claims.toml` or a rewrite.
- [x] Runbooks for each failure mode in `specs/01` §7 — `docs/RUNBOOKS.md`,
      plus two the spec does not list and an operator will meet anyway (disk
      filling, and suspecting the audit trail).

      Each entry ends with **Verified by**, naming the test that exercises the
      path or saying plainly that nobody has run it. That field is the point: a
      runbook nobody has followed is a hypothesis, and the gate below turns on
      having performed them, not on having written them well. Most currently
      read *not yet performed in staging*, because there is no staging.

**Gate (`manual`)** — **Not met.** `specs/08` §7 asks for every failure mode
in `specs/01` §7 simulated in staging with the documented recovery observed. Two
things are missing and neither is a code change:

- **There is no staging environment.** Every runbook entry that says *not yet
  performed in staging* says it for that reason.
- **Nothing pages anybody.** Every "what you will see" in the runbooks assumes
  somebody is looking at a log.

What *is* met is the part CI can hold: the automated half of each failure mode
has a test, and `make ops` runs them together rather than leaving them scattered
across five crates.

**Still open in this milestone:**

- Per-project metrics and internal dashboards. Deliberately not started rather
  than half-started — the requirement is that no SLA number is published without
  a dashboard proving it in production, and a dashboard that shows figures
  before it can say what it measured is what that requirement exists to prevent.
- Auto-pause for idle projects and auto-scale on volume growth. `Provisioner::pause`
  exists and M9.5 gave an operator a route to it; what is missing is the
  idleness signal that would drive it without one.
- Read replicas.

**Depends on:** M1, M4.

---

> An internal review against `specs/04` is in [`SECURITY-REVIEW.md`](SECURITY-REVIEW.md).
> It does not replace the independent test this milestone requires — a review by
> the authors shares the authors' blind spots — but it raises the floor the
> external one starts from. Two of its findings (SEC-1, the Control Plane's
> state living only in memory; SEC-2, encryption at rest) are pre-launch
> blockers rather than M11 work.

## M10.6 — Make the fork cheap

Branch creation is the one operation in `specs/09` §2 whose cost scales with how
much data the parent holds, and on realistic rows it is **~1.7µs/row** — a
million-row branch forks in about 1.7 seconds and misses its own p99 by 8×.
Measured by `crates/theta-storage/tests/branch_cost.rs`.

**Everything here is a representation change behind `MaterializedView`.** No
observable semantics move: a read returns what it returned, a fork means what it
meant, a merge does what it did. That constraint is what makes this milestone
safe to do at all, and every item below is rejected if it cannot meet it.

**The one thing that must not regress** is the property the fork copy buys:
reads that do not degrade with branch depth (`claims.toml`
`reads-do-not-degrade-with-branch-depth`). Every design that makes forking cheap
by *sharing a parent pointer and consulting it on read* is out, because that is
precisely the trade BranchBench found every other system making, and it is the
one axis where ThetaBase currently wins outright.

- [x] **1. Stop cloning the indexes.** Done. `Arc<Indexes>` with copy-on-write
      at the first write. Alone it took the realistic fork from ~30,100µs to
      ~20,300µs — a third, from one field. `Indexes` is `#[serde(skip)]`, its
      `PartialEq` ignores contents, and `rebuild_indexes` regenerates it from
      `keys` and `schema` — it is already, by its own documentation, a derived
      cache. A fork clones it anyway: every indexed value, plus every primary
      key string a third time. `Arc<Indexes>` with copy-on-write at the first
      write to a branch costs nothing to read and removes the largest single
      component of the 6.7×.

- [x] **2. Merge `versions` into `keys` — not done, and no longer worth doing.**
      The point was to halve the key strings a fork copies. A fork copies
      nothing now, and item 5 removed the deferred copy too, so this would buy
      memory rather than time: one `String` per key instead of two.

      That is still real — a million-row branch holds a million redundant key
      strings — but it is a memory item, not a latency one, and it should be
      argued on that basis by whoever picks it up. Recorded as closed rather
      than left open, because an item whose reason has evaporated is noise in a
      list somebody has to read. Two `BTreeMap<String, _>` keyed
      identically means every key string is allocated and copied twice, and two
      trees are walked where one would do. `BTreeMap<String, (Value, u64)>` is
      the same information in half the copies. Mechanical, touches many call
      sites, no semantic change — a deleted key still *removes* the entry, which
      is what makes `Absent` nameable.

- [x] **3. `Arc<str>` keys — not done, same reasoning as item 2.** It would make
      the copy cheaper, and there is no copy. Would still reduce memory, sharing
      one allocation per key across every branch that holds it; that is the
      argument to make if anyone revisits it. Cloning a map of `Arc<str>` copies pointers and
      bumps refcounts instead of heap-allocating every key. Combined with (2)
      this is the whole string cost of a fork, gone.

- [x] **4. Defer the copy — done, and at better granularity than planned.**
      The plan was `Arc<MaterializedView>` in `BranchViews`, which would have
      touched every call site that reaches a view. Putting the `Arc`s on the
      *fields* instead — `keys`, `versions`, `crdts`, `indexes` — gets the same
      result and needed no change outside `view.rs`, because `Arc` is
      transparent to a reader: all 87 external reads go through `Deref`
      unchanged, and only the twelve places that mutate had to say
      `Arc::make_mut`.

      **Result: the fork is constant time.** ~30,100µs → ~660µs on 20,000
      realistic rows, and flat across parent size and row shape — all of it one
      fsync. Reads unmoved at 0.045µs and still depth-independent.

      The copy is deferred to the first write on a branch: ~3,800µs on a
      20,000-row parent, ≈0.19µs/row. A branch forked, read from and discarded
      never pays it. That number is now a claim of its own, because reporting
      constant-time forks without it would describe half the operation.

      **The guard caught its own obsolescence.** `branch_cost.rs` asserted that
      forking off a large parent cost more than off a small one — the property
      it was written to protect. When that stopped being true the test went red
      with the right message, which is what a planted-violation test is for.

      *(Superseded item 4 as written: `Arc<MaterializedView>` in `BranchViews`
      is no longer needed, and would now be a second layer of sharing over one
      that already works.)* A fork
      becomes `Arc::clone` — O(1), no bytes moved — and the copy happens on the
      *first write to that branch*, if there ever is one. **An agent that forks,
      reads, and discards never pays it at all**, and that is the common case the
      whole product is shaped around. A branch that does write pays the same
      total, later. `Arc::make_mut` gives exactly this with no new dependency.

- [x] **5. Persistent maps — done, and the deferred copy is gone.**
      `imbl::OrdMap` for `keys`, `versions` and `crdts`. A write path-copies
      O(log n) nodes and shares the rest, so the first write to a forked branch
      costs what the second one costs: 614µs against 594µs, both one fsync,
      where the `Arc` version had a ~3,800µs difference.

      **Measured before the refactor, not after.** The whole decision turned on
      one number — how much slower a persistent lookup is — so that was probed
      in isolation first: 59ns against 110ns for `get`, 899ns against 2,280ns
      for `insert`, and a clone of a 220,000-row map going from 148ms to 0.13ms.
      With the answer in hand the refactor was obvious; without it, it would
      have been a guess about the hot path.

      **The cost is 40 nanoseconds a read** and it is now a registered
      non-claim. `get` measures 104µs p50 end-to-end against a 5ms target, so
      it is 0.04% of the budget — absorbed, not invisible, and said out loud
      because a system trading read speed for branch speed should be the one to
      say it.

      **Three sites changed outside `view.rs`.** The `Arc` work in item 4 had
      already moved every mutation behind a single call, so switching the type
      underneath touched almost nothing.

      *(Superseded item 5 as written: the goal was O(1) clone, which item 4
      already had. What this actually bought was removing the deferred copy.)* Replace
      `BTreeMap` with a structurally-shared immutable map (`imbl::OrdMap` or
      similar). Clone is O(1) **always**, not just until the first write, because
      a write path-copies O(log n) nodes and shares the rest. Reads stay O(log n)
      with a worse constant — call it 1.5–2×, to be measured, from 45ns to maybe
      70–90ns, which is still four orders of magnitude under the 5ms `get` p50 it
      sits inside.

      *Rationale as written before it was done, kept because it is what the
      decision was made on:*

      **Items 1 and 4 already delivered the paper's two headline properties**
      — constant-time forks *and* depth-independent reads, which BranchBench
      calls "a system architecture that doesn't yet exist". What item 5 adds is
      removing the *deferred* copy rather than moving it: with structural
      sharing a write path-copies O(log n) nodes, so the first write to a
      forked branch costs what any other write costs.

      That is the difference between "forking is free and the first write is
      not" and "forking is free". Worth measuring; not worth claiming before
      the read constant-factor is known.

      Measure before believing it. The constant factor on reads is the risk, and
      `branch_cost.rs` plus the `sla` gate are what decide it.

**Gate (`CI`)** — `sla`, extended: the depth-independence assertion must still
pass, and the realistic-row fork marginal must come down. Numbers get published
only through `claims.toml`, pinned to `specs/09` §2.1, so an improvement that is
not written down is an improvement that did not happen.

**Do this before the comparative benchmark in M11.5**, not after. Publishing a
comparison and then improving by an order of magnitude wastes the one moment
anybody is paying attention.

**Depends on:** M1. Blocks nothing — the product works today, it is just paying
more than it needs to.

---

## M10.5 — Conditional writes, per-key versions, and retention

Three items from a review of what `specs/03` §5 claims against what the code
does. Two of the claims were **too weak**, which is the SEC-2 problem running in
the opposite direction — still drift, and it costs the product a real guarantee
it already has.

### What the review found

**`version_id` on a `get` is a global counter.** `dispatch.rs` returns
`engine.commits_applied()` — the number of commits the whole branch has applied,
presented in a field named as though it were the row's version. Any client using
it as one is using a number that changes when an unrelated key is written. That
is not a limitation to document; it is a field that means something other than
its name.

**Writes to a branch are totally ordered, and the spec says they are not.**
`specs/03` §5 says "no global linearizability guarantee across concurrent
writers to the same key". But `concurrent_writers_to_one_key_serialize_to_one_of_their_values`
already asserts that eight concurrent writers all succeed and the survivor is
always a value somebody wrote — never a blend, never a missing key. There *is* a
total order within a branch. What is genuinely absent is **atomic
read-modify-write**: two clients doing get-then-put can lose an update, because
every put succeeds unconditionally.

Those are different claims, and the weaker word undersells the stronger
property.

**"ACID across shards" describes a system that does not exist.** There is no
sharding in v1 — one project is one log, served by one process. The real
boundary is cross-*project*, and that is not a shortfall: `specs/04` §3 makes
cross-project queries architecturally impossible rather than access-controlled,
and that is the isolation the security model sells.

### The work

- [x] **Per-key versions.** `MaterializedView` tracks the commit that last
      wrote each key — a fold over the log like everything else (invariant 6).
      A deleted key is *removed* rather than tombstoned, which makes "absent" a
      state a precondition can name exactly and stays correct across
      delete-then-recreate: the recreate writes a new commit, so a caller
      holding the old version is still refused.

      `version_of` returns `Option`, and `None` is deliberately not `Some(0)`. A
      sentinel would make "I forgot to send a version" and "I require this row
      to be absent" the same request, and the second is a far stronger claim
      than anyone makes by accident.
- [x] **Conditional put.** `putIf` carries a precondition — absent, or at
      exactly this version — evaluated in the same `&mut self` call that
      appends, so nothing can write to the branch in between. A precondition
      that could be raced is not a precondition.

      Refused with the row's *current* version, so a retry costs no second round
      trip. That matters most on exactly the contended keys where retries
      happen.

      Answered as its own wire response rather than as an error. The request was
      well-formed and the server did what it was asked; conflating that with a
      fault would make an ordinary lost-update retry indistinguishable from an
      outage, in logs and in error budgets alike.

      A distinct request rather than optional fields on `put`, so an older
      server refuses the call outright instead of accepting the write and
      silently dropping the condition. A precondition that can be lost in
      transit is worse than none — the caller believes they are protected and
      they are not.
- [x] **Retention as a policy, not a limit.** `Retention::Forever` is the
      default and a supportable tier rather than a placeholder; `For { ms }`
      expires history beyond a window.

      **Expiry removes a contiguous prefix and never a hole.** The log is a
      fold, so a missing segment in the middle produces a state that never
      existed and looks entirely normal (invariant 6). So expiry stops at the
      first segment still inside the window even when later ones have aged out,
      refuses to empty the archive entirely, and `Manifest::forget` rejects
      anything that is not currently the oldest — belt and braces, because the
      decision and the deletion are made in different places.

      Age is measured from when the round trip was *proved*, not from when the
      write happened. Those differ by however long the archive was unreachable,
      and using the write time would let an outage silently shorten the window a
      customer is paying for.

      `Manifest::horizon` reports the earliest restorable point, so a caller
      asking to restore below it is told beforehand rather than after the
      restore fails.
- [x] **Correct `specs/03` §5** and the README. Both now state the total order,
      name the real gap as read-modify-write, and stop apologising for shards
      that do not exist. The README's section is retitled "What ThetaBase
      guarantees, and what it does not", because listing only the limits was
      selling the product short while still being wrong about them.

### Deliberately excluded

- **Cross-branch transactions.** A branch is a divergent timeline; a transaction
  spanning two has no meaning that merge does not express better.
- **LLM-mediated conflict resolution.** Never, and not for want of capability:
  `CLAUDE.md` invariant 5 exists because a model that resolves a merge silently
  picks a version of somebody's data, and no audit trail un-rings that. It is
  the claim the product is built on rather than a feature it lacks.

**Gate (`CI`)** — **Met.** `crates/thetad/tests/conditional_write.rs` runs 24
clients read-modify-writing one key and requires every increment to survive, and
`the_same_contention_without_a_precondition_loses_updates` runs the identical
loop unconditionally and requires that it *does* lose some. The second is what
makes the first evidence: without it, deleting the precondition check would let
the suite keep passing.

Retention is asserted against what a restore can reach — `horizon` after expiry,
and `gaps()` staying empty — rather than against configuration.

Both SDKs exercise the new path: `make conformance` is now 22 wire cases, up
from 18, driven through the WASM core against a live `thetad`.

**Two bugs found on the way, both worth recording:**

- **`version_id` on a `get` was the branch's commit counter**, in a field named
  for the row. Any client comparing it across two reads of one key was comparing
  the wrong thing, and it moved whenever an unrelated key was written. A
  conditional write built on it would have failed for reasons having nothing to
  do with the row in hand.
- **The Python code generator emitted a name collision on the first union in the
  schema.** It hardcoded `kind`/`value` for a union's tag and payload, and
  `value` is the most common field name in the wire schema — so `PutIfRequest`
  got two `value` annotations, which silently reordered the dataclass fields and
  made the whole generated module fail to import. Both bindings now name a union
  after its group (`expect_kind`/`expect_value`, matching TypeScript's
  `expect: PutIfRequestBody`), which is collision-free by construction because
  capnp already forbids two fields of one struct sharing a name.

  Nothing outside the generated file consumed the old names, so this cost
  nothing to fix — but it would have been a much larger problem discovered
  later, since every future union would have hit it.

**Depends on:** M1, M2, M10.

---

## M11 — Security review

- [ ] Independent penetration test of token minting, scoping, and revocation
      propagation. **Scope written and ready to send: `docs/EXTERNAL-REVIEWS.md`
      review 1.**
- [ ] Cross-project isolation review at the process/VM boundary. **Scope
      written: `docs/EXTERNAL-REVIEWS.md` review 2.** The one worth reading
      first — it is the strongest isolation claim in the product and the one
      with the least in-repo evidence, because it is a deployment property
      rather than a code path.
- [x] Per-project encryption keys at rest; no key shared across projects, even
      within one org. Delivered by SEC-2 — see that entry for the design and for
      what it deliberately leaves open (customer-managed keys, which need a
      rotation story for data already sealed).
- [x] **Optional passkey step-up for org admins** — the long-lived identity
      token is the highest-value target in the system.

      **Step-up rather than 2FA at login**, because of that sentence. If the
      token is what an attacker wants, the attacker who matters is the one who
      *has* it — and a factor checked at login does nothing to them, because the
      login already happened. A `second_factor_verified` flag inside the token
      would make the token as good as the key for its whole lifetime, which is
      precisely the property under attack. So the check happens at the moment of
      the act: deleting a project, changing who may do what, moving money, and
      approving ThetaBase reading your data each need an assertion a token thief
      cannot produce.

      An assertion buys five minutes rather than one call. An admin doing a
      session of administration taps once, not once per click — a control that
      makes routine work painful gets a policy exception written for it, which
      is worse than not having it.

      **WebAuthn/passkeys, and deliberately not TOTP.** "Hardware key" was the
      wrong framing: a passkey is Touch ID, Windows Hello, or Android
      biometrics, so the authenticator is the device the user already carries
      and the adoption argument for TOTP disappears. What remains against TOTP
      is three things — a six-digit code is relayable in real time by a phishing
      proxy where an origin-bound assertion is not, a code can be read aloud to
      whoever asks, and TOTP would put a **shared secret in the Control Plane's
      database**. That last is SEC-1's own subject: a database read would leak
      every admin's seed and turn a Control Plane compromise into a permanent
      2FA bypass. Only public keys are stored here.

      **No TOTP fallback**, and that is a decision rather than an omission. An
      account enrolled in both is only as strong as the weaker factor, because
      that is the one an attacker phishes.

      Off unless an org turns it on, and off is a real answer: a solo user with
      no second device must not be locked out of their own project. Reading is
      outside the recommended set — a prompt with no threat behind it is how the
      whole feature gets switched off.

      **Still open:** this is WebAuthn-*shaped* rather than a full WebAuthn
      verifier. The assertion is an origin-bound, single-use ed25519 signature,
      which gets the properties above; attestation parsing, the UV/UP flags, and
      the signature-counter check that detects a cloned authenticator are not
      there. Recorded in `second_factor.rs` rather than implied, and the path to
      real passkeys is completing that verifier.
- [ ] Independent adversarial review of the Safety Layer. **Scope written:
      `docs/EXTERNAL-REVIEWS.md` review 3.** Not a security review of code but
      an adversarial review of a decision procedure, which needs a different
      reviewer — someone who will think like a prompt-injection researcher and a
      DBA at once, and who writes attacks rather than running a scanner.

**Gate (`manual`)** — `specs/04` §7 and `specs/08` §5. No finding above the
agreed severity threshold left unresolved. **Blocks any paid or production-tier
launch.**

**Depends on:** M4, M5.

---

## M11.5 — Documentation, demo, and the public site

Everything that explains ThetaBase to someone who has not read the specs. Its own
milestone because it is real work with its own failure mode, and because a
product whose central claim is "an agent cannot quietly destroy your data"
cannot afford marketing copy that overstates what the gates actually do.

**The rule for every page here: no claim ships that a gate does not prove.**
`specs/03` §3-4 states guarantees precisely, including what ThetaBase does *not*
promise — bounded staleness within a branch rather than linearizability, no
cross-region multi-master in v1, a breaker that reports rather than enforces at
the billing layer. Those limits go on the site, not just in the specs. A user
who discovers a caveat after their data is in is a user who was mis-sold.

- [x] **Design language and site concept.** Built and reviewed:
      `ThetaBase_design_concept` (Next.js, Tailwind, Framer Motion, a WebGL hero).

      The idea that makes it the right concept rather than a nice one: **"What
      ThetaBase does not claim" is a first-class section**, numbered, struck
      through, given the same visual weight as the gates. Most products bury
      that page. Making it a destination is the design expressing the thing the
      whole codebase is organised around — and it is the single most credible
      thing a technical buyer will see.

      The hero states the thesis in five words and then immediately says what it
      is *not* ("Not 'talk to your database in English'"), which pre-empts the
      wrong mental model every visitor arrives with.

      The interactive labs — safety, breaker, branch — show rather than tell,
      which is the only way to demonstrate a product whose claims are all
      behavioural and whose best feature is invisible when it works. They are
      the same argument as the demo item below, and should share its content.

      Accessibility was handled rather than skipped: `prefers-reduced-motion` is
      respected in the preloader, the smooth-scroll, the text scramble and the
      WebGL scene.

- [x] **The claims registry, and the gate that keeps it honest.**
      `docs/claims.toml` plus `crates/theta-claims`, wired in as `make claims`.

      This milestone's rule — *no claim ships that a gate does not prove* — was
      a review step, and review steps fail silently. The drift note below is the
      proof: two of the design concept's five non-claims were wrong within a day
      of M10.5 landing, and nobody was careless. The specs moved and the copy did
      not, because nothing connected them.

      **How a claim is pinned.** Generating the site from the specs does not
      work: `specs/03` §3.1 is written for someone implementing a database and a
      landing page is not. So the registry holds the *site's* wording and pins it
      with a `spec_quote` — words that must still appear **verbatim** in the
      cited section. Reword the spec and the pin stops matching, the build goes
      red, and whoever changed the spec is shown the sentence on the page that
      depended on it. Applied to the two stale entries, this fails on the commit
      that made them stale rather than on the day a customer notices.

      Also checked: the cited spec file and heading resolve (a renumbered spec
      silently unpins every claim citing it, and that citation is a link a
      technical buyer clicks); every `evidence` name is a function that really
      exists and really carries `#[test]`; the named gate is a Makefile target
      **and** a prerequisite of `gates`; and no retired wording has come back —
      because copy gets reused, and the likeliest source of a false claim on a
      new page is an old page.

      **`planted.rs` is the half that matters.** Seventeen tests feed the checker
      each failure it exists to catch and require it to be reported. A checker
      whose section parser silently matched the whole file would pass the
      registry suite exactly as loudly as a correct one — the same argument
      `conditional_write.rs` makes by running the contended counter *without* a
      precondition and requiring updates to be lost.

      **It found two things on the day it was written.** `conditional_write.rs`
      and `encryption_at_rest.rs` proved central claims, passed, and were in no
      gate's recipe — so was `recorded_positives.rs`. All three are now wired in.
      And the hash-chain guarantee had no home in any spec: `specs/04` §5 said
      the log was immutable and never said the chain was *verified*, which is the
      distinction SEC-8 existed to close. §5 now states it, along with what it
      does not detect — an edit to the newest entry, which nothing inside the log
      commits to.

      **What it deliberately does not check:** that a `statement` is a fair
      paraphrase. No mechanical check can do that, and pretending otherwise would
      be the overclaim this crate exists to prevent. What the pin buys is
      narrower and real — a statement cannot go stale *without someone being
      told*. Judging the paraphrase on the day it is written is still the manual
      half of the gate.

- [ ] **IP protection before anything ships, licence chosen after.**
      Blocks every other item in this milestone, because every one of them is a
      public disclosure.

      **The ordering, and why it is not reversible.** A public disclosure ends
      patent rights outside the United States on the day it happens — the US
      gives twelve months of grace, Europe and most of the world apply absolute
      novelty and give none. Publishing does not start a clock; it closes a
      door, for every jurisdiction but one. And disclosure is not only "make the
      repo public": it is `cargo publish`, `npm publish`, `gem push`, a public
      demo of the mechanism, or a blog post describing how the gate works.

      **The licence cannot be decided first, because it changes what is left to
      protect.** Apache-2.0 §3 contains an express patent grant — publishing the
      engine under it hands every recipient a royalty-free licence to any patent
      reading on what they received. Choosing it and *then* filing would be
      filing on something already given away.

      **This was already live and nobody had decided it.** Every crate in the
      workspace declared `Apache-2.0`, and thirteen were publishable to
      crates.io; four SDK manifests declared it too. The workspace inherited a
      default and the SDKs copied each other. Both are now closed, and
      `crates/theta-claims/tests/release_guard.rs` keeps them closed: while
      `docs/DISTRIBUTION.md` records the state as `ip-protection-pending`, the
      build fails if any package can be published or claims a licence.

      `publish` in `[workspace.package]` is **not** auto-inherited, which is
      exactly why this is a test rather than a comment — twelve crates looked
      covered by the workspace manifest and were publishable in fact.

      **Order:** trademark clearance on the name → decide what is filed → file
      it while the repo is private → choose the licence → flip the state line
      and release. Steps one to three get harder after a release; four and five
      do not. `docs/DISTRIBUTION.md` §0 carries the reasoning and §3 item 6 the
      revised patent position, which the competitive survey inverted.

      **Not legal advice and not written by a lawyer.** Get an IP attorney
      before anything is filed.

- [x] **A comparative branch benchmark, published with its harness.** Run
      2026-08-27: `bench/comparative/branch_compare.py`, results in
      `bench/comparative/results/`, written up in `docs/COMPETITION.md` §4a.

      **The supported claim is narrow and it is the only one:** ThetaBase creates
      branches 3.7× faster than Dolt and 19× faster than PostgreSQL, and does
      not scale with parent size.

      **Two things did not go our way and are in the write-up.** BranchBench's
      read-depth degradation *did not reproduce* — at depth 50 on 20,000 rows
      nothing moved, ours included, so "reads do not degrade with depth" is
      supported by architecture and by nobody's measurement. And ThetaBase loses
      two of four axes: PostgreSQL reads faster at the root and writes three
      times faster.

      **It found a bug worth more than the numbers.** Forking fifty deep
      panicked `thetad`, and behind the assertion was a release-mode failure: a
      store that branched, checkpointed and restarted refused to reopen from its
      snapshot, because the log position was derived by summing per-branch
      counters that double-count inherited history. Fixed; regression test is
      `a_branched_store_reopens_from_its_checkpoint`.

      *(Original entry, kept because it is what the harness was built to:)*
      `docs/COMPETITION.md` §4 measured ThetaBase's branching on its own surface
      and found the shape of the result: reads flat with depth, forks that scale
      with the parent. **Those numbers cannot go in marketing as they stand**,
      and the reason is worth writing down because it is the trap every vendor
      benchmark falls into.

      The read figure is 42 nanoseconds. That is an in-memory map lookup, not a
      database read over a socket through a planner — the comparable number for
      a real `get` is `specs/09`'s 5ms p50, five orders of magnitude away. Any
      table putting 42ns next to Neon's published read latency would be
      comparing our fastest internal operation to their end-to-end one. The
      measurement supports a claim about *the branching architecture adding
      nothing to reads*. It supports nothing whatsoever about ThetaBase being fast.

      **What a defensible comparison needs:**

      1. **The same boundary on every system** — client SDK call to result, over
         a socket, against a running server. Not our in-process number against
         their over-the-wire one.
      2. **The same host, at the same time, on the same data.** Our runs against
         their runs, never against their published figures.
      3. **A workload expressible in both.** This is the hard part, and it has a
         clean answer: BranchBench cannot run here because it needs
         `execute_sql(str)`, but the *agentic* workload is single-table
         key-value — `get(k)`, `put(k, v)`, branch, read at depth. All of that
         is `SELECT v FROM kv WHERE k = $1` and an upsert on the SQL side. Hold
         the data model constant at one table, and the comparison isolates
         branching, which is the only axis being claimed.
      4. **Pre-register the claim and what would falsify it**, before running.
         Otherwise the configuration where we win is the one that gets shipped,
         and every reader can smell it.
      5. **Publish the harness and the raw data**, not a table. M11.5's rule
         already says a comparative claim must resolve to a named gate or a
         published benchmark — so the harness is a deliverable.
      6. **Report where we lose.** Fork cost off a large parent is a real loss
         to copy-on-write systems and it goes in the same table. A vendor
         benchmark showing no losses is discounted to zero by exactly the reader
         worth convincing.

      **Systems:** Dolt and Postgres locally in Docker, Neon and Xata on free
      tiers, Postgres copy-on-write as the control — the same set BranchBench
      used, minus TigerData if it needs a paid plan.

      **The claim to aim for, stated tightly:** *"On a single-table key-value
      workload, ThetaBase's read latency is unchanged at branch depth 500;
      [systems] degrade by N×. ThetaBase's branch creation is slower off a large
      parent, and here is that number."* Specific, falsifiable, and it names the
      loss.

      **What must never be claimed from this:** general query performance,
      throughput, TPC-H, or "the fastest branching database". The measurement
      does not reach any of them.

      Lands as `make bench-comparative`, outside `gates` for the same reason
      `archive-live` is — it needs external services.

- [ ] **Zero-friction start, for agents and for the humans driving them.**
      ThetaBase's user is usually an agent, or a person with an agent open in
      another window. A quickstart that assumes neither is a quickstart written
      for the wrong decade. Four pieces, in the order a visitor meets them:

      **`llms.txt` and `llms-full.txt` — done.** `theta-claims emit-llms` and
      `emit-llms-full`, generated from `docs/claims.toml` and `docs/specs/`.
      The short file leads with what ThetaBase is *not*, because the wrong mental
      model is what a generating agent will otherwise act on; the full file
      inlines every spec rather than linking, because `docs/specs/` is not
      public and may never be. Two tests guard the parts that would go wrong
      unnoticed: the correction preceding the guarantees, and every non-claim
      reaching the file.

      *(Original entry:)* The site's own content, laid out for a model rather
      than a crawler: what ThetaBase is, the guarantees, the
      non-claims, the wire protocol, the SDK surface. It matters more here than
      for most products because the mental model a model arrives with is *wrong*
      — "talk to your database in English" is exactly what ThetaBase is not, and an
      agent that guesses will write against an API that does not exist. Generate
      it from `docs/claims.toml` and the specs via `theta-claims emit`, so it
      falls under the same gate as everything else. A hand-maintained `llms.txt`
      is a second copy of the claims, which is the problem this milestone is
      about.

      **One-click prompt copy.** A button that copies a ready-made prompt into
      the visitor's agent — "here is ThetaBase, here is my key, create a project
      and make the first gated schema change." The value is that the first thing
      the agent does is hit the Safety Layer, which is the product. Copy has to
      be safe to paste: no credential baked into a string that ends up in a chat
      log, so the prompt names an env var and the flow mints the key separately.

      **MCP server, ideally one-click install.** An MCP server is how an agent
      gets tools rather than guesses at HTTP, and it is the natural home for the
      typed surface: `put`, `putIf`, typed `query`, `branch`, `propose`, and the
      Safety Layer's review objects as first-class results. One-click where the
      client supports it (Claude Code, Claude Desktop, Cursor), a copyable JSON
      block where it does not. **The Safety Layer is what makes this safe to
      offer** — handing an agent database tools is exactly the scenario `specs/07`
      was written for, and the MCP server must go through the same classification
      path rather than around it. No tool in it may be capable of an unreviewed
      destructive change; that is a test, not a convention.

      **Signup that does not block the first success.** The 100 GB AT-1 free tier
      below means archiving works with no card, and the quickstart should reach a
      first gated schema change before it asks for anything. A signup wall in
      front of the demo is a wall in front of the only thing that explains the
      product.

      Depends on M11.6 only for the SDK the prompt names; the MCP server sits on
      the wire protocol and can be built as soon as M6 is done.

- [ ] **AT-1 on the site: disclosure first, link second.** ThetaBase's cold
      archive runs on AT-1 ([tinyfiles.io](https://tinyfiles.io)), which is
      owned by ThetaBase's founder. That is worth saying on the site, and the
      order matters.

      **Why it belongs there at all:** it is already true and load-bearing
      rather than a cross-promotion bolted on. `theta-archive` depends on AT-1
      for the reasons M10 records — byte-identical decompression, SHA-256 on the
      container so rot is detectable without a full restore, queryable in place
      over HTTP Range — and the measured result is a real number worth showing:
      **561,670 bytes to 10,576, a ratio of 0.019, with a verified byte-identical
      restore.** AT-1's `worm` command verifies append-only journals, which is
      exactly what a ThetaBase log is. Two products whose properties compose is a
      good story and an honest one.

      **Why the disclosure has to come first.** A technical buyer will
      immediately think: *my database's backups depend on a service run by the
      same person who runs the database.* That is a sharper question than the
      one they would otherwise ask, and it **concentrates** vendor risk rather
      than diversifying it. Discovering the common ownership themselves, after
      the fact, is precisely the mis-sold failure this milestone's rule exists
      to prevent. It goes in the same voice as the non-claims section, which is
      the part of the site best suited to carrying it.

      **The answer sits next to the question, not three pages away.** Encoding
      requires a connected account; **decoding, querying and verifying never
      do.** An archive stays readable with no account, no billing relationship,
      and no vendor — which is what turns a concentration risk into a
      non-issue. Stated at the point AT-1 is named, or it does not do its job.

      **No longer blocked: M10's S3-compatible backend shipped.**
      `theta-archive/src/s3.rs` is a real second implementation of
      `ArchiveBackend`, exercised against MinIO by the shared backend contract
      that `At1Archive` also runs. Until it existed the site could not call the
      archive pluggable — a trait with one non-fake implementation is a seam, not
      a choice, and "bring your own object storage" would have been an overclaim
      of exactly the SEC-2 kind. `[[retired]] pluggable-archive-before-a-second-backend`
      in `docs/claims.toml` records that, and it is the one retirement that is
      conditional rather than permanent: the phrase is usable once a page names
      the second backend next to AT-1.

      It is also what makes this whole section work. While AT-1 was the only
      option, "AT-1 is recommended" was not a recommendation — a reader cannot
      weigh a suggestion against an alternative that does not exist. Now they
      can, which is the difference between a disclosure and an excuse.

      **Say the free tier plainly: the first 100 GB on AT-1 is free, for every
      customer.** It is the strongest thing on the page for a new user, because
      it means the default costs them nothing and archiving works before they
      have opened a cloud account or attached a card. It also defuses the
      conflict better than any wording could — a founder recommending their own
      *paid* product as the default has something to explain, and recommending
      the free-to-100-GB one alongside a supported alternative does not.

      **Do not claim the compression advantage.** It was measured against the
      S3 backend on the same log-shaped data and it is not there: AT-1 gets
      0.019 and zstd gets 0.020. Two real differences remain and both are worth
      stating — `worm` verifies append-only journals, which is exactly what a
      ThetaBase log is, and query-in-place over HTTP Range means a point-in-time
      restore does not start by pulling everything.

      **Carried into the build:**
      - The copy is mock and some of it is now *wrong*, not merely unfinished —
        see the drift note under the gate below.
      - The site cites spec paths (`docs/specs/03-...§4`) as provenance. That is
        excellent for credibility and a dead link until the repo is public,
        which is a `DISTRIBUTION.md` decision rather than a web one.
      - `.env` is not in that repo's `.gitignore`.
- [x] **Competitive survey, and the benchmark it produced.**
      `docs/COMPETITION.md`, surveyed August 2026 and dated because it will go
      stale — a competitive claim that has quietly stopped being true is worse
      than none, which is the argument `claims.toml` already makes about our own
      guarantees.

      **The category is more crowded than it looks, and in the places that
      matter least.** Branching has seven credible systems, several with
      Postgres compatibility that ThetaBase does not have — Dolt is the closest by
      a distance and has been marketing "the database for AI agents" since May
      2026, with an MCP server and a branch-based agent review flow that is our
      shadow-branch story shipping on a wire people already speak. Bytebase
      repositioned in July 2026 as governance "for humans and agents", and
      Atlas's April 2026 policy-as-code post argues for *"a programmatic
      guarantee where changes violating the code simply cannot pass"* — which is
      our sentence, from someone else, four months earlier. **"Deterministic,
      not trust-based" is no longer a differentiator and no page may present it
      as one.**

      What nobody else says: **a control plane can be bypassed by opening a
      connection and a gate inside the engine cannot.** That is the sentence to
      lead with, and it is not currently the one the product leads with.

      **BranchBench cannot be run against ThetaBase, for a reason worth keeping.**
      Columbia's benchmark drives every backend over a PostgreSQL connection —
      its base class issues `information_schema` queries, and every CRUD and DDL
      operation calls `execute_sql(query: str)`. Writing that adapter means
      giving ThetaBase a door that takes a string and executes it, which is
      `CLAUDE.md` invariant 4. The benchmark is unmeasurable here *because of a
      property this database is built to have*.

      **So its two axes were measured on our own operations instead**
      (`crates/theta-storage/tests/branch_cost.rs`, now in the `sla` gate; the
      numbers are explicitly not comparable to theirs):

      - **Reads do not degrade with branch depth.** 1.10× at depth 500 against
        depth 0, which is host noise. BranchBench reports 5–4000× on this axis
        across PostgreSQL, DoltgreSQL, Neon, TigerData and Xata. This is now a
        pinned claim.
      - **Forking is a view copy and is not free**: ~0.6ms fixed plus
        ~0.2µs/row, so ~200ms for a million-row parent — the p99 target exactly.

      **That second number corrected the spec twice.** `specs/09`'s latency table
      said branch create was a *"copy-on-write pointer, not data copy"*; it is a
      copy, and branch creation is the only operation in that table whose cost
      scales with how much data the parent holds. Then the *first* measurement
      was itself wrong: taken while a compile was running, it read 0.76µs/row and
      said the p99 was missed by 4×. Four idle runs cluster at 0.19–0.24, and the
      target is met. **A 6× host effect on a number that was about to go in a
      spec** — the correction is recorded in `claims.toml` rather than quietly
      applied, because the lesson is about how the number was taken, not what it
      turned out to be.

      Both facts are registered as claims — the guarantee and the limit that pays
      for it, because a pointer fork would put depth back in the read path and
      the two cannot be sold separately.

- [ ] **Reference documentation**, generated from the schema and the specs where
      possible, so it drifts as little as the SDKs do. Every guarantee stated as
      precisely as `specs/03` states it.
- [ ] **Guides**: first write, first branch, first schema change through the
      gate, first merge conflict, migrating from Postgres. Written against a
      real running instance, and CI-checked — a quickstart that no longer works
      is worse than none, because it burns the first five minutes of trust.
- [ ] **The product demo.** The Safety Layer is the thing worth showing and it
      is invisible when it works, so the demo has to make an agent *try*
      something destructive and be stopped: propose a drop, watch it gated, see
      the shadow branch validate it, promote it deliberately. That is the
      product. A demo of `get` and `put` is a demo of every database.
- [ ] **Marketing pages.** Positioning against the alternative a user actually
      has today (Postgres or Supabase plus a human reviewing migrations), not
      against a strawman. The comparative numbers come from M8's benchmark and
      nowhere else.
- [ ] **SEO and technical content.** Structured data, sitemap, fast pages, and
      the honest long tail: how branch-per-PR works, why an LLM must not be on a
      database's hot path, what a blast-radius breaker catches that a permission
      model does not. Content that is true is also the content that ranks for
      people who will actually use this.
- [ ] **Pricing page** consistent with the billing surface that exists, and with
      the tier limits once the open decisions on them are answered.
- [ ] **Status page and changelog**, wired to the M10 dashboards rather than
      updated by hand.

**The drift this milestone must solve, demonstrated already.** The design
concept's "does not claim" list was written from `specs/03` and two of its five
entries went stale within a day of M10.5 landing:

- *"Global linearizability for concurrent writers to one key — not without an
  explicit transaction."* Wrong twice over now: writes to a branch **are**
  totally ordered, and the escape hatch is `putIf`, not a transaction.
- *"Infinite time-travel — bounded by log retention. Bounded means bounded."*
  Retention is now a policy whose default is `Forever`, so this undersells it.

Nobody was careless; the specs moved. That is exactly why the gate below is
mechanical rather than a review step. **Marketing copy that quotes a guarantee
must be generated from, or CI-checked against, the spec that states it** — the
same argument `make sdk-check` makes for the bindings, applied to prose.

**Gate (`CI` + `manual`)** — `make claims`, which is the mechanical half and is
built: every guarantee and non-guarantee the site may state is pinned to a
verbatim sentence in a spec and to the tests that prove it, so a spec change
fails the build rather than silently making the site a lie. Then, as the pages
land: `theta-claims copy <site>` over the built content, so a page citing a claim
nobody pinned, repeating a retired wording, or quietly dropping a limitation
fails too. Every code sample and quickstart runs green in CI against a live
instance. Every performance or comparison claim resolves to a named gate or to
M8's published benchmark, checked by a link CI verifies rather than by a person
remembering.

Manual, and not automatable: someone who has not worked on ThetaBase follows the
quickstart and reaches a first gated schema change without asking for help — and
someone reads each `statement` in the registry against the spec it cites and
judges it a fair paraphrase. The pin guarantees a statement cannot go stale
unnoticed; it cannot tell you the statement was right the day it was written.

**Depends on:** M8 for anything comparative, M10 for status and dashboards.
The demo can be built as soon as M5 is done, and the earlier the better — it is
also the best test of whether the Safety Layer's output is legible to someone
who did not write it.

---

---

## M11.6 — SDK expansion

- [x] **Go** — `sdk/go`. All three pieces: generated wire bindings from
      `theta.capnp`, a typed query builder, and a transport binding onto the
      WASM core. Conformant: `make conformance` now runs three bindings and
      requires byte-identical output.

      **wazero** rather than wasmtime-go or wasmer-go, because it is pure Go
      with no cgo — `go build` works on anything Go targets and installing the
      SDK does not drag in a C toolchain. The Scribe core imports nothing, so
      none of the host-function machinery the alternatives offer is needed.

      **What the schedule note above got right, measured.** The generated wire
      types were nearly free — one new emitter beside the existing two. The
      builder and the transport were the real work, and they were most of the
      time. Nothing about that changes for the next five.

      **The gate earned its keep on the first run.** `clone` used
      `append([]string(nil), ...)`, which yields nil rather than an empty slice
      for an empty source; a nil slice marshals to `null`, the core reads
      `columns` as a sequence and refuses null, and every query without an
      explicit projection — `SELECT *`, the common case — failed to render. The
      Go unit tests passed. The binding compiled. Only running the same cases
      through three bindings and diffing found it. It is now a unit test as
      well, because a bug that can only be caught by the expensive gate is a bug
      that gets caught late.

      **Two things it found that were not about Go.** The generated Go collided
      with a hand-written `Welcome` in the transport — a second copy of a wire
      type, which is exactly the drift the generated half exists to prevent, and
      the compiler refused it before it could be believed. And
      `cargo test -p theta-codegen` had a red assertion since M10.5: `sdk-check`
      typechecked the *bindings* and never ran the generator's claims about
      them. It runs them now.

      **`gofmt` reproduced rather than invoked.** The emitter lays out its own
      column alignment, so its output is byte-identical to what `gofmt` writes.
      Shelling out to `gofmt` after generation would make the committed file
      differ from the generator's output and the drift check would report a
      change on every run; making the check call `gofmt` would make its answer
      depend on whether the machine has Go installed.

- [x] **Rust** — `sdk/rust`, a workspace member so `cargo clippy --workspace`
      covers it. Two of the three pieces already existed and writing second
      copies would have been the exact drift the other bindings need a gate to
      catch: `theta_proto::wire` is already generated from `theta.capnp`, and
      `theta-scribe` was already a working client. So the builder is the new
      part, and `Theta` wraps the rest.

      **No WASM runtime.** `theta-scribe-wasm` has always declared
      `crate-type = ["cdylib", "rlib"]`; the crate is named for how it is
      usually compiled, not for what it holds. The cost is precise and worth
      stating: this SDK does not exercise `abi.rs`, the byte-buffer boundary,
      which the other three exercise three times over.

      **The client surface is real here first** — `get`, `put`, `query`,
      `explain`, `propose`, `apply`, branches, `status`. Not special treatment:
      `theta-scribe` is Rust, and the other bindings reach the protocol through
      a core that deliberately contains no sockets. That is also why this SDK
      found what it found — it is the first binding that actually *called* the
      server rather than only encoding for it.

      **Three bugs, all of which the conformance gate was passing.**

      1. **No write from any SDK worked.** The core sent the host's plain JSON
         as a row where the server decodes `theta_core::Value`, whose encoding
         is adjacently tagged because a value's type is never inferred after the
         fact. Every `put` came back "missing field `kind`".
      2. **No parameterised query worked**, the same way — under a case named
         *"a SQL literal is bound and never becomes syntax"*.
      3. **`audit` could not be called at all.** `#[serde(rename_all)]` on a
         tagged enum renames the *variants*; renaming their fields needs
         `rename_all_fields`. So `minRisk` was rejected as unknown while the
         core's own error message named it in camelCase.

      Fixed once, in the core, via `Value::from_json` and `Value::to_json` in
      `theta-core` — so a host writes `{"email": ...}`, and reads back
      `{"email": ...}` rather than the tagged form it never wrote. `Bytes` and
      `Timestamp` do not survive that round trip; that is recorded as a test
      that goes red if someone "fixes" it by guessing, because guessing is the
      coercion `specs/03` §2.2 forbids.

      **Why the gate was green through all three.** It only ever asked whether
      the bindings *agreed*, and three identically broken bindings agree
      perfectly. Every case in `cases.json` now carries an `expect` — a
      response kind plus any dotted paths that must hold — and a case's name is
      its claim. *"dropping a populated column is gated rather than applied"*
      now asserts `gate == "Confirm"`, which is the thing it is named after,
      rather than "the server answered".

- [x] **Java/Kotlin** — `sdk/java`, a Maven module. Records, static factories
      and nothing Java-specific in the way, so Kotlin, Scala and Groovy use it
      unchanged; there is no separate Kotlin binding to keep in step.

      **Chicory** rather than Wasmtime's JNI bindings: pure Java, no native
      library to ship per platform, so the SDK is a jar that works on any JVM.
      The Scribe core imports nothing, so none of the WASI machinery the
      alternatives offer is needed — the same reason Go uses wazero.

      **One `Generated.java`, not fifty files.** Java's usual shape is one file
      per public type, which here is fifty-odd files a generator owns — and a
      directory a generator owns is a directory somebody edits by hand, because
      nothing about a lone `ChangeDiff.java` says it is machine output.

      **Records, and `@JsonProperty` on every component.** A record cannot
      acquire a setter, and a wire type with a setter is a wire type somebody
      mutates after validation. The annotation is usually redundant — Java and
      the wire are both camelCase — and usually is not always: `record
      Foo(String default)` is a syntax error, so a keyword-escaped component
      would otherwise serialise under the wrong name.

      **The Windows encoding bug, for the second time.** Java's `System.out`
      encodes with the platform codepage, so one refusal message's em-dash came
      back mangled and the harness reported a disagreement over a character.
      Same shape as the Python subprocess bug the Go SDK found. The runner now
      writes UTF-8 explicitly rather than depending on how it was launched.

- [x] **C#** — `sdk/csharp`: a library, a conformance runner and an xunit test
      project, because a library with a `Main` in it is a library that ships a
      test harness.

      **The one place a ThetaBase SDK is not pure managed code.** Go has wazero
      and Java has Chicory, both pure and both shipping as one artifact. .NET
      has no mature pure-managed WebAssembly runtime, so this uses Wasmtime's
      bindings, which carry a native library per platform. The NuGet package
      ships them, so a consumer still installs one package — named here because
      it is the only asymmetry across the bindings.

      **`[JsonPropertyName]` on every property**, not a naming policy. C#
      properties are PascalCase and the wire is camelCase, so unlike Java the
      two differ everywhere. A camelCase policy would cover most of them, and
      *most* is the problem: it produces a different wrong name for
      `writeVolumeMB`, where an explicit attribute is either right or absent.

      **`[JsonStringEnumMemberName]` on every enum member**, for a sharper
      reason: without it an enum serialises as an integer, which the server does
      not read — and that failure reads as a schema mismatch rather than a
      naming one. It is what pins the target to `net9.0`; multi-targeting
      `net8.0` needs a hand-written converter over `[EnumMember]`, which is
      worth doing before publishing and not before it works.

      **Third encoding bug of the milestone.** `System.Text.Json` escapes `<`,
      `>` and `&` on the assumption the output lands in a web page, so
      `<redacted>` came back as `<redacted>`. Go's `encoding/json`
      needed the same switch thrown for the same reason. Three languages, three
      defaults chosen for a different context than this one.

- [x] **Ruby** — `sdk/ruby`. `Data.define` for the wire types, so they have no
      setters at all; `Struct` would have given writers for free, and a wire
      type with a setter is a wire type somebody mutates after validation.

      **One Ruby scoping trap, caught by a unit test.** A constant assigned
      inside a `Data.define ... do` block binds to the **enclosing lexical
      scope, not to the class**. So `WIRE = {...}` meant every generated type
      wrote to one `ThetaBase::Wire::WIRE` and the last one won — every `to_wire`
      in the file used the last type's field list, and it surfaced as a
      `NoMethodError` naming a field from an unrelated message. The emitter now
      writes `def self.wire`, and a codegen test refuses a constant so nobody
      tidies it back.

      **Names are mapped, not derived**: snake_case here, camelCase on the wire,
      and the mapping is carried because reversing `snake_case` does not survive
      `writeVolumeMB`. Accessors that would shadow an `Object` method are
      escaped — `class` is the sharp one, since a `Data` member named `class`
      shadows `Object#class` and fails somewhere else entirely.

      Like .NET and unlike Go and Java, there is no pure-Ruby WebAssembly
      runtime; the `wasmtime` gem ships precompiled native extensions, so a
      consumer still runs one `gem install`.

- [x] **Swift** — `sdk/swift`, a SwiftPM package. **WasmKit**: pure Swift, no
      native library per platform, so it builds anywhere Swift does — including
      iOS, where loading a C runtime would not be an option at all. Same
      reasoning as Go's wazero and Java's Chicory.

      **Almost no `CodingKeys`.** Swift and the wire are both camelCase, so for
      once the two agree and `Codable`'s synthesis is already right. The emitter
      writes a `CodingKeys` block only where a name had to be escaped — which
      here is nowhere, and a codegen test refuses one so that the day a schema
      adds a `default` the block appears for a reason a reader can see.

      **Backticks rather than a rename.** Swift escapes a keyword as
      `` `default` ``, so a property keeps the wire's own name where Python and
      Ruby must invent `default_` and then carry two names.

      **A `public init` per struct**, because Swift's synthesised memberwise
      initialiser is `internal` — without it a consumer outside the module could
      decode the wire types and not construct them, which makes the whole surface
      read-only for the people it is for.

      **`FramedSocket` is a protocol.** Swift runs where Foundation's socket
      story differs — a server uses NIO, an iOS app uses `Network.framework` —
      and an SDK that picked one would force it on the other.

      **Two bugs of its own.** `invoke` demanded a return value, and
      `theta_free`/`theta_free_result` return void — so every deallocation threw,
      invisibly, because the frees sit in `defer` behind `try?`. The SDK leaked
      the core's memory on every call and said nothing. And `JSONValue.object` is
      a Dictionary, so the core's key order does not survive the parse; every
      other response in the document happens to be alphabetical, which hid it
      until `{sql, params}` turned up.

      **Windows needs three things macOS and Linux do not**, all recorded in the
      SDK's README: the MSVC toolchain via `vcvars64.bat`, the Swift directories
      put *back* on PATH afterwards because `vcvars64` replaces it, and `SDKROOT`
      pointing at the platform SDK — a per-user Swift install does not set it,
      and without it `swiftc` reports "unable to load standard library".

**Before GA, not after.** An earlier draft of this roadmap put it after M12's
production quarter, on the grounds that `specs/02` §3 wants the first two SDKs
"validated in real use" first. That condition is circular: a Go team cannot
validate a Go SDK that does not exist, so waiting for real use in a language
guarantees there is never any. What the first two actually have to validate is
the *shape* — that a builder compiles to the same AST, that the WASM core
carries it, that `sdk-check` catches drift — and they have.

The cost is not evenly spread, which is worth being honest about when
scheduling it. The generated wire types are close to free: one schema, one
generator, one reading of it, and `make sdk-check` fails when a binding drifts.
The per-language work is the builder and the transport binding, and that is six
times a real piece of work rather than six times a code generation.

Against that, an SDK is how the product is reachable at all. A team evaluating
a database in a language it does not support does not file a request; it picks
something else, and the absence never shows up as feedback. That is the
asymmetry that makes this a launch item.

**Order by demand.** The list is alphabetical, not prioritised. Which languages
come first should follow who is actually asking, and that signal exists before
GA — from the demo, the docs, and the pilot migrations M8 gates on.

**Gate (`CI`)** — `make sdk-check`, unchanged in shape and much larger in
reach: every generated binding compiles and matches the schema, plus the
conformance harness proving each SDK and a live `thetad` agree byte for byte. A
schema change that was not regenerated fails there rather than shipping, because
an SDK describing a protocol the server no longer speaks is worse than no SDK —
it looks like it works.

**Eight bindings now run the same 22 wire cases against a live thetad and the
same 5 typed-query cases, and every case does what its name says.** That last
clause is the part M11.6 added: the harness used to ask only whether the
bindings *agreed*, and three identically broken bindings agree perfectly — which
is how three real bugs (no write worked, no parameterised query worked, `audit`
could not be called) sat green in it. Each case now carries an `expect`.

**What every language taught, in one place.** The generated wire types were
close to free in all seven emitters, as the note above predicted; the builder
and the transport were the work, also as predicted. What was not predicted is
that the interesting bugs were never about the protocol. They were about each
language's defaults: a Go nil slice marshalling to `null`, a Ruby constant
binding to the enclosing scope rather than the class, a Swift helper demanding a
return value from a void export, and *four separate* encoding defaults — Java's
`System.out`, C#'s HTML escaping, Ruby's stdout, Python's subprocess pipes —
each chosen for a context that is not this one, each surfacing as a
"disagreement" over a single em-dash.

**Depends on:** M6.

## M12 — GA readiness

- [ ] All gates M1–M11.6 green simultaneously on one commit, M8.5 included.
- [ ] **IP protection filed and the licence chosen**, per M11.5 and
      `docs/DISTRIBUTION.md` §0. Listed here as well as there because GA *is*
      the disclosure: it is the point at which the repository, the packages, or
      both stop being private, and that is a one-way door for patent rights
      outside the US. `release_guard.rs` fails the build until the state line in
      `DISTRIBUTION.md` moves, so this cannot be forgotten — only decided.
- [ ] One full production quarter of real telemetry before any external SLA
      commitment. Availability numbers get defined from that data — not guessed
      pre-launch (`specs/09` §4, §6).
- [ ] Self-hostable/embeddable core: minimal but genuinely runnable.
- [ ] Documentation: guarantees stated as precisely as `specs/03` §3-4 states
      them, including what ThetaBase does *not* claim.

**Success criteria** (`specs/05` §6): consistency suite passes independent
verification; Safety Layer passes the adversarial corpus with zero unreviewed
destructive changes reaching a protected branch; p50/p99 targets met with zero
LLM calls on the hot path; at least one real migration completed via `eject`,
verified by the adversarial schema-semantics suite that gates M8.

---

---

# v2 — after GA

Rationale, and the argument for why "best at everything" is the wrong target,
in [`ROADMAP-V2.md`](ROADMAP-V2.md). The thesis: **prove what happened, undo it,
refuse it in advance.** Listed here so there is one queue rather than two.

**v3 is in [`ROADMAP-V3.md`](ROADMAP-V3.md)**, and it is a different bet: v1 and
v2 both assume a human is somewhere in the loop, and that assumption is the
ceiling. v3 asks what a database owes a fleet of agents operating faster than
any human reviews — without relaxing invariant 2 or invariant 5, which is the
version of it that would be a different product wearing this one's name.

Ten milestones, M17–M26, in two tracks. **M17–M20 and M23–M24 are product** —
review as a budget rather than a queue, multi-agent merge without a coordinator,
multi-region, and time as a query dimension. **M25–M26 are research** and are
budgeted as research: a machine-checked consistency model, exhaustive
verification of the classifier, proof-carrying migrations, verifiable query
results, and provable in-process isolation.

Nothing below starts before M8.5. A database that cannot restart without
invalidating every session does not get to describe itself as the best in the
world.

---

## M13 — Exploit the log

Every entry already carries `prev_hash` and every author is already recorded.
v1 writes both and throws them away at read time. This milestone is one property
with several surfaces, and it is the cheapest extraordinary thing available.

- [ ] `AS OF` queries — `SELECT ... AS OF <commit | timestamp | branch>`. The
      view is a fold over the log; folding to a different bound is the same
      operation. Needs incremental snapshots to stay inside the SLA.
- [ ] `theta revert <commit>` as a verb: a *new* commit that inverts an old one,
      reviewed by the Safety Layer like any other change. This is what changes
      the risk calculus of letting an agent write at all — not "we have backups"
      but "any change is one command from undone, and the undo is auditable".
- [ ] Branch from any point in history, not only from a head.
- [ ] Agent session replay: re-run one session's writes against a branch. A
      black-box recorder for AI systems, and the answer to "what exactly did it
      do, in order", which no database answers today.
- [ ] Per-cell provenance as queryable pseudo-columns — which agent, which
      session, which commit, which human approved it.

**Gate** — a property test: for any commit `c`, `AS OF c` equals a full replay
to `c`. The incremental path and the ground truth must agree, or time travel is
a different database rather than an earlier one.

**Depends on:** M8.5.

---

## M14 — Least privilege for agents

The largest unclaimed security ground in the market. A token scoped to a whole
project was right when the caller was an application and is far too coarse when
it is an agent.

- [ ] Capability tokens: scope to tables, columns, operations, a row budget and
      a wall-clock lifetime. "Read `orders`, write `orders.status`, at most 500
      rows, for ten minutes."
- [ ] Attenuating delegation: an agent handing work to a sub-agent mints a
      **narrower** token and can never mint a wider one. Multi-agent systems are
      already here and every one of them currently shares one credential.
- [ ] Per-agent blast-radius budgets. The breaker is per project today, so one
      misbehaving agent trips the team's.
- [ ] Purpose binding: the token carries the task it was minted for, and the
      audit trail records intent beside action. "Why did this happen" is
      currently unanswerable from the log.

**Gate** — the adversarial corpus, extended: a token must be provably incapable
of the operation it was not granted, at the `thetad` boundary rather than by an
ACL check a payload could talk its way past.

**Depends on:** M13 (provenance), M4.

---

## M15 — Reach

The release that removes the reasons not to adopt.

- [ ] **Postgres wire protocol, reads.** The largest single item on this page
      and the honest answer to "what have we missed". Adopting ThetaBase today
      means giving up every BI tool, ORM, migration tool and admin GUI, and that
      will kill more deals than any feature comparison. It converts "rewrite
      your stack" into "change a connection string". Reads first; writes
      probably never, because writes are where the Safety Layer lives and
      pgwire has no vocabulary for "this change needs review".
- [ ] **`theta export`** — to Postgres, schema and data intact. This sounds like
      a mistake and is the opposite: the objection that kills infrastructure
      deals is lock-in, and "you can leave whenever you like, here is the
      command, it is tested in CI" defuses it permanently. Cheap on top of M8 —
      the verification pass runs in both directions unchanged.
- [ ] Embedded mode matured past M12's minimum: the on-ramp where a developer
      meets the product with no signup.

**Gate** — a real BI tool connecting over pgwire and rendering a dashboard, and
an `export` → Postgres → `eject` round trip that verifies clean.

**Depends on:** M6, M8.

---

## M16 — Proof

- [ ] Signed authorship: an agent session signs its own writes, so authorship is
      cryptographic rather than a field the server filled in.
- [ ] Merkle checkpoints, published. A customer, their auditor, or a court can
      verify history was not rewritten **without trusting us**.
- [ ] Third-party verification tool, so the claim is checkable by someone who
      does not run ThetaBase.

The claim this unlocks is one no hosted database makes: not "we log everything"
but "you can prove we did not change it, and so can we". Modest engineering,
because the chain already exists.

**Gate** — an independent verifier, built from the published format alone,
detects every tampering the adversarial corpus can produce.

**Depends on:** M13.

---

## Under consideration, not scheduled

Both are real projects that deserve their own decision rather than a slot.

- **Native embeddings.** An agent-native database that cannot store an embedding
  is an odd artefact — agents retrieve before they act, and today that means a
  second system and a second consistency story. Vectors in the same log, on the
  same branch, under the same gate, with the same provenance is coherent in a
  way bolting an index onto a relational database is not. The risk is scope:
  doing vector search adequately is worse than not doing it.
- **Multi-region.** `specs/01` §8 scopes cross-region multi-master out of v1,
  correctly. Read replicas with explicit staleness bounds are the honest answer;
  multi-master with CRDT convergence is the ambitious one, and the data model
  already supports it in a way most databases' do not.

---

## Dogfooding: does ThetaBase's platform run on ThetaBase?

Worth answering deliberately, because the honest answer is "partly", and the
line between the parts is a real engineering constraint rather than a hedge.

### The rule

**Anything needed to bring ThetaBase back up cannot live in ThetaBase.**

This is the same circularity `06-provisioning-identity-flow.md` §4 already hit
with keysets — authenticating a keyset fetch would need a keyset — and solved by
having provisioning deliver keys rather than the instance fetch them. The
recovery path has the same shape: if the org graph, the signing keys, and
instance placement live in a ThetaBase instance, then a ThetaBase bug that makes
that instance unreadable takes away the console at the exact moment it is needed
to fix the bug. A total outage with no way in is a different category of
incident from a degraded one.

### What stays off ThetaBase

The org graph, project registry, memberships and roles, identity signing keys,
instance placement, and billing. Boring, independent, well-understood storage.
These are the things you read *while* recovering.

### What should run on ThetaBase, and soon

Audit trails, metering and usage history, the platform-admin action log (M9.5),
policy version history, and support-case state. All append-heavy, all wanting
time-travel and branching, and none of them on the recovery path: if they are
unavailable the platform is degraded, not unrecoverable.

This is genuine dogfooding rather than the token kind. The platform-admin trail
in particular is a good fit — M9.5 requires it to be append-only and
un-editable by the admin, and "the log is the only source of truth" is the
engine's central property rather than a feature bolted onto it.

It is also the more credible claim publicly. "We run our audit, metering and
admin history on ThetaBase, and we deliberately keep the recovery path on
independent storage" is a story an experienced reader believes. "We run
everything on it" is one they do not, and should not.

### What has to happen first

**The Control Plane does not persist anything today.** The org graph,
memberships, token records, billing state and collected usage are all in memory;
a restart loses every one of them. Only the keyset reaches disk. Nothing depends
on this yet because nothing has run for longer than a test, but it blocks both
halves of the answer above and should be fixed before either — the question of
*which* store holds what is moot while the answer is "none of them".

- [x] Persist the Control Plane's own state — moved to **M8.5 (SEC-1)**, which
      is where a blocker on production testing belongs.
- [ ] Move the platform-admin audit trail onto ThetaBase as the first real
      dogfood, once M9.5 defines it.
- [ ] Metering history onto ThetaBase, once M10's telemetry says what it collects.

---

## Continuous tracks

Not milestones — ongoing from now until after launch.

- **Adversarial corpus growth.** Every real incident and near-miss becomes a
  corpus entry. Entries are never deleted to make the suite pass.
- **Consistency suite on every core-engine change.** Already CI-gated.
- **Quarterly independent security review** as the attack surface grows.
- **Spec drift.** `docs/specs/` is the source of truth. Code that diverges from
  a spec is a bug in one of the two, and which one is a decision to make
  deliberately, not by letting them drift apart.

---

## Open decisions

These are flagged in the specs and need answers before the milestone that
depends on them.

| Decision | Needed by | Notes |
|---|---|---|
| Does ThetaBase's own platform run on ThetaBase? | M9.5 | See **Dogfooding** below. Affects what M9.5 builds: the control plane's own storage, the audit trail platform admins cannot edit, and where usage data lives. |
| Availability target for Pro/Team tiers | M12 | `specs/09` §4 explicitly declines to guess pre-launch. Derive from M10 chaos testing plus real telemetry. |
| Auto-pause and auto-scale thresholds | M10 | Same reasoning — re-specify once usage data exists. |
| Log retention per tier (bounds time-travel/restore) | M10 | `specs/03` §4 states retention is configurable and finite; the tier values are unset. |
| Breaker default ceilings per tier | M10 | The *presets* are now calibrated against a workload corpus (`theta-safety/tests/breaker_calibration.rs`) and gated: protected 100,000 rows/60s, development 250,000. What is still open is whether Free/Pro/Team should differ from those, which needs real usage data rather than more synthetic workloads. |
