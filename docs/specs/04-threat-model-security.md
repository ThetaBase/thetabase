# Threat Model & Security Specification

ThetaBase v1

---

## 1. Scope

This covers identity/token security, tenant isolation, and — specific to this product's core bet — what an AI agent operating against a project **cannot** do, even if its instructions are malicious, compromised, or simply wrong.

---

## 2. Identity & Token Lifecycle

- **User identity token**: minted once at OAuth login (Google/GitHub), long-lived, stored locally, scoped to the user — never to a single project.
- **Project session token**: short-lived (target: minutes-to-hours, configurable), scoped to exactly one project + environment, minted on-demand by the Control Plane when a context is resolved ("Company B / churn-dashboard"), injected directly into the agent/app runtime.
- Project session tokens are never displayed to the user in plaintext by default and never persisted to a dashboard-visible field — if a user wants to inspect one for debugging, that's an explicit, logged action.
- Token scope is enforced at the `thetad` boundary: a token minted for Project A is cryptographically incapable of authenticating against Project B's instance, not merely "not shown it" — this is a hard isolation boundary, not a UI convenience.
- Compromised/leaked token: revocable instantly from the Control Plane; revocation propagates to `thetad` within one heartbeat interval (target: <5s).

---

## 3. Tenant Isolation

- One project = one `thetad` process. **What separates two of them today is an OS user and file permissions, not a process/VM sandbox** — see `01-system-architecture.md` §8.1, which states exactly what is and is not built. The target remains a hard memory/data isolation guarantee with no shared address space between projects regardless of infra tier; it is a target, not a description.
- **Per-project encryption keys for data at rest** (SEC-2, implemented). Segments and the view snapshot are sealed with XChaCha20-Poly1305 under a 256-bit key belonging to one project; no key is shared across projects, including within an organization. A key opens exactly one project's store and is refused by every other.
  - **Enabled per deployment, not by default.** `thetad` reads the key from `THETA_DATA_KEY`; with no key it writes in clear and says so at startup, at `warn`. Whether a given database is encrypted is answerable from its startup log rather than from this document — which is the point, given what this paragraph used to claim.
  - **Each segment records its own state,** so encryption can be turned on for a store that already holds data: existing segments stay readable, new ones are sealed, and the log rolls to a fresh segment at the boundary rather than mixing the two inside one file.
  - **A key that does not decrypt is reported as a wrong key, never as corruption.** Recovery answers corruption by truncating the log, so conflating the two would mean starting the server with the wrong key silently destroyed the database.
  - **What it protects:** a stolen segment file, a backup or volume snapshot taken without the key, a decommissioned disk, an over-broad replica. **What it does not:** anyone who can read the key alongside the data. Where the key is kept is therefore the whole question, and it is a deployment decision — an orchestrator secret or the value the Control Plane injects at launch keeps it off the volume being protected; a file next to the segments does not. The environment is not itself a theta - it is readable through `/proc/<pid>/environ` to anything running as the same user - and it is chosen because every threat in the list above captures a file on that volume and none of them capture a running process.
  - **Not covered: `audit.jsonl`.** The Safety Layer's audit log sits in the same directory and is written in clear. It holds change summaries and schema identifiers - table and column names, risk classifications, who proposed what - and no row values. That is a smaller exposure than the log, and it is still an exposure; it is named here rather than left for a reader to discover, because a reader who has just been told "encryption at rest" will not go looking. Sealing it needs the key to reach `theta-safety`, which is the next increment rather than this one.
  - **Cost:** ~2.6µs to seal a 324-byte record, against a `put` p50 of ~3.6ms that is dominated by an fsync. Encryption does not appear in the latency budget; `crates/theta-storage/tests/seal_cost.rs` keeps that true.
- Cross-project queries are architecturally impossible, not merely access-controlled — there is no code path in `thetad` that accepts a query spanning two project identifiers.

---


### 3.4 Support access

