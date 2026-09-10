# Decision: FSL-1.1-MIT for the engine, Apache-2.0 for the SDKs, proprietary for the Control Plane

Taken 2026-08-28, before filing, as `DISTRIBUTION.md` §0 requires. That ordering
is the whole point: Apache-2.0 §3 contains an express patent grant, so choosing
Apache for the engine and *then* filing would be filing on something already
given away.

`DISTRIBUTION.md` §2 recommended the shape — source-available engine, proprietary
Control Plane, permissive SDKs. This fixes the specifics it left open.

## The decision

| Component | Licence | Converts to |
|---|---|---|
| `theta-core`, `theta-storage`, `theta-query`, `theta-safety`, `theta-proto`, `thetad`, `theta-archive`, `theta-identity`, `theta-embed`, `theta-scribe` | **FSL-1.1-MIT** | MIT, two years after each version's release |
| `theta-cli`, all seven SDKs, `theta-scribe-wasm` | **Apache-2.0** | — |
| `theta-control`, `theta-assist` | **Proprietary.** Not published. | — |

## Why FSL rather than BUSL 1.1

Both stop the one failure mode that matters: a hyperscaler running the engine as
a service, capturing the customers, contributing nothing.

**FSL defines competing use for you.** BUSL 1.1 requires drafting your own
"Additional Use Grant", and that clause is where BUSL adopters get into trouble —
it is the part a lawyer has to write and a court has to read. FSL's restriction
is fixed text: you may not provide the software to third parties as a hosted or
managed service that gives them a substantial set of its features. That is
exactly the line we want and none of the drafting risk.

**Two years reads as confidence; four reads as a moat.** The product's central
claim is a *safety* claim, and safety claims are bought by people who read the
code. A two-year delay on commoditisation is a business decision a buyer will
accept. Four years starts to look like enclosure with a timer on it.

**BUSL carries baggage FSL does not, yet.** Post-HashiCorp, "BUSL" triggers a
specific and negative reaction in exactly the developer communities this product
needs. That is not a legal argument. It is a distribution one, and distribution
is the thing a licence is for.

## Why MIT and not Apache-2.0 as the conversion target

This is the part that is genuinely contested, and it was decided knowingly.

FSL ships in two variants. **FSL-1.1-Apache-2.0** converts each version to
Apache-2.0 after two years; **FSL-1.1-MIT** converts to MIT. The difference that
matters is not permissiveness — both are permissive — it is that **Apache-2.0 §3
grants patent rights and MIT does not.**

Choosing Apache would mean every released version grants a royalty-free patent
licence to its recipients two years later, on a rolling basis. That is not a
theoretical concern: it is precisely the mechanism `DISTRIBUTION.md` §0 identifies
as the reason not to pick Apache before filing. Picking it as a conversion target
does the same thing with a two-year delay.

**Decision: the patent is held indefinitely.** MIT it is.

**What that costs, stated plainly.** A downstream user of converted code gets
copyright permission and no patent permission. Sophisticated buyers notice this,
and some will ask. The honest answer is the one above: the patent protects the
hosted service and provides a defensive position, and the licence is not the
instrument being used to grant it. A separate, explicit patent pledge or grant
can be issued later if that trade turns out to cost more than it protects —
issuing one is easy, and un-issuing one is not, which is the right way round.

## Why the SDKs and CLI stay Apache-2.0

Anything a customer links into their own application should carry no licensing
question at all. The SDKs are generated bindings and thin clients; there is
nothing in them worth protecting and a great deal to lose by making a developer
think about it. Apache-2.0's patent grant is *appropriate* here — it is what
makes the code safe to depend on.

`theta-scribe-wasm` joins them for the same reason: it runs inside the customer's
process.

## Why `theta-assist` is proprietary and not source-available

`DISTRIBUTION.md` §2 listed only `theta-control` as proprietary. Assist is added
here. It is now one process per project
(`DECISION-assist-and-isolation.md`), it holds no database connection, and it is
a thin orchestration around a third-party model — there is no auditability
argument for publishing it, and it is the component most trivially reimplemented
by a competitor as a wrapper around the same models. It is the least defensible
thing in the tree and the least useful to a customer reading source.

## Open before this can be applied — found 2026-09-10

The strategy above survives review. The *table* does not yet match the
dependency graph, and applying it as written would put a false statement in a
manifest.

**1. Every Apache-2.0 component links FSL code.**

| Apache-2.0 crate | FSL crates it depends on |
|---|---|
| `theta-cli` | `theta-core`, `theta-identity`, `theta-proto` |
| `theta-scribe-wasm` | `theta-core`, `theta-proto` |
| `thetabase` (Rust SDK) | `theta-core`, `theta-proto`, `theta-scribe` |

A Rust artefact that links FSL crates is a combined work. Stamping
`license = "Apache-2.0"` on it tells a recipient they have Apache rights to what
they received, and they do not — FSL's competing-use restriction still binds the
linked engine.

**The fix is to move the client-side crates to Apache-2.0, not to relabel the
binaries.** `DISTRIBUTION.md` §0 already says which those are: "the wire
protocol, the SDKs, the log format, the query IR — those want adoption, and
adoption is the opposite of exclusion." `theta-proto` *is* the wire protocol and
`theta-scribe` *is* the client. Neither is the engine, and neither is what the
provisional claims. Putting them under FSL was a slip of the pen against this
document's own reasoning.

That leaves `theta-core` and `theta-identity`, which need a deliberate answer
rather than a default: either they are genuinely engine (and then nothing
linking them can be called Apache), or the parts the client needs are a separate
crate. **This is the one piece of design work the licence decision still owes.**

**2. `theta-cli` declared a dependency on the proprietary `theta-control`.**
Unused — zero references in its source — and now removed. Had it been real, the
CLI could not have been published at all. Worth stating because it is the shape
of the failure to watch for: a `path` dependency costs nothing to add and
silently decides what may ship.

**3. The MIT conversion target quietly commits us to the non-provisional.**
"The patent is held indefinitely" is the entire reason MIT was chosen over
Apache-2.0. The provisional lapses **10 September 2027**. Convert it, or the
cost of MIT — sophisticated buyers noticing there is no patent grant — is being
paid for a patent that no longer exists.

**4. The defensive patent pledge is still unwritten.** `DISTRIBUTION.md` §0
recommends publishing one *alongside* the licence and says explicitly: "write it
at the same time as the licence, not later — a pledge that arrives after the
complaint is a concession rather than a position." It is not in the list below.

**Verified, not a problem:** `FSL-1.1-MIT` is a registered SPDX identifier
(spdx.org/licenses/FSL-1.1-MIT.html), so Cargo's `license` field accepts it and
no `license-file` fallback is needed.

## What happens next, in order

1. **This decision does not open the gate.** `release-state:
   ip-protection-pending` stands in `DISTRIBUTION.md` until the provisional is
   filed, and `release_guard.rs` keeps every crate unpublishable until it changes.
   The licence being *chosen* and the software being *shipped* are two events and
   the guard tracks the second.
2. **Do not write the licence into any manifest yet.** `release_guard.rs` fails on
   any manifest that declares an open-source licence while the state is pending,
   and it is right to: a `license = "MIT"` field in a published crate is a
   distribution, whatever the intent.
3. **After filing**: add `LICENSE-FSL` and `LICENSE-APACHE` at the root, set each
   manifest's `license-file` or `license` field per the table above, write the
   change date into the licence text, and flip the release state.

The conversion date goes in the licence text rather than a promise on a website.
Nobody should have to trust us for it.
