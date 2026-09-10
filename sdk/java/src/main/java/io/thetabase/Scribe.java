// The host half of Scribe: sockets, and nothing else.
//
// Every decision about the protocol — how a request is encoded, what a response
// means, when a cached read must be dropped — lives in the WebAssembly core
// (`crates/theta-scribe-wasm`). This file moves bytes.
//
// The split is why a fifth SDK is cheap. A Java SDK that built wire messages in
// Java would be a fifth implementation of the protocol, and the first time the
// schema moved four of the five would be wrong. What must be identical across
// languages is the protocol; what must differ is I/O.

package io.thetabase;

import com.dylibso.chicory.runtime.ExportFunction;
import com.dylibso.chicory.runtime.Instance;
import com.dylibso.chicory.runtime.Memory;
import com.dylibso.chicory.wasm.Parser;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;

/** The WebAssembly core, loaded once per process. */
public final class Scribe implements AutoCloseable {

    /** `[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out. */
    private static final int RESULT_HEADER = 5;

    private static final int LENGTH_PREFIX_BYTES = 4;

    /** What the core returns for a frame length this protocol does not allow. */
    private static final long NO_LENGTH = 0xffffffffL;

    private static final ObjectMapper JSON = new ObjectMapper();

    private final Instance instance;
    private final ExportFunction alloc;
    private final ExportFunction free;
    private final ExportFunction freeResult;
    private final ExportFunction encode;
    private final ExportFunction decode;
    private final ExportFunction bodyLength;
    private final ExportFunction invalidation;
    private final ExportFunction encodeHello;
    private final ExportFunction decodeWelcome;
    private final ExportFunction renderQuery;

    private Scribe(Instance instance) {
        this.instance = instance;
        this.alloc = export("theta_alloc");
        this.free = export("theta_free");
        this.freeResult = export("theta_free_result");
        this.encode = export("theta_encode");
        this.decode = export("theta_decode");
        this.bodyLength = export("theta_body_length");
        this.invalidation = export("theta_invalidation");
        this.encodeHello = export("theta_encode_hello");
        this.decodeWelcome = export("theta_decode_welcome");
        this.renderQuery = export("theta_render_query");
    }

    private ExportFunction export(String name) {
        try {
            return instance.export(name);
        } catch (RuntimeException e) {
            // Named rather than deferred to a null dereference at the first
            // call: a core built from a different revision is a real failure
            // mode, and "theta_render_query is missing" says which revision.
            throw new IllegalStateException(
                    name + " is not exported by this Scribe core — it was built from a "
                            + "different revision of the schema", e);
        }
    }

    /** Load the core from a module's bytes. */
    public static Scribe load(byte[] wasm) {
        // No host imports: the core is pure computation, which is why it loads
        // identically here, in Node, in a browser and in Go.
        return new Scribe(Instance.builder(Parser.parse(wasm)).build());
    }

    /** Load the core from a file. */
    public static Scribe load(Path wasm) throws IOException {
        return load(Files.readAllBytes(wasm));
    }

    @Override
    public void close() {
        // Chicory holds no OS resources; the instance is ordinary heap. Present
        // so callers can use try-with-resources without knowing that, and so
        // this stays a compatible place to release something later.
    }

    // ---- the core's surface -------------------------------------------------

    /** Frame a request. */
    public byte[] encode(Object request, long requestId, long branchId) {
        return call(encode, json(request), requestId, branchId);
    }

    /** Read a response body into a document. */
    public JsonNode decode(byte[] body) {
        return parse(call(decode, body));
    }

    /** Read the four-byte frame prefix. */
    public int bodyLength(byte[] prefix) {
        int pointer = allocate(LENGTH_PREFIX_BYTES);
        try {
            instance.memory().write(pointer, prefix);
            long length = bodyLength.apply(pointer)[0] & 0xffffffffL;
            if (length == NO_LENGTH) {
                throw new ProtocolException(
                        "the peer sent a length this protocol does not allow");
            }
            return (int) length;
        } finally {
            free.apply(pointer, LENGTH_PREFIX_BYTES);
        }
    }

    /**
     * Frame the handshake.
     *
     * <p>The protocol version is the core's to state, not the host's: a host
     * that could name its own version could claim a compatibility it does not
     * have, and the negotiation that refuses mismatched versions rather than
     * guessing would be negotiating with itself.
     */
    public byte[] encodeHello(String token, String clientName) {
        return call(encodeHello, json(Map.of("token", token, "clientName", clientName)));
    }

