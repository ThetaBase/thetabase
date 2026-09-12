# API & Wire Protocol Specification

ThetaBase v1

---

## 1. Transport

- Wire protocol: Cap'n Proto (zero-copy), framed as `[u32 length][message]`, over a persistent connection between the Scribe (edge SDK) and `thetad`.
- All requests carry a scoped session token (see Provisioning & Identity Flow Spec) — never a raw project credential the user has seen or copied.

### Encryption in transit

> **Resolution.** This section did not exist. §1 described the framing and the
> token and said nothing about encryption, so every SDK implemented exactly
> what was written — a plain socket — and no gate could catch the absence of a
> property nothing had claimed. The clients could not reach a provisioned
> instance at all, because a shared address is routed by the name in the TLS
> handshake; the symptom was a hang. The rule below is what they now implement,
> and it is here so the next binding cannot make the same choice for the same
> reason.

**Every connection to a hosted instance is TLS.**
**A client never falls back to plaintext.**
A session token authorises reads and writes on a customer's database; putting
one on an unencrypted socket across the public internet is not a trade-off
this product offers.

- **The transport is chosen from the address.** Port `443` means TLS. That is
  the port the platform's proxy listens on and the port the Control Plane hands
  out. It is the only port in this product's vocabulary that
  implies a terminator in front of `thetad`, which the engine never is
  itself. Local and self-hosted plaintext instances are on `7700` and upwards.
- **`THETA_TLS` overrides it, in both directions.** `1`/`true`/`require`/`yes`
  forces TLS; `0`/`false`/`off`/`no` forces plaintext. A self-hosted instance
  behind its own terminating proxy on another port needs the first; a developer
  tunnelling `443` to a local process needs the second.
- **The choice is inferred, never carried beside the address.** The address
  travels through `THETA_ADDRESS` into every SDK and every `theta exec` child. A
  second variable that had to agree with it would be a second thing to get
  wrong, and the symptom of disagreement is a *hang* rather than an error: a
  plaintext frame sent to a TLS listener is read as a ClientHello, found
  malformed, and discarded.
- **The host in the address is the SNI name.** A shared address is routed *by*
  SNI, so getting it wrong does not produce a certificate error — it produces a
  connection to the wrong instance, or to none. A hosted instance is therefore
  reachable by hostname and not by IP.
- **Certificates are verified, chain and name, and there is no option to stop.**
  No client in this repository exposes a "skip verification" flag. A flag that
  disables certificate checking is a flag that ends up set in production.
- **`THETA_TLS_CA` adds a deployment's own CA to the platform roots.** It
  widens what a client accepts; it does not pin. A deployment with one
  self-hosted instance and one hosted instance reaches both from one process, so
  naming a private CA must not displace the public set. A path that cannot be
  read, or that holds no certificates, is a refusal rather than a warning: an
  operator who set it has said "trust this CA", and continuing without it would
  connect under a laxer policy than the one they chose.

---

## 2. Core RPC Surface

> **Resolution (M2).** This section originally sketched the surface as a Cap'n
> Proto `interface ThetaRpc`. That cannot coexist with the framing fixed in §1:
> a capnp `interface` is served by capnp-rpc, which carries its own
> multi-segment framing, handshake and capability table. The framing in §1 is
> the load-bearing half — it is explicit, testable, and what every SDK binding
> must agree on byte-for-byte — so the surface is expressed as request/response
> structs and the framing stands. The shape below is unchanged from the original
> interface. Nothing is lost: capnp-rpc's promise pipelining has no use here,
> because no ThetaBase call takes a capability returned by another.
>
> The authoritative schema is `crates/theta-proto/schema/theta.capnp`.

Each request carries a `requestId` and a `branchId`; responses echo the
`requestId`, so several calls may be in flight on one connection and may
complete out of order.

| Call | Request | Response |
|---|---|---|
| `get` | `key` | `found`, `value` (canonical JSON), `versionId` |
| `put` | `key`, `value`, `ttl` | `commitId` |
| `delete` | `key` | `commitId` |
| `query` | `QueryPlan` | `resultSet` (Arrow IPC), `planHash`, `rowCount` |
| `explain` | `QueryPlan` | `explanation` — EXPLAIN without executing |
| `proposeSchemaChange` | `change`, impact estimate | `ChangeDiff` |
| `applySchemaChange` | `changeId`, `change`, `confirm` | `commitId` |
| `createBranch` | `name`, `from` | `branchId` |
| `merge` | `sourceBranch`, `targetBranch` | `MergeResult` |
| `status` | — | `ProjectStatus` |

```capnp
struct QueryPlan {
  planHash    @0 :UInt64;      # cache key if precompiled
  rawQuery    @1 :Text;        # typed query language / SQL-subset
  contextVars @2 :List(KeyValue);   # bound, never interpolated
}

struct ChangeDiff {
  changeId        @0 :Text;
  destructive     @1 :Bool;
  rowsAffected    @2 :UInt64;
  reversible      @3 :Bool;
  estimatedCostMs @4 :UInt32;
  requiresConfirm @5 :Bool;
  shadowBranchId  @6 :UInt64;  # zero until a shadow branch exists
  reason          @7 :Text;    # plain-language gate decision
  affectedTable   @8 :Text;
  affectedColumn  @9 :Text;
  changeType      @10 :Text;
}

struct MergeResult {
  enum Status { ok @0; conflict @1; blocked @2; upToDate @3; }
  status        @0 :Status;
  conflictCount @1 :UInt32;
  conflicts     @2 :List(ConflictRef);  # non-CRDT fields needing human resolution
  converged     @3 :List(Text);         # fields reconciled by CRDT convergence
}

struct ProjectStatus {
  projectId             @0 :Text;
  branch                @1 :Text;
  writeVolumeMB         @2 :Float32;
  circuitBreakerTripped @3 :Bool;
  replicaRegions        @4 :List(Text);
  breakerWindowRows     @5 :UInt64;
  protocolVersion       @6 :UInt32;
  commitsApplied        @7 :UInt64;
}
```

