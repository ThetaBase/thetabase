// The host half of Scribe: sockets, and nothing else.
//
// Every decision about the protocol — how a request is encoded, what a response
// means, when a cached read must be dropped — lives in the WebAssembly core
// (`crates/theta-scribe-wasm`). This file moves bytes.
//
// The split is why a sixth SDK is cheap. A C# SDK that built wire messages in C#
// would be a sixth implementation of the protocol, and the first time the schema
// moved five of the six would be wrong. What must be identical across languages
// is the protocol; what must differ is I/O.

using System.Buffers.Binary;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Wasmtime;

namespace ThetaBase;

/// <summary>A protocol error the core rejected, kept distinct from a transport failure.</summary>
/// <remarks>
/// The distinction is the whole point: a caller may retry a transport failure
/// and must not retry a protocol one. Collapsing them would turn "you sent
/// something invalid" into "try again", forever.
/// </remarks>
public sealed class ProtocolException(string message) : Exception(message);

/// <summary>The WebAssembly core, loaded once per process.</summary>
public sealed class Scribe : IDisposable
{
    /// <summary>`[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out.</summary>
    private const int ResultHeader = 5;

    private const int LengthPrefixBytes = 4;

    /// <summary>What the core returns for a frame length this protocol forbids.</summary>
    private const uint NoLength = 0xffffffff;

    private static readonly JsonSerializerOptions Json = new()
    {
        // The core reads and writes exactly what it is given; nothing here
        // should reformat it on the way past.
        WriteIndented = false,
    };

    private readonly Engine _engine;
    private readonly Store _store;
    private readonly Instance _instance;
    private readonly Memory _memory;
    private readonly Lock _lock = new();

    private Scribe(Engine engine, Store store, Instance instance)
    {
        _engine = engine;
        _store = store;
        _instance = instance;
        _memory = instance.GetMemory("memory")
            ?? throw new InvalidOperationException(
                "this Scribe core exports no memory — it was built from a different revision");
    }

    /// <summary>Load the core from a module's bytes.</summary>
    public static Scribe Load(byte[] wasm)
    {
        var engine = new Engine();
        // No host imports: the core is pure computation, which is why it loads
        // identically here, in Node, in a browser, in Go and on the JVM.
        var module = Module.FromBytes(engine, "theta_scribe_wasm", wasm);
        var store = new Store(engine);
        return new Scribe(engine, store, new Linker(engine).Instantiate(store, module));
    }

    /// <summary>Load the core from a file.</summary>
    public static Scribe Load(string path) => Load(File.ReadAllBytes(path));

    public void Dispose()
    {
        _store.Dispose();
        _engine.Dispose();
    }

    // ---- the core's surface -------------------------------------------------

    /// <summary>Frame a request.</summary>
    public byte[] Encode(object request, long requestId, long branchId) =>
        Call("theta_encode", Serialize(request), requestId, branchId);

    /// <summary>Read a response body into a document.</summary>
    public JsonNode Decode(byte[] body) => Parse(Call("theta_decode", body));

    /// <summary>Read the four-byte frame prefix.</summary>
    public int BodyLength(byte[] prefix)
    {
        lock (_lock)
        {
            var pointer = Allocate(LengthPrefixBytes);
            try
            {
                prefix.CopyTo(_memory.GetSpan(pointer, prefix.Length));
                var length = (uint)Invoke("theta_body_length", pointer);
                if (length == NoLength)
                {
                    throw new ProtocolException(
                        "the peer sent a length this protocol does not allow");
                }
                return (int)length;
            }
            finally
            {
                Free(pointer, LengthPrefixBytes);
            }
        }
    }

    /// <summary>Frame the handshake.</summary>
    /// <remarks>
    /// The protocol version is the core's to state, not the host's: a host that
    /// could name its own version could claim a compatibility it does not have,
    /// and the negotiation that refuses mismatched versions rather than guessing
    /// would be negotiating with itself.
    /// </remarks>
    public byte[] EncodeHello(string token, string clientName) =>
        Call("theta_encode_hello",
            Serialize(new Dictionary<string, string> { ["token"] = token, ["clientName"] = clientName }));

    /// <summary>Read the server's reply to a handshake. A refusal throws.</summary>
    public JsonNode DecodeWelcome(byte[] body) => Parse(Call("theta_decode_welcome", body));

    /// <summary>Render a typed query AST to SQL-subset source and bound parameters.</summary>
    /// <remarks>
    /// In the core rather than here, so a query built the same way in Python
    /// renders to the same bytes — and so the rule that a value never becomes
    /// query text has one implementation.
    /// </remarks>
    public JsonNode RenderQuery(object ast) => Parse(Call("theta_render_query", Serialize(ast)));

    /// <summary>What a request invalidates: one key, everything, or nothing.</summary>
    public JsonNode Invalidation(object request) =>
        Parse(Call("theta_invalidation", Serialize(request)));

    // ---- the byte-buffer boundary -------------------------------------------

