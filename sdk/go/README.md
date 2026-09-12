# ThetaBase Go SDK

```go
import thetabase "github.com/ThetaBase/thetabase/sdk/go"
```

Three pieces, the same three every ThetaBase SDK has:

- **`generated.go`** — the wire types, emitted from
  `crates/theta-proto/schema/theta.capnp` by `make sdk`. Do not edit it; `make
  sdk-check` fails the build if it and the schema disagree.
- **`query.go`** — the typed query builder. It produces an AST, never SQL text.
- **`scribe.go`** — the transport, sitting on the WebAssembly core that every
  SDK shares. Every decision about the protocol lives in the core; this file
  moves bytes.

## Why wazero

Pure Go, no cgo, so `go build` works on anything Go targets and installing this
SDK does not drag in a C toolchain. The Scribe core imports nothing, so none of
the host-function machinery the alternatives offer is needed.

## Why the core rather than a Go implementation of the protocol

A Go SDK that built wire messages in Go would be a third implementation of the
protocol, and the first time the schema moved two of the three would be wrong.
What must be identical across languages is the protocol; what must differ is
I/O, because WebAssembly has no sockets and the host runtimes do not agree on
what a socket is.

That claim is checked rather than asserted. `make conformance` runs the same
suite through every binding against a live `thetad` and requires them to produce
byte-identical output — three SDKs that each pass their own suite prove nothing
about whether they agree. It found a real bug on this SDK's first run: `clone`
used `append([]string(nil), ...)`, which yields nil for an empty source, and a
nil slice marshals to `null` where the core reads a sequence. Every query
without an explicit projection failed to render.

## Status

Wire types, query builder and transport are complete and conformant. The
`Theta` client surface — `Get`, `Put`, `Query`, `schema.Propose` and the rest —
is not here yet; it is the same gap the TypeScript and Python SDKs have, and it
lands with the client work rather than with the bindings.

## Building

```sh
make wasm          # the Scribe core this SDK loads
make sdk           # regenerate the bindings and build
cd sdk/go && go test ./...
```

`LoadCore(ctx, nil)` looks for `theta_scribe_wasm.wasm` next to the SDK and then
in the repository's build output. Pass the bytes explicitly in anything shipped.
