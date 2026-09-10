# ThetaBase v2 — the roadmap after GA

v1 is a database an agent cannot quietly break. That is a good product and a
narrow one. This is what v2 has to be for the answer to "why this instead of
Postgres" to stop being a list of features and start being obvious.

---

## The thesis, and an argument against the brief

The brief was "the best database in the world, hands down, that nobody can
compete with". The first half of that is achievable. The second half, read as
*best at everything*, is not — and chasing it is the most reliable way to end up
with a mediocre general-purpose database competing against Postgres on
Postgres's terms, which is a fight nobody has won in thirty years.

What *is* achievable is being unquestionably the best at something that is about
to matter enormously, and being so far ahead on it that competing means starting
over rather than adding a feature. So:

> **ThetaBase is the database that can prove what happened, undo it, and refuse it
> in advance.**

Every theme below compounds that. Anything that does not is v3 or never.

**The one thing v1 built and never used.** Every log entry carries `prev_hash`.
The log is already a hash chain, and nothing exploits it. Time travel, undo,
tamper-evidence, session replay and per-cell provenance are all sitting in data
that is written today and thrown away at read time. That is not six features; it
is one property with six surfaces, and it is the cheapest extraordinary thing
available.

---

## Theme 1 — Time is a dimension, not a backup

Every database has point-in-time recovery, and it is a support ticket: you call
someone, wait, and get a copy of yesterday. The log makes time a *query
surface*, and that is a different product.

- **`AS OF` queries.** `SELECT ... AS OF <commit | timestamp | branch>`. The
  materialized view is a fold over the log; folding to a different point is the
  same operation with a different bound. Needs incremental snapshots to stay
  fast, which M10's segment work already gestures at.
- **`theta revert <commit>` as a first-class verb.** Not a restore — a *new
  commit* that inverts an old one, reviewable by the Safety Layer like any other
  change. This is the feature that changes the risk calculus of letting an agent
  write at all: not "we have backups" but "any change is one command from
  undone, and the undo is itself auditable".
- **Branch from any point in history.** Branching from `main` is v1. Branching
  from *`main` as it was before the agent ran* is how you debug an agent.
- **Session replay.** Every entry records the agent session that authored it.
  Replaying one session's writes against a branch is a black-box recorder for AI
  systems. When an agent does something inexplicable at 3am, the question is
  always "what exactly did it do, in order" — and no database answers that today.

**Why nobody else can follow quickly.** Retrofitting this onto a
mutate-in-place engine means rebuilding storage. It is the compounding advantage
of the log-structured bet v1 already made.

---

## Theme 2 — Provenance and proof

- **Per-cell provenance, queryable.** Which agent, which session, which commit,
  which human approved it. The log knows all of it; expose it as pseudo-columns
  so `WHERE _authored_by = 'agent:...'` works. Nobody offers this, and every
  team running agents wants it the first time something goes wrong.
- **Signed authorship.** An agent session signs its own writes, so authorship is
  cryptographic rather than a field the server filled in. Turns the audit trail
  from "our server's record" into evidence.
- **Verifiable audit: Merkle checkpoints, published.** Periodically sign the
  chain head and publish it. A customer — or their auditor, or a court — can
  then verify that history was not rewritten, *without trusting us*. This is the
  compliance moat, and it is a claim no hosted database makes: not "we log
  everything" but "you can prove we didn't change it, and so can we".

That last one is worth stating as a product line rather than a feature. Regulated
industries buy it, and the engineering is modest because the chain already
exists.

---

## Theme 3 — Least privilege for agents *(the real security gap)*

v1's token is scoped to a project and an environment. That was right for a world
where the caller is an application. It is far too coarse for a world where the
caller is an agent, and it is the largest *unclaimed* security ground in the
market.

- **Capability tokens.** Scope to tables, columns, operations, a row budget, and
  a wall-clock lifetime: *"read `orders`, write `orders.status`, at most 500
  rows, for the next 10 minutes."*
- **Attenuating delegation.** An agent handing work to a sub-agent can mint a
  **narrower** token and never a wider one. Multi-agent systems are already here
  and every one of them currently shares one credential.
- **Per-agent blast-radius budgets.** v1's circuit breaker is per project
  (`specs/07` §5). One misbehaving agent should trip its own breaker, not the
  team's.
- **Purpose binding.** A token carries the task it was minted for, and the audit
  trail records intent alongside action. "Why did this happen" is currently
  unanswerable from the log; it should not be.

This theme is where v2 is most defensible, because it requires the Safety Layer
and the token system to have been designed together — which they were, and which
a competitor bolting agent support onto an existing database cannot replicate
without touching both.

---

## Theme 4 — Prevention that explains itself

- **Write-EXPLAIN.** `EXPLAIN` for a write: what it would touch, what it would
  destroy, whether it is reversible — without doing it. v1 has shadow-branch
  validation, which is correct and heavyweight. Most of the value comes from a
  cheap answer to "what would this do", available on every write.