    private Function Export(string name) =>
        _instance.GetFunction(name)
        // Named rather than deferred to a null dereference at the first call: a
        // core built from a different revision is a real failure mode, and
        // "theta_render_query is missing" says which revision.
        ?? throw new InvalidOperationException(
            $"{name} is not exported by this Scribe core — it was built from a "
            + "different revision of the schema");

    private long Invoke(string name, params ValueBox[] args) =>
        Convert.ToInt64(Export(name).Invoke(args)
            ?? throw new ProtocolException($"{name} returned nothing"));

    private int Allocate(int length) => (int)Invoke("theta_alloc", length);

    private void Free(int pointer, int length) => Export("theta_free").Invoke(pointer, length);

    private byte[] Call(string name, byte[] input, params long[] extra)
    {
        // The core owns one linear memory and every call allocates into it, so
        // two threads encoding at once would interleave allocations against one
        // heap. Encoding is microseconds and a connection is sequential anyway.
        lock (_lock)
        {
            var pointer = Allocate(input.Length);
            try
            {
                input.CopyTo(_memory.GetSpan(pointer, input.Length));
                var args = new ValueBox[2 + extra.Length];
                args[0] = pointer;
                args[1] = input.Length;
                for (var i = 0; i < extra.Length; i++)
                {
                    args[2 + i] = extra[i];
                }
                return Take((int)Convert.ToInt64(Export(name).Invoke(args)!));
            }
            finally
            {
                Free(pointer, input.Length);
            }
        }
    }

    /// <summary>Read a result buffer out of the core's memory and free it.</summary>
    /// <remarks>
    /// The tag byte distinguishes a result from a refusal, and it is checked
    /// before the payload is used — the refusal path must not depend on the
    /// success path having been valid.
    /// </remarks>
    private byte[] Take(int pointer)
    {
        var header = new byte[ResultHeader];
        _memory.GetSpan(pointer, ResultHeader).CopyTo(header);
        var length = (int)BinaryPrimitives.ReadUInt32LittleEndian(header);
        var tag = header[4];

        var payload = new byte[length];
        _memory.GetSpan(pointer + ResultHeader, length).CopyTo(payload);
        Export("theta_free_result").Invoke(pointer);

        if (tag != 0)
        {
            throw new ProtocolException(Encoding.UTF8.GetString(payload));
        }
        return payload;
    }

    private static byte[] Serialize(object value) => JsonSerializer.SerializeToUtf8Bytes(value, Json);

    private static JsonNode Parse(byte[] bytes) =>
        JsonNode.Parse(bytes)
        ?? throw new ProtocolException("the core returned something that is not JSON");

    // ---- transport ----------------------------------------------------------

    /// <summary>A framed connection to `thetad`.</summary>
    /// <remarks>
    /// Takes a <see cref="Stream"/> rather than a <see cref="Socket"/> so the
    /// same framing serves a plaintext connection and a TLS one: a provisioned
    /// instance is reached through Fly's TLS proxy, and <c>SslStream</c> is a
    /// stream. The socket overload below keeps every existing caller working.
    /// </remarks>
    public sealed class Connection(Scribe core, Stream transport) : IDisposable
    {
        /// <summary>A connection over a bare socket, for a plaintext instance.</summary>
        public Connection(Scribe core, Socket socket)
            : this(core, new NetworkStream(socket, ownsSocket: true)) { }

        public void Write(byte[] bytes)
        {
            transport.Write(bytes, 0, bytes.Length);
            // Flushed explicitly. `NetworkStream.Flush` is a no-op, but
            // `SslStream` buffers -- so without this the handshake frame would
            // sit in the client and the server would wait for it.
            transport.Flush();
        }

        /// <summary>Read exactly <paramref name="n"/> bytes, or fail if the peer closes first.</summary>
        public byte[] ReadFully(int n)
        {
            var buffer = new byte[n];
            var read = 0;
            // A stream delivers whatever arrived, not what was asked for, and a
            // short read here would frame the next message wrong.
            while (read < n)
            {
                var got = transport.Read(buffer, read, n - read);
                if (got <= 0)
                {
                    throw new IOException("the server closed the connection");
                }
                read += got;
            }
            return buffer;
        }

        /// <summary>Read one length-prefixed frame.</summary>
        public byte[] Frame() => ReadFully(core.BodyLength(ReadFully(LengthPrefixBytes)));

        /// <summary>One request/response exchange.</summary>
        /// <remarks>
        /// Sequential by construction: a caller that needs concurrency opens
        /// more connections. Multiplexing on one socket would need request-id
        /// correlation on the read side, and correlating replies to the wrong
        /// request is a data-corruption bug rather than a performance one.
        /// </remarks>
        public JsonNode Exchange(object request, long requestId, long branchId)
        {
            Write(core.Encode(request, requestId, branchId));
            return core.Decode(Frame());
        }

        public void Dispose() => transport.Dispose();
    }
}
