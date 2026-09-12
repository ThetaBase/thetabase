# ThetaBase developer entry points.
# Every target here is also what CI runs — if it passes locally it passes in CI.

# Windows names an executable with `.exe`, and `go build -o <path>` does not add
# it for you when the path is explicit. `sdk/conformance/harness.py` looks for
# the suffixed name, so without this the gate builds one binary and runs
# another — which it did, silently, for two weeks: a stale `conformance-go.exe`
# from an earlier session kept passing while every rebuild went to a file
# nothing executed.
EXE := $(if $(filter Windows_NT,$(OS)),.exe,)

.PHONY: help build test check fmt lint gates consistency adversarial hotpath wire query sla identity sdk sdk-check wasm conformance archive archive-live eject eject-live durability kms full proto clean

help:
	@grep -E '^[a-z-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

build: ## Build the whole workspace
	cargo build --workspace

test: ## Run every test
	cargo test --workspace

check: fmt lint test ## Everything CI runs

fmt: ## Check formatting
	cargo fmt --all --check

lint: ## Clippy, warnings are errors
	cargo clippy --workspace --all-targets -- -D warnings

# ---- validation gates (docs/specs/08-test-validation-plan.md) ---------------
# Each of these gates a milestone in docs/ROADMAP.md. A milestone is not done
# until its gate passes; if a gate cannot be met on schedule, the schedule moves.

# Run a cargo command only if the crate is a member of this workspace.
#
# The public repository is a clean export with the proprietary crates
# removed, so a gate that names one fails there with "package ID
# specification ... did not match any packages" -- a red badge for a guard
# that is working. This skips, loudly, naming what was not run and why.
#
# `cargo metadata --no-deps` rather than a directory check: a crate can be
# present on disk and not a workspace member, and it is membership that
# `-p` resolves against.
define if_member
@if cargo metadata --no-deps --format-version 1 2>/dev/null | grep -q '"name":"$(1)"'; then \
		echo "+ $(2)"; $(2); \
	else \
		echo "- skipped: $(1) is not in this workspace (proprietary; absent from the public export)"; \
	fi
endef

gates: consistency adversarial hotpath wire query sla identity sdk-check conformance eject assist platform ops claims ## Run every validation gate

# The fast gate is a convenience, never a substitute.
#
# `gates` deliberately leaves out anything that needs a live external service,
# so it stays quick enough to run on every change — a suite that takes minutes
# is a suite that stops being run. The cost is that `gates` can be green while
# a live path has not been exercised at all.
#
# So: nothing gets tagged before `full` is green. If a live suite skips for want
# of a credential, that is not a pass, and `full` says which one skipped.
full: gates archive-live eject-live durability kms ## Everything, including live services. Required before any tag.
	@echo
	@echo "full suite green — including live services. Safe to tag."

eject: ## Gate M8 — type mapping, value parsing, and the verification pass
	# The half that needs no database: what a Postgres type becomes, what a
	# rendered value parses to, and what the verification pass counts as a
	# mismatch. `eject-live` is the half that migrates a real one.
	cargo test -p theta-eject --lib

eject-live: ## Gate M8 — a real migration end to end (needs a Postgres)
	# Not in `gates`: it needs a database. THETA_REQUIRE_LIVE turns a skip
	# into a failure, for the same reason `archive-live` does — a suite that
	# skipped for want of a database reports the same green as one that passed,
	# and this is the suite the milestone's gate turns on.
	#
	#   see crates/theta-eject/tests/fixtures/shopdb.sql for the
	#   container command and the schema it expects.
	THETA_REQUIRE_LIVE=1 cargo test -p theta-eject --test live_migration -- --nocapture
	# Reported, never asserted: `specs/09` owns the published numbers, and a
	# latency assertion here would pin a claim to whichever machine ran it.
	THETA_REQUIRE_LIVE=1 cargo test -p theta-eject --test comparative_benchmark --release -- --nocapture

