// The typed query builder.
//
// Fluent in TypeScript, because that is what makes a query pleasant to write.
// What it produces is an AST, and the AST is rendered to SQL-subset source and
// bound parameters by the WebAssembly core — so the same query built in Python
// reaches the server as the same bytes, and the rule that a value never becomes
// query text has one implementation rather than one per language.
//
// Nothing here interpolates. `where("name", "=", userInput)` puts `userInput`
// in the parameter map and `$p0` in the text, whatever `userInput` contains.

import type { ScribeCore } from "./scribe.js";

export type CompareOp = "eq" | "ne" | "lt" | "lte" | "gt" | "gte";

export type Predicate =
  | { kind: "compare"; column: string; op: CompareOp; value: unknown }
  | { kind: "in"; column: string; values: unknown[] }
  | { kind: "isNull"; column: string }
  | { kind: "notNull"; column: string }
  | { kind: "and"; terms: Predicate[] }
  | { kind: "or"; terms: Predicate[] }
  | { kind: "not"; term: Predicate };

export interface QueryAst {
  table: string;
  columns: string[];
  filter?: Predicate;
  orderBy: { column: string; descending: boolean }[];
  limit?: number;
  offset?: number;
}

/** A rendered query: text with placeholders, and the values they stand for. */
export interface RenderedQuery {
  sql: string;
  params: Record<string, string>;
}

export const eq = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "eq", value });
export const ne = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "ne", value });
export const lt = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "lt", value });
export const lte = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "lte", value });
export const gt = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "gt", value });
export const gte = (column: string, value: unknown): Predicate =>
  ({ kind: "compare", column, op: "gte", value });
export const isIn = (column: string, values: unknown[]): Predicate =>
  ({ kind: "in", column, values });
export const isNull = (column: string): Predicate => ({ kind: "isNull", column });
export const notNull = (column: string): Predicate => ({ kind: "notNull", column });
export const and = (...terms: Predicate[]): Predicate => ({ kind: "and", terms });
export const or = (...terms: Predicate[]): Predicate => ({ kind: "or", terms });
export const not = (term: Predicate): Predicate => ({ kind: "not", term });

/** Start a query against `name`. */
export function table(name: string): Query {
  return new Query({ table: name, columns: [], orderBy: [] });
}

export class Query {
  constructor(private readonly ast: QueryAst) {}

  private with(changes: Partial<QueryAst>): Query {
    // A new Query each time: a builder that mutated would make a shared base
    // query change under whoever else was holding it.
    return new Query({ ...this.ast, ...changes });
  }

  select(...columns: string[]): Query {
    return this.with({ columns: [...this.ast.columns, ...columns] });
  }

  /** Keep rows matching `predicate`. Repeated calls are ANDed. */
  where(predicate: Predicate): Query {
    const filter = this.ast.filter ? and(this.ast.filter, predicate) : predicate;
    return this.with({ filter });
  }

  orderBy(column: string, descending = false): Query {
    return this.with({ orderBy: [...this.ast.orderBy, { column, descending }] });
  }

  limit(count: number): Query {
    return this.with({ limit: count });
  }

  offset(count: number): Query {
    return this.with({ offset: count });
  }

  /** The AST, for a caller that wants to inspect or send it. */
  toAst(): QueryAst {
    return structuredClone(this.ast);
  }

  /** Render through the core. Throws if an identifier could carry syntax. */
  render(core: ScribeCore): RenderedQuery {
    return core.renderQuery(this.ast);
  }
}
