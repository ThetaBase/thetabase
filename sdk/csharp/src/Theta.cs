// The client a caller holds.
//
// Everything about *what* a message means lives in the WebAssembly core, which
// `Scribe` loads. This file decides when to send one, unwraps the answer, and
// turns a refusal into an exception a caller can act on. It builds no wire
// messages, which is why adding a capability to the protocol reaches every
// binding at once.

using System.Net.Sockets;
using System.Text.Json.Nodes;

namespace ThetaBase;

/// <summary>Thrown when the Safety Layer refuses a change. Carries the diff to act on.</summary>
public sealed class SafetyGateException(string message, JsonNode? diff) : Exception(message)
{
    /// <summary>What the change would do, as the server classified it.</summary>
    public JsonNode? Diff { get; } = diff;
}

/// <summary>Thrown when the blast-radius breaker is open.</summary>
public sealed class CircuitBreakerException(string message, long windowRows, long ceiling)
    : Exception(message)
{
    public long WindowRows { get; } = windowRows;
    public long Ceiling { get; } = ceiling;
}

/// <summary>A precondition on a row's current state.</summary>
public abstract record Expect
{
    /// <summary>The row must not exist. Create-only.</summary>
    public sealed record Absent : Expect;

    /// <summary>The row must be at exactly this version.</summary>
    public sealed record Version(long Value) : Expect;

    internal object ToWire() =>
        this switch
        {
            Absent => new { kind = "absent" },
            Version v => (object)new { kind = "version", value = v.Value },
            _ => throw new ProtocolException($"unknown precondition {GetType().Name}"),
        };
}

/// <summary>One operation inside a transaction.</summary>
public sealed record TxOp
{
    public required string Key { get; init; }

    /// <summary>Null means unconditional, which is the common case.</summary>
    public Expect? Expect { get; init; }

    /// <summary>Null deletes the row; anything else writes it.</summary>
    public object? Value { get; init; }

    /// <summary>
    /// Whether this operation deletes.
    /// </summary>
    /// <remarks>
    /// Explicit rather than inferred from a null <see cref="Value"/>. Null is a
    /// value a caller may legitimately want to store, and a client that treated
    /// storing null as a delete would silently drop rows.
    /// </remarks>
    public bool Delete { get; init; }

    internal object ToWire() =>
        Delete
            ? new { key = Key, action = "delete", expect = Expect?.ToWire() }
            : new { key = Key, action = "put", value = Value, expect = Expect?.ToWire() };
}

/// <summary>
/// A connection to one ThetaBase project.
/// </summary>
/// <remarks>
/// No connection string, no API key, no <c>.env</c>: the project is named and
/// the scoped token is resolved and injected by the toolchain. There is
/// deliberately no constructor taking an address and a token — accepting one
/// would undo the property that exists to keep credentials out of application
/// code.
/// </remarks>
public sealed class Theta : IDisposable
{
    private readonly Scribe _core;
    private readonly Scribe.Connection _connection;
    private long _nextRequestId = 1;
    private long _branchId;

    private Theta(Scribe core, Scribe.Connection connection, string project)
    {
        _core = core;
        _connection = connection;
        Project = project;
    }

    public string Project { get; }

    /// <summary>Open a connection, taking the address and token from the environment.</summary>
    public static Theta Connect(Scribe core, string? project = null)
    {
        var address = Environment.GetEnvironmentVariable("THETA_ADDRESS");
        var token = Environment.GetEnvironmentVariable("THETA_TOKEN");
        if (string.IsNullOrEmpty(address) || string.IsNullOrEmpty(token))
        {
            throw new ProtocolException(
                "THETA_ADDRESS and THETA_TOKEN are not both set. Run this process "
                    + "under `theta exec`, which resolves a scoped token for the "
                    + "project and injects both."
            );
        }

        // `LastIndexOf` rather than `IndexOf`: an IPv6 address is full of
        // colons, and splitting on the first one yields nonsense.
        var separator = address.LastIndexOf(':');
        if (separator <= 0 || !int.TryParse(address[(separator + 1)..], out var port))
        {
            throw new ProtocolException($"`{address}` is not `host:port`");
        }

        var socket = new Socket(SocketType.Stream, ProtocolType.Tcp);
        socket.Connect(address[..separator], port);

        var connection = new Scribe.Connection(core, socket);
        // The handshake, before anything else travels. `thetad` also
        // re-authorises on every request, so this is the introduction rather
        // than the whole of the authentication.
        connection.Write(core.EncodeHello(token, "thetabase-csharp"));
        core.DecodeWelcome(connection.Frame());

        return new Theta(
            core,
            connection,
            project ?? Environment.GetEnvironmentVariable("THETA_PROJECT") ?? ""
        );
    }

    /// <summary>Operate on a named branch for subsequent calls.</summary>
    public void UseBranch(long branchId) => _branchId = branchId;

    /// <summary>Point lookup. Hot path: no model call, ever.</summary>
    public JsonNode? Get(string key)
    {
        var value = Expect(Call(new { op = "get", key }), "get");
        // `found: false` is a successful answer meaning the row is not there,
        // and it is distinct from a null value that is.
        return value?["found"]?.GetValue<bool>() == true ? value["value"] : null;
    }

