# ThetaBase

A log-structured, branchable database engine built on one operating assumption:
**most writes and schema changes will be authored by an AI agent, not typed by a
human.**

That is not a feature framing. It changes what the database has to guarantee.
Existing tools are built for human-reviewed workflows and offer no real safety
net for agent-authored, human-supervises-after-the-fact ones. Closing that gap is
what this is — not "talk to your database in English."

> **Status: pre-alpha, in active build-out.** The core engine (durable log,
> crash recovery, branching and merge), the wire protocol, the server, the
> query planner and executor, the Scribe edge runtime, and the identity stack
> — OAuth login with PKCE, per-project token signing, revocation, and the
> billing surface — are implemented and tested, as is the Safety Layer and its
> operator surface: shadow-branch validation, the audit trail, the review
> commands, and signed per-project policy. The generated SDKs and the `eject`
> migration flow are not started.
> See [`docs/ROADMAP.md`](docs/ROADMAP.md) for exactly what exists and what
> comes next. Unimplemented modules say so, in the module, with the milestone
> that delivers them.

## What makes it different

- **The Safety Layer.** Every schema change and large write batch is classified
  before it can land — rule-based and deterministic, never model-inferred, so it
  cannot be argued or prompt-injected into calling a destructive change safe. An
  irreversible, high-impact change cannot be cleared by confirmation at all; it
  has to be validated on a shadow branch and promoted.
- **A blast-radius circuit breaker.** Independent of whether an operation is
  "destructive" by type. This is what catches a runaway agent loop, where every
  individual write is fine and the aggregate is not.
- **Branch everything, for free.** Data *and* schema get git-like copy-on-write
  branches. Every agent session, PR, and preview environment gets its own.
- **No LLM on the hot path.** `get`, `put`, and typed `query` never make a model
  call. Asserted in CI structurally, not measured once and assumed.
- **Identity over credentials.** One login. Project context is resolved from a
  conversational hint, not from a dashboard visit, an API key, or a `.env` file.

## Repository layout

```
crates/
  theta-core      Log entries, content addressing, CRDTs, value/type lattice, schema ops
  theta-storage   Log store, WAL, branch registry, materialized views
  theta-query     Typed plan IR, planner, EXPLAIN, SQL-subset front end
  theta-safety    Change classification, circuit breaker, audit summaries
  theta-proto     Cap'n Proto wire schema, codegen, and the framed codec
  theta-scribe    Scribe: the edge runtime the SDKs sit on
  thetad          The per-project engine daemon and its RPC server
  theta-control   Control plane: org graph, resolution, minting, provisioning
  theta-identity  Scoped session tokens: per-project keys, signing, revocation
  theta-cli       The `theta` CLI
sdk/
  typescript      TS client surface — this is also the JavaScript SDK; it
                  compiles to plain JS, and a JS consumer imports the built
                  output and gets types as a bonus
  python          Python client surface
docs/
  specs/          The nine design specs. Source of truth.
  ROADMAP.md      Build-out to production, milestone by milestone
```

## Getting started

```sh
curl -fsSL https://thetabase.co/install.sh | sh
```

