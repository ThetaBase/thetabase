# Scope — the review TUI

**Status: built.** `theta review`. This started as a scope document for a
decision; the decisions are marked where the implementation disagreed with
them.

---

## The problem it solves

The Safety Layer's whole design rests on one assumption: **that a human actually
reads the thing they are confirming.** Invariant 5 in `docs/INVARIANTS.md` sends non-CRDT
conflicts to a person, `07-agent-safety-layer.md` gates destructive changes on
human confirmation, and the entire product claim collapses if that person is
rubber-stamping.

Today the review path is four separate commands:

```sh
theta change list            # what is waiting
theta change show <id>       # read one
theta change confirm <id>    # answer it
theta change promote <id>    # land it
```

Each is a round trip. Each requires copying an identifier. To review five
pending changes a person runs twenty commands and, in practice, stops reading
carefully somewhere around the third.

**That is not a UX complaint, it is a safety defect.** We built a gate whose
correctness depends on attention, and then made attention expensive. Dolt's
model — "branch everything, a human reads the diff" — fails for exactly this
reason at exactly this point, and it is the failure our classifier is supposed
to avoid. Making the *classification* cheap and leaving the *reading* expensive
only moves where the rubber-stamping starts.

## Why a TUI rather than a web dashboard

`06-provisioning-identity-flow.md` §5 fixes the portal's scope at billing and
account administration, and that boundary is load-bearing: the product's
interface is the CLI and the agent tooling, and a dashboard that grew schema
review would make the claim false.

A TUI keeps the boundary. It is `theta review` — a mode of the CLI, in the
terminal the engineer is already in, next to the agent that proposed the change.
No context switch, no second authentication surface, no browser.

It is also the only option that works over SSH, which is where a meaningful
share of production access happens.

## What it does

One screen. A list on the left, the selected change on the right.

```
┌ Review — acme/checkout ─────────────────────────────────────────────────────┐
│                                                                              │
│  ▸ chg_8f2a  DROP COLUMN     │  chg_8f2a                                    │
│    chg_3b91  ALTER TYPE      │  ─────────────────────────────────────────   │
│    chg_c04d  ADD INDEX       │  orders.legacy_ref  →  dropped               │
│                              │                                              │
│  3 waiting · 1 destructive   │  Gate       shadow_validate                  │
│                              │  Rows       48,201 affected                  │
│                              │  Reversible no                               │
│                              │  Branch     main (protected)                 │
│                              │                                              │
│                              │  Proposed by                                 │
│                              │    agent · sess_7c1f · 14:22                 │
│                              │                                              │
│                              │  Shadow validation                           │
│                              │    ✓ applied to shadow branch                │
│                              │    ✓ 48,201 rows migrated                    │
│                              │    ✓ no constraint violations                │
│                              │                                              │
│  [enter] open  [c] confirm  [r] reject  [p] promote  [q] quit                │
└──────────────────────────────────────────────────────────────────────────────┘
```

### In scope

- **Queue view** — everything waiting, ordered by `safety::triage`, which
  already produces a total order. The TUI must not re-sort; a second ordering
  would be a second opinion about urgency.
- **Detail view** — the full `ChangeDiff`, the gate and *why* that gate, row
  impact, reversibility, branch protection, who proposed it and under which
  session.
- **Shadow validation results** where the gate is `shadow_validate`, because
  that is the evidence the confirmation is supposed to rest on.
- **Answering** — confirm, reject with a reason, promote a validated change.
- **Live refresh** as new changes arrive.

### Out of scope, deliberately

- **Proposing changes.** Review is a reading tool. A screen that could both
  propose and approve is a screen where one person does both halves of a
  two-party control.
- **Editing a proposal.** Invariant 3: prevent, don't correct. A rejected change
  is rejected and the agent submits a corrected one.
- **Anything an agent does.** Schema, queries, branches, keys stay in the
  non-interactive CLI and the MCP server. If an agent would do it, it does not
  belong here — the same test the account portal uses.
- **Bulk confirm.** See below; this is the important one.

## The design decisions that need making, with recommendations

### 1. There is no "confirm all", and that has to survive product pressure

A select-all that answers every pending gate is the single feature most likely
to be requested and the one that would void the product's central claim. The
gate exists so a person decides; a batch confirm makes deciding a formality and
we would have rebuilt the thing we sell against.

