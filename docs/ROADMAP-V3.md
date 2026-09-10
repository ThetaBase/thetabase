# v3 — after the log is exploited and least privilege lands

v1 gets to GA. [`ROADMAP-V2.md`](ROADMAP-V2.md) is *prove what happened, undo
it, refuse it in advance* — the log turned into a product surface, and agents
given the narrowest possible authority.

**v3 is a different bet, and it is worth naming the bet before the features.**

## The thesis

v1 and v2 both assume a human is somewhere in the loop — reviewing a gated
change, resolving a conflict, holding a passkey. That assumption is load-bearing
and it is also the ceiling. **v3 is about what a database owes a fleet of agents
operating faster than any human reviews, without giving up the property that
makes v1 worth having: nothing destructive lands unreviewed.**

The resolution is not "review less". It is that **review is a scarce resource
and should be spent where it changes an outcome.** Everything below is a way of
either reducing what needs a human, or making the human's attention land on the
thing that actually mattered.

**What v3 is not.** Not "the agent decides". The Safety Layer stays rule-based
(invariant 2) and non-CRDT conflicts still go to a human (invariant 5). A
version of this that quietly relaxed either would be a different product wearing
this one's name.

---

## M17 — Review as a budget, not a queue

**Done.** `specs/07` §6.1 and §6.2; `crates/theta-safety/src/budget.rs` and
`triage.rs`; claims `an-exhausted-review-budget-refuses-rather-than-downgrades`
and `a-batch-is-gated-at-its-strongest-member`.

The gate today asks one question per change: does this need a human? At ten
agents that is a queue. At a thousand it is a queue nobody reads, and a queue
nobody reads is an auto-approve with extra steps.

- [x] **Risk budgets per branch, per agent, per window.** Built, with one change
      from the sketch above. The plan was to meter *destructive impact* — rows,
      tables, irreversibility — reusing the breaker's number. What is actually
      metered is **review cost**, a pure function of the gate, because rows are
      already bounded by the breaker and metering them twice would mean an agent
      doing a large amount of provably safe work gets throttled by a mechanism
      whose whole purpose is rationing human attention.

      So an `auto_apply` change costs zero, however many of them there are. That
      keeps the incentive pointing the right way: the cheapest route to staying
      under budget is proposing changes that do not need review.

      A proposal is charged against project, branch and agent, and refused if any
      is exhausted — charging fewer leaves an agent that has spent its own budget
      simply opening a new branch.

- [x] **Batched review of related changes.** Grouped by target table rather than
      by shared shadow branch: a shadow branch is created *per proposal*, so
      grouping by it would have grouped nothing.

      Two properties make it safe and both are tested: a batch carries the gate
      of its **strongest** member, and batching never alters a member's own
      classification. Without the first it is a mechanism for smuggling a drop
      through inside a group of index additions.

- [x] **Review triage that is itself rule-based.** Ordered by gate strictness,
      then blast radius, then key — total, deterministic, and it never filters.
      A triage step that hides a change has decided about it; when the queue is
      too long the answer is the budget refusing new work, not a filter
      concealing existing work.

- [x] **Expiring proposals.** Expiry releases the reserved budget *and*
      invalidates the proposal in the same operation, so they cannot disagree.
      Releasing the budget while leaving the proposal promotable would be worse
      than no expiry: a reviewer's approval would land on a change that sat
      untouched for hours while the schema moved underneath it.

**What the planted violations found.** Seven were planted against this
milestone; five went red. The two that did not were not violations — they were
*semantically equivalent* code, which no test can distinguish:

- Reading the batch gate from `members.first()` instead of taking the maximum
  was correct, because the members had already been sorted strongest-first. The
  property was resting on statement order. The fix was in the source rather than
  the test: the maximum is now computed **before** the sort, so the fold is the
  only thing that can produce the answer, and the positional read then fails
  loudly.
- The sort's final tiebreak is unreachable, because batch keys are unique. The
  comment claiming it was what kept the queue stable was wrong; stability comes
  from grouping through a `BTreeMap`, which normalises arrival order away. Both
  the comment and the test now say the true thing.

This is the same lesson as M25's: a property that holds *because of* an
incidental ordering is a property that will stop holding without anyone editing
the line that states it.

**Why first.** Every other v3 item increases the rate at which changes arrive.
Doing them before this one makes the queue problem worse.

---

## M18 — Multi-agent, without a coordinator

**Done.** `specs/03` (merge queues), `specs/04` §6 (agent identity),
`crates/theta-storage/src/{mergequeue,attribution}.rs`, and the claims
`an-agent-is-told-it-will-conflict-before-it-moves-on` and
`what-an-agent-says-about-itself-cannot-be-edited-later`.

Branch-per-agent works and is what v1 ships. What it did not answer is what
happens when fifty agents branch from one head and forty-nine merges conflict.

- [x] **Merge queues per target branch**, with the conflict detected at enqueue
      rather than at merge. A merge that will not land is refused there, with
      its conflicts and with the ticket it disagrees with — "you conflict with
      the target" sends an agent to look at `main`, and "you conflict with ticket
      7" sends it to the branch that actually disagrees with it.

- [x] **Speculative merge.** Validated against the target as it *will* be.

      **The first implementation did nothing, and the tests said so.** It
      overlaid the queued merges onto a copy of the target's materialised view.
      But `merge` is three-way: it derives each side's changes from the two
      branches' *histories* since the base, and uses the view only for CRDT
      state and types. Overlaying a view has no effect on conflict detection
      whatsoever, so every merge queued cleanly and three tests failed
      immediately.

      The fix speculates on what `merge` actually reads — a store wrapper that
      answers `history(target, ..)` with the real history plus one synthetic
      entry per queued merge. That reuses the existing conflict logic instead of
      reimplementing overlap detection, which would have been a second
      implementation free to disagree with the first exactly where it mattered.

