// Runs the shared conformance suite from Node, against a live thetad.
//
// Imports the *built* SDK rather than the source, because that is what a user
// installs: a suite that passed against TypeScript sources and failed against
// the published package would be testing the wrong artifact.
//
// Prints one JSON document on stdout. The Python runner prints the same
// document from the same cases, and `make conformance` diffs them: two SDKs
// that each pass their own suite prove nothing about whether they agree.

import { readFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ScribeCore, ProtocolError } from "../typescript/dist/scribe.js";
import * as qb from "../typescript/dist/query.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");

const address = process.argv[2];
const token = process.argv[3];
if (!address || !token) {
  console.error("usage: run.mjs <host:port> <token>");
  process.exit(2);
}

const core = await ScribeCore.load(
  readFileSync(path.join(root, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm")),
);
const suite = JSON.parse(readFileSync(path.join(here, "cases.json"), "utf8"));

const [host, port] = address.split(":");
const socket = net.createConnection({ host, port: Number(port) });
await new Promise((resolve, reject) => {
  socket.once("connect", resolve);
  socket.once("error", reject);
});

// Buffered reader: a socket delivers whatever arrived, not what was asked for.
let buffered = Buffer.alloc(0);
let waiter = null;
socket.on("data", (chunk) => {
  buffered = Buffer.concat([buffered, chunk]);
  if (waiter && buffered.length >= waiter.want) waiter.check();
});

function read(n) {
  if (buffered.length >= n) {
    const out = buffered.subarray(0, n);
    buffered = buffered.subarray(n);
    return Promise.resolve(new Uint8Array(out));
  }
  return new Promise((resolve, reject) => {
    const check = () => {
      if (buffered.length < n) return;
      waiter = null;
      const out = buffered.subarray(0, n);
      buffered = buffered.subarray(n);
      resolve(new Uint8Array(out));
    };
    waiter = { want: n, check };
    socket.once("error", reject);
    socket.once("close", () => reject(new Error("the server closed the connection")));
  });
}

function write(bytes) {
  return new Promise((resolve, reject) =>
    socket.write(bytes, (e) => (e ? reject(e) : resolve())),
  );
}

async function exchange(framed) {
  await write(framed);
  const prefix = await read(4);
  return read(core.bodyLength(prefix));
}

// Handshake first: the server refuses anything else until it has one.
await write(core.encodeHello(token, "conformance-node"));
const welcomePrefix = await read(4);
const welcome = core.decodeWelcome(await read(core.bodyLength(welcomePrefix)));

const results = [];
let requestId = 1n;

for (const testCase of suite.cases) {
  let outcome;
  try {
    const framed = core.encode(testCase.request, requestId++, 0n);
    const body = await exchange(framed);
    outcome = { status: "ok", response: core.decode(body) };
  } catch (e) {
    outcome =
      e instanceof ProtocolError
        ? { status: "clientError", message: firstLine(e.message) }
        : { status: "transportError", message: firstLine(String(e.message ?? e)) };
  }
  results.push({ name: testCase.name, ...redact(outcome, testCase.redact ?? []) });
}

socket.end();

// The typed builder renders rather than calls, so these cases need no server.
// Each binding builds with its own fluent API; the rendered output is compared.
const queries = [];
for (const testCase of suite.queries) {
  try {
    queries.push({ name: testCase.name, status: "ok", rendered: buildQuery(testCase.build).render(core) });
  } catch (e) {
    queries.push({
      name: testCase.name,
      status: e instanceof ProtocolError ? "clientError" : "error",
      message: firstLine(String(e.message ?? e)),
    });
  }
}

console.log(
  JSON.stringify({ protocolVersion: welcome.protocolVersion, results, queries }, null, 2),
);

function buildQuery(spec) {
  let q = qb.table(spec.table);
  for (const column of spec.select ?? []) q = q.select(column);
  for (const [op, column, value] of spec.where ?? []) q = q.where(qb[op](column, value));
  for (const [column, descending] of spec.orderBy ?? []) q = q.orderBy(column, descending);
  if (spec.limit !== undefined) q = q.limit(spec.limit);
  if (spec.offset !== undefined) q = q.offset(spec.offset);
  return q;
}

function firstLine(text) {
  return String(text).split("\n")[0];
}

/** Blank out values that legitimately differ between runs. */
function redact(outcome, paths) {
  if (outcome.status !== "ok") return outcome;
  const response = structuredClone(outcome.response);
  // `requestId` differs by position in the run for the two languages only if
  // they disagree about how many requests they sent — which is worth catching,
  // so it is compared rather than redacted.
  for (const dotted of paths) {
    let node = response;
    const parts = dotted.split(".");
    for (const part of parts.slice(0, -1)) {
      node = node?.[part];
      if (node === undefined) break;
    }
    const last = parts[parts.length - 1];
    if (node && last in node) node[last] = "<redacted>";
  }
  return { status: "ok", response };
}
