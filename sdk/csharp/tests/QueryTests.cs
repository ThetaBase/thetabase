// What the builder has to guarantee, checked against the real core.
//
// Against the core rather than against the AST alone: the AST is only
// interesting because of what the core makes of it, and a test that asserted on
// JSON shape would pass just as happily if the core rejected every query.

using System.Text.Json;
using ThetaBase;
using Xunit;

namespace ThetaBase.Tests;

public sealed class QueryTests : IDisposable
{
    private readonly Scribe _core;

    public QueryTests()
    {
        var root = Directory.GetCurrentDirectory();
        while (!File.Exists(Path.Combine(root, "sdk/conformance/cases.json")))
        {
            root = Directory.GetParent(root)!.FullName;
        }
        _core = Scribe.Load(
            Path.Combine(root, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"));
    }

    public void Dispose() => _core.Dispose();

    [Fact]
    public void AValueNeverReachesTheQueryText()
    {
        // The property the whole builder exists for.
        const string hostile = "'; DROP TABLE users; --";
        var rendered = Query.Table("users").Where(Query.Eq("name", hostile)).Render(_core);

        Assert.Equal("SELECT * FROM users WHERE name = $p0", rendered["sql"]!.GetValue<string>());
        Assert.DoesNotContain("DROP", rendered["sql"]!.GetValue<string>());
        Assert.Equal($"\"{hostile}\"", rendered["params"]!["p0"]!.GetValue<string>());
    }

    [Fact]
    public void RepeatedFiltersAreAndedRatherThanReplaced()
    {
        var rendered = Query.Table("users")
            .Where(Query.Eq("email", "a@example.com"))
            .Where(Query.Gt("age", 30))
            .Render(_core);

        Assert.Contains(" AND ", rendered["sql"]!.GetValue<string>());
        Assert.Equal(2, rendered["params"]!.AsObject().Count);
    }

    [Fact]
    public void ABuilderNeverMutatesWhatItWasDerivedFrom()
    {
        // Two queries from one base is the case that matters. `with` copies the
        // record and the collection expressions build fresh lists — but "cannot
        // alias" is what the Go SDK's slices looked like too, and they could.
        var baseQuery = Query.Table("users").Select("id");
        var left = baseQuery.Select("email");
        var right = baseQuery.Select("name");

        Assert.Single(baseQuery.Ast.Columns);
        Assert.Equal("SELECT id, email FROM users", left.Render(_core)["sql"]!.GetValue<string>());
        Assert.Equal("SELECT id, name FROM users", right.Render(_core)["sql"]!.GetValue<string>());
    }

    [Fact]
    public void AnIdentifierThatCouldCarrySyntaxIsRefused()
    {
        // Refused rather than escaped. Escaping would mean deciding what is safe
        // to quote, and the safe answer is that a name needing quotes is not a
        // name this subset accepts.
        Assert.Throws<ProtocolException>(() => Query.Table("users; DROP TABLE x").Render(_core));
    }

    [Fact]
    public void AnEmptyProjectionSerialisesAsAListRatherThanNull()
    {
        // The Go SDK shipped this broken: an empty slice can be nil there and
        // marshals to `null`, which the core reads as a missing sequence. Pinned
        // in every binding rather than only in the one that got it wrong.
        var json = JsonSerializer.SerializeToNode(Query.Table("users").Ast)!;
        Assert.NotNull(json["columns"]);
        Assert.Equal(JsonValueKind.Array, json["columns"]!.GetValueKind());
        Assert.Equal(JsonValueKind.Array, json["orderBy"]!.GetValueKind());
        Assert.Equal("SELECT * FROM users", Query.Table("users").Render(_core)["sql"]!.GetValue<string>());
    }

    [Fact]
    public void AGeneratedEnumCarriesItsWireSpelling()
    {
        // Without `[JsonStringEnumMemberName]` an enum serialises as an integer,
        // which the server does not read — and that failure looks like a schema
        // mismatch rather than a naming one.
        Assert.Equal("\"autoApply\"", JsonSerializer.Serialize(Gate.AutoApply));
        Assert.Equal("\"upToDate\"", JsonSerializer.Serialize(Status.UpToDate));
    }
}