A ThetaBase engineer reaching a customer's data needs a `DataAccessGrant`: one
admin, one project, one window, one stated reason, approved by the customer with
their own credential (§3.3). This section is about what happens once they have
one.

**An impersonated session never claims to be the customer.**

The obvious implementation mints a token that looks like the customer's own. It
is the wrong one, and not subtly: every record produced during the session would
then say the *customer* did it. Support reproduces a bug, a row changes, and six
weeks later the customer's audit log attributes the change to them, with nothing
inside the tenant indicating anyone else was ever there. That turns the audit
trail from evidence into something confidently wrong, which is worse than a trail
with a gap, because a gap is visible.

So a support session carries **both** identities. Every tenant-side record names
the customer's principal acting *on behalf of* a named admin, under a named
grant.

**A support session is read-only.** There is no flag that changes this and no
field for one. An engineer who can write as a customer can do everything the
customer can, including the destructive acts `07-agent-safety-layer.md` exists to
gate, while holding an authority the customer granted for *debugging*. "Read my
data to work out what went wrong" and "change my data" are different asks, and
consent to the first must not confer the second. When a write is genuinely
needed, the customer makes it — slower, and correct: the person accountable for
the data is the person who changes it.

**Consent is re-read on every request, not at session start.** A customer who
withdraws expects it to stop now. Checking once at open would leave a window as
long as the grant in which withdrawal does nothing, which is precisely the window
they would care about. There is no cached "this session is valid" bit, because a
cached bit is one a withdrawal cannot reach.

Ending a session and withdrawing consent are separate acts. An operator ending
their own session leaves the grant untouched; collapsing the two would make an
operator's cleanup look, in the record, like the customer changed their mind.

### 3.5 Ordering of authentication and parsing

Platform routes **authenticate, parse, authorise, act** — in that order.

The ordering is load-bearing and easy to lose. A web framework runs body
extractors before the handler, so a route that declares a typed body
deserialises the request before any of our code reads the `Authorization`
header. A caller with no credential and a malformed body then receives a
different status from one with a well-formed body — which confirms the route
exists and tells them the body was the only thing wrong. It also means the
server does deserialisation work on attacker-controlled bytes for a caller it has
not identified.

**An unauthenticated caller must be answered identically whatever they sent.**
This is asserted by sweeping every platform route with several malformed bodies,
not only the one shape that happens to satisfy every request type — the earlier
version of that sweep used a single fixed body and therefore only ever proved
routes refused strangers *whose body parsed*.

Because the audit record for an action names the project and the reason, and both
live in the body, parsing has to happen after authentication and before the
capability check. That is the whole of why the order is four steps and not three.


## 4. Threat Model: What a Compromised or Malicious Agent Can Do

Given the core bet — that an AI agent is the primary author of schema changes and queries — this section is the one that has to hold up under real adversarial testing, not just documentation.

| Threat | Mitigation |
|---|---|
| Agent proposes a destructive schema change (drop table, narrow type) | Blocked pre-commit by the Safety Layer; requires explicit confirmation or shadow-branch validation — cannot land on `main`/`prod` unreviewed |
| Agent issues a runaway write loop (accidental infinite loop, N+1 amplification) | Blast-radius guardrail trips the circuit breaker at a configurable cost/row-impact ceiling, independent of whether the operation is "destructive" by type |
| Agent attempts to read/write outside its project scope | Architecturally blocked — token scope + process isolation, not just an ACL check that could be bypassed by a clever payload |
| Agent-generated query contains an injection-style payload targeting the query planner | Typed query builder / parameterized SQL-subset only — no raw string interpolation into the execution path, same discipline as parameterized SQL in traditional RDBMS |
| Prompt-injection via data returned from `query()` back into an agent's context, aiming to get the agent to issue a bad follow-up command | Out of scope for the database itself to fully prevent (this is an agent-runtime concern), but the Safety Layer's pre-commit gate means even a successfully manipulated agent cannot get a destructive change through without triggering confirmation/shadow-branch review |
| Malicious actor obtains a leaked project session token | Short token lifetime limits exposure window; instant revocation; token scope prevents lateral movement to other projects |
| Malicious actor obtains the long-lived user identity token | Standard OAuth token security practices (secure local storage, device-bound where possible); this is the single highest-value target and should get proportionate protection (e.g., optional hardware-key/2FA binding for org admins) |

