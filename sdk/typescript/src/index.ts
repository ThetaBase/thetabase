/**
 * ThetaBase TypeScript SDK.
 *
 * STATUS: types and surface only — every method throws, with one exception:
 * `assist.suggestQuery` is live as of M9. It can be, because Assist is a
 * separate HTTP service rather than something reached over the Cap'n Proto
 * transport, so it does not wait on the client work the rest of this file does. The transport exists
 * (ROADMAP M2, and `thetad` serves the full surface), but nothing here is wired
 * to it, because from M6 this file is generated from
 * `crates/theta-proto/schema/theta.capnp` and hand-written bodies would be
 * overwritten. M6 is what delivers a working SDK.
 *
 * The surface is fixed now because it is the contract an agent writes against,
 * and three properties of it are load-bearing (see docs/specs/02, §3):
 *
 * 1. No connection string, no API key, no `.env`. A project is named, and the
 *    scoped token is resolved and injected by the toolchain.
 * 2. Schema changes cannot be applied in one call. `propose` returns a diff;
 *    applying is a separate, explicit act.
 * 3. `assist` returns a *candidate* plan and never executes. Its output has to
 *    pass back through `query()` like anything else.
 */

export type Value =
  | null
  | boolean
  | number
  | string
  | Uint8Array
  | Value[]
  | { [key: string]: Value };

export interface ThetaOptions {
  /** Project name or id. Resolved via the identity/org graph — never a key. */
  project: string;
  /** Defaults to `dev`, or `preview` when running inside a PR branch. */
  environment?: "dev" | "preview" | "prod";
  /** Branch to operate on. Defaults to the session's own branch. */
  branch?: string;
  /**
   * Where AI Query-Assist is running, if it is.
   *
   * Absent by default, and absent is the normal case: Assist is a separate,
   * optional service (`specs/01` §3), and a deployment that never starts it
   * loses suggestions and nothing else. Naming it here rather than deriving it
   * from the project keeps that separation visible at the call site — you
   * cannot reach Assist without having said where it is.
   */
  assistUrl?: string;
}

// The wire types are not restated here.
//
// They used to be, and they had drifted: this file's `ChangeDiff` had no `gate`
// — the field that says whether confirmation is even a route — its
// `ConflictRef` carried a `table` and `column` the wire does not send, and its
// `MergeResult.status` was missing `upToDate`. A caller writing against those
// types was writing against a protocol that does not exist.
//
// One definition, generated from the schema, re-exported here so the import
// path stays `@thetabase/client`.
export type {
  ChangeDiff,
  ConflictRef,
  Gate,
  MergeResult,
  ProjectStatus,
  AuditEntryWire as AuditEntry,
  BranchInfo,
  ChangeStateResponse as ChangeState,
  ValidationCheckWire as ValidationCheck,
} from "./generated.js";

import type { ChangeDiff, MergeResult, ProjectStatus } from "./generated.js";

export interface Explain {
  planHash: string;
  steps: { depth: number; operator: string; detail: string }[];
  estimatedRows: number;
  estimatedCostMs: number;
  indexesUsed: string[];
  /** Always 0 on the typed path. Asserted in CI, not assumed. */
  llmCalls: number;
}

/**
 * What `assist.suggestQuery` returns: a proposal, not a result.
 *
 * `plan` is a typed plan, never SQL text. Text would have to be re-parsed by
 * whoever received it, and a second parser is where the rule that a value never
 * becomes query text gets broken.
 */
export interface QueryCandidate {
  /** Hand to `query()` to run it. Nothing has run yet. */
  plan: QueryBuilder;
  /** `EXPLAIN` output — what a reviewer reads before approving. */
  planPreview: Explain;
  /** The SQL the model wrote, for a human to read. Informational only. */
  sql: string;
  /** Which model produced it. */
  model: string;
  /** Always `false`. Present so the caller does not have to assume it. */
  executed: boolean;
}

/** Thrown when the Safety Layer refuses a change. Carries the diff to act on. */
export class SafetyGateError extends Error {
  constructor(
    message: string,
    readonly diff: ChangeDiff,
  ) {
    super(message);
    this.name = "SafetyGateError";
  }
}

/** Thrown when the blast-radius breaker is open. */
export class CircuitBreakerError extends Error {
  constructor(
    message: string,
    readonly windowRows: number,
    readonly ceiling: number,
  ) {
    super(message);
    this.name = "CircuitBreakerError";
  }
}

const notImplemented = (what: string, milestone: string): never => {
  throw new Error(`${what} is not implemented yet — lands in ${milestone} (see docs/ROADMAP.md)`);
};

export class Theta {
  constructor(readonly options: ThetaOptions) {}

  /** Point lookup. Hot path: p50 5ms, and no model call, ever. */
  async get(_key: string): Promise<Value | undefined> {
    return notImplemented("Theta.get", "M6 (Generated SDKs)");
  }

  async put(_key: string, _value: Value): Promise<{ commitId: string }> {
    return notImplemented("Theta.put", "M6 (Generated SDKs)");
  }

  async query<T = Value>(_plan: QueryBuilder): Promise<T[]> {
    return notImplemented("Theta.query", "M6 (Generated SDKs)");
  }

  /** EXPLAIN without executing — what a reviewer reads before approving. */
  async explain(_plan: QueryBuilder): Promise<Explain> {
    return notImplemented("Theta.explain", "M6 (Generated SDKs)");
  }

