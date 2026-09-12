// A `Socket` over Node's TCP stack.
//
// Separate from `session.ts` because it is the one file here that cannot run in
// a browser or a Workers runtime. Another host supplies its own `Socket` and
// reuses everything else — which is the reason the interface exists at all.

import { readFileSync } from "node:fs";
import { connect as tcpConnect } from "node:net";
import { connect as tlsConnect } from "node:tls";

import { ProtocolError, type Socket } from "./scribe.js";

/**
 * Whether this address should be dialled with TLS.
 *
 * `THETA_TLS` wins when it is set, so a self-hosted instance behind a
 * terminating proxy on some other port can say so — and so can a developer
 * tunnelling 443 to a local plaintext process.
 *
 * Otherwise: port 443 means TLS. That is the port Fly's proxy listens on and
 * the port the Control Plane hands out, and it is the only port in this
 * product's vocabulary that implies a terminator in front of `thetad`. Local
 * instances are on 7700 and upwards.
 *
 * Inferred from the address rather than carried beside it, deliberately, and
 * identically to the Rust and Python clients. The address travels through
 * `THETA_ADDRESS` into every SDK; a second variable that had to agree with it
 * would be a second thing to get wrong, and the symptom of getting it wrong is
 * a *hang* rather than an error — a plaintext frame sent to a TLS listener is
 * not rejected, it is read as a ClientHello, found malformed, and the
 * connection dropped.
 */
function wantsTls(port: number, env: Record<string, string | undefined>): boolean {
  const override = env.THETA_TLS?.toLowerCase();
  if (override === "1" || override === "true" || override === "require" || override === "yes") {
    return true;
  }
  if (override === "0" || override === "false" || override === "off" || override === "no") {
    return false;
  }
  return port === 443;
}

/**
 * A deployment's own CA, if it named one, added to the built-in roots.
 *
 * `THETA_TLS_CA` widens what the client will accept; it does not pin. A
 * customer running `thetad` behind their own terminating proxy signs with their
 * own CA, and a client that trusts only the public set cannot reach it — and
 * "turn TLS off instead" is not an answer for a database.
 *
 * There is deliberately no way to disable verification. A flag that turns off
 * certificate checking is a flag that ends up set in production, and the whole
 * reason this transport exists is that a session token was about to cross the
 * open internet.
 *
 * A path that cannot be read is a refusal, not a warning: an operator who set
 * this has said "trust this CA", and continuing without it would silently
 * connect under a laxer trust policy than the one they chose.
 */
function extraRoots(env: Record<string, string | undefined>): Buffer | undefined {
  const path = env.THETA_TLS_CA;
  if (!path) return undefined;
  try {
    return readFileSync(path);
  } catch (cause) {
    throw new ProtocolError(
      `THETA_TLS_CA points at \`${path}\`, which cannot be read: ${String(cause)}`,
    );
  }
}

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

  const env = process.env as Record<string, string | undefined>;

  return new Promise((resolve, reject) => {
    if (!wantsTls(port, env)) {
      const socket = tcpConnect({ host, port });
      socket.setNoDelay(true);
      socket.once("error", reject);
      socket.once("connect", () => {
        socket.removeListener("error", reject);
        resolve(new NodeSocket(socket));
      });
      return;
    }

    let ca: Buffer | undefined;
    try {
      ca = extraRoots(env);
    } catch (error) {
      reject(error);
      return;
    }

    // `servername` is the SNI name, and on a shared address it is what the
    // proxy routes on — so getting it wrong does not produce a certificate
    // error, it produces a connection to the wrong instance or to none.
    const socket = tlsConnect({
      host,
      port,
      servername: host,
      // Named rather than left to the default, so that a later edit cannot
      // turn it off by accident.
      rejectUnauthorized: true,
      ...(ca ? { ca: [ca] } : {}),
    });
    socket.setNoDelay(true);
    socket.once("error", reject);
    // `secureConnect`, not `connect`: `connect` fires when the TCP socket is
    // up and the handshake has not happened, so writing a frame there would
    // race the encryption.
    socket.once("secureConnect", () => {
      socket.removeListener("error", reject);
      if (!socket.authorized) {
        const reason = socket.authorizationError ?? "the certificate was not accepted";
        socket.destroy();
        reject(
          new ProtocolError(
            `the TLS certificate for \`${host}\` was refused: ${String(reason)}. ` +
              "A provisioned instance presents a publicly signed certificate; a " +
              "self-hosted one needs its CA in THETA_TLS_CA.",
          ),
        );
        return;
      }
      resolve(new NodeSocket(socket));
    });
  });
}

/**
 * Either socket.
 *
 * `tls.TLSSocket` extends `net.Socket`, so everything below this line —
 * buffering, framing, the close handler — is unchanged by adding TLS. That is
 * the reason the transport choice lives in `connect` and nowhere else.
 */
type NetSocket = ReturnType<typeof tcpConnect> | ReturnType<typeof tlsConnect>;

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