- [x] **Agent identity in the log, not just authorship.** A schema change to the
      log, done now because it is hard to do late.

      **Which half is trustworthy is the whole design.** `session_id` comes from
      the credential and cannot be chosen; everything an agent says about itself
      cannot be verified, because the agent is the caller. So it is forensic
      rather than authorising, and no gate reads it. What it buys is that a
      claim is hashed into the entry the moment it is made: an agent may lie at
      the time, and nobody — including the agent — can change the lie later.

      **A prompt hash, never a prompt.** Prompts routinely contain customer data,
      and the log is replicated, archived and readable by support under a grant.
      A hash answers "was this the same instruction" exactly as well, and answers
      one the prompt would not: one hash across four thousand writes is a loop.

      The field is omitted rather than nulled when absent, so every entry written
      before it existed keeps its hash and the chain over it still verifies.

- [x] **Convergence under concurrent schema change.** Already built when this
      milestone was reached: `merge`'s declaration keying, table-scoped
      detection and rename-collision check were in place, and both halves were
      tested. Re-checked through the queue, since that is what an agent actually
      meets.

      One correction came out of it. A test here asserted that two agents adding
      the same column was a conflict, and the code disagreed and was right: two
      agents adding `orders.currency` as the same nullable text column have not
      disagreed about anything. It is one change proposed twice, and landing it
      once is correct. The real disagreement is one name with two definitions,
      and that is now the test.

---

## M19 — The database explains itself

**Done**, except example-driven discovery's richer statistics (histograms,
cardinality), which want the collection wiring M10 is still missing.

An agent writing against a schema it did not design spends most of its budget
discovering the schema. Every one of these is answerable from the log, and none
of them is a model call.

- [x] **`describe` as a first-class RPC.** — **done.**
      `theta.capnp`, `crates/thetad/src/describe.rs`, `specs/02`, claim
      `describe-tells-an-agent-what-it-needs-without-showing-it-rows`.

      One schema change carried it into all seven SDKs: the codegen reads the
      compiled Cap'n Proto IR rather than parsing the schema a second time, so
      TypeScript, Python, Go, Java, C#, Ruby and Swift picked up `DescribeRequest`,
      `GateRule` and `Remedy` without a line of per-language work. That decision
      was made in M10.5 and this is the first time it has been paid back at full
      width.

      **Examples are off by default.** Not an authorisation boundary — a caller
      who can describe a table can already query it. It is a decision about the
      default: `describe` is what an agent calls to orient itself, often
      automatically, and one that returns rows by default pulls customer data
      into a model's context for a call whose purpose was to learn the shape of
      the data rather than any of it. Distribution facts that disclose no
      individual value are sent regardless, because those are usually what the
      caller actually wanted.

      **The rationale is on the wire now**, which was the open half of item 2.
      Unknown gates, rules and remedies all fail closed, and an unknown *remedy*
      matters most: reading it as `none` would tell a client the change applied,
      so it would stop waiting for a decision still pending.

- [x] **Example-driven schema discovery.** — **done**, as the opt-in half
      of `describe` rather than as a separate call.

      Examples are distinct, capped, and drawn from a bounded sample, so a
      column holding `0, 1, 2, 0, 1, 2, ...` returns three different values
      rather than the same one three times. Above the cap a caller is reading a
      column rather than characterising it, and `query` is the call for that.

---

## M20 — Durability nobody has to think about

**Done.** `specs/04` §7, `crates/theta-storage/src/{anchor,signing,verifier}.rs`,
`crates/theta-archive/src/drill.rs`. Four claims, and the standing non-claim
`the-newest-entry-is-not-tamper-evident` **narrowed rather than retired** — the
window is now the anchoring interval instead of unbounded, and an instance that
is not anchoring still has the original gap.

- [x] **An external anchor.** Publishing the head somewhere we do not control.

      **The important half is that a missing anchor is an alarm.** We cannot
      retract a published anchor; we can decline to publish the next one, so an
      operator rewriting history would stop anchoring first. A verifier checking
      only the anchors it had would find them all consistent and report success,
      which is why staleness fails and a never-anchored branch fails rather than
      passing vacuously.

      Verification reports **how many entries the newest anchor covers**, because
      the sentence this feature must keep saying is that everything after it is
      exactly as unprotected as before.

- [x] **Signed commits.** The author becomes a claim the log can check rather
      than one it records.

      Stated at its real width: it protects a customer **from us**, not from
      their own agent. A stolen session key signs as well as an honest one. That
      is filed as a non-claim rather than left to be inferred.

      A signature cannot live inside the thing it signs, so signatures sit
      beside entries — which means a missing one does not break the chain.
      Verification is therefore driven by the entries expected to be signed, not
      by the signatures present, because otherwise stripping one leaves nothing
      to notice.

- [x] **Continuous verification.** Bounded rate, recorded position, and it
      reports **coverage rather than a verdict**: nothing reads as verified until
      a pass completes.

      A pass covers the log as it was when the pass began. Letting the target
      move with the log is how a verifier on a busy instance never finishes
      while reporting steadily increasing coverage.

- [x] **Restore drills on a clock, with the result recorded.** Gaps are checked
      before anything is fetched, because that is the failure where the *restore
      succeeds* and produces a state that never existed.

      A shallow drill says it was shallow. "The newest ten segments restore" is
      true and useful and is not "the archive restores", so only a full pass
      answers "can I restore". Passes are recorded as well as failures: an
      operator mid-incident wants the date of the last success, not the absence
      of a recent alarm.

---

## M21 — Cost as a first-class dimension

**Done.** `specs/07` §6.3, `crates/theta-safety/src/spend.rs`,
`crates/theta-storage/src/attribution_bytes.rs`, three claims.

- [x] **Query cost ceilings**, enforced before execution.

      The interesting case is an estimate that is a guess. `from_statistics` is
      false for an unanalysed table, and refusing on that would refuse real work
      for not having run `ANALYZE` — while ignoring it would make the ceiling
      bypassable by never analysing. So an unmeasured estimate is **admitted and
      marked**, and the budget below catches it by what it actually cost.
      Prediction refuses; measurement bills. Neither has to be right about
      everything, which is why there are two.

- [x] **Per-agent spend budgets**, in M17's shape. Permission is asked with an
      estimate and charged with a **measurement**: billing a project for what
      the planner guessed would be trusting the number this module already
      refuses to trust when it is unmeasured.