durability: ## Gate M8.5 — Control Plane state survives a restart (needs a Postgres)
	# SEC-1. Not in `gates`: it needs a database, and the failure it guards
	# against is specifically that the state was in memory - so testing it in
	# memory would be testing the bug.
	#
	#   docker run -d --name thetabase-cp-pg -p 55433:5432 	#     -e POSTGRES_PASSWORD=thetabase -e POSTGRES_USER=thetabase 	#     -e POSTGRES_DB=controlplane postgres:16
	$(call if_member,theta-control,THETA_REQUIRE_LIVE=1 cargo test -p theta-control --test restart_durability -- --nocapture)
	# The same argument for M9.5's state. The audit trail matters most here: an
	# append-only log a deploy empties is a log that says whatever happened
	# since the last deploy. Includes tampering with a row through raw SQL,
	# which is what an attacker with database access would actually do.
	$(call if_member,theta-control,THETA_REQUIRE_LIVE=1 cargo test -p theta-control --test platform_durability -- --nocapture)

kms: ## Gate M8.5 — the KMS wrapper, against a KMS (needs LocalStack)
	# Behind a feature, so `gates` does not make every build carry an AWS SDK.
	# LocalStack rather than real AWS: what is under test is our use of the API,
	# and that needs neither Amazon's bill nor Amazon's uptime. What it cannot
	# tell us is whether IAM is right in a real account, which is a deployment
	# question a test could never have answered.
	#
	#   docker run -d --name thetabase-kms -e SERVICES=kms -p 4566:4566 	#     localstack/localstack:3
	$(call if_member,theta-control,THETA_REQUIRE_LIVE=1 cargo test -p theta-control --features kms --test kms_wrapper -- --nocapture)

archive: ## Cold archive logic, against a backend that misbehaves on demand
	cargo test -p theta-archive

archive-live: ## Cold archive against the real AT-1 service (needs `at1 login`)
	# Not in `gates`: it hits an external service and takes minutes. Run it
	# deliberately, and before claiming the cold archive works — everything in
	# `archive` runs against a fake, which cannot tell us whether the commands
	# this crate issues still do what the CLI's docs say.
	#
	# THETA_REQUIRE_LIVE turns a skip into a failure. A suite that skipped for
	# want of a credential reports the same green as one that passed, which is
	# exactly the confusion that lets a release go out on an unexercised path.
	THETA_REQUIRE_LIVE=1 cargo test -p theta-archive --test live_at1 -- --ignored --nocapture

consistency: ## Gate M1 — convergence, durability, crash recovery, and partitions
	cargo test -p theta-core --test convergence
	cargo test -p theta-storage --test convergence_durable
	cargo test -p theta-storage --test crash_consistency
	cargo test -p theta-storage --test durability
	cargo test -p theta-storage --test durable_store
	cargo test -p theta-storage --test merge
	# `specs/03` §3.1 and §3.3: read-modify-write has a precondition, and the
	# same loop without one provably loses updates. In the gate because the
	# second half is what makes the first half evidence — a total order that
	# nothing contends is not a claim about contention.
	cargo test -p thetad --test conditional_write
	# The partition half of the M1 gate, and the M2 entry that named the same
	# fault-injecting transport. Faults are injected in front of the server, so
	# the production read/write path carries nothing that exists only for tests.
	cargo test -p thetad --test network_partitions

adversarial: ## Gate M5 — no unreviewed destructive change reaches a protected branch
	cargo test -p theta-safety --test adversarial_corpus
	cargo test -p theta-safety --test audit_encryption
	cargo test -p thetad --test safety_gate
	cargo test -p thetad --test shadow_lifecycle
	cargo test -p thetad --test policy_authority
	cargo test -p thetad --test adversarial_sequences
	# The breaker's ceilings are calibrated against a corpus rather than chosen.
	# In the gate so a changed default has to face the same evidence.
	cargo test -p theta-safety --test breaker_calibration

hotpath: ## Gate M3 — zero LLM calls on the typed read/write path
	cargo test -p thetad --test no_llm_on_hot_path

sla: ## Gate M3 — latency targets, measured over a real socket
	# Release build: a debug build measures rustc's bounds checks, not the engine.
	cargo test -p theta-scribe --test sla --release -- --nocapture
	# The cost model's constants, checked against a clock. Here rather than in
	# `query` because it is a timing test and skips itself in a debug build —
	# running it there would report a green that measured nothing.
	cargo test -p theta-query --test cost_calibration --release -- --nocapture
	# What encryption at rest costs per record (SEC-2). Here because it is a
	# timing test, and because the end-to-end SLA cannot answer the question:
	# a put is dominated by its fsync, and host noise dwarfs the cipher.
	cargo test -p theta-storage --test seal_cost --release -- --nocapture
	# What branching costs, on the two axes Columbia's BranchBench names.
	# Here because it is a timing test; in the gate because the read-depth
	# result is a claim the site makes and the fork cost is a limit it has to
	# state next to it. See docs/COMPETITION.md §4 for why BranchBench itself
	# cannot be run against ThetaBase.
	cargo test -p theta-storage --test branch_cost --release -- --nocapture

