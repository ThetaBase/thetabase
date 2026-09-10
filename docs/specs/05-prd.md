# Product Requirements Document

ThetaBase v1 — The Agent-Native Database

---

## 1. Problem Statement

Most databases were designed on the assumption that a human authors migrations, reviews queries, and notices dangerous operations before they run. That assumption is already false for most vibe-coded projects: an AI agent (Claude Code, Cursor, etc.) is writing most of the schema changes and queries, largely unreviewed. Existing tools (Supabase, Firebase, PlanetScale, Neon) are excellent at "human-reviewed" workflows and provide no real safety net for "agent-authored, human-supervises-after-the-fact" workflows. That gap is what ThetaBase is built to close — not "talk to your database in English."

---

## 2. Personas

| Persona | Description | Pain Point Solved |
|---|---|---|
| The Vibe Coder | Builds multiple apps, across multiple companies/projects, mostly via an AI coding agent. | Never wants to touch a dashboard, copy a key, or set up an env file to switch context between projects. |
| The Agent Itself | The actual primary author of most schema changes and queries in this workflow. | Needs a database that prevents it from causing unreviewable, irreversible damage — without slowing down its normal workflow. |
| The Hackathon/Small Team | Multiple people, multiple branches, rapid iteration. | Wants free, instant branching for both data and schema, with safe merge-back. |
| The Migrating Founder | Existing Postgres/Supabase app. | Wants to move without a rewrite and without giving up SQL-like guarantees. |

---

## 3. Core Principles

- **Identity over credentials.** One login, ever. Org/project context is resolved conversationally, not configured manually.
- **Prevention over silent correction.** Bad agent behavior is caught before it lands, not fixed after the fact by guessing intent.
- **Deterministic execution path.** The hot path (reads/writes/typed queries) never depends on an LLM call.
- **Branch everything, for free.** Data and schema both get git-like, instant, copy-on-write branches.
- **Dashboard is for money only.** Provisioning, keys, and org context never require opening it.

---

## 4. User Journeys

### Journey A — New project, mid-conversation, no context switch
```
User (to agent): "Start a new db in Company B / project churn-dashboard, build the churn tracking feature."
```
Agent resolves org/project via identity graph, provisions instantly, scoped token injected automatically, builds against the live typed schema. No dashboard, no key, no `.env`.

### Journey B — Switching companies mid-session
```
User: "Now check the inventory db in Company A."
```
Tooling swaps active scoped context; no re-login, no separate CLI profile.

### Journey C — Agent proposes a risky schema change
Agent attempts to drop a column with live data. Safety Layer intercepts, returns a structured diff (rows affected, reversibility, cost estimate). Non-destructive parts auto-apply; the destructive part either prompts for confirmation or is validated against a shadow branch first — the app never silently loses or mutates data.

### Journey D — Migration from Supabase
`eject` flow reflects existing Postgres schema, maps types to ThetaBase's typed schema (with CRDT-type suggestions where concurrency matters), streams data in, and runs a verification pass comparing before/after schema semantics — flags mismatches for human review rather than assuming a clean 1:1 mapping.

---

## 5. V1 Scope

**In scope:**
- Identity/org-graph provisioning (Google/GitHub OAuth)
- Core log + branching engine, CRDT merge for standard types
- Typed query language + SQL-subset compiler
- Safety Layer (diff/preview, blast-radius guardrails)
- Generated typed SDKs (start with JS/TS, Python; expand after validation)
- Self-hostable/embeddable core (minimal but real)
- Branch-per-PR GitHub integration

**Explicitly out of scope for v1:**
- NL-as-runtime query execution (AI assist is optional, non-authoritative, layered on top)
- Cross-region multi-master writes
- LLM-mediated merge conflict resolution
- Full dashboard/observability UI beyond CLI + billing

---

## 6. Success Criteria for V1 (pre-GA)

- Consistency guarantees in the Data Model spec pass an independent verification suite (Jepsen-style).
- Safety Layer passes an adversarial test suite: no destructive, unreviewed change reaches a protected branch across a defined attack corpus.
- p50/p99 latency targets met for typed read/write path (see SLA & Performance Spec) with zero LLM calls on that path.
- At least one real migration from an existing Postgres/Supabase project completed via `eject`, verified correct by the automated schema-semantics pass — which is itself gated on detecting deliberately planted mismatches, since a detector is only worth the failures it can find.