- [x] **Storage attribution per branch**, and the naive version is wrong in the
      direction that overcharges.

      Branches share structure, so summing each branch's view counts the shared
      part once per branch. On a bill that is not a rounding error: constant-time
      forks exist so a thousand preview branches cost about what one costs, and
      a naive attribution would report a thousand times the storage and undo the
      feature in the invoice.

      A branch is charged for what only it holds; the shared part is a pool
      charged to the project. **Exclusive plus shared equals the real total**,
      which is what makes it defensible — parts that do not sum to the whole
      are what a customer eventually finds. Two branches holding one key at
      different values share nothing but the name and are each charged, which is
      the roadmap's own case: a thousand cheap branches are cheap until one holds
      a copy of everything.

- [x] **The breaker reports, then enforces, at the billing layer.** The
      enforcing position is **refuse-writes**, and there is deliberately no
      refuse-everything: a customer over their limit must always be able to get
      their data out, so a billing state must not become data loss by another
      name. Filed as a claim rather than left in a comment.

**A correction found by planting.** `in_window` carried a comment warning about
the saturating-cutoff bug the breaker has and the review budget reintroduced.
Swapping it for the subtracted form passed every test — because these
timestamps are `i64`, where the subtraction goes negative rather than flooring at
zero, so both forms are correct here. The warning was carried over from a `u64`
context where it is real. The comment now says which, and a boundary test pins
what the window actually has to get right.

---

## M22 — Embeddable, honestly

**Done.** `crates/theta-embed`, `crates/theta-storage/src/sync.rs`,
`specs/01`, three claims (one of them a non-claim).

- [x] **One engine, two deployments.** The embedded crate is a facade over the
      identical engine the server runs — same log, same fold, same classifier,
      same gates. It reimplements no decision, and it exposes the engine rather
      than hiding it, because a facade that hides forces the next feature to be
      reimplemented as a subset. What is absent is the *server*.

- [x] **The Safety Layer, embedded.** Not optional, and there is no
      `auto_confirm`, no `force`, and no configuration that disables it. What
      changes is who reviews: in-process the caller is the reviewer and says so
      by calling `confirm`. Confirmation is still refused at the shadow gate.

      `SafetyPolicy` is a required argument to `open`. A default would be a gate
      somebody did not know they had, and the hosted product's policy is signed
      by a Control Plane that is not there.

      **What embedding drops is a value, not a README paragraph.** No
      cross-project isolation, no token scoping, no managed archive, no
      untamperable audit trail — filed as a non-claim so a caller can assert on
      it and so removing one is a diff.

- [x] **Sync between an embedded instance and a hosted one.**

      **This found a real incompatibility between two features built in this
      same milestone sequence.** Sync re-appends entries into another chain,
      which necessarily changes their hash. M20's signatures covered
      `LogEntry::hash`, which commits to `prev_hash` — so every synced entry
      would have arrived unverifiable, and signed commits and sync would have
      been mutually exclusive. Nobody would have found out until they used both.

      Fixed by separating the two questions the log was answering with one
      number. `hash()` is a chain *position* and makes history tamper-evident;
      `content_hash()` is what an entry *says* and is what a signature covers.
      Reordering is still caught, by the chain, which is its job rather than the
      signature's.

      The same split makes sync digests work at all: two instances holding one
      write have the same content hash and different chain hashes, so a digest of
      chain hashes would report everything as missing on both sides and carry the
      whole log every time.

      **Sync resolves nothing.** Two sides that wrote one key independently are a
      conflict for a human — a sync that picked a side would be auto-resolving
      a non-CRDT conflict at the largest scale available. Concurrent CRDT
      mutations are excluded, because converging is what they are for.

---

## M23 — Multi-region, in the order the guarantees allow

**Items 1 and 2 done. Item 3 deliberately not built**, with the reasoning in
`docs/DECISION-single-branch-multi-master.md`.
`crates/theta-storage/src/region.rs`, `specs/03`, three claims (one a
non-claim).

- [x] **Regional branches.** Placement, routing and a merge cadence. Read-your-
      writes holds within a region because a region is a branch; across regions
      you see another region's writes after a merge, which is what a branch has
      always meant. Total order within a branch is untouched.

      Two refusals worth naming. An **unplaced region is refused**, never sent to
      the home branch: a fallback would route a write across an ocean silently,
      which is the exact latency the arrangement exists to avoid. A placement
      **cannot be silently replaced**: doing so would move every subsequent write
      while the writes already on the old branch stopped being visible to callers
      still reading where they were told to.

      A region that has never merged is reported as **lagging**, not current.
      Zero lag for "no merge recorded" would make a new region that is silently
      failing to merge look like the healthiest in the fleet.

- [x] **Follower reads with a version floor.** A stale read becomes a redirect,
      never a wrong answer. The floor is **per key**, because a global one
      redirects every read on a replica behind on anything.

      **A planted violation found a conflation in my own code.** `Serve` carried
      a bare `u64` that defaulted to `0` for a key the replica did not hold —
      so a caller reading a non-existent key recorded a floor of zero for it, and
      a later read was served by a replica that still did not have it. The
      redirect branch two lines away already refused exactly that conflation.
      `Serve` now carries `Option<u64>`, and the floor is recorded from the
      decision rather than from a number, so there is no call that can set a
      floor for a key nobody had.

- [ ] **One branch, many writers, one order.** **Not built, deliberately.**

      The roadmap said this "does not get built quietly", and the reason holds:
      the engineering (hybrid logical clocks) is well understood, and the cost
      is not engineering. Every non-CRDT field either becomes a CRDT — which
      changes what the product is — or becomes a conflict, and under concurrent
      same-branch writers that stops being the rare case. That multiplies the
      rate at which work reaches a human, which is the review-queue failure M17
      exists to prevent, arriving from a different direction.

      It also changes `totally-ordered-writes` from immediate to eventual, which
      is exactly the kind of weakening that gets published as though it were not.

      Recorded as a decision with the conditions that would revisit it, and filed
      as a non-claim so the product's position is stated rather than inferred
      from an absence.

---

## M24 — Time as a query dimension