- **Semantic branch diff.** Not "these rows differ" but "this branch narrows a
  type, drops a constraint, and changes what `status` means". The Safety Layer's
  classifier already reasons in these terms for single changes; lift it to
  branches.
- **Policy as versioned log entries.** Safety policy should be proposed,
  diffed, reviewed and reverted exactly like schema. A policy change is a
  security change, and today it is configuration.
- **Learned CRDT suggestions.** `eject` guesses CRDT types from column names.
  A running system can do far better: it can observe actual conflicts and say
  *"these two agents collided on `inventory.count` eleven times this week; a
  Counter would have converged every one."* Evidence rather than a heuristic —
  and note it stays advisory, because invariant 2 keeps the Safety Layer
  rule-based and this is a suggestion to a human, not a classifier input.

---

## Theme 5 — The things we have not specced, ranked by how much they change the outcome

This is the honest answer to "what have we missed". Each is a real project.

### 5.1 Postgres wire compatibility *(the big one)*

**Speak the Postgres wire protocol for reads.** Every BI tool, ORM, migration
tool, admin GUI and dashboard already speaks pgwire. Today, adopting ThetaBase
means giving all of them up — and that, not any feature comparison, is what will
kill deals.

It is a large piece of work and it is the highest-leverage item on this page.
It converts "rewrite your stack" into "change a connection string", and it makes
`eject` a genuine on-ramp rather than a one-way door with nothing on the other
side. Reads first; writes probably never, because writes are where the Safety
Layer lives and pgwire has no vocabulary for "this change needs review".

### 5.2 Native embeddings and vector search

An agent-native database that cannot store an embedding is an odd artefact.
Agents retrieve before they act, and today that means a second system, a second
consistency story, and a second thing to keep in sync with the rows it describes.

Vectors in the same log, on the same branch, under the same safety gate, with the
same provenance — *"which agent wrote this embedding, and against which version
of the row"* — is coherent in a way that bolting a vector index onto a
relational database is not. The risk is scope: vector search is a serious field
with serious competitors, and doing it adequately is worse than not doing it.
Scope it to "embeddings that travel with your rows", not "compete with
purpose-built vector databases".

### 5.3 Export, not just import

`eject` brings people in. **`theta export` lets them leave** — to Postgres, with
schema and data intact.

This sounds like a mistake and is the opposite. The objection that kills
infrastructure deals is lock-in, and the answer "you can leave whenever you like,
here is the command, it is tested in CI" defuses it permanently. It also makes
the migration path bidirectional and therefore *trustworthy*, which is what
makes people willing to try it at all. Cheap to build on top of the reflection
and verification work M8 already did — the verification pass runs in both
directions unchanged.

### 5.4 Multi-region and the read-replica story

`specs/01` §8 explicitly scopes cross-region multi-master out of v1, and that
was right. It becomes a real objection the first time an enterprise asks about
latency in three regions. Read replicas with explicit staleness bounds are the
honest answer; multi-master with CRDT convergence is the ambitious one, and the
data model already supports it in a way most databases' do not.

### 5.5 A "why did this change" surface

Not a feature so much as the payoff of Themes 1–3 combined: point at any value
and get its whole story — which agent wrote it, on whose authority, under which
policy, in which commit, what it was before, and what would happen if you
reverted it.

That is the demo. It is also the thing that, once someone has seen it, makes
every other database feel like it is hiding something.

---

## What v2 deliberately does not chase

Saying this out loud is what keeps the roadmap honest:

- **OLAP throughput.** M8's benchmark already says analytical throughput against
  a mature planner "is not a comparison ThetaBase wins or should claim". Still true.
- **Being a general-purpose Postgres replacement.** The wire compatibility in
  5.1 is about *tooling reach*, not about claiming feature parity.
- **Prompt-injection defence at the agent runtime.** `specs/04` §6 scopes this
  out correctly. ThetaBase's job is that a successfully manipulated agent still
  cannot do unreviewed damage.
- **Being cheapest.** The product is trust and reversibility. Competing on price
  argues the buyer should be comparing on price.

---

## Sequencing

Ordered by leverage per unit of work, not by ambition.

**v2.0 — exploit the log.** `AS OF`, `revert`, branch-from-history, per-cell
provenance. Small relative to their impact, because the data already exists.
This is also the release that produces the demo.

**v2.1 — least privilege.** Capability tokens, attenuating delegation, per-agent
budgets. The security story that has no competitor.

**v2.2 — reach.** Postgres wire reads, `export`, embedded mode maturity. The
release that removes the reasons not to adopt.

**v2.3 — proof.** Signed authorship, Merkle checkpoints, third-party verifiable
audit. The release that opens regulated markets.

**Under consideration, not scheduled:** native embeddings (5.2), multi-region
(5.4). Both are real projects that deserve their own decision rather than a slot
on a list.

**Before any of it:** SEC-1 and SEC-2 from the security review. A database that
cannot restart without invalidating every session, and does not encrypt data at
rest, does not get to describe itself as the best in the world. Fixing those is
not v2 work — it is what makes v2 worth building on.