    public string Put(string key, object? value) =>
        CommitId(Call(new { op = "put", key, value }));

    public string Delete(string key) => CommitId(Call(new { op = "delete", key }));

    /// <summary>A write conditional on the row's current state.</summary>
    /// <returns><c>null</c> when the condition was not met.</returns>
    /// <remarks>
    /// A failed precondition is not an exception: the request was well formed
    /// and the server did what it was asked. A lost-update retry that threw
    /// would make an ordinary contended key look like a fault.
    /// </remarks>
    public string? PutIf(string key, object? value, Expect expect)
    {
        var response = Call(new { op = "putIf", key, value, expect = expect.ToWire() });
        return Kind(response) == "preconditionFailed" ? null : CommitId(response);
    }

    /// <summary>Several writes that land as one commit, or none of them.</summary>
    /// <returns><c>null</c> when a precondition was not met.</returns>
    /// <remarks>
    /// Unlike sending several writes, this batches the durability boundary and
    /// not merely the network. Every operation's precondition is checked before
    /// any write is applied, so a transaction that would violate one changes
    /// nothing.
    /// </remarks>
    public string? Transaction(IEnumerable<TxOp> ops)
    {
        var response = Call(new { op = "transaction", ops = ops.Select(o => o.ToWire()) });
        return Kind(response) == "preconditionFailed" ? null : CommitId(response);
    }

    /// <summary>Run a typed plan. Rendered to SQL by the core, never here.</summary>
    public JsonNode? Query(object plan)
    {
        var rendered = _core.RenderQuery(plan);
        var response = Call(
            new { op = "query", sql = rendered["sql"]?.ToString(), @params = rendered["params"] }
        );
        return Expect(response, "query")?["rows"];
    }

    /// <summary>EXPLAIN without executing — what a reviewer reads before approving.</summary>
    public JsonNode? Explain(object plan)
    {
        var rendered = _core.RenderQuery(plan);
        var response = Call(
            new
            {
                op = "explain",
                sql = rendered["sql"]?.ToString(),
                @params = rendered["params"],
            }
        );
        return Expect(response, "explain");
    }

    public JsonNode? Status() => Expect(Call(new { op = "status" }), "status");

    /// <summary>What is in here, and where it came from.</summary>
    public JsonNode? Describe() => Expect(Call(new { op = "describe" }), "description");

    /// <summary>Submit a schema change. Always returns a diff; never applies anything.</summary>
    public JsonNode? Propose(object change) =>
        Expect(Call(new { op = "proposeSchemaChange", change }), "propose");

    /// <summary>Confirm a pending change, by id.</summary>
    public void Apply(string changeId, bool confirm) =>
        Expect(Call(new { op = "applySchemaChange", changeId, confirm }), "commit");

    /// <summary>Merge a validated shadow branch onto its target.</summary>
    public void Promote(string changeId) =>
        Expect(Call(new { op = "promoteChange", changeId }), "commit");

    /// <summary>Refuse a change and reclaim its shadow branch.</summary>
    public void Reject(string changeId, string reason) =>
        Expect(Call(new { op = "rejectChange", changeId, reason }), "ok");

    public void Dispose() => _connection.Dispose();

    private JsonNode Call(object request) =>
        _connection.Exchange(request, _nextRequestId++, _branchId);

    private static string? Kind(JsonNode response) => response["kind"]?.ToString();

    private static string CommitId(JsonNode response) =>
        Expect(response, "commit")?["commitId"]?.ToString()
        ?? throw new ProtocolException("a commit response carried no commit id");

    /// <summary>
    /// Unwrap a response of the expected kind, or throw something actionable.
    /// </summary>
    /// <remarks>
    /// The three named failures stay distinct. A gate refusal carries the diff
    /// the caller has to act on and is an answer rather than a fault; an open
    /// breaker is a limit this project set rather than a bad request; and
    /// everything else is an error with a message. Collapsing them would make
    /// the first two unactionable, which is why the wire separates them.
    /// </remarks>
    private static JsonNode? Expect(JsonNode response, string kind)
    {
        if (Kind(response) == kind)
        {
            return response["value"];
        }

        if (Kind(response) == "error")
        {
            var value = response["value"];
            var message = value?["message"]?.ToString() ?? "the server refused the request";
            var code = value?["code"]?.ToString() ?? "";

            throw code switch
            {
                "ConfirmationRequired" => new SafetyGateException(message, value?["diff"]),
                "BreakerOpen" => new CircuitBreakerException(
                    message,
                    value?["windowRows"]?.GetValue<long>() ?? 0,
                    value?["ceiling"]?.GetValue<long>() ?? 0
                ),
                _ => new ProtocolException(message),
            };
        }

        // What the core produces for a response this build does not know.
        // Reported as a version problem because that is what it is, and
        // upgrading is the remedy.
        if (Kind(response) == "raw")
        {
            throw new ProtocolException(
                "the server sent a response this SDK does not understand; it is "
                    + "newer than this client. Upgrade the SDK."
            );
        }

        throw new ProtocolException($"expected a `{kind}` response, got `{Kind(response)}`");
    }
}