**Items 1–3 done; item 4 is M2's open Arrow work and stays there.**
`crates/theta-storage/src/temporal.rs`, `specs/03`, two claims.

- [x] **`AS OF` on any query.** The state at a commit, a timestamp, or a branch
      point.

      Retention is the bound and it **refuses rather than approximates**. A point
      below the horizon is refused with the horizon, never answered from the
      earliest state available — that would be the worst option on offer: the
      caller asked for Tuesday, got Thursday, and has no way to tell.

      A timestamp query **says which commit it got**. Timestamps are advisory, so
      `AS OF` a time resolves to the last commit at or before it, and a caller
      who assumed they got their exact instant is drawing conclusions about a
      different one.

- [x] **Diff as a result set.** A change carries **both** values, and a removal
      carries the value that is gone. Reporting only the new value answers "what
      does it say now", which the caller could already read; a bare key name for
      a removal makes the value unrecoverable from the diff, which is exactly
      when somebody needs it.

- [x] **Change data capture as a subscription.** The same fold, streamed.

      **A consumer away longer than retention is told, never silently resumed.**
      Resuming from wherever history now begins leaves a gap the consumer cannot
      see, and every downstream aggregate built from that feed is quietly wrong.

      Changes are folded from *state*, not read off the operations: an op says
      what was written and a change says what the state did, and they differ
      whenever a write sets a key to the value it already held. A consumer that
      saw that as a change would act on a no-op.

- [ ] **Arrow all the way out.** Unstarted here, because it is M2's open
      zero-copy item rather than a temporal one. Listing it under both would
      make it look like two pieces of work.

**The boundary that stays.** No distributed execution, no cost-based optimiser
for star schemas, no competing with a columnar engine on its own ground. What
this claims is narrower and defensible: **the temporal dimension, which is free
here and expensive everywhere else.**

---

## M25 — Proof rather than evidence

Everything in this repository is defended by tests, and a test is evidence about
the cases it runs. These are about replacing evidence with proof — and each is
tractable *because* of a decision made early. An invariant that makes a proof
possible is worth more than the invariant looks.

- [x] **A machine-checked consistency model.** — **done, bounded.**
      `crates/theta-storage/tests/convergence_model.rs`.

      `claims.toml` said strong eventual consistency was "provable from the log
      and CRDT design rather than asserted", and nobody had proved it. The
      evidence was two example-based tests.

      This checks convergence, idempotence, associativity and **preservation**
      exhaustively over every interleaving of up to five operations across two
      branches, and over three branches for associativity — which two branches
      cannot exercise, and which is the ordinary shape in this product.

      **It is a model check, not a proof**, and that is stated rather than
      blurred: no counterexample exists below the bound, which is weaker than
      TLA+ over an unbounded model. It is also where the bugs are: almost every
      convergence bug that has shipped anywhere is reachable in three or four
      operations.

      **The M25 lesson applied to itself.** The exhaustive classifier check
      recorded that three properties all of the form "never gets weaker" shared a
      blind spot. The equivalent here is checking only that merges *agree* — a
      merge that discarded everything would agree perfectly. So preservation is
      checked alongside agreement, and a planted symmetric discarding merge
      confirmed it: convergence, idempotence and associativity all passed, and
      only preservation caught it.

- [x] **Exhaustive verification of the classifier.** — **done, and it found
      something.** Invariant 2 says classification is a pure function of change
      kind, row impact, reversibility and branch protection. Those are four
      bounded inputs, and the state space turned out to be small enough to check
      exhaustively rather than sample. `specs/07` §4.1 now states the three
      properties; `crates/theta-safety/tests/exhaustive_classification.rs`
      checks them; `claims.toml` carries them as
      `the-decision-table-has-no-holes-or-inversions`.

      The clearest case of an early decision paying late: a classifier that read
      identifier text would have an unbounded input space, and this would have
      been impossible rather than merely hard.

      **What it found was a gap in the verification rather than in the
      classifier.** Three properties were written — text independence,
      totality, monotonicity — and all three passed on the first run, which is
      when a test is least trustworthy. Four violations were then planted in
      `classify.rs` to check the tests could see them. Three went red. The
      fourth, reordering `decide_gate`'s branches so the `destructive` check
      returns before the irreversible-and-wide check, stayed green against all
      three properties: it makes `ShadowValidate` **unreachable**, and a
      uniformly weaker table is still a monotone one. "Never gets weaker" cannot
      see a rule that has been deleted.

      That produced the fourth property, **reachability** — every gate is still
      produced by something, and the shadow threshold gates an irreversible
      change differently on either side of it. It is the only one of the four
      that catches the reorder, and the reorder is the most plausible of the
      four plants: both branches are still present in the source, so it survives
      review by looking untouched.

      The generalisable point is that the first three properties were all of the
      form "never gets weaker", and a family of properties that all point the
      same direction shares a blind spot. Worth applying to the machine-checked
      consistency model below before writing it, not after.

- [x] **Proof-carrying migrations.** — **done.** `specs/07` §6.4,
      `crates/theta-safety/src/proof.rs`.

      A change may carry a claim about what it preserves, and a verified claim
      stops it being gated for its size alone. **The claim is checked, never
      trusted** — a caller-supplied proof the server accepted would be the
      `rowsAffected: 0` hole with a longer name.

      **A drop is still a drop.** No claim about what a column contains makes
      removing it reversible, and that is the case somebody will ask for.

      A failed claim names the offending row by **key, never by value**: a
      refusal reaches logs and an agent's context, and the caller already knows
      which rows exist.

- [x] **Verifiable query results.** — **done.** `specs/04` §8,
      `crates/theta-storage/src/inclusion.rs`.

      A client holding one trusted hash can verify an entry is in the history
      under it, without the server's cooperation.

      **The root has to come from outside, and that is the whole question.**
      Verifying against a root the server supplied proves only that the server is
      internally consistent. So this composes with M20's anchor: without one an
      inclusion proof is a consistency check, with one it is evidence. The two
      features look independent and are not.

      **Filed with a non-claim**, because presence is not completeness: a server
      can still lie by omission and no inclusion proof catches it. Proving
      otherwise needs an authenticated ordered map rather than a chain, and is
      not built.

      A fixture bug found by its own test: the forgery case built its "invented"
      chain with the same deterministic helper as the real one, so the two were
      byte-identical and the forged proof was a genuine proof. The test was wrong
      and the code was right, which is the more useful of the two ways round.

