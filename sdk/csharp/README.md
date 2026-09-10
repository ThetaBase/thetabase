# ThetaBase C# SDK

```csharp
using var core = Scribe.Load("theta_scribe_wasm.wasm");
using var conn = new Scribe.Connection(core, socket);

conn.Write(core.EncodeHello(token, "my-app"));
var welcome = core.DecodeWelcome(conn.Frame());

var rendered = Query.Table("users")
    .Where(Query.Eq("email", "alice@example.com"))
    .Limit(10)
    .Render(core);
```

Three projects: `src` is the SDK, `conformance` is the harness runner, `tests`
is xunit. A library with a `Main` in it is a library that ships a test harness.

## Three pieces

- **`src/Generated.cs`** — the wire types, emitted from
  `crates/theta-proto/schema/theta.capnp` by `make sdk`. Do not edit it; `make
  sdk-check` fails the build if it and the schema disagree.
- **`src/Query.cs`** — the typed query builder. It produces an AST, never SQL
  text.
- **`src/Scribe.cs`** — the transport, sitting on the WebAssembly core every SDK
  shares.

## The one place a ThetaBase SDK is not pure managed code

Go has wazero and Java has Chicory — both pure, both shipping as one artifact.
.NET has no mature pure-managed WebAssembly runtime, so this uses Wasmtime's
bindings, which carry a native library per platform. The NuGet package ships
them, so a consumer still installs one package; it is named here because it is
the one asymmetry across the six bindings.

## Why `[JsonPropertyName]` on every property

C# properties are PascalCase and the wire is camelCase, so unlike Java the two
genuinely differ everywhere rather than only on escaped names.
`System.Text.Json` has a camelCase naming policy that would cover most of them,
and *most* is the problem: a policy silently produces a different wrong name for
`writeVolumeMB`, where an explicit attribute is either right or absent.

Enums carry `[JsonStringEnumMemberName]` for a sharper reason. Without it an
enum serialises as an integer, which the server does not read — and that failure
looks like a schema mismatch rather than a naming one.

## Runtime

Targets `net9.0`, for `JsonStringEnumMemberName`. Multi-targeting `net8.0` needs
a hand-written converter reading `[EnumMember]`; worth doing before this is
published, and not worth doing before it works.

## Status

Wire types, query builder and transport are complete and conformant. The
high-level client — `Get`, `Put`, `Query` as methods on a `Theta` — is not here
yet; the same gap every binding but Rust has.

## Building

```sh
make wasm
cd sdk/csharp && dotnet test tests
make conformance
```
