// Runs the shared conformance suite from C#, against a live thetad.
//
// Prints one JSON document on stdout. Every other runner prints the same
// document from the same cases, and `make conformance` diffs them: bindings that
// each pass their own suite prove nothing about whether they agree.

using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace ThetaBase;

public static class Conformance
{
    /// <summary>Drive the client a customer holds, against a live instance.</summary>
    /// <remarks>
    /// The conformance suite above exercises the protocol core through this
    /// binding's shim. Nothing there touches <see cref="Theta"/>, so without
    /// this the C# client could be wired and broken at once — which is exactly
    /// what the Python and TypeScript clients turned out to be.
    /// </remarks>
    private static int Smoke(string root, string address, string token)
    {
        Environment.SetEnvironmentVariable("THETA_ADDRESS", address);
        Environment.SetEnvironmentVariable("THETA_TOKEN", token);

        using var core = Scribe.Load(
            Path.Combine(root, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"));

        var failures = new List<string>();
        void Check(string name, bool held, string detail = "")
        {
            Console.WriteLine($"  {(held ? "ok  " : "FAIL")}  {name}");
            if (!held) failures.Add($"{name}: {detail}");
        }

        using (var theta = Theta.Connect(core, "conformance"))
        {
            Check("get of an absent row is null", theta.Get("cs/absent") is null);

            theta.Put("cs/a", 1);
            Check("put then get round-trips", theta.Get("cs/a")?.GetValue<int>() == 1);

            var created = theta.PutIf("cs/b", 2, new Expect.Absent());
            Check("a create-only write lands when the row is absent", created is not null);

            var again = theta.PutIf("cs/b", 3, new Expect.Absent());
            Check("the same write is refused once the row exists", again is null);
            Check("the refused write changed nothing", theta.Get("cs/b")?.GetValue<int>() == 2);

            var committed = theta.Transaction(new[]
            {
                new TxOp { Key = "cs/tx1", Value = 10 },
                new TxOp { Key = "cs/tx2", Value = 20 },
            });
            Check("a transaction commits", committed is not null);
            Check("both of its writes are visible", theta.Get("cs/tx2")?.GetValue<int>() == 20);

            // The precondition fails on the *second* operation; the first must
            // not survive it.
            var refused = theta.Transaction(new[]
            {
                new TxOp { Key = "cs/tx3", Value = 30 },
                new TxOp { Key = "cs/b", Value = 99, Expect = new Expect.Absent() },
            });
            Check("a transaction with a failing precondition is refused", refused is null);
            Check("its other write did not land", theta.Get("cs/tx3") is null);

            theta.Delete("cs/a");
            Check("delete removes the row", theta.Get("cs/a") is null);
        }

        if (failures.Count > 0)
        {
            Console.Error.WriteLine(string.Join(Environment.NewLine, failures));
            return 1;
        }
        return 0;
    }

    public static int Main(string[] args)
    {
        if (args.Length < 2)
        {
            Console.Error.WriteLine("usage: Conformance <host:port> <token>");
            return 2;
        }

        var root = RepoRoot();

        // A second mode in this runner rather than a second project: the
        // conformance build is already in the gate, and a separate executable
        // would be a second thing to keep built. `--smoke` drives the
        // high-level `Theta` client; the default drives the protocol core.
        if (args.Contains("--smoke"))
        {
            return Smoke(root, args[0], args[1]);
        }

        var suite = JsonNode.Parse(File.ReadAllText(Path.Combine(root, "sdk/conformance/cases.json")))!;

        using var core = Scribe.Load(
            Path.Combine(root, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"));

        var colon = args[0].LastIndexOf(':');
        var socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        socket.Connect(args[0][..colon], int.Parse(args[0][(colon + 1)..]));

        var report = new JsonObject();
        var results = new JsonArray();
        var queries = new JsonArray();

        using (var connection = new Scribe.Connection(core, socket))
        {
            // Handshake first: the server refuses anything else until it has one.
            connection.Write(core.EncodeHello(args[1], "conformance-csharp"));
            var welcome = core.DecodeWelcome(connection.Frame());
            report["protocolVersion"] = welcome["protocolVersion"]!.GetValue<int>();

            long requestId = 1;
            foreach (var testCase in suite["cases"]!.AsArray())
            {
                results.Add(Exchange(core, connection, testCase!, requestId++));
            }
        }

        // The typed builder renders rather than calls, so these need no server.
        // Each binding builds with its own API; the rendered output is compared.
        foreach (var testCase in suite["queries"]!.AsArray())
        {
            queries.Add(Render(core, testCase!));
        }

        report["results"] = results;
        report["queries"] = queries;

        // A UTF-8 stream, explicitly. The console's default encoding on Windows
        // is the platform codepage, and one refusal message contains an em-dash
        // — this output is compared byte for byte against five other runners, so
        // that would fail the gate over a character rather than the protocol.
        using var stdout = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false));
        stdout.WriteLine(Pretty(report, ""));
        return 0;
    }

    private static JsonObject Exchange(
        Scribe core, Scribe.Connection connection, JsonNode testCase, long requestId)
    {
        var outcome = new JsonObject { ["name"] = testCase["name"]!.GetValue<string>() };
        try
        {
            var request = JsonSerializer.Deserialize<object>(testCase["request"]!.ToJsonString());
            var response = connection.Exchange(request!, requestId, 0);
            outcome["status"] = "ok";
            outcome["response"] = Redact(response, testCase["redact"]);
        }
        catch (ProtocolException e)
        {
            outcome["status"] = "clientError";
            outcome["message"] = FirstLine(e.Message);
        }
        catch (IOException e)
        {
            outcome["status"] = "transportError";
            outcome["message"] = FirstLine(e.Message);
        }
        return outcome;
    }

    private static JsonObject Render(Scribe core, JsonNode testCase)
    {
        var outcome = new JsonObject { ["name"] = testCase["name"]!.GetValue<string>() };
        var spec = testCase["build"]!;
        try
        {
            var query = Query.Table(spec["table"]!.GetValue<string>());
            foreach (var column in Array(spec["select"]))
            {
                query = query.Select(column!.GetValue<string>());
            }
            foreach (var clause in Array(spec["where"]))
            {
                query = query.Where(Predicate(
                    clause![0]!.GetValue<string>(),
                    clause[1]!.GetValue<string>(),
                    JsonSerializer.Deserialize<object>(clause[2]!.ToJsonString())));
            }
            foreach (var clause in Array(spec["orderBy"]))
            {
                query = query.OrderBy(clause![0]!.GetValue<string>(), clause[1]!.GetValue<bool>());
            }
            if (spec["limit"] is { } limit)
            {
                query = query.Limit(limit.GetValue<long>());
            }
            if (spec["offset"] is { } offset)
            {
                query = query.Offset(offset.GetValue<long>());
            }

            outcome["status"] = "ok";
            outcome["rendered"] = query.Render(core);
        }
        catch (ProtocolException e)
        {
            outcome["status"] = "clientError";
            outcome["message"] = FirstLine(e.Message);
        }
        catch (Exception e)
        {
            outcome["status"] = "error";
            outcome["message"] = FirstLine(e.Message);
        }
        return outcome;
    }

    /// <summary>Map a case's operator name onto the builder.</summary>
    /// <remarks>
    /// A switch rather than reflection, which is what the Node and Python
    /// runners use. It has one advantage worth keeping: a case naming an
    /// operator this SDK does not have fails here by name rather than three
    /// frames later.
    /// </remarks>
    private static Predicate Predicate(string op, string column, object? value) => op switch
    {
        "eq" => Query.Eq(column, value),
        "ne" => Query.Ne(column, value),
        "lt" => Query.Lt(column, value),
        "lte" => Query.Lte(column, value),
        "gt" => Query.Gt(column, value),
        "gte" => Query.Gte(column, value),
        "isNull" => Query.IsNull(column),
        "notNull" => Query.NotNull(column),
        _ => throw new ArgumentException(
            $"the suite names an operator this SDK does not have: \"{op}\""),
    };

    /// <summary>Blank out values that legitimately differ between runs.</summary>
    private static JsonNode Redact(JsonNode response, JsonNode? paths)
    {
        foreach (var dotted in Array(paths))
        {
            var parts = dotted!.GetValue<string>().Split('.');
            JsonNode? node = response;
            foreach (var part in parts[..^1])
            {
                node = node?[part];
            }
            if (node is JsonObject target && target.ContainsKey(parts[^1]))
            {
                target[parts[^1]] = "<redacted>";
            }
        }
        return response;
    }

    private static IEnumerable<JsonNode?> Array(JsonNode? node) =>
        node is JsonArray array ? array : [];

    /// <summary>Print the way `JSON.stringify(value, null, 2)` does.</summary>
    /// <remarks>
    /// `JsonSerializerOptions.WriteIndented` indents with four spaces in .NET 9
    /// and writes `{}` for an empty object where JavaScript writes `{}` too —
    /// but the indent alone is enough to fail a byte-for-byte comparison, and
    /// `IndentSize` is newer than the framework this targets is guaranteed to
    /// have everywhere. Twenty lines is cheaper than the version check.
    /// </remarks>
    private static string Pretty(JsonNode? node, string indent)
    {
        var inner = indent + "  ";
        switch (node)
        {
            case JsonObject obj:
                if (obj.Count == 0)
                {
                    return "{}";
                }
                var fields = obj.Select(field =>
                    $"{inner}{Scalar(JsonValue.Create(field.Key))}: {Pretty(field.Value, inner)}");
                return "{\n" + string.Join(",\n", fields) + "\n" + indent + "}";

            case JsonArray array:
                if (array.Count == 0)
                {
                    return "[]";
                }
                var items = array.Select(item => inner + Pretty(item, inner));
                return "[\n" + string.Join(",\n", items) + "\n" + indent + "]";

            default:
                return Scalar(node);
        }
    }

    /// <summary>Serialise one scalar without HTML escaping.</summary>
    /// <remarks>
    /// `System.Text.Json` escapes `&lt;`, `&gt;` and `&amp;` by default, on the
    /// assumption the output lands in a web page. JavaScript's `JSON.stringify`
    /// does not, and this document is compared byte for byte against five other
    /// runners — one of which redacts a value to `&lt;redacted&gt;`. Go's
    /// `encoding/json` needed the same switch thrown for the same reason.
    /// </remarks>
    private static string Scalar(JsonNode? node) => node?.ToJsonString(Unescaped) ?? "null";

    private static readonly JsonSerializerOptions Unescaped = new()
    {
        Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    };

    private static string FirstLine(string text) => text.Split('\n')[0];

    private static string RepoRoot()
    {
        var dir = Directory.GetCurrentDirectory();
        while (dir is not null)
        {
            if (File.Exists(Path.Combine(dir, "sdk/conformance/cases.json")))
            {
                return dir;
            }
            dir = Directory.GetParent(dir)?.FullName;
        }
        throw new InvalidOperationException("no ThetaBase workspace above the working directory");
    }
}