---

## M26 — The engineering nobody attempts casually

**Two of four built. Two not, with the reasons stated rather than left as
absences.**

- [x] **Deterministic replay of an agent session.** — **done.**
      `specs/07` §6.5, `crates/theta-safety/src/replay.rs`, claim
      `a-rule-change-can-be-measured-against-real-history`.

      Exact replay is impossible and stays impossible; everything downstream of
      the proposal is deterministic, so the *decisions* replay exactly. Same
      function for forensics and for regression-testing a rule change — a
      separate "what-if" path would be a second implementation of the decision.

      **Loosening is counted separately from tightening**, because one is a
      review-load question and the other is a safety question, and a single
      difference count would let a hundred harmless tightenings hide one
      loosening.

      A test tried to construct a loosening by raising the irreversible shadow
      threshold and could not: `effective_irreversible_shadow_threshold` caps
      what a project may ask for, so raising it has no effect. The cap was doing
      its job and the test was trying to build something the design forbids.

- [~] **Provable in-process isolation.** — **the code half is now checked; the
      deployment half is not, and the sandbox is not built.**
      `specs/04` §9, `crates/thetad/tests/one_project_per_process.rs`, filed with
      a non-claim.

      `specs/04` §3's claim has two halves. "No code path accepts two project
      identifiers" was true because somebody had read the code; it is now true
      because a test fails when it stops being. "Two projects are two processes"
      is not checkable from inside the repository and is still what external
      review 2 is for.

      **A third vacuous test, caught by planting.** A self-check asserted that
      the caveat was still in the file, using `include_str!` on the file itself —
      so the assertion's own string literal satisfied it and removing the caveat
      left it passing. Prose cannot guard itself; `claims.toml` can, and the
      limitation is filed there instead.

- [ ] **Zero-downtime promotion under load.** **Not built.**

      The roadmap called this "the item most likely to be discovered by a
      customer rather than by us, because our own tables are never hot", and that
      is still true — which means the first thing needed is a measurement, not an
      implementation. Building a mitigation for a stall nobody has measured is
      how you get a mechanism that addresses the wrong bottleneck.

      What it needs: a benchmark that makes a table genuinely hot, promotion
      timed against it, and the stall published as a number. Then a decision
      about whether it needs solving. Not started rather than half-started.

- [ ] **Sub-millisecond p99 on the write path.** **Not built, and deliberately
      last.**

      `specs/09` targets 8ms p50 for a `put`, dominated by its fsync. An order of
      magnitude means group commit, io_uring or direct I/O.

      The roadmap's own warning is the reason to leave it: *the failure mode of
      every fast write path is that it stopped being durable and nobody noticed*.
      This repository currently proves durability with `crash_consistency` and
      `durability` suites written against the *existing* write path. Changing
      that path without first extending those suites to the new one would trade
      a measured guarantee for an unmeasured number, which is the specific trade
      the whole product exists not to make.

      Order of work, when it is picked up: extend the durability suites to cover
      batched commits **first**, watch them fail against the current
      implementation, then build group commit until they pass. Not the reverse.

---

## Where v3 stands

**31 of 36 items built.** One partial, four open, and each of the five has a
reason rather than a status.

### The four not built, and why

**One branch, many writers, one order** (M23). Not an engineering problem — the
engineering is well understood. Every non-CRDT field either becomes a CRDT, which
changes what the product is, or becomes a conflict, which multiplies the rate at
which work reaches a human. It also turns `totally-ordered-writes` from immediate
to eventual. Recorded in `DECISION-single-branch-multi-master.md` with the
conditions that would revisit it, and filed as a non-claim so the position is
stated rather than inferred.

**Arrow all the way out** (M24). This is M2's open zero-copy item rather than a
temporal one. Listing it under both would make one piece of work look like two.

**Provable in-process isolation** (M26, partial). The code half is checked by a
test; the deployment half is not checkable from inside the repository and is what
external review 2 is for. The sandbox that would make it compiler-enforced is not
built.

**Zero-downtime promotion under load** (M26). The first thing needed is a
*measurement*: our own tables are never hot, so nobody has seen the stall.
Building a mitigation for a bottleneck nobody has measured is how you get a
mechanism aimed at the wrong one.

**Sub-millisecond p99 on the write path** (M26). Deliberately last. The failure
mode of every fast write path is that it stopped being durable and nobody
noticed, and the durability suites here are written against the *current* path.
The order of work is: extend those suites to cover batched commits first, watch
them fail against today's implementation, then build group commit until they
pass. Not the reverse.

### What the work found

Six things worth carrying forward, all of them discovered by planting violations
rather than by reading code:

1. **A family of properties pointing one direction shares a blind spot.** The
   classifier's first three properties were all "never gets weaker", and none
   could see a rule that had been *deleted*. The convergence model repeats the
   lesson: agreement, idempotence and associativity are all satisfied by a merge
   that discards everything, and only preservation catches it.

2. **Five tests turned out to be checking nothing.** A migration test that
   compared a value to itself; a stability test that re-ran identical input; a
   caveat self-check whose own assertion string satisfied it. Each read, in a
   list of test names, exactly like a test that worked.

3. **Two features built in the same sequence were mutually exclusive.** M20's
   signatures covered the chain hash; M22's sync necessarily changes it. Nobody
   would have found out until they used both. Fixed by separating what an entry
   *says* from where it *sits*.

4. **The conformance fixture covered 11 of 20 request variants** while claiming a
   gap would be visible. `putIf` — which exists so a precondition cannot be
   dropped in transit — had never had its encoding executed.

5. **Comments carry cautions across type boundaries.** A warning about a
   saturating-subtraction bug, true for `u64`, was copied into `i64` code where
   it does not apply. The plant passed and the comment was wrong.

6. **Some plants are not violations.** Twice the "uncaught" plant was
   semantically equivalent code, and the right response was to change the
   *source* so the property no longer rested on an incidental ordering — not to
   write a cleverer test.