---

## 5. Audit & Forensics

- Every write, schema change, branch, and merge is an immutable log entry with author (human or specific agent session id), making a full forensic reconstruction of "what happened and who/what did it" always possible — this is a direct consequence of the log-based architecture, not a bolted-on feature.
- Human-legible audit summaries (ranked by risk) surfaced per the Agent-Safety Layer Spec, so the forensic trail is usable without requiring someone to read raw log entries.
- **The log is hash-chained, and the chain is verified rather than merely written** (SEC-8). Each entry carries the content hash of its predecessor, and replay refuses an entry whose parent it does not recognise — so an edit to history stops recovery rather than folding into the state as though it had always been there. `DurableLogStore::verify_chain` is the full-log version, because a recovery resuming from a checkpoint never reads the entries the snapshot already covers and an edit below the checkpoint is invisible to it. Asserted by `crates/theta-storage/tests/tamper_evidence.rs`, which plants the tampering and requires it to be found — writing a chain and reading one are different things, and this repo shipped the first without the second for a while.
  - **What it does not detect: an edit to the newest entry.** Nothing inside the log commits to it, so truncating or rewriting the tail is indistinguishable from that write never having happened. Detecting it needs an anchor the disk does not control — a signature, or a hash published somewhere else — which is a v2 item and is deliberately not claimed here. `editing_the_last_entry_is_not_detectable_from_inside_the_log` records the limit as a test so it cannot be quietly assumed away.

---

## 5b. Trust boundaries assumed elsewhere

- **`eject` trusts the source database's catalog.** Migration reads table, column and type names out of the source's `information_schema` and interpolates them into SQL as quoted identifiers (parameters cannot carry an identifier). Quote-doubling is the correct escaping and Postgres identifiers cannot contain a null byte, so the residual risk is a deliberately hostile source database — which is a database the operator already chose to trust with their data. Stated because an unstated assumption is one a later change can violate without anyone noticing.

---

## 6. Explicit Non-Goals for v1