A connection opens with `Hello` (client protocol version, scoped session token)
and is answered with `Welcome` or refused. See §5.

---

## 3. Unified SDK Surface (per-language binding, generated from one core protocol)

```javascript
const db = new Theta({ project: "churn-dashboard" }); // resolved via identity/org graph, no manual keys

// Typed, deterministic hot path
await db.put("user:123", { name: "Alice" });
const user = await db.get("user:123");
const results = await db.query(theta.table("users").where({ churnRisk: true }));

// Schema changes always go through propose → diff → confirm/auto
const diff = await db.schema.propose(change);
if (!diff.requiresConfirm) await db.schema.apply(diff.changeId, true);

// Optional AI assist — never executes on its own authority
const candidate = await db.assist.suggestQuery("users who haven't logged in this week", schema);
// candidate.planPreview is human/agent-readable before db.query(candidate.plan) is ever called
```

`assist` reaches a **separate service over plain HTTP**, not this wire protocol, and is configured with its own address (`assistUrl`). That separation is deliberate and enforced: routing a suggestion through the protocol the hot path speaks would put a model call inside it, which `crates/thetad/tests/no_llm_on_hot_path.rs` fails the build over. A deployment that never runs Assist loses suggestions and nothing else; calling `suggestQuery` without configuring it raises rather than returning an empty answer.

- Bindings generated for JS/TS, Python, Go, Rust, Java/Kotlin, Swift, C#, Ruby from the single Cap'n Proto schema (Stainless or equivalent generator) — kept in lockstep since they compile from one source of truth, avoiding drift between an agent's generated code and the live schema.

---

## 4. Query Language

- Typed query builder as the canonical form; a SQL-subset compiler translates familiar SQL syntax down to the same typed plan, for teams migrating from Postgres-family tools.
- Every query plan is inspectable via `EXPLAIN`-equivalent output (plan hash, estimated cost, index usage) before execution — required output for anything the Safety Layer or a human reviewer needs to reason about.

---

## 5. Versioning & Compatibility

- Wire protocol is versioned; `thetad` and Scribe negotiate protocol version on connect, refusing silently-incompatible combinations rather than guessing.
- Schema changes are additive-by-default at the wire level (new optional fields) to avoid breaking older SDK versions mid-rollout.

---

## `describe`: what is in here, and where it came from

Everything this returns could be assembled by a caller from `query`, `audit` and
a few sampled rows. An agent arriving at an unfamiliar database does exactly
that, and every step it takes before it can ask the question is a step it can
get wrong — usually by reading rows it did not need in order to learn a shape
the schema already knew. So it is one call.

**The most useful field is `crdt`, not `type`.** An agent deciding whether two
of its writes can race needs to know whether concurrent modification of a field
converges or becomes a conflict a human resolves
(`03-data-model-consistency.md` §3.2). That is the difference between "retry
freely" and "you have just created work for a person", and nothing else in a
schema description tells them. A field with no CRDT reports an **empty** kind
rather than a default one: a default would be a confident answer to the question
that matters most here.

**Examples are off by default, and that is a decision about the default rather
than an authorisation boundary.** A caller who can describe a table can already
query it, so examples reveal nothing they could not fetch, and pretending
otherwise would be theatre. What it prevents is different: `describe` is the
call an agent makes to orient itself, often automatically and often first, and
one that returns rows by default pulls customer data into a model's context —
and into whatever logs that context — for a call whose purpose was to learn the
shape of the data rather than any of it.

Distribution facts that disclose no individual value — the row count, the null
fraction — are sent regardless, because those are usually what a caller wanted
when it reached for examples.

**Asking for more than the cap gets the cap.** Above it a caller is no longer
characterising a column, they are reading it, and `query` is the call for that:
gated, logged and counted, none of which this is.

**Withholding is reported, never silent.** When examples are asked for and not
returned, the response says so and why. A client that asked and got an empty
list cannot otherwise distinguish "there were none" from "we would not give them
to you", and those lead to different next steps.

**Provenance comes from the branch being described.** `declaredAt` points at the
commit that established a column's canonical type, and is **empty** when that
commit is outside the visible log — a branch forked after the column existed, or
a segment expired by retention. Absent rather than guessed: a pointer to a
commit the branch never saw would be worse than the absence it replaced, because
a wrong pointer is followed and a missing one is not.

### The gate rationale on `ChangeDiff`

`reason` is prose and is **rendered from** the structured fields beside it, never
written independently. Two representations of one decision, maintained
separately, are free to disagree with nothing able to notice — the mistake §4
already records about `gate` and `requiresConfirm`.

`remedy` is what makes a refusal actionable. "Blocked on row count" and "blocked
on irreversibility" arrive identically as `requiresConfirm: true` and imply
opposite next moves; an agent that guesses wrong retries the same rejected thing
forever. `reduceBlastRadius` is never sent for an irreversible change, because a
smaller drop is still a drop.

**Unknown enum values fail closed.** A `gate`, `rule` or `remedy` this build does
not recognise comes from a newer peer and reads as the strictest value: the
strongest gate, the irreversible rule, and `validateOnShadowBranch`. Reading an
unknown remedy as `none` would be the dangerous direction — `none` means the
change *applied*, so a client would stop waiting for a decision that is still
pending.
