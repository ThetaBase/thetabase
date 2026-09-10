# Runbooks

One per failure mode in [`specs/01` §7](specs/01-system-architecture.md), plus
the operational procedures M10 introduces.

**What a runbook is for here.** The gate M10 turns on is not that these read
well — it is that each one has been *performed in staging and the documented
behaviour observed*. A runbook nobody has followed is a hypothesis. Each entry
below therefore ends with **Verified by**, naming either the test that exercises
the path or, honestly, that nobody has run it yet.

A runbook that says "the system handles this automatically" is still a runbook:
the operator's job in that case is to confirm it did, and to know what it looks
like when it did not.

---

## 1. Storage node crash

> *Auto-restart, replay WAL from last durable checkpoint, resume in seconds.*

**What you will see.** The instance is unreachable; the Control Plane's courier
reports it as a failure and carries on with the rest of the fleet. Existing
scoped tokens keep working the moment it returns — they are verified against
keys the instance already holds, not against a live call.

**What happens without you.** On restart, `thetad` opens the log, replays from
the last checkpoint, and truncates any torn tail. A write that was acknowledged
is durable by definition: the fsync happens before the acknowledgement, under
both sync policies.

**What to check.**

1. `GET /healthz` on the instance returns `ok`.
2. The startup log names the recovery: look for `log was truncated during
   recovery`. Its absence means nothing was lost, which is the common case.
3. If it *is* present, the truncated bytes came from a write nobody was told
   succeeded. That is correct behaviour, not data loss — but it is worth
   recording in the incident, because the client that issued it saw a failure
   and may have retried.

