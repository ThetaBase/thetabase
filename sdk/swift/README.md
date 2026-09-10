# ThetaBase Swift SDK

```swift
import ThetaBase

let core = try Scribe(path: URL(fileURLWithPath: "theta_scribe_wasm.wasm"))
let socket: FramedSocket = MySocket(host: host, port: port)

try socket.write(core.encodeHello(token: token, clientName: "my-app"))
let welcome = try core.decodeWelcome(core.frame(from: socket))

let rendered = try Query.table("users")
    .filter(.eq("email", "alice@example.com"))
    .limit(10)
    .render(core)
```

## Three pieces

- **`Sources/ThetaBase/Generated.swift`** — the wire types, emitted from
  `crates/theta-proto/schema/theta.capnp` by `make sdk`. Do not edit it; `make
  sdk-check` fails the build if it and the schema disagree.
- **`Sources/ThetaBase/Query.swift`** — the typed query builder. It produces an
  AST, never SQL text.
- **`Sources/ThetaBase/Scribe.swift`** — the transport, sitting on the WebAssembly
  core every SDK shares.

## Why WasmKit

Pure Swift, no native library to ship per platform, so the package builds
anywhere Swift does — including iOS, where loading a C runtime would not be an
option at all. Same reasoning as Go's wazero and Java's Chicory.

## `FramedSocket` is a protocol, not a socket

Swift runs where `Foundation`'s socket story differs: a server uses NIO, an iOS
app uses `Network.framework`. An SDK that picked one would force it on the
other, so this one asks for `write`, `readFully` and `close` and stays out of the
way. The conformance runner has a small blocking implementation if you want a
starting point.

## Two names worth knowing about

**`Predicate` collides with `Foundation.Predicate`.** Qualify it as
`ThetaBase.Predicate` where you need the type by name — at most call sites you do
not, because `.eq(...)` infers. The name is kept because the AST field is
`filter` and the node is a predicate in all eight bindings; diverging here would
buy one less qualification and cost a concept that reads the same everywhere.

**`filter`, not `where`.** `where` is a Swift keyword.

## Building on Windows

`swift build` needs the MSVC toolchain and the Swift standard library on PATH:

```
"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
```

`vcvars64.bat` *replaces* PATH, so the Swift toolchain and runtime directories go
back on afterwards, and a per-user Swift install needs `SDKROOT` pointing at
`Platforms\<version>\Windows.platform\Developer\SDKs\Windows.sdk` — without it
`swiftc` reports "unable to load standard library". macOS and Linux need none of
this.

## Status

Wire types, query builder and transport are complete and conformant. The
high-level client — `get`, `put`, `query` on a `Theta` — is not here yet; the
same gap every binding but Rust has.

## Building

```sh
make wasm
cd sdk/swift && swift test
make conformance
```
