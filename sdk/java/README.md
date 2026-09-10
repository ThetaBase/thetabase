# ThetaBase Java SDK

```xml
<dependency>
  <groupId>io.thetabase</groupId>
  <artifactId>thetabase</artifactId>
  <version>0.0.1</version>
</dependency>
```

```java
try (Scribe core = Scribe.load(Path.of("theta_scribe_wasm.wasm"));
     Scribe.Connection conn = new Scribe.Connection(core, new Socket(host, port))) {

    conn.write(core.encodeHello(token, "my-app"));
    var welcome = core.decodeWelcome(conn.frame());

    var rendered = Query.table("users")
        .where(Query.eq("email", "alice@example.com"))
        .limit(10)
        .render(core);
}
```

Usable from Kotlin, Scala and Groovy unchanged — the surface is records and
static factories, with nothing Java-specific in the way.

## Three pieces

- **`Generated.java`** — the wire types, emitted from
  `crates/theta-proto/schema/theta.capnp` by `make sdk`. One file rather than
  fifty, because a directory a generator owns is a directory somebody edits by
  hand. Do not edit it; `make sdk-check` fails the build if it and the schema
  disagree.
- **`Query.java`** — the typed query builder. It produces an AST, never SQL
  text.
- **`Scribe.java`** — the transport, sitting on the WebAssembly core every SDK
  shares. Every decision about the protocol lives in the core; this moves bytes.

## Why Chicory

Pure Java, no native library to ship per platform, so the SDK is a jar that
works on any JVM. The Scribe core imports nothing, so none of the WASI machinery
the alternatives offer is needed.

## Why records

A record is immutable, gets equality and `toString` for free, and cannot acquire
a setter. That last one matters more than it looks: a wire type with a setter is
a wire type somebody mutates after validation.

Every component carries `@JsonProperty`. The core reads camelCase and so does
Java, so the annotation is usually redundant — but `record Foo(String default)`
is a syntax error, and a keyword-escaped component would otherwise serialise
under the wrong name. Annotating everything means the escaping rule and the wire
name can never disagree.

## Status

Wire types, query builder and transport are complete and conformant. The
high-level client — `get`, `put`, `query` as methods on a `Theta` — is not here
yet; the same gap TypeScript, Python and Go have, and it lands with the client
work rather than with the bindings. Rust has it because `theta-scribe` is Rust.

## Building

```sh
make wasm             # the Scribe core this SDK loads
cd sdk/java && mvn test
make conformance      # every binding, one live thetad, identical output
```
