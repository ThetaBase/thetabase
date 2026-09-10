# ThetaBase Ruby SDK

```ruby
require "thetabase"

core = ThetaBase::Scribe.load("theta_scribe_wasm.wasm")
conn = ThetaBase::Connection.new(core, TCPSocket.new(host, port))

conn.write(core.encode_hello(token, "my-app"))
welcome = core.decode_welcome(conn.frame)

rendered = ThetaBase::Query.table("users")
                         .where(ThetaBase::Query.eq("email", "alice@example.com"))
                         .limit(10)
                         .render(core)
```

## Three pieces

- **`lib/thetabase/generated.rb`** — the wire types, emitted from
  `crates/theta-proto/schema/theta.capnp` by `make sdk`. Do not edit it; `make
  sdk-check` fails the build if it and the schema disagree.
- **`lib/thetabase/query.rb`** — the typed query builder. It produces an AST,
  never SQL text.
- **`lib/thetabase/scribe.rb`** — the transport, sitting on the WebAssembly core
  every SDK shares.

## `Data.define`, and one Ruby scoping trap

Wire types are `Data`, not `Struct`: `Data` has no setters at all, and a wire
type with a setter is a wire type somebody mutates after validation.

Each type carries its wire-name mapping in its own `wire` method rather than a
`WIRE` constant, and that is not a style choice. **A constant assigned inside a
`Data.define ... do` block binds to the enclosing lexical scope, not to the
class.** Every generated type wrote to one `ThetaBase::Wire::WIRE` and the last one
won, so every `to_wire` in the file used the last type's field list — which
surfaced as a `NoMethodError` naming a field from an unrelated message. A unit
test caught it before the conformance gate did.

## Names are mapped, not derived

snake_case here, camelCase on the wire. The mapping is carried rather than
computed, because deriving it would mean reversing `snake_case` and
`writeVolumeMB` does not survive that.

Accessors that would shadow an `Object` method are escaped. `class` is the sharp
one: a `Data` member named `class` gives every instance an accessor shadowing
`Object#class`, and the failure surfaces somewhere else entirely.

## Status

Wire types, query builder and transport are complete and conformant. The
high-level client — `get`, `put`, `query` on a `Theta` — is not here yet; the
same gap every binding but Rust has.

## Building

```sh
make wasm
ruby sdk/ruby/test/query_test.rb
make conformance
```
