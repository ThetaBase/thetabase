// The typed query builder.
//
// Chainable in Swift, because that is what makes a query pleasant to write. What
// it produces is an AST, and the AST is rendered to SQL-subset source and bound
// parameters by the WebAssembly core — so the same query built in TypeScript,
// Python, Go, Rust, Java, C# or Ruby reaches the server as the same bytes, and
// the rule that a value never becomes query text has one implementation rather
// than one per language.
//
// Nothing here interpolates. `where(.eq("name", userInput))` puts `userInput` in
// the parameter map and `$p0` in the text, whatever it contains.

import Foundation

/// A binary comparison.
///
/// A closed enum on purpose: an open set would eventually carry an operator the
/// renderer does not know, and the only place left to put it would be the query
/// text.
public enum CompareOp: String, Codable, Sendable {
    case eq, ne, lt, lte, gt, gte
}

/// One node of a filter tree.
///
/// One struct with a `kind` rather than an `indirect enum` with a case per arm.
/// The AST crosses a JSON boundary into the core; an enum would need a hand-
/// written `Codable` conformance on this side and a matching reader on the
/// other, and the shape that survives the round trip unchanged is the one the
/// other seven SDKs already send.
public struct Predicate: Codable, Equatable, Sendable {
    public let kind: String
    public let column: String?
    public let op: CompareOp?
    public let value: JSONValue?
    public let values: [JSONValue]?
    public let terms: [Predicate]?
    public let term: [Predicate]?

    private init(
        kind: String,
        column: String? = nil,
        op: CompareOp? = nil,
        value: JSONValue? = nil,
        values: [JSONValue]? = nil,
        terms: [Predicate]? = nil,
        term: Predicate? = nil
    ) {
        self.kind = kind
        self.column = column
        self.op = op
        self.value = value
        self.values = values
        self.terms = terms
        // Boxed in a single-element array rather than made `indirect`: a struct
        // cannot contain itself, and one allocation at the two places `not` is
        // used beats an indirect enum's rewrite of everything above.
        self.term = term.map { [$0] }
    }

    /// The inverted term, for `not`.
    public var inner: Predicate? { term?.first }

    private static func compare(_ column: String, _ op: CompareOp, _ value: JSONValue) -> Predicate {
        Predicate(kind: "compare", column: column, op: op, value: value)
    }

    /// Keep rows where `column` equals `value`.
    public static func eq(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .eq, value)
    }

    /// Keep rows where `column` does not equal `value`.
    public static func ne(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .ne, value)
    }

    /// Keep rows where `column` is less than `value`.
    public static func lt(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .lt, value)
    }

    /// Keep rows where `column` is at most `value`.
    public static func lte(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .lte, value)
    }

    /// Keep rows where `column` is greater than `value`.
    public static func gt(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .gt, value)
    }

    /// Keep rows where `column` is at least `value`.
    public static func gte(_ column: String, _ value: JSONValue) -> Predicate {
        compare(column, .gte, value)
    }

    /// Keep rows where `column` is one of `values`.
    public static func `in`(_ column: String, _ values: [JSONValue]) -> Predicate {
        Predicate(kind: "in", column: column, values: values)
    }

    /// Keep rows where `column` is null.
    public static func isNull(_ column: String) -> Predicate {
        Predicate(kind: "isNull", column: column)
    }

    /// Keep rows where `column` is not null.
    public static func notNull(_ column: String) -> Predicate {
        Predicate(kind: "notNull", column: column)
    }

    /// Keep rows matching every term.
    public static func and(_ terms: Predicate...) -> Predicate {
        Predicate(kind: "and", terms: terms)
    }

    /// Keep rows matching any term.
    public static func or(_ terms: Predicate...) -> Predicate {
        Predicate(kind: "or", terms: terms)
    }

    /// Invert a term.
    public static func not(_ term: Predicate) -> Predicate {
        Predicate(kind: "not", term: term)
    }
}

/// One sort key.
public struct Sort: Codable, Equatable, Sendable {
    public let column: String
    public let descending: Bool
}

/// What the core renders.
public struct QueryAst: Codable, Equatable, Sendable {
    public let table: String
    public let columns: [String]
    public let filter: Predicate?
    public let orderBy: [Sort]
    public let limit: Int64?
    public let offset: Int64?
}

/// A typed plan under construction.
///
/// A value type, so `let base = Query.table("users")` cannot change under
/// whoever else is holding it. Swift gives that for free where Go, Ruby and
/// TypeScript each had to copy a collection by hand.
public struct Query: Sendable {
    public private(set) var ast: QueryAst

    private init(_ ast: QueryAst) { self.ast = ast }

    /// Start a query against `name`.
    public static func table(_ name: String) -> Query {
        Query(
            QueryAst(table: name, columns: [], filter: nil, orderBy: [], limit: nil, offset: nil)
        )
    }

    private func with(
        columns: [String]? = nil,
        filter: Predicate?? = nil,
        orderBy: [Sort]? = nil,
        limit: Int64?? = nil,
        offset: Int64?? = nil
    ) -> Query {
        Query(
            QueryAst(
                table: ast.table,
                columns: columns ?? ast.columns,
                filter: filter ?? ast.filter,
                orderBy: orderBy ?? ast.orderBy,
                limit: limit ?? ast.limit,
                offset: offset ?? ast.offset
            )
        )
    }

    /// Add columns to the projection. No projection means every column.
    public func select(_ columns: String...) -> Query {
        with(columns: ast.columns + columns)
    }

    /// Keep rows matching `predicate`. Repeated calls are ANDed.
    public func filter(_ predicate: Predicate) -> Query {
        with(filter: ast.filter.map { .and($0, predicate) } ?? predicate)
    }

    /// Add a sort key.
    public func orderBy(_ column: String, descending: Bool = false) -> Query {
        with(orderBy: ast.orderBy + [Sort(column: column, descending: descending)])
    }

    /// Cap the number of rows returned.
    public func limit(_ count: Int64) -> Query {
        with(limit: count)
    }

    /// Skip rows before returning any.
    public func offset(_ count: Int64) -> Query {
        with(offset: count)
    }

    /// Render through the core. Throws if an identifier could carry syntax.
    public func render(_ core: Scribe) throws -> JSONValue {
        try core.renderQuery(ast)
    }
}
