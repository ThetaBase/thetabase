// The host half of Scribe: sockets, and nothing else.
//
// Every decision about the protocol — how a request is encoded, what a response
// means, when a cached read must be dropped — lives in the WebAssembly core
// (`crates/theta-scribe-wasm`). This file moves bytes.
//
// The split is deliberate. A TypeScript SDK that built wire messages in
// TypeScript would be a second implementation of the protocol and the Python
// one a third, and the first time the schema moved two of the three would be
// wrong. What must be identical across languages is the protocol; what must
// differ is I/O, because WebAssembly has no sockets and the host runtimes do
// not agree on what a socket is.

/** A framed connection to `thetad`. */
export interface Socket {
  write(bytes: Uint8Array): Promise<void>;
  /** Resolves with exactly `n` bytes, or rejects if the peer closes first. */
  read(n: number): Promise<Uint8Array>;
  close(): void;
}

interface Exports {
  memory: WebAssembly.Memory;
  theta_alloc(len: number): number;
  theta_free(ptr: number, len: number): void;
  theta_free_result(ptr: number): void;
  theta_encode(ptr: number, len: number, requestId: bigint, branchId: bigint): number;
  theta_decode(ptr: number, len: number): number;
  theta_body_length(ptr: number): number;
  theta_invalidation(ptr: number, len: number): number;
  theta_encode_hello(ptr: number, len: number): number;
  theta_decode_welcome(ptr: number, len: number): number;
  theta_render_query(ptr: number, len: number): number;
}

/** `[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out. */
const RESULT_HEADER = 5;
const LENGTH_PREFIX_BYTES = 4;

/** A protocol error the core rejected, kept distinct from a transport failure. */
export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

/** The core, loaded once per process. */
export class ScribeCore {
  private constructor(private readonly exports: Exports) {}

  static async load(wasm?: BufferSource): Promise<ScribeCore> {
    const bytes = wasm ?? (await defaultModule());
    // No imports: the core is pure computation, which is why it loads
    // identically in Node, a browser, Deno and an edge runtime.
    const { instance } = await WebAssembly.instantiate(bytes, {});
    return new ScribeCore(instance.exports as unknown as Exports);
  }

  private bytes(): Uint8Array {
    // Re-read every time: a call that grows linear memory detaches any
    // previously held view, and using a stale one reads freed memory.
    return new Uint8Array(this.exports.memory.buffer);
  }

  private send(input: Uint8Array, call: (ptr: number, len: number) => number): Uint8Array {
    const ptr = this.exports.theta_alloc(input.length);
    try {
      this.bytes().set(input, ptr);
      return this.take(call(ptr, input.length));
    } finally {
      this.exports.theta_free(ptr, input.length);
    }
  }

  /** Read a result buffer out and free it. */
  private take(ptr: number): Uint8Array {
    const view = new DataView(this.exports.memory.buffer);
    const len = view.getUint32(ptr, true);
    const tag = view.getUint8(ptr + 4);
    const out = this.bytes().slice(ptr + RESULT_HEADER, ptr + RESULT_HEADER + len);
    this.exports.theta_free_result(ptr);

    if (tag !== 0) {
      throw new ProtocolError(new TextDecoder().decode(out));
    }
    return out;
  }

  encode(request: unknown, requestId: bigint, branchId: bigint): Uint8Array {
    const json = new TextEncoder().encode(JSON.stringify(request));
    return this.send(json, (ptr, len) =>
      this.exports.theta_encode(ptr, len, requestId, branchId),
    );
  }

  decode(body: Uint8Array): Record<string, unknown> {
    const out = this.send(body, (ptr, len) => this.exports.theta_decode(ptr, len));
    return JSON.parse(new TextDecoder().decode(out));
  }

  bodyLength(prefix: Uint8Array): number {
    const ptr = this.exports.theta_alloc(LENGTH_PREFIX_BYTES);
    try {
      this.bytes().set(prefix, ptr);
      const len = this.exports.theta_body_length(ptr);
      if (len === 0xffffffff) {
        throw new ProtocolError("the peer sent a length this protocol does not allow");
      }
      return len;
    } finally {
      this.exports.theta_free(ptr, LENGTH_PREFIX_BYTES);
    }
  }

  /**
   * Encode the handshake.
   *
   * The protocol version is the core's to state, not the host's: a host that
   * could name its own version could claim a compatibility it does not have,
   * and the negotiation that refuses mismatched versions rather than guessing
   * would be negotiating with itself.
   */
  encodeHello(token: string, clientName: string): Uint8Array {
    const json = new TextEncoder().encode(JSON.stringify({ token, clientName }));
    return this.send(json, (ptr, len) => this.exports.theta_encode_hello(ptr, len));
  }

  /** Decode the server's reply to a handshake. A refusal throws. */
  decodeWelcome(body: Uint8Array): { protocolVersion: number; projectId: string; serverName: string } {
    const out = this.send(body, (ptr, len) => this.exports.theta_decode_welcome(ptr, len));
    return JSON.parse(new TextDecoder().decode(out));
  }

  /**
   * Render a typed query AST to SQL-subset source and bound parameters.
   *
   * In the core rather than in this file, so a query built the same way in
   * Python renders to the same bytes — and so the rule that a value never
   * becomes query text has one implementation.
   */
  renderQuery(ast: unknown): { sql: string; params: Record<string, string> } {
    const json = new TextEncoder().encode(JSON.stringify(ast));
    const out = this.send(json, (ptr, len) => this.exports.theta_render_query(ptr, len));
    return JSON.parse(new TextDecoder().decode(out));
  }

  /** What this request invalidates: one key, everything, or nothing. */
  invalidation(request: unknown): { key?: string; all?: boolean } {
    const json = new TextEncoder().encode(JSON.stringify(request));
    const out = this.send(json, (ptr, len) => this.exports.theta_invalidation(ptr, len));
    return JSON.parse(new TextDecoder().decode(out));
  }
}

async function defaultModule(): Promise<BufferSource> {
  // Node only, and reached only when the caller did not pass a module. A
  // browser or edge build passes one in, because there is no filesystem to read
  // it from and the bundler decides how it arrives — so these imports must not
  // be at the top of the file, where they would break those builds outright.
  const { readFileSync } = await import("node:fs");
  const { fileURLToPath } = await import("node:url");
  const path = await import("node:path");

  const here = path.dirname(fileURLToPath(import.meta.url));
  for (const candidate of [
    path.join(here, "theta_scribe_wasm.wasm"),
    path.join(here, "..", "theta_scribe_wasm.wasm"),
  ]) {
    try {
      return readFileSync(candidate);
    } catch {
      // Try the next location before giving up.
    }
  }
  throw new Error(
    "the Scribe core was not found next to the SDK — run `make wasm` to build it, " +
      "or pass the module to ScribeCore.load()",
  );
}

/**
 * One request/response exchange over a socket.
 *
 * Sequential by construction: a caller that needs concurrency opens more
 * connections, which is what the pool is for. Multiplexing on one socket would
 * need request-id correlation on the read side, and correlating replies to the
 * wrong request is a data-corruption bug rather than a performance one.
 */
export async function exchange(
  core: ScribeCore,
  socket: Socket,
  request: unknown,
  requestId: bigint,
  branchId: bigint,
): Promise<Record<string, unknown>> {
  await socket.write(core.encode(request, requestId, branchId));

  const prefix = await socket.read(LENGTH_PREFIX_BYTES);
  const body = await socket.read(core.bodyLength(prefix));
  return core.decode(body);
}