**When to worry.** `OrphanedSegments` in the startup error. That means a torn
segment has intact segments *after* it, so replaying would silently reorder the
log. The process refuses to start rather than guess, and that refusal is
deliberate — see [Recovering from a torn log](#7-recovering-from-a-torn-log).

**Verified by.** `crates/theta-storage/tests/crash_consistency.rs`, and
`crates/thetad/tests/network_partitions.rs` for the acknowledged-write property
under partition. **Not yet performed in staging.**

---

## 2. Archive / object storage unavailable

> *Writes continue locally, snapshots queue and retry; extended outage pages the
> team and falls back to a secondary local snapshot target.*

**What you will see.** `archive sweep: nothing to do` gives way to deferred
counts in the sweep summary. Nothing else changes: reads and writes are
unaffected, because the archive is not on any request path.

**What happens without you.** Each tick tries again. Segments accumulate on
local disk — that is the intended trade, and it is why this is not an incident
at five minutes and *is* one at five days.

**What to check.**

1. `archived 0, released 0 (0 bytes), deferred N` in the sweep log, with N
   growing across ticks.
2. Local disk headroom on the instance. This is the resource the outage
   consumes, and the number that decides how long you have.

**What to do.**

- **Under a few hours:** nothing. It will drain itself.
- **Longer:** the segments are safe but unarchived, and disk is finite. Free
  space, or accept the growth, and keep watching.

**Still open, and stated plainly:** *extended outage pages the team and falls
back to a secondary local snapshot target* is **not built**. There is no pager
integration and no secondary target. Today an extended outage looks exactly like
a short one until somebody notices the disk. This is tracked in ROADMAP M10 and
is the largest gap in this document.

**Verified by.** `an_unreachable_archive_releases_nothing_and_is_not_an_incident`
in `crates/theta-archive/tests/custodian_release.rs`. The paging and fallback
have nothing to verify.

---

## 3. Query planner degraded or slow

> *Falls back to a simpler, non-optimized plan rather than blocking; never falls
> back to unchecked raw execution.*

**What you will see.** Query latency rising while `get` and `put` are unaffected
— the planner is not on the point-lookup path.

**What to check.** `EXPLAIN` the slow query. The two things to look for:

1. `indexes used: none` on a large table. The usual cause, and usually a
   missing index rather than a planner fault.
2. `est. cost` far from the observed time. That means the statistics are stale,
   not that the planner is broken.

**What matters most.** The second half of the spec line is the load-bearing one:
*never falls back to unchecked raw execution*. There is no degraded mode that
skips the typed plan, because there is no representation for one — the query IR
has no variant that can carry executable text, and
`crates/thetad/tests/recorded_positives.rs` fails the build if one appears. A
slow query stays a slow *checked* query.

**Verified by.** `crates/theta-query/tests/cost_calibration.rs` for the
estimator tracking reality. **The fallback-to-simpler-plan path is not
separately tested**, and should be before this milestone's gate is claimed.

---

## 4. Merge conflict on a non-CRDT field

> *Surfaced explicitly, blocked from auto-merge, never resolved silently.*

**What you will see.** A merge returns conflicts rather than completing.

**What happens without you.** Nothing, and that is the design (`docs/INVARIANTS.md`
invariant 5). Neither side wins by timestamp, by branch, or by any rule the
system could apply on its own.

**What to do.** The conflict names the branch, the table and the column. A human
decides, and the decision is applied as an ordinary write on the target branch —
so it lands in the log with an author, like every other change.

**What never to do.** There is no `--force` and no auto-resolve flag, and adding
one would not be a feature. The whole claim of the merge model is that a
non-CRDT conflict reaches a person.

**Verified by.** `crates/theta-storage/src/merge.rs` tests and
`crates/theta-storage/tests/convergence_durable.rs`. **Not yet performed in
staging.**

---

## 5. Destructive change attempted

> *Blocked pre-commit unless confirmed or run through shadow-branch validation
> first.*

**What you will see.** A proposal returns a gate of `confirm` or
`shadowValidate` rather than applying.

**What happens without you.** Nothing applies. The Safety Layer classifies from
(change kind, row impact, reversibility, branch protection) and nothing else —
it reads no identifier text, so a table named to look harmless is classified on
what it does.

**What to do.** Read the diff. `confirm` wants a human to say yes;
`shadowValidate` wants the change run against a shadow branch first, because its
blast radius is large enough that "it looked right" is not enough.

**When to worry.** A destructive change that came back `autoApply`. That is a
classifier bug and a serious one — capture the proposal and add it to
`crates/theta-safety/tests/adversarial_corpus.rs`, which only ever grows.

**Verified by.** The adversarial corpus, `make adversarial`. **Not yet performed
in staging.**

---

## 6. Local disk filling up

Not in `specs/01` §7, and it belongs here because M10 is what makes it
survivable.

**What you will see.** Storage bytes climbing in the courier's readings for one
project without a matching rise in rows.

**Cause, most likely.** The archive custodian is not running, or is deferring.
Before M10 nothing released a proved segment, so any instance running long
enough grew without bound.

**What to check.**

1. Is the sweeper running? A tick logs every interval, at `debug` when it had
   nothing to do.
2. `released N (M bytes)` in the summary. Zero releases with non-zero archives
   means the log is refusing them — which it does when the checkpoint has not
   moved, so the question becomes why checkpointing has stalled.

**What never to do.** Delete a segment by hand. The engine refuses to release
anything at or above the checkpoint precisely because that segment is what
recovery reads; `rm` does not consult it.

**Verified by.** `crates/theta-archive/tests/custodian_release.rs`, including
that the bytes actually leave the disk and that the active segment never does.

---

## 7. Recovering from a torn log

**What you will see.** `thetad` refuses to start, reporting `OrphanedSegments`.

**What it means.** A segment is damaged and intact segments follow it. Replaying
past the hole would produce a state that never existed — the log is a fold, so
applying segment 5 after a missing 4 gives a result that looks entirely normal
and is wrong (`docs/INVARIANTS.md` invariant 6).

**What to do.** This is a decision, not a procedure, which is why the process
stops and asks:

- **Restore from archive** if the segments are there. The manifest's gap check
  runs every sweep, so you should already know whether it is complete.
- **Truncate to the damaged segment**, accepting the loss of everything after
  it, if the archive cannot cover it.

Both destroy data. That is why neither happens automatically.

**Verified by.** `crates/theta-storage/tests/crash_consistency.rs`. **Not yet
performed in staging.**

---

## 8. Suspecting the audit trail

**Platform trail.** `GET /platform/v1/audit/verify` walks the chain and
recomputes every hash. `intact: false` means an entry was altered after it was
written — by something with database access, since no API path can edit one.

The Control Plane also refuses to start on a trail that does not chain, so a
process that is up has already verified its history at least once.

**Its limit, so you do not over-trust it.** An edit to the *last* entry breaks
no link, and neither does truncation of the tail. `head` is returned by that
endpoint so it can be published somewhere we do not control; until it is, this
detects rewritten history but not shortened history.

**Project log.** `DurableLogStore::verify_chain` is the full-log pass. Ordinary
startup resumes from a checkpoint and cannot speak for entries below it.

**Verified by.** `crates/theta-control/tests/platform_durability.rs` (including
tampering through raw SQL) and
`crates/theta-storage/tests/tamper_evidence.rs`.

---

## What is not covered

Stated rather than left as a gap for someone to find during an incident:

- **No pager integration anywhere.** Every "you will see" above assumes somebody
  is looking.
- **No secondary archive target**, so item 2's fallback does not exist.
- **No staging environment** in which to perform these, which is why almost
  every entry above ends with *not yet performed in staging* — and why M10's
  gate is not met.