- Not defending against a fully compromised host OS/hypervisor at the infra-provider level (standard shared responsibility model with Fly/equivalent).
- Not providing agent-runtime-level prompt-injection defenses (that's the responsibility of the agent framework calling into ThetaBase) — ThetaBase's job is to ensure that even a successfully-manipulated agent cannot cause unreviewed destructive damage or unbounded cost.

---

## 7. Required Pre-Launch Validation

- Independent security review / penetration test of the token minting and scoping system before any production-grade or paid-tier launch.
- Adversarial test suite specifically targeting the Safety Layer (see Test & Validation Plan) — this is the doc's central claim and must be proven, not asserted.

---

## 6. Agent identity in the log

`Author::Agent` records the session a change arrived on. A log entry may also
carry what the agent said about itself: which agent implementation, a hash of
the instruction it was following, and the task the work belongs to.

### Which half is trustworthy

This is the distinction to get right, because a control built on the wrong half
is a control an attacker configures.

- `session_id` and `user_id` come from the **credential**. The server reads them
  off a verified token and a caller cannot choose them.
- Everything else is **self-reported**. An agent says which agent it is and
  which instruction it was following, and nothing verifies either, because
  nothing can: the agent is the caller.

So this is forensic, not authorising. It reconstructs what a cooperative agent
said it was doing, and against a hostile one establishes only what was claimed.
**No gate reads it**, and none can: classification never sees an author at all
(§2, and `07-agent-safety-layer.md` §2).

What it does buy is that a claim becomes tamper-evident the moment it is
written. Provenance is serialised inside the author, and the author is hashed
into the entry, so a self-report cannot be edited afterwards without breaking
the chain. An agent may lie at the time; nobody can change the lie later —
including the agent.

### A prompt hash, never a prompt

Prompts routinely contain customer data: a user pastes a row in and asks what is
wrong with it. The log is replicated, archived, and read by support under a
grant (§3.4), so a prompt stored here would carry customer data into all three
for a field whose only job is answering "was this the same instruction".

A hash answers that exactly as well and carries none of the data. It also
answers a question the prompt itself would not: one hash across four thousand
writes is a loop, and four thousand distinct ones is a very different session.

### Absent provenance changes nothing

The field is omitted from the serialised author when it is absent, rather than
written as null. That is load-bearing rather than tidy: the author is hashed
into the entry, so an entry written before this existed keeps its hash and the
chain over it still verifies. Tamper-evidence survives in both directions —
adding provenance to an entry that had none changes its hash, and stripping it
from one that had some does too.

---

## 7. External anchoring, signing, and continuous verification

§5 records the chain's honest limit: it detects an edit anywhere except the
newest entry, because nothing inside the log commits to the tail. These are the
three mechanisms that narrow that, and each is narrower than its name suggests.

### 7.1 The anchor closes the gap up to itself and not one entry further

Publishing the head hash somewhere we do not control means a rewrite below that
point produces a log whose head no longer matches what was published, and we
cannot un-publish it.

**Everything written since the last anchor is exactly as unprotected as
before.** The interval between anchors *is* the size of the exposed window, so it
is a stated policy value rather than an implementation detail, and verification
reports how many entries the newest anchor covers so the remainder is visible.

**Silence is a finding.** We cannot retract a published anchor; we can decline to
publish the next one. An operator rewriting history would stop anchoring first,
so a verifier that checked only the anchors it had would find them consistent
and report success. A gap longer than the policy allows is therefore a failure,
and a branch nobody has ever anchored fails rather than passing vacuously.

**Where an anchor goes decides what it is worth.** A counterparty who keeps
their own copy can produce the receipt independently and requires two
organisations to collude; a transparency log is publicly checkable and depends
on its operator; a public chain is the most expensive to rewrite. Verification
asks the sink to produce what it took, because an anchor the sink cannot
reproduce is our own record of our own action.

### 7.2 Signing protects a customer from us, not from their agent

`author` is currently something the *server* records, so the record is exactly
as good as the server: an operator with disk access can write an entry
attributed to anyone. A signature over the entry hash, by a key the server never
holds, makes the author a claim the log can check.

It proves the holder of the session key produced the entry. It does not
establish which human or model was behind that key, and **a stolen session key
signs exactly as well as an honest one**. So this upgrades "the server says this
session wrote it" to "the session key holder wrote it, and the server could not
have forged it" — real, and narrower than "signed commits" usually implies.

**The caller builds the entry; the server appends it verbatim.** This follows
from the sentence above rather than being a separate decision. If the server
assembled the entry and then attached the caller's signature, the signature
would be over something the signer never saw — and the whole claim reduces to
the server agreeing with itself.

So the two fields the server would otherwise choose are chosen by the caller and
*validated*: the commit id must be the branch's next, and the timestamp must sit
within an accepted skew. **The commit id is published**, on the branch listing,
because a caller that had to derive it would derive it from the entries it had
seen — and be wrong the moment anybody else wrote, signing for a position it did
not hold. A mechanism whose starting value cannot be obtained is complete and
unusable. A caller that loses the race for a position is refused
and re-signs, exactly as a conditional write is refused and retried. The
timestamp bound matters because the timestamp is inside the signature: without
one, a caller could sign an entry dated years ahead, hold it, and the record
would say it was written then.

The `author` is not on the wire. It is derived from the token on both sides, so
there is no field in which a caller could sign one author and be recorded as
another — and no key id either, because the key to check against is looked up
from the author. A caller cannot nominate which key should verify its own
signature.

**The public key rides in the session token.** It is the one channel already
authenticated by the project key, and a separate registration call would have
needed authentication of its own — with the very token this arrives in. The
session id and the key are covered by the same signature, so a caller can only
ever register the key issued to them.

A signature cannot live inside the thing it signs, so signatures sit beside
entries. That has a consequence worth stating rather than discovering: **a
missing signature does not break the chain**, and stripping one is undetectable
from the log alone. Verification is therefore driven by the entries that were
*expected* to be signed, never by the signatures that happen to be present.

Together with §6: a self-report is unverifiable and a signature is not, so the
pair says "this key holder made this change and claimed to be that agent while
doing it" — which is what makes an attribution query evidence rather than a
filter.

### 7.3 Verification reports coverage, never a verdict

A full replay on demand runs when somebody asks, which in practice is during an
incident. A background verifier at a bounded rate with a recorded position makes
"when would we have noticed" arithmetic rather than a hope.

**It must never claim to have verified what it has not.** A verifier reporting
"chain intact" having covered a third of the log converts a known unknown into a
false certainty, so what it reports is coverage, and nothing reads as verified
until a pass has completed.

A pass covers the log as it was when the pass began. Entries appended during a
pass belong to the next one — otherwise a busy instance outruns the cursor
forever while coverage climbs reassuringly and no pass ever finishes.

Whether the verifier is *keeping up* is a separate question from whether the
slice it checked was intact, and is asked separately: a tick that finds no
broken link says nothing about whether the pass will ever end.

### 7.4 Restore drills

A backup nobody has restored is a hypothesis. The per-segment round-trip proof
in `theta-archive` is about the moment a segment was written; a drill asks
whether the archive restores *today*.

A drill checks the manifest for gaps **before** fetching anything, because that
is the failure that matters most: every segment can round-trip individually
while one is absent, and a restore that skips it produces a state that never
existed and looks entirely normal.

A drill may be shallow, and a shallow one says so. "The newest ten segments
restore" is true and useful and is not "the archive restores", so the two are
reported as different facts and only a full pass answers "can I restore".

Results are recorded whether they pass or fail. An operator deciding whether to
attempt a restore mid-incident wants the date of the last successful drill, not
the absence of a recent alarm.

---

## 8. Verifiable query results

A client today trusts the server: it asks for a row, gets one, and cannot check
that the answer is what the log implies. The log is content-addressed and
hash-chained, so a client holding one trusted hash can verify that a specific
entry is in the history under it — without the server's cooperation and without
downloading the log.

**This is what would let ThetaBase be run by somebody the data owner does not
trust**, which is a different market rather than a feature.

### 8.1 The root has to come from outside

A proof is only as good as the root it is checked against. A client that asked
the server for the root and verified against it has established that the server
is internally consistent, which is not the property anyone wanted.

So this composes with §7.1: the root a client checks against is one published
where the operator cannot reach it. Without an anchor an inclusion proof is a
consistency check; with one it is evidence. The two features look independent and
are not.

### 8.2 What a proof does not establish

It establishes **presence** — that an entry is in the history under a root. It
does not establish **completeness**: that the server told you about every entry
it should have.

The asymmetry is inherent. Proving a result omitted nothing needs the client to
know what the full key set should be, which is the thing it asked the server for.
A server can still lie by omission and no inclusion proof catches it.

Completeness proofs need a different structure — an authenticated ordered map
rather than a chain — and are not built. Stated here because a proof shipped
without this paragraph would be read as proving more than it does.

---

## 9. What the isolation claim is checked by

§3 claims cross-project queries are architecturally impossible rather than
access-controlled. That claim has two halves and they have very different
evidence behind them.

**The code half** — no code path accepts two project identifiers — is checked by a
test that reads the source of every single-project crate and fails if one
appears. Until that test existed the sentence was true because somebody had read
the code; now it is true because it fails when it stops being.

**The deployment half** — that two projects really are two processes — is not
checkable from inside the repository. It remains a property of how ThetaBase is
run, it remains without in-repo evidence, and it remains what external review 2
is for. A WASM sandbox per project would turn it into a compiler-enforced
property; that is not built.

Stated as two halves because a test covering one of them would otherwise be read
as covering both.