7. **Two deadlines governed one lifetime and the shorter one silently won.**
   Integration wired the review budget into the engine, which then pruned
   abandoned proposals on `shadow_ttl_ms` (a day) while the budget expired their
   reservations on `proposal_ttl_ms` (four hours). The budget's deadline always
   fired first, so the engine's settle-on-expiry was dead code that logged a
   warning about a reservation already released. A planted violation deleting it
   outright changed nothing, which is what dead code looks like from outside.

   The lesson is not about TTLs. Two modules each held a correct answer to "how
   long does a proposal live" and neither was wrong on its own; the defect
   existed only in the seam, which is exactly the region unit tests cannot see
   and the reason this integration pass is worth doing at all.

8. **Held and spent review are indistinguishable at the moment of settling.**
   Both count against the ceiling, so a test that confirmed proposals and counted
   acceptances could not tell a settled reservation from an unsettled one — and a
   plant removing settlement from the confirm path passed. They diverge only
   *later*: a held reservation is auto-refunded at the proposal TTL, a settled
   one persists for the spend window. So the engine under the plant returned
   review four hours after a change was confirmed, making a reviewed change
   cheaper than an ignored one.

   The property was real and the test was aimed at the wrong instant. Where two
   states are observationally identical now, the test has to run until they
   aren't.

9. **The `AnchorSink` contract set a trap and the obvious test hid it.** Anchor
   verification compared the sink's copy to ours *including the receipt* — but
   `publish` hands the sink an anchor whose receipt field is still empty, since
   the receipt is what `publish` is in the middle of producing. `InMemorySink`
   happened to stamp it back in; a second, equally reasonable sink that stored
   its argument unchanged failed verification as `ReceiptNotHonoured`, a message
   accusing the counterparty of dishonesty when the only fault was taking the
   argument at face value.

   It surfaced only because a test written the easy way — calling `forget` on a
   sink the engine had never published to — was rewritten to share storage with
   the engine. The easy version passed while exercising nothing, which is the
   fourth of those this project has produced. Verification now compares the
   claim (branch, head, coverage, publication time) and not the key it was
   reached through, and every field is exercised: an earlier version altered
   only `entries_covered`, and a plant deleting `head` from the comparison went
   unnoticed — the one field where a lying sink could attest to an entirely
   different log.

10. **A whole module had no source of input and its tests could not tell.**
    `replay` takes a `RecordedDecision` carrying the change, the impact the
    server measured, branch protection and the branch id. The audit trail stored
    a `ChangeDiff`, which carries the *outcome* — gate, rows, reversibility — and
    none of those inputs. So the module was unreachable from real data: every
    test it had was written over decisions constructed by hand, and each one
    passed.

    This is the failure the review-budget tests warned about in their own doc
    comment — "a budget module with passing tests and nothing calling it bounds
    nothing at all" — arriving in the module next door. Unit tests establish that
    a function is correct on its domain and say nothing about whether anything in
    the system can produce a value in it. The fix was on the *recording* side, not
    the replay side: the trail now captures the classifier's inputs beside the
    diff, and a replay reports how many trail entries it could not cover, for the
    same reason the chain verifier reports coverage rather than a verdict.

11. **A defence written at the wrong layer looks load-bearing and isn't.**
    `review_queue` sorted proposals by change id before handing them to
    `triage`, with a comment explaining that a tiebreak fed unordered input is
    not a tiebreak. Reasonable, and wrong: `triage` orders members by
    `(strictness, rows, change_id)`, and that last component already makes the
    order total. The sort could not change any answer, and a plant removing it
    passed.

    Worse than merely redundant. The comment asserted that the determinism
    guarantee lived in `review_queue`, so a later change moving or dropping the
    sort would have looked safe and a change weakening `triage`'s tiebreak would
    have looked irrelevant. Removing it and pointing at where the guarantee
    actually lives means the test now goes red when `triage`'s tiebreak is
    broken — which is the line that has to stay right.

12. **Two tests could not reach the rule they were aiming at.** Wiring
    proof-carrying migrations, a plant lowering the gate on a *drop* — the one
    thing no proof may do — passed. The test used a claim that was false, and
    `apply` returns early on a failed verdict, so it never reached the drop rule
    at all. A second plant deleting the CRDT half of the row scan also passed,
    because nothing in the system can currently write a CRDT row for the checker
    to be blind to.

    Both look like the same failure and are not. The first was a fixture that
    stopped one step short and is now fixed. The second is a guard against a
    state that `merge` and `sync` both produce in the formats but that no live
    path creates, so it cannot be tested end-to-end today — which is recorded as
    a note beside the tests rather than left as an unexplained gap, because a
    missing test that looks like an oversight is worse than one that says why.

14. **A test named for a guard that its own code path cannot reach.** Signing
    has a check that the key a signature is filed under belongs to the entry's
    author — without it, any registered key could sign for any author and the
    mechanism would be a check that *somebody* signed, saying nothing about who.
    A test called `one_session_cannot_sign_for_another` passed with that guard
    deleted.

    The guard is fine. `append_signed` derives the key id from the author, so a
    mismatch cannot occur on that path and the attacker's signature fails against
    the victim's key one step earlier. The test was passing for a reason its name
    did not describe, which is how a guard rots: it reads as covered, and the
    coverage is somewhere else entirely. It is now tested at the layer where a
    signature can arrive with a key id attached, and the engine test is renamed
    for what it actually shows.

15. **Two write paths read the same wire field two different ways.**
    `SignedOp::Put` decoded its `value` as raw JSON coerced through
    `Value::from_json`; `RequestBody::Put` decoded it as `Value`'s own serde
    representation. The same bytes became two different values.

    Ordinarily that is a bug worth a line. Here it was worse, because the
    signature covers the *decoded* value: a client encoding a row the way `put`
    documents would have got a signature failure, with nothing in the message to
    suggest the encoding was the cause. The failure mode is a security feature
    that looks broken and an encoding bug that looks secure.

    It surfaced only because an end-to-end test wrote through both paths. Neither
    path was wrong on its own, which is the recurring shape of everything this
    pass found.

