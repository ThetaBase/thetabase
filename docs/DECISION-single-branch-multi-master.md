# One branch, many writers: what it would cost, and why it is not built

ROADMAP-V3 M23's third item is multi-master in the strict sense — concurrent
writers to *the same branch* in different regions, converging without a
coordinator. The roadmap says of it:

> **This is where a published guarantee is genuinely at risk**, and it does not
> get built quietly. `totally-ordered-writes` is in `claims.toml`, pinned to
> `specs/03` §3.1. If the order becomes eventual rather than immediate, that
> claim changes before the code does — or it does not change at all.

This document is the "before the code does" half. **It is not built**, and the
decision to leave it unbuilt is recorded here rather than left as an absence
somebody later reads as an oversight.

---

## What is already built, and why it covers most of the need

M23's first two items are done:

- **Regional branches.** Each region writes to its own branch and merges
  explicitly. A caller in Frankfurt writes locally with no cross-region round
  trip, which is the property people actually want from multi-master.
- **Follower reads with a version floor.** A replica that would serve a stale
  read redirects instead.

Together these give local writes, local reads, and no wrong answers. What they
do *not* give is two regions writing the same key at the same instant and both
landing without anybody arbitrating.

## What the strict version would require

Three things, and each has a cost that lands somewhere specific.

### 1. The order becomes a function of the entries, not of their arrival

Today a branch has one writer at a time and the log's order is the order writes
arrived. Multi-master means two entries can be created concurrently in different
places, so the order has to be derivable from the entries themselves — hybrid
logical clocks, or something equivalent.

That is well-understood engineering. It is not the problem.

### 2. Every non-CRDT field becomes a CRDT or becomes a conflict

This is the problem.

`docs/INVARIANTS.md` invariant 5 says a non-CRDT conflict goes to a human, and `specs/03`
§3.2 says the same. Under single-writer branches that is rare: a conflict needs
two branches to have touched one key, and merges are explicit and infrequent.

Under concurrent writers to one branch it stops being rare. Two agents in two
regions writing one key a few milliseconds apart is the *ordinary* case, not the
exceptional one, and every instance of it becomes something a person has to
arbitrate. The alternative is last-write-wins, which is silent data loss with a
timestamp attached.

So the honest options are:

- **Make every field a CRDT.** Then concurrent writes converge and nobody
  arbitrates. This changes what ThetaBase *is*: a CRDT-only database has different
  semantics, and "the surviving value is always one somebody wrote" stops being
  a property of ordinary fields and becomes a property of the merge function.
- **Keep non-CRDT fields and accept the conflict rate.** Then multi-master
  delivers local writes and a review queue that grows with cross-region
  concurrency, which is the review-queue failure M17 exists to prevent, arriving
  from a different direction.

Neither is obviously wrong. Both are product decisions rather than engineering
ones.

### 3. `totally-ordered-writes` changes meaning

The claim reads:

> Writes to a branch are totally ordered. Concurrent writes to one key do not
> interleave: every one of them succeeds, and the surviving value is always one
> somebody wrote — never a blend, never a missing key.

Under single-branch multi-master the first sentence survives only as *eventual*
total order: the order exists, and a reader in one region may not have it yet.
A caller who wrote and then read from their own region still sees their write —
but two readers in two regions can, briefly, disagree about which write won.

That is a real weakening and it is exactly the kind that gets published as
though it were not. Anyone shipping this must edit that claim first.

## The decision

**Not built, and not deferred quietly.** The reasons, in order of weight:

1. **It trades a guarantee for a capability the first two items mostly already
   deliver.** Regional branches give local writes. The residue — same key, two
   regions, same instant, without arbitration — is a narrower need than
   "multi-region", and it is worth being asked for by name before it is paid for.

2. **The cost lands on the review queue, which is the product's scarcest
   resource.** M17 exists because a review queue with unbounded arrival has one
   steady state. Multiplying the conflict rate is the most direct way to reach
   it.

3. **It is a commercial decision, not a technical one.** Whether ThetaBase becomes
   CRDT-only, or keeps arbitration and accepts the rate, decides what the product
   is. That is not a call to make inside an implementation.

## What would have to be true to change this

Written down so the decision is revisitable rather than permanent:

- A customer asks for concurrent same-key writes across regions **by name**,
  having been told what regional branches already do.
- The claim change is agreed and `claims.toml` is edited **first**, with
  `totally-ordered-writes` restated as eventual-within-a-branch and its evidence
  re-pointed at tests that check the weaker property.
- A conflict-rate measurement exists from a real workload, so "the review queue
  absorbs this" is a number rather than a hope.
- `specs/03` §3.1 and §3.2 are rewritten together, because the second stops
  being about a rare case.

Until then, the honest position is that ThetaBase is multi-region and is not
multi-master, and the difference is stated rather than blurred.
