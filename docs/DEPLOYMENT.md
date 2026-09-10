# Deployment

Target: **Fly.io**, with `thetad` as one auto-stopping Machine per project and
the Control Plane as an always-on app. **Neon** for the Control Plane's
Postgres. **AWS KMS** kept as the root of trust. Object storage on Tigris or S3.

This document is the step-by-step. `STAGING.md` is the *order* things must come
up in and why; this is how to do it. Where they disagree, `STAGING.md` is the
argument and this is the mechanics.

---

## Why Fly, and why the previous answer was wrong

This guide targeted AWS ECS Fargate until the cold start was measured. The
reasoning then was that the code already assumed AWS — `secrets_kms.rs` is AWS
KMS, the archive is `aws_sdk_s3` — and that moving would mean reimplementing the
root of trust.

**That overstated it.** `theta-archive/src/s3.rs` already takes an endpoint
override "for anything that is not AWS", so any S3-compatible store works
unchanged. KMS is an optional feature behind a `KeyWrapper` seam with two
implementations. One crate touches the S3 SDK. The AWS coupling was a seam, not
a foundation.

**And it missed the thing that decides it.** Hibernation — stopping the process
behind an idle project — is what makes the cost model work: about $0.15 a month
for a stopped machine against $2 for a running one. It only works if waking is
fast enough that nobody notices.

| | Cold start |
|---|---|
| Fly, resume a stopped Firecracker machine | **200–500ms** |
| ECS Fargate `RunTask` | **10–90s; 30–45s typical** |

The difference is what "stopped" means. A stopped Fly Machine keeps its rootfs
and resumes. A stopped ECS task does not exist — waking it means scheduling onto
capacity, attaching an ENI (10–30s by itself) and re-pulling the image.

`09-sla-performance.md` §2 budgets 500ms p50 / 1.5s p99. Fargate misses it by
20–90×, so on AWS the honest options were "never hibernate" — $27 per project,
which is what makes a free tier unaffordable — or a warm pool, which does not
help the free tier where the idle projects are.

**Fly Machines are Firecracker microVMs**, the same isolation primitive Fargate
gives, so the boundary external review R2-01 asked for is unchanged: two
projects are two microVMs with separate kernels.

### What this costs us

Fly has had public reliability incidents and AWS has not, in the way that
matters. We are selling a database, so that is a real trade rather than a
detail. What makes it acceptable: durability is the log in object storage rather
than the machine, `theta eject` means no customer is trapped, and we are not
publishing an availability figure until a production quarter has been measured
(`specs/09` §6). Revisit if enterprise compliance becomes a year-one target —
`MARKETING-PLAN.md` §1.5 says it is not.

---

## What you need before step 1

- A **Fly** account and `flyctl`.
- A **Neon** project, for the Control Plane's Postgres. Its free tier is 0.5GB
  and covers this for a long time — the Control Plane stores the org graph,
  projects, instances, tokens and billing. Customer data lives in `thetad`
  instances, not here.
- An **AWS** account, for KMS and (optionally) S3. Far narrower than before: one
  key and one bucket, no VPC, no ECS, no ALB, no NAT.
- A **domain**, and one **GitHub OAuth app**.
- `docs/RUNBOOKS.md` open in another window. Step 8 walks it.

Region: pick one and keep the archive, the KMS key and the machines together.

### Two code gaps that stop this guide working today

Checked 2026-09-10, against the tree rather than against this document. Neither
is about credentials, and no amount of `fly secrets set` gets past them.

**1. The binary cannot select the Fly runtime.** `fly.rs` compiles, the `fly`
feature exists, and `FlyRuntime` implements `Runtime` — but `main.rs` never
calls `.with_runtime(...)`, so a deployed Control Plane runs the default that
`Provisioner::new` installs: `ExternallyManaged`, whose `start`, `stop`,
`release` and `restore` are all `Ok(())`. It is honest in the topology it was
written for, where there are no tasks to start. On Fly it means **provisioning a
project reports success and starts nothing.** Wiring is one constructor call and
a feature-gated branch; the reason it is listed here rather than done quietly is
that it changes what a successful provision means.