16. **A client could not learn the commit id it had to sign for.** A signed entry
    commits to its own position, so the position has to be known before it can be
    signed for. Nothing on the wire published it — `status` reports commits
    applied across the project, which is a different number — so the only ways to
    get one were to guess, or to send a write and read the right answer out of
    the conflict error. `BranchInfo` now carries `nextCommit`.

    The mechanism was complete and unusable, which is the same failure as the
    sequence removal that could not name an element: a write path finished
    everywhere except at the point a caller has to start.

17. **"Named in the README rather than implied" is not a fix.** The MCP server
    shipped with two tools that did not do what their descriptions said, and the
    README said so plainly. That felt like honesty and was not: a tool list is a
    promise a model reads and acts on, and a caveat in a file the model never
    sees does not reach the party being misled.

    Both were small once looked at squarely, and one of the two stated reasons
    was simply wrong:

    - **`theta_query` did not need the SQL front end.** The wire carries the SQL
      as *text* and the server compiles it, so nothing had to be linked. What was
      actually missing was decoding the Arrow IPC result into rows — real work,
      but a different piece of work from the one the caveat named. The reason had
      never been checked before it was written down.
    - **`theta_review_queue` needed a wire request**, which did not exist because
      nothing had asked for one. Adding it took a request, a response type, a
      codec, a dispatch arm and conformance fixtures — and the conformance
      coverage guard demanded those fixtures the moment the variants appeared,
      which is exactly what it was added for.

    The lesson is narrow and worth keeping: writing a limitation down makes it
    honest, not resolved, and a stated reason nobody has verified is a guess with
    better presentation.

13. **An untouched branch was a mergeable branch.** `merge` reports `UpToDate`
    when the source head *is* the base — but a freshly forked branch is not its
    parent's ancestor, because forking appends a `BranchCreate` entry. So every
    branch nobody had written to fell through and produced `Merged` carrying four
    empty collections: a merge that lands nothing, indistinguishable to a caller
    from one that does something.

    It surfaced only at the queue, where `EnqueueError::NothingToMerge` turned
    out to be unreachable — an agent could queue a branch it had never written to
    and watch a no-op wait its turn. The first fix was to report `UpToDate`
    whenever the computed merge came out empty, and three existing tests went red
    saying why that is wrong: two branches independently writing the same value
    also produce an empty merge, and that is a different fact — the source
    genuinely changed something and the target happened to agree. The test is on
    the source's *entries*, not on the result.

---

## Integration status

A module with passing tests and nothing calling it bounds nothing, so this tracks
which of v3's mechanisms are reachable from a running instance rather than only
from their own test files. Every one of them was green before it was wired, and
wiring four of them found defects the unit tests could not see.

| Mechanism | Reachable from | Found on wiring |
|---|---|---|
| `safety::rationale` | proposal responses | — |
| `safety::budget` | `propose_within_budget` | ceiling ordering; reservation leak; dead settle-on-expiry |
| `safety::spend` | `record_spend` / `may_spend` | — |
| `safety::triage` | `Engine::review_queue` | a redundant sort that hid where determinism lives |
| `safety::replay` | `Engine::replay_decisions` | the trail recorded no classifier inputs, so replay had no input at all |
| `storage::provenance` | `FieldDef::declared_at` | — |
| `storage::anchor` | `Engine::maintain`, daemon timer | `AnchorSink` compared receipts, trapping correct implementors |
| `storage::verifier` | `Engine::maintain`, daemon timer | log order — the store walks backwards |
| `storage::attribution` | `Engine::sessions` / `activity` | shared ancestors counted once per descendant branch |
| `safety::proof` | `Engine::propose_with_claim` | a proof-lowered gate would have replayed as a policy divergence |
| `storage::temporal` | `read_as_of` / `diff_between` / `changes_since` | — |
| `storage::inclusion` | `prove_inclusion_under_anchor` | — |
| `storage::attribution_bytes` | `Engine::storage_attribution` | — |
| `storage::region` (placement) | `place_region` / `route_write` | the status surface reported no regions unconditionally |
| `storage::mergequeue` | `enqueue_merge` / `revalidate_merges` / `land_next_merge` | an untouched fork was a queueable merge that lands nothing |
| `storage::signing` | `Engine::append_signed` / `verify_signatures` | a test named for the key-id guard never reached it |
| CRDT writes *(new)* | `Engine::apply_crdt`, `crdt` on the wire | the write path was half-built: no way to name an element to remove |

One thing remains unreachable, and it is unreachable for a reason rather than by
omission:

- **`storage::region`'s follower reads.** A replica that refuses to answer below
  a version the caller has already seen, redirecting rather than serving a stale
  row. This instance is a primary, and a primary is never behind — `may_serve`
  against its own view can only ever return `Serve`, so the `Redirect` arm is
  unreachable by construction. Wiring it would add a call that always says yes,
  and report a guarantee as enforced that never had occasion to enforce
  anything. That is worse than the gap, because it would look covered. It is
  implemented and tested in `theta-storage`, and lands with read replicas.

**Signed writes, which had been blocked on a protocol change.**
`04-threat-model-security.md` §7.2 requires the signature to be made "by a key
the server never holds", so the server must verify and must not sign — and the
entry hash covers the commit id and timestamp, which a client could not know
before the server assigned them. The honest scope was a client-side change, and
that is what was built.

The caller now builds the entry and the server appends it verbatim. The two
fields the server would otherwise choose are chosen by the caller and validated:
the commit id must be the branch's next, the timestamp must sit within an
accepted skew. A caller that loses the race for a position is refused with a
`Conflict` and re-signs — the same shape as a conditional write, which the
protocol already had.

Three things fell out of building it:

- **Neither `author` nor a key id is on the wire.** The author is derived from
  the token on both sides, and the key to check against is looked up *from* the
  author. So there is no field in which a caller could sign one author and be
  recorded as another, and none in which it could nominate the key that verifies
  its own signature. A planted violation deleting the key-id guard in
  `SignatureBook` passed every engine test, because that path cannot reach it —
  the property holds by construction there, and the guard covers the paths where
  a signature arrives with a key id attached. It is now tested where it is
  reachable rather than where it merely reads as relevant.

