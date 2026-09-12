// A `Socket` over Node's TCP stack.
//
// Separate from `session.ts` because it is the one file here that cannot run in
// a browser or a Workers runtime. Another host supplies its own `Socket` and
// reuses everything else — which is the reason the interface exists at all.

import { connect as tcpConnect } from "node:net";

import { ProtocolError, type Socket } from "./scribe.js";

/**
 * Connect to `thetad` at `host:port`.
 *
 * `address` is the `THETA_ADDRESS` the toolchain injects, in `host:port` form.
 */
export function connect(address: string): Promise<Socket> {
  const separator = address.lastIndexOf(":");
  if (separator <= 0) {
    // `lastIndexOf` rather than `indexOf`, and rejected rather than guessed:
    // an IPv6 address is full of colons and splitting on the first one yields a
    // host of "[2a09" and a port of nonsense.
    throw new ProtocolError(
      `\`${address}\` is not \`host:port\`. THETA_ADDRESS is set by \`theta exec\`; ` +
        "a hand-written value is usually a URL by mistake.",
    );
  }
  const host = address.slice(0, separator);
  const port = Number(address.slice(separator + 1));
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new ProtocolError(`\`${address}\` does not end in a port number`);
  }

  return new Promise((resolve, reject) => {
    const socket = tcpConnect({ host, port });
    socket.once("error", reject);
    socket.once("connect", () => {
      socket.removeListener("error", reject);
      resolve(new NodeSocket(socket));
    });
  });
}

type NetSocket = ReturnType<typeof tcpConnect>;

class NodeSocket implements Socket {
  /**
   * Bytes that have arrived and not yet been asked for.
   *
   * A framed protocol asks for an exact number of bytes and TCP delivers
   * whatever it likes, so the two have to be reconciled somewhere. Doing it
   * here — rather than by reading and hoping — is what makes
   * `read(n)` mean what its signature says.
   */
  private buffered: Uint8Array[] = [];
  private bufferedBytes = 0;
  private waiter: { want: number; resolve: (b: Uint8Array) => void; reject: (e: Error) => void } | null =
    null;
  private failure: Error | null = null;

  constructor(private readonly socket: NetSocket) {
    socket.on("data", (chunk: Buffer) => {
      this.buffered.push(new Uint8Array(chunk));
      this.bufferedBytes += chunk.length;
      this.serve();
    });
    socket.on("error", (error: Error) => this.fail(error));
    socket.on("close", () =>
      this.fail(
        new ProtocolError(
          "the connection closed while a response was outstanding. `thetad` " +
            "closes a connection it cannot authorise, so the usual cause is an " +
            "expired or revoked token rather than a network fault.",
        ),
      ),
    );
  }

  write(bytes: Uint8Array): Promise<void> {
    return new Promise((resolve, reject) => {
      this.socket.write(bytes, (error) => (error ? reject(error) : resolve()));
    });
  }

  read(n: number): Promise<Uint8Array> {
    if (this.failure) return Promise.reject(this.failure);
    if (this.waiter) {
      // One read at a time. `Session` serialises calls, so two concurrent reads
      // mean a bug somewhere above rather than something to queue for.
      return Promise.reject(new ProtocolError("a read is already outstanding on this socket"));
    }
    return new Promise((resolve, reject) => {
      this.waiter = { want: n, resolve, reject };
      this.serve();
    });
  }

  close(): void {
    this.socket.destroy();
  }

  /** Hand over the awaited bytes, if enough have arrived. */
  private serve(): void {
    const waiter = this.waiter;
    if (!waiter || this.bufferedBytes < waiter.want) return;

    const out = new Uint8Array(waiter.want);
    let filled = 0;
    while (filled < waiter.want) {
      const chunk = this.buffered[0]!;
      const take = Math.min(chunk.length, waiter.want - filled);
      out.set(chunk.subarray(0, take), filled);
      filled += take;
      if (take === chunk.length) {
        this.buffered.shift();
      } else {
        // Partially consumed: keep the remainder, or the next frame loses its
        // first bytes.
        this.buffered[0] = chunk.subarray(take);
      }
    }
    this.bufferedBytes -= waiter.want;
    this.waiter = null;
    waiter.resolve(out);
  }

  private fail(error: Error): void {
    // Recorded as well as delivered: a read that arrives after the socket died
    // must fail immediately rather than hang forever waiting for bytes that
    // cannot come.
    this.failure ??= error;
    const waiter = this.waiter;
    this.waiter = null;
    waiter?.reject(this.failure);
  }
}