**Recommendation: no bulk confirm, at any tier, ever.** Write it into
`07-agent-safety-layer.md` so it is a spec decision rather than a preference
someone can overrule in a planning meeting, and add a test that fails if a bulk
path appears — the same shape as the MCP server's `HUMAN_ONLY` list.

Multi-select for **reject** is fine and should exist. The asymmetry is the
point: refusing many things at once is safe, allowing many things at once is
not.

### 2. Confirming a destructive change should cost a keystroke more

For `shadow_validate` gates on a protected branch — the highest tier — require
typing the change id rather than pressing `c`. Not a modal "are you sure",
which trains people to dismiss modals, but an action proportionate to what is
about to happen.

**Recommendation: adopt.** It is four seconds against dropping a column from a
protected branch, and it makes the confirmation an act rather than a reflex.

### 3. Does it hold a connection open, or poll?

A TUI sitting open all day on a dev project is exactly the thing hibernation
(#23) is designed to stop paying for, and an open connection would defeat it.

**This recommendation was wrong and the implementation says so.** Treating the
poll as non-activity server-side is not possible: *answering* a poll requires
the instance to be running, so a request that reaches it has already woken it.
There is no server-side rule that can make polling free.

**What was built instead is the client-side half.** The screen polls every five
seconds while somebody is using it, and stops entirely after fifteen minutes
with no keypress — `Mode::Dormant`, with the footer saying so. A terminal
forgotten in a tmux pane stops paying for an instance overnight, which was the
actual concern; the keypress that wakes it is spent waking it, so the first
thing typed after a break does not also answer a gate.

### 4. Read-only mode for someone without the role

Roles decide who can answer a gate. Someone without the role should still be
able to *see* the queue — understanding what is waiting is not the same
privilege as answering it, and hiding it makes the system less legible for no
security gain.

**Recommendation: show the queue to any member, grey out the actions, and say
which role is required.** A refusal that names what would be needed is worth
three sentences of documentation.

## Build cost

`ratatui` and `crossterm`, both mature, both pure Rust, neither adding a network
dependency to a crate that has none. Roughly:

| | |
|---|---|
| Queue and detail views over the existing wire calls | 2–3 days |
| Key handling, confirm/reject/promote, refresh | 2 days |
| The step-up confirmation and role-aware actions | 1 day |
| Tests — the interesting ones are below | 2 days |
| **Total** | **~8 days** |

`theta_review_queue` already has a wire request and the engine already produces
the triaged batches, so this is a client over an existing surface rather than
new plumbing.

## What the tests have to catch

Not "does the widget render". The properties that carry the safety claim:

1. **No code path confirms more than one change per keystroke.** The structural
   version of decision 1, and the one that must fail loudly if someone adds a
   convenience later.
2. **The queue order is `triage`'s order.** A TUI that sorted by timestamp would
   quietly reorder urgency.
3. **A destructive change on a protected branch cannot be confirmed by a single
   key.** Decision 2, tested rather than trusted.
4. **Actions are refused for a member without the role**, and the refusal names
   the role.
5. **The rendered detail contains the row count, reversibility and gate.** A
   detail view that omitted row impact would be a confirmation screen that hides
   the blast radius — and someone would still confirm.
6. **A screen nobody is touching stops polling.** Otherwise an open TUI pins a
   project awake and hibernation saves nothing on exactly the projects under
   active development. Tested at the boundary in both directions: it must not
   go dormant early, and it must not stay awake.

## Status

**Built.** `theta review`, with the state machine and rendering separated from
the terminal so every rule below is a property a test asserts rather than
behaviour somebody has to drive a terminal to observe. Twenty tests, eight
planted defects, all caught.

The one correction is in decision 3 above: polling cannot be made free on the
server side, so the screen goes dormant instead.

## Original recommendation

**Build it, after the runtime binding for hibernation (#26) and before public
beta.**

The ordering matters. Hibernation gates open signup because it is unbounded
cost. The TUI gates whether the product's central claim survives contact with a
person who has five changes waiting and a deploy to get out — which is a slower
failure and a worse one, because nothing about it looks broken.

It is also, straightforwardly, the best demo we have. Thirty seconds of an agent
proposing a `DROP COLUMN`, the gate stopping it, and a human answering in a
terminal is the entire product in one recording — and `docs/business/MARKETING-PLAN.md`
§3 is built around that asset existing.
