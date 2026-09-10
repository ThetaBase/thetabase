# ThetaBase Rust SDK

```toml
[dependencies]
thetabase = { path = "sdk/rust" }
```

```rust
use thetabase::{query, Theta, ThetaConfig};

let theta = Theta::connect(ThetaConfig::new("127.0.0.1:7777"), token);

theta.put("users:1", &theta_core::Value::from_json(&serde_json::json!({
    "email": "alice@example.com",
}))).await?;

let rows = theta.query(
    &query::table("users")
        .filter(query::eq("email", "alice@example.com"))
        .limit(10),
).await?;
```

## Two of the three pieces already existed

Every ThetaBase SDK is generated wire types, a typed query builder, and a
transport binding onto the Scribe core. Rust is the one language where two of
those were already in this repository, and writing second copies would have been
the exact drift the other bindings need a gate to catch.

- **Wire types** — `thetabase::wire` re-exports `theta_proto::wire`, generated
  from `theta.capnp` by `theta-proto`'s build script. There is no
  `generated.rs` here and there must not be one.
- **Transport** — `Theta` wraps `theta_scribe::Scribe`: pooling, the read
  cache, batching, token refresh.
- **The builder** is the new part.

## No WebAssembly runtime

The other SDKs load the Scribe core as a WASM module, because that is the only
way to run one protocol implementation inside Node, CPython and Go. Rust links
it — `theta-scribe-wasm` has always declared `crate-type = ["cdylib", "rlib"]`,
and the crate is named for how it is usually *compiled*, not for what it holds.

Be precise about what that costs. This SDK does not exercise `abi.rs`, the
byte-buffer boundary, which the other three exercise three times over. It is
held to the same standard everywhere else: `make conformance` runs the same
cases through every binding and requires byte-identical output.

## The client surface is real here first

`get`, `put`, `delete`, `query`, `explain`, `propose`, `apply`, branches and
`status` all work. The TypeScript and Python SDKs still stub theirs — not
because Rust got special treatment, but because `theta-scribe` is Rust and the
other bindings reach the protocol through a core that deliberately contains no
sockets.

There is no method that takes a SQL string. An SDK that offers one invites the
injection the typed path exists to prevent; a caller who genuinely needs to send
text can reach `Theta::scribe()` and be explicit about it.

## `filter`, not `where`

`where` is a Rust keyword. Naming the method `filter` beats `r#where` at every
call site, and it is what a Rust caller reaches for anyway.

## Running the gates

```sh
cargo test -p thetabase
make conformance      # every binding, one live thetad, identical output
```