**2. Keyset delivery assumes a shared filesystem.** `deliver_keyset` writes
`keys.json` into a directory under `THETA_INSTANCE_ROOT` — correct when the
Control Plane and `thetad` share a disk, which is the topology this predates
Fly. On Fly, `thetad` is a separate Firecracker machine with its own volume at
`/var/lib/thetabase`, and the Control Plane cannot write into it. The keyset
would land in the Control Plane's own container and be read by nobody.
`KEYSET_FILE`'s own doc comment names the symptom: *"a mismatch would surface
only as an instance that refuses everything."*

`fly.rs` already points at the fix — "Fly secrets are per-app, and each
instance is its own app" — so the keyset goes in as a secret at machine
creation rather than as a file. That is a real change to `provision.rs`, not a
config edit.

**Also true, and said plainly in `fly.rs`'s module docs:** nothing behind the
`fly` feature has ever run against the live API. Its two live tests are
`#[ignore]`.

**Fixed while checking:** all three Dockerfiles copied `Cargo.toml`, `Cargo.lock`
and `crates/` but not `sdk/rust`, which is a workspace member — so every image
build failed at manifest load, before compiling anything.

---

## 1. Postgres

Create a Neon project and take the connection string. Nothing else to configure
— no VPC, no subnet group, no parameter group.

**Verify before going further.** `STAGING.md` is emphatic and right: if Control
Plane state does not survive a restart, nothing above it is worth deploying.

```sh
THETA_CONTROL_PG='postgres://…' THETA_REQUIRE_LIVE=1 \
  cargo test -p theta-control --test restart_durability
```

That test is SEC-1. Failing here is the cheapest place it will ever fail.

---

## 2. KMS

One customer-managed key, rotation on. This is the only AWS resource the
Control Plane needs.

```sh
aws kms create-key --description "ThetaBase root of trust" \
  --key-usage ENCRYPT_DECRYPT --key-spec SYMMETRIC_DEFAULT
aws kms enable-key-rotation --key-id "$KEY_ID"

THETA_KMS_KEY_ID="$KEY_ID" make kms
```

**Not one key per project.** Each project gets its own 32-byte data key,
generated by the Control Plane and stored wrapped under this one — so every
project has distinct key material and AWS bills for one key rather than
thousands. Per-project master keys would put a price on the thing we most want
customers to create freely.

Calls are rare — once per project per key change, never on a data path — so
reaching AWS from Fly costs nothing that matters.

`STAGING.md` §2 makes the point: LocalStack cannot tell you whether IAM is right
in a real account, and IAM is the only question here.

---

## 3. Object storage

Either works. `theta-archive` takes an endpoint override, so this is
configuration rather than code.

**Tigris** (built into Fly, S3-compatible):

```sh
fly storage create
```

**Or S3:**

```sh
aws s3api create-bucket --bucket thetabase-archive-prod \
  --create-bucket-configuration LocationConstraint="$AWS_REGION"
aws s3api put-bucket-versioning --bucket thetabase-archive-prod \
  --versioning-configuration Status=Enabled
aws s3api put-public-access-block --bucket thetabase-archive-prod \
  --public-access-block-configuration "BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true"
```

**Review finding R2-03 is not fixed by either.** The archive credential scopes
to the whole bucket, with projects separated by key prefix only. The fix is a
per-project credential whose policy carries a condition on the prefix. Do it
when you provision the first project rather than deferring it.

---

## 4. The Control Plane

```sh
cd deploy/fly
fly launch --config control.toml --no-deploy
fly secrets set \
  THETA_CONTROL_DATABASE_URL='postgres://…' \
  THETA_GITHUB_CLIENT_ID='…' \
  THETA_CONTROL_KMS_KEY_ID='arn:aws:kms:…' \
  AWS_ACCESS_KEY_ID='…' AWS_SECRET_ACCESS_KEY='…'
fly deploy --config control.toml
```

`control.toml` sets `min_machines_running = 1` and does **not** auto-stop.
Everything depends on the Control Plane being reachable — an instance that
loses the revocation poll stops serving in-flight traffic after 15 seconds, so a
Control Plane cold start would be a fleet-wide outage rather than one slow
request.

**Confirm encryption from the log, not from this document.** Startup emits one
of two lines and `STAGING.md` §3 is right that which one must be answerable
from the log:

```
encryption at rest is enabled; new segments will be sealed with the project data key
```

or, if you forgot the key:

```
encryption at rest is NOT enabled: set THETA_DATA_KEY to seal segments and the
view snapshot. Until it is, archived segments leave this host in cleartext
```

Two more warnings are worth reading rather than skipping. Without
`THETA_STRIPE_SECRET_KEY` paid plans are refused rather than given away;
without `THETA_BILLING_WEBHOOK_SECRET` billing webhooks are refused, so a
failed payment never reaches the dunning ledger. Both are fine before billing is
live and both say so at `warn`.

---

## 5. The first project

Provision it **through the Control Plane**, not by hand. `STAGING.md` §4 is
explicit and the reason is that provisioning is the path under test; a hand-made
project tests nothing and hides whatever provisioning gets wrong.

```sh
curl -sX POST https://control.yourdomain/v1/projects \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -d '{"orgId":"org_…","name":"first"}'
```

The Control Plane creates a Fly app per project from `deploy/fly/project.toml`
and starts one Machine. Per project you get:

- one `thetad` Machine, its own Firecracker microVM, its own volume at
  `/var/lib/thetabase`
- one Assist Machine, started with `--project <id>` (**required, no default** —
  see `DECISION-assist-and-isolation.md`), auto-stopping on the same terms
- a wrapped data key, and object-storage credentials scoped to this project's
  prefix

### Hibernation is configuration, not code

`project.toml` carries `auto_stop_machines = "stop"` and
`min_machines_running = 0`. That is the whole of what #26 was going to build
against ECS: the proxy watches connections, stops the Machine when there are
none, and starts it on the next one while holding the client.

**Production projects get two settings changed** — `auto_stop_machines = false`
and `min_machines_running = 1` — which is the same distinction
`Hibernation::may_hibernate` makes in code and the same one the plans bill on. A
customer is charged for always-on behaviour exactly when they get it.

---

## 6. Cold start, and what to expect

Measured, and it has two halves:

| | |
|---|---|
| Platform, resuming a stopped Machine | 200–500ms |
| `Engine::open` | ~1.3µs per row — 13ms at 10k rows, 131ms at 100k |

So a hibernating development project wakes well inside the 500ms p50 budget. A
project with a million rows would take about 1.3 seconds of our half alone,
which is why `specs/09` §2 now qualifies the row by size rather than stating a
flat number.

This does not get worse over time: recovery tracks a project's *current size*,
not its history. The same rows rewritten five times recover no more slowly
(measured at 0.67×) because the view is identical. `crates/thetad/tests/cold_start.rs`
holds all three properties.

---

## 7. The platform superuser

```sh
cargo run -p theta-control --example platform_admin -- bootstrap
```

Load the directory row and open a session end to end. `STAGING.md` §5: an admin
who cannot authenticate is otherwise discovered during an incident, which is the
one time you need them.

---

## 8. Walk the runbooks

`docs/RUNBOOKS.md`, in order. Do not skip the archive round trip — a backup
nobody has restored is a backup nobody has, and `TERMS.md` §2.2 says verifying a
restore is the customer's responsibility, which is only fair if ours works.

---

## What is missing, and you will feel it

**Metrics.** M10's open item, and `STAGING.md` lists it as the one piece that
does not exist. There is no dashboard and no alerting beyond
`GET /platform/v1/economics`, which reports duty cycle and cold-start latency
with provenance. Deploy without more for an alpha; do not run a public beta on
it. The first things you will want are breaker trips, review-queue depth, gate
distribution, and revocation-list age — that last one because of the next
paragraph.

**Revocation staleness drops live connections.** Since R1-01, an instance that
loses its Control Plane heartbeat stops serving after
`REVOCATION_STALENESS_LIMIT_MS` (15s). Fail-closed is correct and it means a
Control Plane outage becomes a data-plane outage in fifteen seconds. Alert on
revocation-list age *before* it reaches the limit, and choose that 15s
deliberately rather than inheriting it.

**No IaC.** Everything above is CLI. Worth doing before the second environment
exists, not after — though `fly.toml` files in `deploy/fly/` are already most of
it, which is one more thing this target makes cheaper.

---

## Rollback

```sh
fly releases --app thetabase-control
fly deploy --config control.toml --image registry.fly.io/thetabase-control:<previous>
```

Tag images with the commit, never `latest`. A deployment you cannot name is a
deployment you cannot roll back, and the first time that matters is the worst
time to find out.
