# Decision: Assist is per-project, and isolation gates paid multi-tenancy

The open item from the external review (R2-01, with `/v1/suggest` downstream of
it). Recorded here because two of the three parts are decisions rather than
defects, and a decision that lives only in a chat log is one nobody can appeal.

## The decision

1. **Assist serves exactly one project per process.** Implemented.
2. **`/v1/suggest` stays unauthenticated, and its transport becomes the
   boundary.** Not implemented — see "what is not done".
3. **Isolation gates paid multi-tenancy.** Until the boundary matches the claim,
   ThetaBase is sold single-tenant.

## 1. Assist is per-project

Assist was one shared process on `127.0.0.1:7800` answering for every caller,
holding one cache. That shape is what made R2-02 possible: a cache key missing
the tenant leaked one project's row estimates to another, and **a shared cache is
only a cross-project surface because the process is shared**.

Keying the cache closed the leak. This closes the shape that produced it.

- `theta-assist` takes `--project` at startup. Required, no default — a default
  is a process that answers for whoever asks first.
- A question naming a different project is **refused**, not routed to another
  cache slot. Partitioning makes a misdirected agent quiet: it gets an answer,
  just a cold one. Refusing makes it loud and says where to go instead.
- `theta-assist` joins `SINGLE_PROJECT_CRATES` in
  `one_project_per_process.rs`, so a second project identifier appearing in the
  crate fails a test rather than a review.
- The project came **out** of the cache key. With one project per process the
  cache holds one project's entries because the process does, and a project id in
  the key would be a second copy of a guarantee that lives elsewhere. A planted
  violation removing it changed nothing, which is what a defence at the wrong
  layer looks like.

The alternative was to keep Assist shared and put a token on the route. That
authenticates a service that is still on the wrong side of the isolation
boundary — it answers the question "who is asking" while leaving "why is one
process holding two projects' derived data" untouched.

## 2. `/v1/suggest` stays unauthenticated, and that is not finished

A per-project Assist on loopback is **not sufficient today**, and this is the
part most likely to be read as done when it is not.

Loopback is not an OS-user boundary. Under the topology this repo builds —
projects separated by OS user and file permissions (`specs/01` §8.1) — another
project's process on the same host runs as a different user and *can still reach
`127.0.0.1:<port>`*. So a per-project Assist bound to a TCP port on loopback is
reachable by every other project on the machine. The refusal above stops a
misconfigured agent. It does not stop a determined one.

**What closes it:** the transport becomes a Unix domain socket owned by the
project's user, with permissions matching the data directory. Then the Assist
boundary is *the same boundary* as the data boundary — one mechanism, one thing
to get right, and no new authentication scheme to design, deploy and rotate.

That is why no token was added. A token would be a second, weaker boundary beside
a stronger one that is not yet plugged in. It lands with the isolation work
below, because it is the same work.

Stated plainly so nobody has to infer it: **on a shared host today, one project
can reach another project's Assist.** What it gets is a suggestion engine, not
data — Assist holds no database connection and never executes — but it can prime
and read a cache whose entries are derived from another project's statistics.

## 3. Isolation gates paid multi-tenancy

An OS-user boundary is real. It is not what "per-project isolated process/VM"
leads a customer to expect, and the specs now say which one is built.

The decision: **do not take a paid multi-tenant customer until the boundary
matches the claim.** Either

- build the per-project sandbox and the key-injection path, so the boundary is
  enforced by something other than file permissions; or
- sell single-tenant, where the question does not arise, and say so.

Both are honest. What is not honest is a shared host, an OS-user boundary, and a
spec that used to say process/VM.

`claims.toml` has been right about this all along — the non-claim
`in-process-isolation-is-checked-in-code-not-in-deployment` records that a
deployment really running two projects as two processes "is not checkable from
inside the repository" and that "a per-project sandbox that made the boundary
compiler-enforced is not built". The registry never overclaimed. The specs did,
and no longer do.

## What is not done

| | Status |
|---|---|
| Assist takes one project; foreign questions refused | Done |
| `theta-assist` under `one_project_per_process` | Done |
| Assist transport as a Unix socket owned by the project's user | **Not done** |
| Per-project sandbox (container/VM) for `thetad` | **Not done** |
| Key injection so the host never holds every project's data key | **Not done** |

The last three are one piece of work, and it is the piece the review called the
most consequential structural gap. It needs staging to exist before it can be
verified, and the reviewer's closing note says the same: a targeted engagement on
the deployment boundary once staging exists is worth more than re-attacking a
surface now confirmed twice.

## Why this is written down

Two of the three parts are choices, not repairs. The third — "do not sell
multi-tenant yet" — is a commercial constraint derived from a technical fact, and
those are exactly the constraints that quietly lapse when the person who set them
is not in the room. `DECISION-single-branch-multi-master.md` exists for the same
reason.