- **The public key rides in the session token.** It is the one channel already
  authenticated by the project key; a separate registration call would have
  needed authentication of its own, with the very token the key arrives in.

- **A signature is evidence of authorship, never an exemption.** A signed write
  is type-checked and counts against the breaker like any other, and a forged one
  is refused *before* it reaches either — otherwise anyone who could reach the
  port could exhaust a project's write ceiling without writing anything.

What this establishes stays narrow, and the spec says so: it proves the holder of
the session key produced the entry. A stolen session key signs exactly as well as
an honest one. What it removes is the operator.

**The CRDT write path, which was missing entirely.** Found while wiring the
proof checker, which reads both the plain and the CRDT row maps: CRDT state was
first-class everywhere except where it would be created. The log format had
`OpType::Crdt`, `view.rs` folded it, `merge.rs` converged it, `sync.rs` declined
to call it a conflict, `Engine::get_crdt` read it, and the wire carried a
field's declared kind — and no production code constructed one. Every appearance
outside the enum was a match arm handling it or a test building one directly.

So a CRDT-typed field could be declared, described and read, and never set by any
caller; and the convergence guarantees the tests demonstrated were demonstrated
over state no user of the system could produce. That is the most complete way a
feature can be absent: every layer around it present, tested, and passing.

It is now built — a `crdt` request on the wire, `Engine::apply_crdt`, and the
seven SDK bindings regenerated. Three things came out of building it that are
worth keeping:

1. **The wire type cannot express a client-chosen element id.** An RGA id fixes
   both an element's identity and its order among concurrent siblings, so a
   client that chose its own could collide with another writer's element or
   displace one. The server mints it from `(commit, branch)`. Expressed as a
   shape the protocol cannot carry rather than a field the server remembers to
   ignore — a field that must be ignored is a field somebody eventually reads.

2. **A mismatched mutation is refused, not recorded.** The fold marks an
   operation that disagrees with the field's kind as rejected and moves on,
   which is correct for a fold and wrong as an answer to a caller: the entry
   would be in the log, the write reported as succeeding, and the value
   unchanged.

3. **The write path was half-built and looked whole.** `SeqRemove` names an
   element by id, and a caller reading through `get_crdt` sees values only —
   there was no way to learn an id, so the removal was a request nobody could
   issue. It surfaced only when a test tried to remove something.

## Known limitations introduced by integration

**Anchoring runs on the engine task.** `maintain` is called from the same task
that serves requests, because publishing an anchor has to record its receipt
back into the anchor log and therefore needs exclusive access to the engine. A
slow or hanging sink stalls request serving for as long as it takes — which was
not true of the shadow sweep that previously owned this timer, since that only
walked open proposals in memory. Publishing off-task is a design change rather
than a `spawn`, and until it is made a sink is expected to carry its own
timeout. Stated here rather than discovered during the first sink outage.

**Anchors and verifier state do not survive a restart.** Both sit beside the log
rather than in it, for the same reason signatures do: an anchor commits to a
head, so putting it in the log would change the head it commits to. The
consequence is that a restarted instance reports every branch as never-anchored
until the next interval, and starts a fresh verification pass. The second is
deliberate — a resumed cursor would let an instance that restarts often report
climbing coverage while no pass ever completes. The first is not, and needs the
anchor log persisted beside the log before this is a durability claim rather
than a runtime one.

## What is deliberately not here

Two things left this list on review, and it is worth saying why rather than
quietly editing them out. **Cross-region multi-master** was excluded because any
design delivering it weakens a guarantee — but `specs/01` §7 excludes it *"at
launch"*, which is a scope decision, and the guarantee it threatens turned out
to be narrower than it appeared. It is M23. **Analytics** was excluded as a
category when the objection only ever applied to part of it; the temporal half
is free here and expensive everywhere else. It is M24.

Both were excluded by reflex rather than by argument, which is the failure mode
of a "not doing this" list: it accumulates things that were once hard and stops
being re-read.

What remains out, with the argument:

**A model in the Safety Layer.** Not in v3, not in v4. Invariant 2 is the
product, and a classifier that consults a model is a classifier that can be
argued with.

There is an adjacent thing that is *not* excluded and is worth distinguishing:
**a model helping a human review faster** — summarising a diff, drafting the
question a reviewer should ask, proposing corpus entries from an incident. That
is Assist's shape applied to review, and the boundary is exact: it may inform
the person and it may never inform the gate. If it ever ranks what a reviewer
sees, it is deciding what a human reviews, which is invariant 2 relocated rather
than respected.

**A general-purpose analytics engine.** M24 takes the temporal half and stops.
`eject` remains the honest answer for someone who wants a warehouse, and a
product that tries to keep that user is a product that gets worse at the one it
has.

**Anything that makes a fresh install slower to a first gated schema change.**
That path is the product's argument, and every feature above is a chance to make
it longer. M23's regional placement and M26's sandbox are the two most likely to
do it by accident.

---

## How to read this

None of it starts before v1 is at GA and v2's M13–M16 are honestly assessed.
The ordering within v3 is M17 first — because everything else increases the rate
at which decisions arrive, and a review queue that has already failed is not
improved by more input.

After that the six split into two tracks that do not block each other.
**M18–M20, M23 and M24 are product**: things a customer asks for, each buildable
by a competent team on a schedule. **M25 and M26 are research** and should be
budgeted as research — a fixed allocation with a decision point, not a date.
The distinction matters because M25's items would change what ThetaBase *is*
rather than what it does, and the temptation with those is to keep funding them
past the point where the answer is known.

Two are worth attempting even at low odds, because a negative result is still
valuable. **Exhaustive classifier verification** either proves the central claim
or finds the input that breaks it, and there is no bad outcome. Same for the
**machine-checked consistency model**: a model that fails to check is telling
you something about the design that no test was ever going to.

Each milestone here is a *bet with a stated thesis*, not a feature list. If the
thesis turns out to be wrong — if agents do not operate at a rate that breaks
human review, or if fleets do not converge on one database — the right response
is to delete the milestone rather than to build a smaller version of it.
