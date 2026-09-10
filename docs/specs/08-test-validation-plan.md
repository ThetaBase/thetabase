# Test & Validation Plan

ThetaBase v1 — Written Before Build Starts, Not After

---

## 1. Principle

Given the stated approach — careful validation before release, not just fast shipping — this plan gates every major build milestone with a specific, falsifiable test, and no milestone is considered "done" until its gate passes. This doc should be treated as a build blocker list, not aspirational QA.

---

## 2. Consistency Verification (gates: Core Log + Branching milestone)

- **Jepsen-style adversarial consistency suite**: inject network partitions, node crashes, and concurrent conflicting writes across branches; verify every claim in the Data Model & Consistency Spec holds — specifically:
  - Read-your-writes never violated for the writing session.
  - CRDT-typed fields converge to the same state regardless of merge order (property-based test: generate random operation interleavings, assert convergence).
  - Non-CRDT conflicts are always surfaced explicitly, never silently resolved.
- **Gate**: 100% pass across a defined suite of interleaving scenarios (target: thousands of randomized property-based test runs, not a handful of hand-written cases) before Core Engine milestone is considered complete.

---

## 3. Agent-Safety Layer Adversarial Testing (gates: Safety Layer milestone)

- **Adversarial corpus**: a maintained, growing library of real and synthetic bad agent outputs —
  - Destructive schema changes disguised as benign (e.g., a rename that's actually a drop+recreate).
  - Runaway write loops (accidental and simulated-malicious).
  - Prompt-injected instructions attempting to get an agent to issue a destructive command framed as safe.
  - Bulk operations just under/over configured thresholds (boundary testing).
- **Gate**: zero unreviewed destructive changes reach a protected branch across the full corpus; every corpus entry has a recorded expected outcome (blocked / shadow-validated / auto-approved) and the suite is re-run on every Safety Layer code change (CI-gated, not manual).
- This corpus should keep growing post-launch as real incidents (or near-misses) are discovered — it's a living asset, not a one-time checklist.

---

## 4. Performance Validation (gates: Query Planner milestone)

- Benchmark suite covering the SLA targets (see SLA & Performance Spec) under realistic load profiles (mixed read/write, concurrent branch operations, cold-start provisioning).
- **Gate**: p50/p99 latency targets met with zero LLM calls observed on the typed read/write hot path (instrumented and asserted in CI, not just measured manually once).

---

## 5. Security Validation (gates: pre-GA)

- Independent penetration test of the identity/token issuance flow (scoping, revocation propagation time, cross-project isolation).
- **Gate**: no finding above a defined severity threshold left unresolved before any paid/production-tier launch.

---

## 6. Migration Correctness (gates: `eject` flow milestone)

- At least one real-world Postgres/Supabase project migrated end-to-end via `eject`, in CI, against a real Postgres.
- Automated schema-semantics verification pass (compare pre/post schema meaning, not just column names).
- The verification pass is itself tested adversarially: mismatches are planted deliberately — a row removed, a row invented, a value altered, a timestamp rounded, a column dropped — and each must be detected. A pass that reports "clean" unconditionally would satisfy a clean migration, so the clean migration alone proves nothing.
- **Gate**: zero data-meaning mismatches undetected by the verification pass on the pilot migration(s).

> Human review was the original sign-off here and has been replaced by the adversarial suite above. A reviewer reading a diff of a migration confirms that it looks right; the suite confirms the detector can tell when it is not, which is what the gate actually turns on.

---

## 7. Chaos & Recovery Testing (gates: pre-GA)

- Simulate each failure mode listed in the System Architecture Doc's Section 7 (node crash, archive unavailable, planner degradation, merge conflict, blocked destructive change) in a staging environment and confirm documented recovery behavior actually occurs — not just that it's described correctly in the runbook.

---

## 8. Ongoing Post-Launch Validation

- Safety Layer adversarial corpus: continuously expanded.
- Consistency suite: re-run on every core-engine change, not just at launch.
- Quarterly independent security review as the product and its attack surface grow.

---

## 9. Explicit Non-Negotiable

No milestone in the Development Timeline (post-build doc) should be marked complete without its corresponding gate in this document passing. If a gate can't be met on schedule, the schedule moves — not the gate.