    /** Read the server's reply to a handshake. A refusal throws. */
    public JsonNode decodeWelcome(byte[] body) {
        return parse(call(decodeWelcome, body));
    }

    /**
     * Render a typed query AST to SQL-subset source and bound parameters.
     *
     * <p>In the core rather than here, so a query built the same way in Python
     * renders to the same bytes — and so the rule that a value never becomes
     * query text has one implementation.
     */
    public JsonNode renderQuery(Object ast) {
        return parse(call(renderQuery, json(ast)));
    }

    /** What a request invalidates: one key, everything, or nothing. */
    public JsonNode invalidation(Object request) {
        return parse(call(invalidation, json(request)));
    }

    // ---- the byte-buffer boundary -------------------------------------------

    private int allocate(int length) {
        return (int) alloc.apply(length)[0];
    }

    private byte[] call(ExportFunction fn, byte[] input, long... extra) {
        int pointer = allocate(input.length);
        try {
            instance.memory().write(pointer, input);
            long[] args = new long[2 + extra.length];
            args[0] = pointer;
            args[1] = input.length;
            System.arraycopy(extra, 0, args, 2, extra.length);
            return take((int) fn.apply(args)[0]);
        } finally {
            free.apply(pointer, input.length);
        }
    }

    /**
     * Read a result buffer out of the core's memory and free it.
     *
     * <p>The tag byte distinguishes a result from a refusal, and it is checked
     * before the payload is used — the refusal path must not depend on the
     * success path having been valid.
     */
    private byte[] take(int pointer) {
        Memory memory = instance.memory();
        int length = ByteBuffer.wrap(memory.readBytes(pointer, 4))
                .order(ByteOrder.LITTLE_ENDIAN)
                .getInt();
        byte tag = memory.read(pointer + 4);
        byte[] payload = memory.readBytes(pointer + RESULT_HEADER, length);
        freeResult.apply(pointer);

        if (tag != 0) {
            throw new ProtocolException(new String(payload, java.nio.charset.StandardCharsets.UTF_8));
        }
        return payload;
    }

    private static byte[] json(Object value) {
        try {
            return JSON.writeValueAsBytes(value);
        } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
            throw new IllegalArgumentException("that request is not encodable as JSON", e);
        }
    }

    private static JsonNode parse(byte[] bytes) {
        try {
            return JSON.readTree(bytes);
        } catch (IOException e) {
            throw new ProtocolException("the core returned something that is not JSON: " + e);
        }
    }

    /** A protocol error the core rejected, kept distinct from a transport failure. */
    public static final class ProtocolException extends RuntimeException {
        public ProtocolException(String message) {
            super(message);
        }
    }

    // ---- transport ----------------------------------------------------------

    /** A framed connection to `thetad`. */
    public static final class Connection implements AutoCloseable {
        private final Scribe core;
        private final Socket socket;
        private final InputStream in;
        private final OutputStream out;

        public Connection(Scribe core, Socket socket) throws IOException {
            this.core = core;
            this.socket = socket;
            this.in = socket.getInputStream();
            this.out = socket.getOutputStream();
        }

        public void write(byte[] bytes) throws IOException {
            out.write(bytes);
            out.flush();
        }

        /** Read exactly {@code n} bytes, or fail if the peer closes first. */
        public byte[] readFully(int n) throws IOException {
            byte[] buffer = new byte[n];
            int read = 0;
            // A socket delivers whatever arrived, not what was asked for, and a
            // short read here would frame the next message wrong.
            while (read < n) {
                int got = in.read(buffer, read, n - read);
                if (got < 0) {
                    throw new IOException("the server closed the connection");
                }
                read += got;
            }
            return buffer;
        }

        /** Read one length-prefixed frame. */
        public byte[] frame() throws IOException {
            return readFully(core.bodyLength(readFully(LENGTH_PREFIX_BYTES)));
        }

        /**
         * One request/response exchange.
         *
         * <p>Sequential by construction: a caller that needs concurrency opens
         * more connections. Multiplexing on one socket would need request-id
         * correlation on the read side, and correlating replies to the wrong
         * request is a data-corruption bug rather than a performance one.
         */
        public ObjectNode exchange(Object request, long requestId, long branchId)
                throws IOException {
            write(core.encode(request, requestId, branchId));
            return (ObjectNode) core.decode(frame());
        }

        @Override
        public void close() throws IOException {
            socket.close();
        }
    }
}
