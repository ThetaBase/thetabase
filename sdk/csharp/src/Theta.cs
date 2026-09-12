// The client a caller holds.
//
// Everything about *what* a message means lives in the WebAssembly core, which
// `Scribe` loads. This file decides when to send one, unwraps the answer, and
// turns a refusal into an exception a caller can act on. It builds no wire
// messages, which is why adding a capability to the protocol reaches every
// binding at once.

using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography.X509Certificates;
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

        var connection = new Scribe.Connection(core, Dial(address));
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

    /// <summary>Open a connection to <paramref name="address"/>, with TLS if the
    /// address calls for it.</summary>
    /// <remarks>
    /// <para>
    /// Public and separate from <see cref="Connect"/> because anything that
    /// needs a connection needs this exact behaviour. When the parsing, the
    /// socket and the TLS decision lived inline in <c>Connect</c>, the
    /// conformance harness wrote its own and got a plaintext socket — which is
    /// part of how this SDK shipped with no TLS at all.
    /// </para>
    /// <para>
    /// <paramref name="address"/> is <c>host:port</c>, as <c>THETA_ADDRESS</c>
    /// carries it.
    /// </para>
    /// </remarks>
    public static Stream Dial(string address)
    {
        // `LastIndexOf` rather than `IndexOf`: an IPv6 address is full of
        // colons, and splitting on the first one yields nonsense.
        var separator = address.LastIndexOf(':');
        if (separator <= 0 || !int.TryParse(address[(separator + 1)..], out var port))
        {
            throw new ProtocolException(
                $"`{address}` is not `host:port`. THETA_ADDRESS is set by `theta exec`; "
                    + "a hand-written value is usually a URL by mistake."
            );
        }

        var host = address[..separator].Trim('[', ']');

        // No AddressFamily: the parameterless constructor picks one from the
        // resolved address, so an IPv6-only host is reachable. Naming
        // `InterNetwork` here — which the conformance harness did — makes the
        // client IPv4-only.
        var socket = new Socket(SocketType.Stream, ProtocolType.Tcp);
        // Nagle off. Every exchange is one small frame and then a wait for the
        // answer, which is the exact shape Nagle delays.
        socket.NoDelay = true;
        socket.Connect(host, port);

        Stream transport = new NetworkStream(socket, ownsSocket: true);
        return WantsTls(port) ? OpenTls(transport, host) : transport;
    }

    /// <summary>Whether this address should be dialled with TLS.</summary>
    /// <remarks>
    /// <para>
    /// <c>THETA_TLS</c> wins when it is set, so a self-hosted instance behind a
    /// terminating proxy on some other port can say so — and so can a developer
    /// tunnelling 443 to a local plaintext process.
    /// </para>
    /// <para>
    /// Otherwise: port 443 means TLS. That is the port Fly's proxy listens on
    /// and the port the Control Plane hands out, and it is the only port in this
    /// product's vocabulary that implies a terminator in front of
    /// <c>thetad</c>. Local instances are on 7700 and upwards.
    /// </para>
    /// <para>
    /// Inferred from the address rather than carried beside it, deliberately,
    /// and identically to the Rust, Python and TypeScript clients. The address
    /// travels through <c>THETA_ADDRESS</c> into every SDK; a second variable
    /// that had to agree with it would be a second thing to get wrong, and the
    /// symptom of getting it wrong is a <em>hang</em> rather than an error — a
    /// plaintext frame sent to a TLS listener is read as a ClientHello, found
    /// malformed, and discarded.
    /// </para>
    /// </remarks>
    public static bool WantsTls(int port)
    {
        var over = Environment.GetEnvironmentVariable("THETA_TLS")?.ToLowerInvariant();
        if (over is "1" or "true" or "require" or "yes")
        {
            return true;
        }
        if (over is "0" or "false" or "off" or "no")
        {
            return false;
        }
        return port == 443;
    }

    /// <summary>Wrap a connected stream in TLS, verifying the certificate.</summary>
    /// <remarks>
    /// <para>
    /// <c>targetHost</c> is the SNI name, and on a shared address it is what the
    /// proxy routes on — so getting it wrong does not produce a certificate
    /// error, it produces a connection to the wrong instance or to none.
    /// </para>
    /// <para>
    /// <c>THETA_TLS_CA</c> adds a deployment's own CA to the platform roots. It
    /// widens what the client will accept; it does not pin. A customer running
    /// <c>thetad</c> behind their own terminating proxy signs with their own CA,
    /// and a client that trusts only the public set cannot reach it — and "turn
    /// TLS off instead" is not an answer for a database.
    /// </para>
    /// <para>
    /// There is deliberately no way to disable verification. A flag that turns
    /// off certificate checking is a flag that ends up set in production, and
    /// the whole reason this transport exists is that a session token was about
    /// to cross the open internet.
    /// </para>
    /// </remarks>
    private static SslStream OpenTls(Stream transport, string targetHost)
    {
        var extra = Environment.GetEnvironmentVariable("THETA_TLS_CA");
        X509Certificate2Collection? roots = null;
        if (!string.IsNullOrEmpty(extra))
        {
            // A refusal, not a warning. An operator who set this has said
            // "trust this CA"; continuing without it would silently connect
            // under a laxer trust policy than the one they chose.
            try
            {
                roots = new X509Certificate2Collection();
                roots.ImportFromPemFile(extra);
            }
            catch (Exception cause)
            {
                throw new ProtocolException(
                    $"THETA_TLS_CA points at `{extra}`, which cannot be read as PEM: {cause.Message}"
                );
            }
            if (roots.Count == 0)
            {
                throw new ProtocolException(
                    $"THETA_TLS_CA `{extra}` contains no certificates. An empty trust "
                        + "file is almost certainly the wrong file, and ignoring it would "
                        + "mean connecting under a trust policy nobody chose."
                );
            }
        }

        var ssl = new SslStream(
            transport,
            leaveInnerStreamOpen: false,
            userCertificateValidationCallback: roots is null
                ? null
                : (_, certificate, chain, errors) =>
                {
                    // Only the "unknown authority" case is reconsidered, and
                    // only against the CA the operator named. A name mismatch
                    // or an expired certificate is still a refusal.
                    if (errors == SslPolicyErrors.None)
                    {
                        return true;
                    }
                    if (errors != SslPolicyErrors.RemoteCertificateChainErrors)
                    {
                        return false;
                    }
                    if (certificate is null || chain is null)
                    {
                        return false;
                    }

                    using var rebuilt = new X509Chain();
                    rebuilt.ChainPolicy.TrustMode = X509ChainTrustMode.CustomRootTrust;
                    rebuilt.ChainPolicy.CustomTrustStore.AddRange(roots);
                    rebuilt.ChainPolicy.RevocationMode = X509RevocationMode.NoCheck;
                    return rebuilt.Build(new X509Certificate2(certificate));
                }
        );

        try
        {
            ssl.AuthenticateAsClient(
                new SslClientAuthenticationOptions
                {
                    TargetHost = targetHost,
                    EnabledSslProtocols = SslProtocols.Tls12 | SslProtocols.Tls13,
                }
            );
        }
        catch (Exception cause)
        {
            ssl.Dispose();
            throw new ProtocolException(
                $"the TLS handshake with `{targetHost}` failed: {cause.Message}. A "
                    + "provisioned instance presents a publicly signed certificate; a "
                    + "self-hosted one needs its CA in THETA_TLS_CA."
            );
        }

        return ssl;
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