Windows: download the `.zip` from
[Releases](https://github.com/ThetaBase/thetabase/releases).

Then, in about ninety seconds, watch the Safety Layer stop something:

```sh
theta login                          # opens the browser; the hosted plane uses GitHub
theta use acme checkout --create     # resolves, provisions and returns a token
theta demo                           # seeds a table and rows to work against
theta schema propose drop-legacy-ref.json
```

That last command is the product. It proposes dropping a column five rows
depend on, and instead of doing it you get:

```
chg_8f2a
  drop column on customers.legacy_ref — 5 row(s), irreversible
  A destructive change on a protected branch needs a human.

Validated on shadow branch 2. Review it, then:
  theta schema show chg_8f2a
  theta schema promote chg_8f2a
  theta schema reject chg_8f2a --reason "..."
```

The change was applied to a **shadow branch** and validated there, so what comes
back already says what the checks found. Nothing touched `main`. An agent
holding your connection string cannot route around that, because the gate is
inside the engine rather than in a proxy in front of it.

**A change you confirm still happens.** `theta schema promote chg_8f2a` drops
the column. The gate exists to make sure a person decided — not to decide for
them.

### Building from source

```sh
make build      # build the workspace
make test       # run every test
make check      # fmt + clippy + test, exactly what CI runs
make gates      # the validation gates that block milestones
```

Requires Rust 1.90+. Node 20+ and Python 3.10+ for the SDKs; `capnp` only for
regenerating protocol bindings.

## Validation gates

Per [`docs/specs/08-test-validation-plan.md`](docs/specs/08-test-validation-plan.md),
no milestone is complete until its gate passes, and a gate that cannot be met on
schedule moves the schedule rather than the gate. Three gates are live in CI
today:

| Gate | What it proves | Where |
|---|---|---|
| Consistency | CRDT merges converge regardless of order; recovery after a crash at any byte yields a prefix of what was written; a restart reproduces the fold exactly | `crates/theta-core/tests/convergence.rs`, `crates/theta-storage/tests/` |
| Agent safety | No unreviewed destructive change reaches a protected branch, across an adversarial corpus — including prompt-injected identifiers, sustained agent loops, and migrations benign in isolation | `crates/theta-safety/tests/adversarial_corpus.rs`, `crates/thetad/tests/{safety_gate,adversarial_sequences,policy_authority}.rs` |
| Breaker calibration | The ceiling sits above every legitimate workload and below the lightest runaway, measured rather than guessed | `crates/theta-safety/tests/breaker_calibration.rs` |
| Hot path | No LLM or HTTP client exists in the dependency closure of the typed read/write path | `crates/thetad/tests/no_llm_on_hot_path.rs` |
| Wire | Every request and response round-trips unchanged; incompatible protocol versions are refused rather than downgraded | `crates/theta-proto/tests/conformance.rs`, `crates/thetad/tests/server.rs` |
| Query | A SQL literal never becomes syntax; unsupported clauses are refused rather than dropped; optimizer passes preserve meaning | `crates/theta-query/tests/` |
| SLA | Latency targets met over a real socket, with zero model calls on the plan that runs | `crates/theta-scribe/tests/sla.rs` |
| Identity | A token for one project cannot verify against another; a revocation reaches instances well inside 5s | `crates/theta-identity/`, `crates/thetad/tests/revocation.rs` |
| Conditional writes | 24 clients read-modify-write one key concurrently and no increment is lost; the same loop without a precondition provably loses some | `crates/thetad/tests/conditional_write.rs` |
| Platform authority | No tenant credential reaches any platform route; no tenant data without a recorded grant; every platform action lands in a trail with no route that can edit it | `crates/theta-control/tests/platform_isolation.rs` |
| Recovery | Every failure mode in `specs/01` §7 that CI can exercise, including that a proved segment is released and local disk actually shrinks | `make ops`, `docs/RUNBOOKS.md` |

## What ThetaBase guarantees, and what it does not

Stated plainly in both directions, because a database that overstates its
guarantees is worse than one with narrower ones — and one that *understates*
them is selling itself short while still being wrong.

**Within a branch:**

- **Writes are totally ordered.** One project is one log served by one process.
  Concurrent writes to the same key do not interleave: every one succeeds, and
  the survivor is always a value somebody wrote — never a blend, never a missing
  key.
- **Read-modify-write is safe when you ask for it.** An unconditional `put`
  always succeeds, so two clients doing get-then-put would both win and one
  update would be lost. `putIf` carries a precondition — the row is absent, or
  at exactly the version your `get` returned — evaluated in the same call that
  appends, and refused with the row's *current* version so a retry costs no
  extra round trip.
- **Multi-key transactions** are all-or-nothing within one branch.

**Across branches:** a branch is a divergent timeline. Merges are deterministic
for CRDT-typed fields and go to a human otherwise. That is the design, not a
weakness of it.

**What it does not claim:**

- Not a distributed, strictly serializable system *across projects*. There are
  no shards within a project to be ACID across; between projects there is
  deliberately nothing, because cross-project queries are architecturally
  impossible rather than access-controlled.
- No cross-branch distributed transactions in v1. A transaction spanning two
  divergent timelines has no meaning that merge does not express better.
- Time-travel and restore reach back as far as retention allows. The default
  keeps everything — the archive compresses log-shaped segments about 53× with a
  verified restore, so long history is cheap — and a shorter window expires a
  contiguous prefix, never a hole.
- **LLM-mediated merge conflict resolution: never.** Not for want of capability.
  A model that resolves a merge silently picks a version of your data, and no
  audit trail un-rings that. Non-CRDT conflicts go to a human.

See [`docs/specs/03-data-model-consistency.md`](docs/specs/03-data-model-consistency.md)
for the full statement and the tests behind each line.

## License

Apache-2.0.


## What is not in this repository, and why

This is an export. Some of the tree is not published, and files here cite
documents you will not find -- `docs/ROADMAP.md` in a test comment,
`docs/claims.toml` in the workspace manifest. Those pointers are accurate; the
documents are private. They are left as written rather than scrubbed, because a
citation rewritten to hide its target is worse than one you cannot follow.

**Not published, and not planned to be:**

- `theta-control` and `theta-assist`. The Control Plane and Query-Assist are
  proprietary. `DECISION-licence.md` fixed that before the patent was filed, and
  saying so plainly beats a repository that quietly omits them.
- The roadmaps. They describe mechanisms that are not built, and publication is
  an absolute-novelty bar outside the United States for anything in them worth
  filing. US provisional 64/151,729 covers what it covers.
- Operational material -- runbooks, deployment configuration, the account of
  which review findings are outstanding. That is a map of a service that is
  running.
- The claims register, the marketing plan, and the internal decision records.

**What is here** is the engine and its tests, all eight clients, the wire
protocol, the nine design specifications, and the benchmarks -- including the
rows where PostgreSQL beats us, which are published deliberately.
