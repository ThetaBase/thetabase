# frozen_string_literal: true

Gem::Specification.new do |spec|
  spec.name = "thetabase"
  spec.version = "0.0.1"
  spec.summary = "The ThetaBase Ruby SDK: a typed query builder and a transport, over the Scribe core."
  spec.authors = ["ThetaBase"]
  # No licence chosen yet; see docs/DISTRIBUTION.md.
  spec.license = "Nonstandard"
  # RubyGems refuses a push to any host but this one, and this one does not
  # exist. A publish is a public disclosure.
  spec.metadata["allowed_push_host"] = "https://none.invalid"
  spec.homepage = "https://github.com/FelixKramer/ThetaBase"
  # Data.define, which is what makes a wire type immutable without hand-writing
  # a reader per field.
  spec.required_ruby_version = ">= 3.2.0"

  spec.files = Dir["lib/**/*.rb", "README.md"]
  spec.require_paths = ["lib"]

  # Wasmtime's Ruby bindings. Like .NET and unlike Go and Java, there is no
  # pure-Ruby WebAssembly runtime; the gem ships precompiled native extensions
  # for the common platforms, so a consumer still runs one `gem install`.
  spec.add_dependency "wasmtime", "~> 47.0"
end