query: ## Gate M3 — executor, SQL subset, planner, and optimizer
	cargo test -p theta-query

wire: ## Gate M2 — wire conformance, negotiation, and the RPC surface
	cargo test -p theta-proto
	cargo test -p thetad --test server
	cargo test -p theta-scribe --test edge_runtime
	# The transport under the framing. Here rather than in `identity` -- which
	# also runs `theta-cli` -- because encryption in transit is a property of
	# the wire, and the claim registry pins it to `specs/02 §Encryption in
	# transit`. The clients shipped with no TLS at all precisely because no
	# gate owned this.
	cargo test -p theta-cli --test data_transport

identity: ## Gate M4 — token isolation, OAuth, and revocation propagation
	cargo test -p theta-identity
	$(call if_member,theta-control,cargo test -p theta-control)
	cargo test -p theta-cli
	cargo test -p thetad --test revocation
	# M7 rides here: branch-per-PR is identity-adjacent, and its perimeter is
	# the same kind of claim — an unauthenticated caller must reach nothing.
	$(call if_member,theta-control,cargo test -p theta-control --test github_webhook)
	$(call if_member,theta-control,cargo test -p theta-control --test preview_lifecycle)
	$(call if_member,theta-control,cargo test -p theta-control --test github_rest)
	# Isolation at rest, which is the same claim as token isolation one layer
	# down (`specs/04` §3, SEC-2): one project's key opens one project's store
	# and is refused by every other.
	cargo test -p theta-storage --test encryption_at_rest
	# SEC-8's three source-level guards, for the properties no behavioural test
	# can distinguish — constant-time comparison being the clearest.
	cargo test -p thetad --test recorded_positives

# ---- SDKs & protocol -------------------------------------------------------

wasm: ## Build Scribe's WebAssembly core, which every SDK sits on
	# Size matters here in a way it does not elsewhere: this module ships to the
	# caller's app runtime, so it is a number every user downloads.
	rustup target add wasm32-unknown-unknown
	cargo build -p theta-scribe-wasm --target wasm32-unknown-unknown --profile wasm
	@ls -l target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm | awk '{print "scribe core:", $$5, "bytes"}'

conformance: ## Gate M6 — every SDK, a live thetad, identical output
	$(MAKE) wasm
	cargo build -q -p thetad --example conformance_server
	cd sdk/typescript && npm ci --silent && npx tsc -p tsconfig.json
	cd sdk/go && go build -o ../../target/conformance-go$(EXE) ./cmd/conformance
	cargo build -q -p thetabase --example conformance
	# `build-classpath` writes the resolved jar list where the harness can read
	# it. Maven knows where its local repository is and what the SDK actually
	# resolved to; a hand-built path would be a second answer to that.
	cd sdk/java && mvn -B -q compile dependency:build-classpath -Dmdep.outputFile=target/classpath.txt
	cd sdk/csharp && dotnet build conformance -v q --nologo
	# Ruby needs no build step; the runner is the source. Named here anyway so
	# the target lists every binding the harness will try to run.
	ruby -c sdk/ruby/conformance.rb
	# On Windows this needs the MSVC environment and SDKROOT; see
	# sdk/swift/README.md. macOS and Linux need neither.
	cd sdk/swift && swift build
	python3 sdk/conformance/harness.py
	# The harness drives the protocol core through each host shim. This drives
	# the `Theta` classes a customer actually holds, which no runner touches —
	# without it the clients can be wired and broken at the same time, and were.
	python3 sdk/conformance/client_smoke.py
	# Adversarial probes. `specs/04` section 7 requires a penetration test of
	# token minting and scoping before a production launch; this is the
	# mechanical part of it, not a substitute for the outside review it also asks
	# for.
	python3 sdk/conformance/red_team.py