  async status(): Promise<ProjectStatus> {
    return notImplemented("Theta.status", "M6 (Generated SDKs)");
  }

  readonly schema = {
    /**
     * Submit a change. Always returns a diff; never applies anything to the
     * target branch.
     *
     * A change the rules put at the shadow gate is applied to an ephemeral
     * shadow branch and validated there as part of this call, so the diff
     * comes back already saying what the checks found. Landing it still takes
     * an explicit `promote`.
     */
    propose: async (_change: SchemaChange): Promise<ChangeDiff> =>
      notImplemented("Theta.schema.propose", "M6 (Generated SDKs)"),

    /** A proposal's diff, and what validating it found. */
    show: async (_changeId: string): Promise<ChangeDiff> =>
      notImplemented("Theta.schema.show", "M6 (Generated SDKs)"),

    /**
     * Confirm a proposed change, by id.
     *
     * Deliberately takes no change body and no branch: the server applies what
     * it classified under this id, on the branch that proposal targeted. A
     * caller that could supply either could confirm one change and execute
     * another (docs/specs/07, §5.1).
     *
     * Confirmation is not a path at all for a change at the shadow gate — that
     * one lands through `promote`.
     */
    apply: async (_changeId: string, _confirm: boolean): Promise<void> =>
      notImplemented("Theta.schema.apply", "M6 (Generated SDKs)"),

    /**
     * Re-run validation against a change's shadow branch.
     *
     * Not a required step — `propose` already ran it. This is for a shadow
     * branch that moved afterwards, which makes the earlier result stale and
     * blocks promotion until it is re-checked.
     */
    validate: async (_changeId: string): Promise<{ shadowBranch: string; passed: boolean }> =>
      notImplemented("Theta.schema.validate", "M6 (Generated SDKs)"),

    /** Merge a validated shadow branch onto its target. Never re-executes. */
    promote: async (_changeId: string): Promise<void> =>
      notImplemented("Theta.schema.promote", "M6 (Generated SDKs)"),

    /** Refuse a change and reclaim its shadow branch. */
    reject: async (_changeId: string, _reason: string): Promise<void> =>
      notImplemented("Theta.schema.reject", "M6 (Generated SDKs)"),
  };

  readonly branch = {
    create: async (_name: string, _from?: string): Promise<string> =>
      notImplemented("Theta.branch.create", "M6 (Generated SDKs)"),
    merge: async (_source: string, _into?: string): Promise<MergeResult> =>
      notImplemented("Theta.branch.merge", "M6 (Generated SDKs)"),
    discard: async (_name: string): Promise<void> =>
      notImplemented("Theta.branch.discard", "M6 (Generated SDKs)"),
  };

  readonly assist = {
    /**
     * Translate natural language into a *candidate* typed plan.
     *
     * Explicitly invoked, never on the hot path, and never authoritative: the
     * returned candidate carries a readable preview and must be passed to
     * `query()` deliberately before anything runs. This method cannot execute
     * anything — it returns a proposal and stops.
     *
     * Plain HTTP rather than the Cap'n Proto transport the rest of this class
     * uses, because Assist is a different service that `thetad` does not speak
     * to. Routing it through the wire protocol would put a model call inside
     * the protocol the hot path speaks, which is precisely what
     * `no_llm_on_hot_path` exists to prevent.
     *
     * Throws if no `assistUrl` was configured. That is not a degraded mode to
     * paper over: a caller who did not deploy Assist should hear so, not
     * receive a silent `undefined` where a query was expected.
     */
    suggestQuery: async (prompt: string, schema: unknown): Promise<QueryCandidate> => {
      const base = this.options.assistUrl;
      if (!base) {
        throw new Error(
          "AI Query-Assist is not configured. Set `assistUrl` to where the " +
            "theta-assist service is running. It is optional — everything on " +
            "the read/write path works without it.",
        );
      }

      const response = await fetch(`${base.replace(/\/$/, "")}/v1/suggest`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ question: prompt, schema }),
      });

      if (!response.ok) {
        const refusal = (await response.json().catch(() => ({}))) as {
          error?: string;
          remedy?: string;
        };
        // The remedy is carried through rather than swallowed: the service says
        // what the caller can do, and dropping it here would turn an actionable
        // refusal into a status code.
        throw new Error(
          `assist declined (${response.status}): ${refusal.error ?? "no detail"}` +
            (refusal.remedy ? `\n${refusal.remedy}` : ""),
        );
      }

      const body = (await response.json()) as {
        plan: unknown;
        preview: Explain;
        sql: string;
        model: string;
        executed: boolean;
      };

      return {
        plan: body.plan as QueryBuilder,
        planPreview: body.preview,
        sql: body.sql,
        model: body.model,
        executed: body.executed,
      };
    },
  };
}

/** Opaque handle to a typed plan. Built by `theta.table(...)`, never by string. */
export interface QueryBuilder {
  readonly __plan: unique symbol;
}

export type SchemaChange =
  | { change: "add_table"; table: string }
  | { change: "drop_table"; table: string }
  | { change: "add_column"; table: string; column: string; type: string; nullable?: boolean }
  | { change: "drop_column"; table: string; column: string }
  | { change: "alter_column_type"; table: string; column: string; to: string }
  | { change: "rename_column"; table: string; from: string; to: string };
