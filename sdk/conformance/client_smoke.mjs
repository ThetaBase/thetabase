// The TypeScript client, against a live `thetad`.
//
// Run by `client_smoke.py`, which starts the server and passes its address and
// token in the environment — the same way `theta exec` does, because the client
// refuses to take either as a parameter.
//
// Imports `dist/` rather than `src/` for the reason the conformance runner
// does: a suite that passed against sources and failed against the published
// artifact would be testing the wrong thing.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "../..");

// `pathToFileURL`, not the bare path. A dynamic import takes a URL, and on
// Windows an absolute path begins with a drive letter that Node reads as a URL
// scheme — `d:` — and refuses. On POSIX the same string happens to work, which
// is why this is the kind of thing that passes everywhere it was written and
// fails on the machine it has to run on.
const load = (relative) => import(pathToFileURL(join(ROOT, relative)).href);

const { Theta, ScribeCore } = await load("sdk/typescript/dist/index.js");
const { connect } = await load("sdk/typescript/dist/node-socket.js");

const failures = [];

function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${name}`);
  if (!ok) failures.push(`${name}: got ${JSON.stringify(got)}, wanted ${JSON.stringify(want)}`);
}

const wasm = readFileSync(
  join(ROOT, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"),
);
const core = await ScribeCore.load(wasm);
const socket = await connect(process.env.THETA_ADDRESS);
const theta = await Theta.open({ project: "conformance" }, socket, core);

try {
  check("get of an absent row is undefined", await theta.get("ts/absent"), undefined);

  await theta.put("ts/a", 1);
  check("put then get round-trips", await theta.get("ts/a"), 1);

  const created = await theta.putIf("ts/b", 2, { kind: "absent" });
  check("a create-only write lands when the row is absent", created !== null, true);

  const again = await theta.putIf("ts/b", 3, { kind: "absent" });
  check("the same write is refused once the row exists", again, null);
  check("the refused write changed nothing", await theta.get("ts/b"), 2);

  const committed = await theta.transaction([
    { key: "ts/tx1", action: "put", value: 10 },
    { key: "ts/tx2", action: "put", value: 20 },
  ]);
  check("a transaction commits", committed !== null, true);
  check("both of its writes are visible", await theta.get("ts/tx2"), 20);

  // The precondition fails on the *second* operation; the first must not
  // survive it.
  const refused = await theta.transaction([
    { key: "ts/tx3", action: "put", value: 30 },
    { key: "ts/b", action: "put", value: 99, expect: { kind: "absent" } },
  ]);
  check("a transaction with a failing precondition is refused", refused, null);
  check("its other write did not land", await theta.get("ts/tx3"), undefined);

  await theta.delete("ts/a");
  check("delete removes the row", await theta.get("ts/a"), undefined);
} finally {
  theta.close();
}

if (failures.length) {
  console.error(failures.join("\n"));
  process.exit(1);
}
