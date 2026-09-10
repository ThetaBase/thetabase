// The typed query builder.
//
// Chainable in C#, because that is what makes a query pleasant to write. What it
// produces is an AST, and the AST is rendered to SQL-subset source and bound
// parameters by the WebAssembly core — so the same query built in TypeScript,
// Python, Go, Rust or Java reaches the server as the same bytes, and the rule
// that a value never becomes query text has one implementation rather than one
// per language.
//
// Nothing here interpolates. `Where(Query.Eq("name", userInput))` puts
// `userInput` in the parameter map and `$p0` in the text, whatever it contains.

using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace ThetaBase;

/// <summary>A binary comparison.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<CompareOp>))]
public enum CompareOp
{
    [JsonStringEnumMemberName("eq")] Eq,
    [JsonStringEnumMemberName("ne")] Ne,
    [JsonStringEnumMemberName("lt")] Lt,
    [JsonStringEnumMemberName("lte")] Lte,
    [JsonStringEnumMemberName("gt")] Gt,
    [JsonStringEnumMemberName("gte")] Gte,
}

/// <summary>One node of a filter tree.</summary>
/// <remarks>
/// One record with a Kind rather than a hierarchy with a subtype per arm. The
/// AST crosses a JSON boundary into the core; a hierarchy would need a custom
/// converter on this side and a matching reader on the other, and the shape that
/// survives the round trip unchanged is the one the other five SDKs already
/// send.
/// </remarks>
public sealed record Predicate(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("column"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? Column = null,
    [property: JsonPropertyName("op"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] CompareOp? Op = null,
    [property: JsonPropertyName("value"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] object? Value = null,
    [property: JsonPropertyName("values"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] IReadOnlyList<object?>? Values = null,
    [property: JsonPropertyName("terms"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] IReadOnlyList<Predicate>? Terms = null,
    [property: JsonPropertyName("term"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] Predicate? Term = null);

/// <summary>One sort key.</summary>
public sealed record Sort(
    [property: JsonPropertyName("column")] string Column,
    [property: JsonPropertyName("descending")] bool Descending);

/// <summary>What the core renders.</summary>
public sealed record QueryAst(
    [property: JsonPropertyName("table")] string Table,
    [property: JsonPropertyName("columns")] IReadOnlyList<string> Columns,
    [property: JsonPropertyName("filter"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] Predicate? Filter,
    [property: JsonPropertyName("orderBy")] IReadOnlyList<Sort> OrderBy,
    [property: JsonPropertyName("limit"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] long? Limit,
    [property: JsonPropertyName("offset"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] long? Offset);

/// <summary>A typed plan under construction.</summary>
public sealed class Query
{
    private readonly QueryAst _ast;

    private Query(QueryAst ast) => _ast = ast;

    /// <summary>Start a query against <paramref name="name"/>.</summary>
    public static Query Table(string name) =>
        new(new QueryAst(name, [], null, [], null, null));

    private static Predicate Compare(string column, CompareOp op, object? value) =>
        new("compare", Column: column, Op: op, Value: value);

    /// <summary>Keep rows where <paramref name="column"/> equals <paramref name="value"/>.</summary>
    public static Predicate Eq(string column, object? value) => Compare(column, CompareOp.Eq, value);

    /// <summary>Keep rows where <paramref name="column"/> does not equal <paramref name="value"/>.</summary>
    public static Predicate Ne(string column, object? value) => Compare(column, CompareOp.Ne, value);

    /// <summary>Keep rows where <paramref name="column"/> is less than <paramref name="value"/>.</summary>
    public static Predicate Lt(string column, object? value) => Compare(column, CompareOp.Lt, value);

    /// <summary>Keep rows where <paramref name="column"/> is at most <paramref name="value"/>.</summary>
    public static Predicate Lte(string column, object? value) => Compare(column, CompareOp.Lte, value);

    /// <summary>Keep rows where <paramref name="column"/> is greater than <paramref name="value"/>.</summary>
    public static Predicate Gt(string column, object? value) => Compare(column, CompareOp.Gt, value);

    /// <summary>Keep rows where <paramref name="column"/> is at least <paramref name="value"/>.</summary>
    public static Predicate Gte(string column, object? value) => Compare(column, CompareOp.Gte, value);

    /// <summary>Keep rows where <paramref name="column"/> is one of <paramref name="values"/>.</summary>
    public static Predicate In(string column, IReadOnlyList<object?> values) =>
        new("in", Column: column, Values: values);

    /// <summary>Keep rows where <paramref name="column"/> is null.</summary>
    public static Predicate IsNull(string column) => new("isNull", Column: column);

    /// <summary>Keep rows where <paramref name="column"/> is not null.</summary>
    public static Predicate NotNull(string column) => new("notNull", Column: column);

    /// <summary>Keep rows matching every term.</summary>
    public static Predicate And(params Predicate[] terms) => new("and", Terms: terms);

    /// <summary>Keep rows matching any term.</summary>
    public static Predicate Or(params Predicate[] terms) => new("or", Terms: terms);

    /// <summary>Invert a term.</summary>
    public static Predicate Not(Predicate term) => new("not", Term: term);

    /// <summary>Add columns to the projection.</summary>
    /// <remarks>
    /// Every method here returns a new Query rather than mutating this one. A
    /// builder that mutated would make a shared base query change under whoever
    /// else was holding it.
    /// </remarks>
    public Query Select(params string[] columns) =>
        new(_ast with { Columns = [.. _ast.Columns, .. columns] });

    /// <summary>Keep rows matching <paramref name="predicate"/>. Repeated calls are ANDed.</summary>
    public Query Where(Predicate predicate) =>
        new(_ast with { Filter = _ast.Filter is null ? predicate : And(_ast.Filter, predicate) });

    /// <summary>Add a sort key.</summary>
    public Query OrderBy(string column, bool descending) =>
        new(_ast with { OrderBy = [.. _ast.OrderBy, new Sort(column, descending)] });

    /// <summary>Cap the number of rows returned.</summary>
    public Query Limit(long count) => new(_ast with { Limit = count });

    /// <summary>Skip rows before returning any.</summary>
    public Query Offset(long count) => new(_ast with { Offset = count });

    /// <summary>The plan, for a caller that wants to inspect or send it.</summary>
    public QueryAst Ast => _ast;

    /// <summary>Render through the core. Throws if an identifier could carry syntax.</summary>
    public JsonNode Render(Scribe core) => core.RenderQuery(_ast);
}