sdk: ## Regenerate the SDK bindings from the wire schema, then typecheck them
	cargo run -q -p theta-codegen
	cd sdk/typescript && npm ci --silent && npx tsc -p tsconfig.json --noEmit
	cd sdk/python && python3 -c "import sys; sys.path.insert(0, 'src'); import thetabase; print('python sdk ok')"
	cd sdk/go && go build ./... && go build -o ../../target/conformance-go$(EXE) ./cmd/conformance
	cd sdk/java && mvn -B -q compile dependency:build-classpath -Dmdep.outputFile=target/classpath.txt
	# Rust needs no generated file: `theta-proto` already produces the wire
	# types from the same schema, and a second copy is the drift this target
	# exists to prevent. So the Rust SDK's check is that it builds and tests.
	cargo build -q -p thetabase --example conformance
	cd sdk/csharp && dotnet build conformance -v q --nologo
	cd sdk/swift && swift build

sdk-check: ## Gate M6 — the SDK bindings match the wire schema
	# A schema change that was not regenerated fails here rather than shipping.
	# An SDK describing a protocol the server no longer speaks is worse than no
	# SDK, because it looks like it works.
	cargo run -q -p theta-codegen -- --check
	# The generator's own tests, which nothing ran until the Go binding was
	# added. One of them had been red since M10.5 renamed a union field: the
	# gate typechecked the *bindings* and never checked the generator's claims
	# about them, so an assertion could rot without anything noticing.
	cargo test -q -p theta-codegen
	cd sdk/typescript && npm ci --silent && npx tsc -p tsconfig.json --noEmit
	cd sdk/python && python3 -c "import sys; sys.path.insert(0, 'src'); import thetabase; print('python sdk ok')"
	# `gofmt -l` prints any file that is not canonically formatted, and prints
	# nothing otherwise — so the `[ -z ]` is what turns it into a check. The
	# generated file is included deliberately: the emitter reproduces gofmt's
	# column alignment rather than shelling out to it, and this is what keeps
	# that true.
	cd sdk/go && [ -z "$$(gofmt -l .)" ] || (gofmt -l . && echo "run gofmt -w sdk/go" && false)
	cd sdk/go && go vet ./... && go test ./...
	cargo test -q -p thetabase
	cd sdk/java && mvn -B -q test
	cd sdk/csharp && dotnet test tests -v q --nologo
	ruby sdk/ruby/test/query_test.rb
	cd sdk/swift && swift test

assist: ## Gate M9 — Assist is separate, cannot execute, and its output is safe by construction
	# The hot-path guard is the load-bearing half: it now names `theta-assist`
	# as a forbidden dependency, so wiring Assist into `thetad` fails here
	# rather than in review.
	cargo test -p thetad --test no_llm_on_hot_path
	$(call if_member,theta-assist,cargo test -p theta-assist)
	# Assist's own latency, measured with a scripted model so the number is
	# about this service rather than about a third party. `specs/09` §2 budgets
	# 200ms p50; a cold model call cannot meet that and does not claim to.
	$(call if_member,theta-assist,cargo test -p theta-assist --test service -- --nocapture)
	# Both SDKs against a live Assist, agreeing on one candidate. Two clients
	# that each pass their own tests prove nothing about whether they agree,
	# and "it works in Python" is how that reaches a user. Scripted model, so
	# this measures the SDKs and needs no API key.
	$(call if_member,theta-assist,cargo build -q -p theta-assist --example assist_scripted)
	cd sdk/typescript && npm ci --silent && npx tsc -p tsconfig.json
	python3 sdk/conformance/assist_harness.py

platform: ## Gate M9.5 — platform authority is separate, bounded, and recorded
	# Three properties, swept over the HTTP surface rather than spot-checked:
	# no tenant credential reaches any platform route, no tenant row data
	# without a recorded grant, and every action - including every refusal -
	# lands in a trail with no route that could edit it.
	$(call if_member,theta-control,cargo test -p theta-control --test platform_isolation)
	$(call if_member,theta-control,cargo test -p theta-control --lib platform)
	$(call if_member,theta-control,cargo test -p theta-control --test deployment_isolation)

