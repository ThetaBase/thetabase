// A connection to `thetad`: handshake, request ids, and one response at a time.
//
// Everything about *what* a message means still lives in the WebAssembly core.
// This file decides when to send one and how to match the answer to the
// question, which is the part that genuinely differs between a host runtime
// with sockets and one without.

import { ProtocolError, ScribeCore, type Socket } from "./scribe.js";

/** Where to connect and what to present, as `theta exec` injects them. */
export interface Connection {
  address: string;
  token: string;
}

/**
 * Read the connection out of the environment.
 *
 * There is deliberately no way to pass an address and a token as options. A
 * project is named and the toolchain resolves a scoped token for it — that is
 * the first of the three load-bearing properties in `specs/02` §3, and an SDK
 * that accepted a connection string would quietly undo it.
 */
export function connectionFromEnv(env: Record<string, string | undefined>): Connection {
  const address = env.THETA_ADDRESS;
  const token = env.THETA_TOKEN;

  if (!address || !token) {
    throw new ProtocolError(
      "THETA_ADDRESS and THETA_TOKEN are not both set. Run this process under " +
        "`theta exec`, which resolves a scoped token for the project and injects " +
        "both. They are not options on the client on purpose: a connection " +
        "string in application code is the thing the identity graph exists to " +
        "remove.",
    );
  }
  return { address, token };
}

/**
 * One connection, and the request ids that travel over it.
 *
 * # Why requests are serialised
 *
 * The wire allows several calls in flight — a response carries the id of the
 * request it answers precisely so they may arrive out of order. This class does
 * not use that yet: it holds one in-flight call at a time and queues the rest.
 *
 * That is a deliberate first step rather than an oversight. Pipelining needs a
 * map from request id to a pending promise and a read loop that owns the socket
 * for the life of the session, and getting that wrong produces a client that
 * resolves the wrong promise with the right-looking data — the kind of bug that
 * survives a test suite and surfaces as one customer's row appearing in another
 * customer's response. Serial is slower and it is correct, and the id is on the
 * wire either way, so pipelining stays available without a protocol change.
 */
export class Session {
  private nextRequestId = 1n;
  private branchId = 0n;
  private pending: Promise<unknown> = Promise.resolve();

  private constructor(
    private readonly core: ScribeCore,
    private readonly socket: Socket,
    readonly projectId: string,
    readonly serverName: string,
  ) {}

  /**
   * Handshake, then return a session ready to carry requests.
   *
   * The token is presented once here, and again on every request — `thetad`
   * re-authorises per request rather than per connection, so that a revoked
   * token stops working within one heartbeat rather than at the end of a
   * long-lived connection.
   */
  static async open(core: ScribeCore, socket: Socket, connection: Connection): Promise<Session> {
    const hello = core.encodeHello(connection.token, "thetabase-typescript");
    await socket.write(hello);

    const body = await readFrame(core, socket);
    const welcome = core.decodeWelcome(body);

    return new Session(core, socket, welcome.projectId, welcome.serverName);
  }

  /** Operate on a named branch for subsequent calls. */
  useBranch(branchId: bigint): void {
    this.branchId = branchId;
  }

  /**
   * Send one request and resolve with the decoded response.
   *
   * Queued behind any call already in flight, so two concurrent callers on one
   * session cannot interleave their frames on the socket.
   */
  async call(request: unknown): Promise<Record<string, unknown>> {
    const run = this.pending.then(
      () => this.exchange(request),
      // A previous call failing must not poison the queue — the next caller
      // gets its own attempt, and its own error if the socket is really gone.
      () => this.exchange(request),
    );
    this.pending = run.catch(() => undefined);
    return run;
  }

  private async exchange(request: unknown): Promise<Record<string, unknown>> {
    const requestId = this.nextRequestId++;
    const framed = this.core.encode(request, requestId, this.branchId);
    await this.socket.write(framed);

    const body = await readFrame(this.core, this.socket);
    const response = this.core.decode(body);

    // The id is checked rather than trusted. On a serial session a mismatch
    // means the stream has desynchronised, and continuing would hand this
    // caller an answer to somebody else's question.
    const answered = response.requestId;
    if (answered !== undefined && BigInt(answered as string | number | bigint) !== requestId) {
      this.socket.close();
      throw new ProtocolError(
        `the server answered request ${String(answered)} while ${requestId} was ` +
          "outstanding; the connection has desynchronised and has been closed",
      );
    }
    return response;
  }

  close(): void {
    this.socket.close();
  }
}

/** Read one `[u32 length][body]` frame. */
async function readFrame(core: ScribeCore, socket: Socket): Promise<Uint8Array> {
  const prefix = await socket.read(4);
  // The core validates the length against the frame limit, so an attacker
  // cannot make the host allocate by sending four bytes.
  const length = core.bodyLength(prefix);
  return socket.read(length);
}
