/**
 * ThetaBase TypeScript SDK.
 *
 * Open one with {@link Theta.open}, giving it a socket and a loaded core:
 * `node-socket.ts` supplies the first under Node, and another runtime brings
 * its own.
 *
 * Nothing in this file builds a wire message. Requests are handed to the
 * WebAssembly core as plain objects and come back decoded, which is why seven
 * SDKs do not mean seven implementations of the protocol — and why a value in
 * this file never becomes query text, including on the `query` path, where the
 * core renders the plan.
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

import { ProtocolError, ScribeCore, type Socket } from "./scribe.js";
import { Session, connectionFromEnv } from "./session.js";
import type { QueryAst } from "./query.js";

export { ProtocolError, ScribeCore, Session, connectionFromEnv };
export type { Socket };

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

/** One operation inside a {@link Theta.transaction}. */
export type TxOp = {
  key: string;
  /** Omitted means unconditional, which is the common case. */
  expect?: Expect;
} & ({ action: "put"; value: Value } | { action: "delete" });

/** A precondition on a row's current state. */
export type Expect = { kind: "absent" } | { kind: "version"; value: number };

/**
 * Unwrap a response of the expected kind, or throw something a caller can act
 * on.
 *
 * The three named failures are separated deliberately. A gate refusal carries
 * the diff the caller has to act on and is an *answer*, not a fault; an open
 * breaker is a limit this project set, not a bad request; and everything else
 * is an error with a message. Collapsing them into one `Error` would make the
 * first two unactionable, which is the whole reason the wire distinguishes
 * them.
 */
function expect(response: Record<string, unknown>, kind: string): Record<string, unknown> {
  if (response.kind === kind) {
    return (response.value ?? {}) as Record<string, unknown>;
  }

  if (response.kind === "error") {
    const value = (response.value ?? {}) as Record<string, unknown>;
    const message = String(value.message ?? "the server refused the request");
    const code = String(value.code ?? "");

    if (code === "ConfirmationRequired") {
      throw new SafetyGateError(message, value.diff as ChangeDiff);
    }
    if (code === "BreakerOpen") {
      throw new CircuitBreakerError(
        message,
        Number(value.windowRows ?? 0),
        Number(value.ceiling ?? 0),
      );
    }
    throw new Error(message);
  }

  // `raw` is what the core produces for a response this build does not know.
  // Reported as such rather than as a parse failure, because the cause is a
  // newer server rather than corrupt data, and the remedy is upgrading.
  if (response.kind === "raw") {
    throw new Error(
      "the server sent a response this SDK does not understand; it is newer " +
        "than this client. Upgrade the SDK.",
    );
  }

  throw new Error(`expected a \`${kind}\` response, got \`${String(response.kind)}\``);
}

export class Theta {
  /**
   * Not a constructor, because opening a connection is asynchronous and a
   * constructor that returned an unusable object would make every call site
   * remember to await something else first.
   *
   * `socket` is supplied rather than created here: Node has `node-socket.ts`,
   * and another runtime brings its own without this file knowing about it.
   */
  static async open(options: ThetaOptions, socket: Socket, core: ScribeCore): Promise<Theta> {
    const session = await Session.open(core, socket, connectionFromEnv(process.env));
    return new Theta(options, session, core);
  }

  private constructor(
    readonly options: ThetaOptions,
    private readonly session: Session,
    private readonly core: ScribeCore,
  ) {}

  /** Release the connection. Further calls will fail. */
  close(): void {
    this.session.close();
  }

  /** Point lookup. Hot path: p50 5ms, and no model call, ever. */
  async get(key: string): Promise<Value | undefined> {
    const response = await this.session.call({ op: "get", key });
    const value = expect(response, "get");
    // `found: false` is a successful answer meaning the row is not there, and
    // it is distinct from a null value that is.
    return value.found === true ? (value.value as Value) : undefined;
  }

  async put(key: string, value: Value): Promise<{ commitId: string }> {
    const response = await this.session.call({ op: "put", key, value });
    return { commitId: expect(response, "commit").commitId as string };
  }

  async delete(key: string): Promise<{ commitId: string }> {
    const response = await this.session.call({ op: "delete", key });
    return { commitId: expect(response, "commit").commitId as string };
  }

  /**
   * A write conditional on the row's current state.
   *
   * Resolves to `null` when the condition was not met. That is not an error:
   * the request was well formed and the server did what it was asked, and a
   * lost-update retry that threw would make an ordinary contended key look like
   * a fault.
   */
  async putIf(key: string, value: Value, expectRow: Expect): Promise<{ commitId: string } | null> {
    const response = await this.session.call({ op: "putIf", key, value, expect: expectRow });
    if (response.kind === "preconditionFailed") return null;
    return { commitId: expect(response, "commit").commitId as string };
  }