ops: ## Gate M10 — the automated half of each failure mode in specs/01 §7
	# The manual half needs a staging environment and is not met; see
	# docs/RUNBOOKS.md, where every entry says which of the two it is.
	#
	# Gathered here rather than left scattered across five crates, because
	# "recovery works" is one claim and reading it out of five separate suites
	# is how a regression in one of them goes unnoticed.
	cargo test -p theta-storage --test crash_consistency
	cargo test -p theta-storage --test tamper_evidence
	cargo test -p theta-archive --test custodian_release
	cargo test -p theta-archive --lib
	# Restore drills (M20). Filed here rather than under `archive` because these
	# need no live service: the fake backend produces the failures a real
	# archive cannot be asked for on demand, and a gate that only runs when
	# somebody has AT-1 credentials is a gate that proves nothing on most
	# commits.
	cargo test -p theta-archive --test drill
	# External anchoring, entry signatures and the continuous verifier (M20).
	# The same argument: "we would have detected tampering" is a recovery claim,
	# and it belongs with the other recovery claims rather than in a suite of
	# its own that nobody thinks to run.
	cargo test -p theta-storage --lib anchor
	cargo test -p theta-storage --lib signing
	cargo test -p theta-storage --lib verifier
	# Inclusion proofs (M25). A client checking a result against a root it
	# trusts is a recovery-and-evidence claim, and it composes with the anchor
	# tests above rather than standing alone.
	cargo test -p theta-storage --lib inclusion
	# The bounded model check of convergence (M25). Exhaustive below its bound
	# rather than sampled, and fast enough to run on every commit — which is the
	# only reason it is a gate rather than a nightly.
	cargo test -p theta-storage --test convergence_model
	cargo test -p theta-safety --test adversarial_corpus

claims: ## Gate M11.5 — nothing on the site outlives the spec that stated it
	# Every customer-facing claim is pinned to a verbatim sentence in a spec and
	# to the tests that prove it. Reword the spec and this fails, which is the
	# whole mechanism: the alternative is a review step, and the review step
	# already failed once - two entries in the design concept's "does not claim"
	# list went stale within a day of M10.5 landing.
	#
	# `planted.rs` is the half that matters: it feeds the checker each failure
	# it exists to catch and requires it to be reported. A checker whose section
	# parser silently matched the whole file would pass `registry.rs` just as
	# loudly as a correct one.
	$(call if_member,theta-claims,cargo test -p theta-claims)
	# The two files a model reads, regenerated so the gate covers what a site
	# build would publish rather than what happens to be committed. Both are
	# derived from claims.toml and docs/specs/ — a hand-maintained llms.txt is
	# a second copy of the claims, and the copy a model reads is the worst one
	# to let drift.
	$(call if_member,theta-claims,cargo run -q -p theta-claims -- emit-llms > /dev/null)
	$(call if_member,theta-claims,cargo run -q -p theta-claims -- emit-llms-full > /dev/null)
	# `release_guard.rs` rides in this target: nothing in the tree may be
	# publishable, or claim a licence, while docs/DISTRIBUTION.md records the
	# IP decision as pending. A publish is a public disclosure, and a public
	# disclosure ends patent rights outside the US on the day it happens.

bench-comparative: ## Branch cost against Dolt and PostgreSQL (needs Docker)
	# Outside `gates` for the reason `archive-live` is: it needs external
	# services. Run it deliberately, and read docs/COMPETITION.md §4a before
	# quoting any number out of it — the supported claim is narrow and two of
	# the four axes are losses.
	#
	#   docker run -d --name thetabase-bench-pg -p 55440:5432 	#     -e POSTGRES_PASSWORD=bench -e POSTGRES_USER=bench -e POSTGRES_DB=bench postgres:16
	#   docker run -d --name thetabase-bench-dolt -p 55441:3306 	#     -v $$PWD/bench/comparative/dolt-init.sql:/docker-entrypoint-initdb.d/init.sql 	#     dolthub/dolt-sql-server:latest
	$(MAKE) wasm
	cargo build -q -p thetad --example conformance_server
	python3 bench/comparative/branch_compare.py --json bench/comparative/results/latest.json

proto: ## Check the wire schema compiles and its bindings build
	# Bindings are generated in-process by theta-proto's build script, so this
	# needs no capnp toolchain. The `capnp compile` below is a second opinion
	# from the reference implementation, and is skipped when it is absent.
	cargo build -p theta-proto
	@command -v capnp >/dev/null \
		&& capnp compile -o- crates/theta-proto/schema/theta.capnp > /dev/null \
		&& echo "schema ok (validated against reference capnp)" \
		|| echo "schema ok (bindings built; reference capnp not installed)"

clean:
	cargo clean