  /**
   * Several writes that land as one commit, or none of them.
   *
   * Unlike sending several `put`s, this batches the durability boundary and not
   * merely the network. Each operation may carry its own precondition, and all
   * of them are checked before any write is applied — so a transaction that
   * would violate one changes nothing.
   *
   * Resolves to `null` when a precondition was not met, for the reason
   * {@link putIf} gives.
   */
  async transaction(ops: TxOp[]): Promise<{ commitId: string } | null> {
    const response = await this.session.call({ op: "transaction", ops });
    if (response.kind === "preconditionFailed") return null;
    return { commitId: expect(response, "commit").commitId as string };
  }

  /**
   * Run a typed plan.
   *
   * Takes a {@link QueryAst} rather than the opaque `QueryBuilder` this file
   * used to name. `QueryBuilder` was a phantom type with no runtime shape — a
   * placeholder for a builder that does not exist — and a method taking one
   * could never have been called. `QueryAst` is what `query.ts` actually
   * produces.
   *
   * The plan is rendered to SQL *by the core*, not here. That is the invariant
   * this whole SDK is arranged around: a value never becomes query text in
   * TypeScript, so there is nothing in this file to inject into.
   */
  async query<T = Value>(plan: QueryAst): Promise<T[]> {
    const { sql, params } = this.core.renderQuery(plan);
    const response = await this.session.call({ op: "query", sql, params });
    return expect(response, "query").rows as T[];
  }

  /** EXPLAIN without executing — what a reviewer reads before approving. */
  async explain(plan: QueryAst): Promise<Explain> {
    const { sql, params } = this.core.renderQuery(plan);
    const response = await this.session.call({ op: "explain", sql, params });
    return expect(response, "explain") as unknown as Explain;
  }

  async status(): Promise<ProjectStatus> {
    const response = await this.session.call({ op: "status" });
    return expect(response, "status") as unknown as ProjectStatus;
  }

  /** What is in here, and where it came from. */
  async describe(): Promise<Record<string, unknown>> {
    const response = await this.session.call({ op: "describe" });
    return expect(response, "description");
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
    propose: async (change: SchemaChange): Promise<ChangeDiff> => {
      const response = await this.session.call({ op: "proposeSchemaChange", change });
      return expect(response, "propose") as unknown as ChangeDiff;
    },

    /** A proposal's diff, and what validating it found. */
    show: async (changeId: string): Promise<ChangeDiff> => {
      const response = await this.session.call({ op: "showChange", changeId });
      return expect(response, "change") as unknown as ChangeDiff;
    },

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
    apply: async (changeId: string, confirm: boolean): Promise<void> => {
      const response = await this.session.call({ op: "applySchemaChange", changeId, confirm });
      expect(response, "commit");
    },

    /**
     * Re-run validation against a change's shadow branch.
     *
     * Not a required step — `propose` already ran it. This is for a shadow
     * branch that moved afterwards, which makes the earlier result stale and
     * blocks promotion until it is re-checked.
     */
    validate: async (changeId: string): Promise<{ shadowBranch: string; passed: boolean }> => {
      const response = await this.session.call({ op: "showChange", changeId });
      const change = expect(response, "change");
      return {
        shadowBranch: String(change.shadowBranchId ?? ""),
        passed: change.validationPassed === true,
      };
    },

    /** Merge a validated shadow branch onto its target. Never re-executes. */
    promote: async (changeId: string): Promise<void> => {
      const response = await this.session.call({ op: "promoteChange", changeId });
      expect(response, "commit");
    },

    /** Refuse a change and reclaim its shadow branch. */
    reject: async (changeId: string, reason: string): Promise<void> => {
      const response = await this.session.call({ op: "rejectChange", changeId, reason });
      expect(response, "ok");
    },
  };

  readonly branch = {
    create: async (name: string, from?: string): Promise<string> => {
      const response = await this.session.call({ op: "createBranch", name, from: from ?? null });
      return String(expect(response, "branch").branchId);
    },
    merge: async (source: string, into?: string): Promise<MergeResult> => {
      const response = await this.session.call({
        op: "merge",
        sourceBranch: source,
        targetBranch: into ?? null,
      });
      return expect(response, "merge") as unknown as MergeResult;
    },
    discard: async (name: string): Promise<void> => {
      const response = await this.session.call({ op: "discardBranch", name });
      expect(response, "ok");
    },
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
