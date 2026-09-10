# frozen_string_literal: true

# Runs the shared conformance suite from Ruby, against a live thetad.
#
# Prints one JSON document on stdout. Every other runner prints the same document
# from the same cases, and `make conformance` diffs them: bindings that each pass
# their own suite prove nothing about whether they agree.

require "json"
require "pathname"
require "socket"

$LOAD_PATH.unshift(File.expand_path("lib", __dir__))
require "thetabase"

def repo_root
  dir = Pathname.new(Dir.pwd)
  dir = dir.parent until dir.join("sdk/conformance/cases.json").file? || dir.root?
  raise "no ThetaBase workspace above the working directory" if dir.root?

  dir
end

def first_line(text)
  text.to_s.split("\n").first.to_s
end

# Blank out values that legitimately differ between runs.
def redact(response, paths)
  (paths || []).each do |dotted|
    parts = dotted.split(".")
    node = response
    parts[0..-2].each { |part| node = node.is_a?(Hash) ? node[part] : nil }
    node[parts.last] = "<redacted>" if node.is_a?(Hash) && node.key?(parts.last)
  end
  response
end

# Map a case's operator name onto the builder.
def predicate(op, column, value)
  case op
  when "eq", "ne", "lt", "lte", "gt", "gte" then ThetaBase::Query.public_send(op, column, value)
  when "isNull" then ThetaBase::Query.is_null(column)
  when "notNull" then ThetaBase::Query.not_null(column)
  else raise ArgumentError, "the suite names an operator this SDK does not have: #{op.inspect}"
  end
end

def build(spec)
  query = ThetaBase::Query.table(spec["table"])
  (spec["select"] || []).each { |column| query = query.select(column) }
  (spec["where"] || []).each { |op, column, value| query = query.where(predicate(op, column, value)) }
  (spec["orderBy"] || []).each { |column, descending| query = query.order_by(column, descending) }
  query = query.limit(spec["limit"]) if spec.key?("limit")
  query = query.offset(spec["offset"]) if spec.key?("offset")
  query
end

# Print the way `JSON.stringify(value, null, 2)` does.
#
# Ruby's `JSON.pretty_generate` writes `{}` for an empty object and two-space
# indent, which matches — but it also writes `[\n\n]` for an empty array where
# JavaScript writes `[]`. Twenty lines here is cheaper than discovering that in a
# diff.
def pretty(node, indent = "")
  inner = "#{indent}  "
  case node
  when Hash
    return "{}" if node.empty?

    body = node.map { |k, v| "#{inner}#{JSON.generate(k)}: #{pretty(v, inner)}" }
    "{\n#{body.join(",\n")}\n#{indent}}"
  when Array
    return "[]" if node.empty?

    "[\n#{node.map { |v| inner + pretty(v, inner) }.join(",\n")}\n#{indent}]"
  else
    JSON.generate(node)
  end
end

def main(argv)
  if argv.length < 2
    warn "usage: conformance.rb <host:port> <token>"
    return 2
  end

  root = repo_root
  suite = JSON.parse(File.read(root.join("sdk/conformance/cases.json")))
  core = ThetaBase::Scribe.load(root.join("target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))

  host, port = argv[0].split(":")
  connection = ThetaBase::Connection.new(core, TCPSocket.new(host, port.to_i))

  # Handshake first: the server refuses anything else until it has one.
  connection.write(core.encode_hello(argv[1], "conformance-ruby"))
  welcome = core.decode_welcome(connection.frame)

  results = suite["cases"].each_with_index.map do |test_case, index|
    outcome = { "name" => test_case["name"] }
    begin
      response = connection.exchange(test_case["request"], index + 1, 0)
      outcome["status"] = "ok"
      outcome["response"] = redact(response, test_case["redact"])
    rescue ThetaBase::ProtocolError => e
      outcome.merge!("status" => "clientError", "message" => first_line(e.message))
    rescue IOError, SystemCallError => e
      outcome.merge!("status" => "transportError", "message" => first_line(e.message))
    end
    outcome
  end
  connection.close

  # The typed builder renders rather than calls, so these need no server. Each
  # binding builds with its own API; the rendered output is compared.
  queries = suite["queries"].map do |test_case|
    outcome = { "name" => test_case["name"] }
    begin
      outcome["status"] = "ok"
      outcome["rendered"] = build(test_case["build"]).render(core)
    rescue ThetaBase::ProtocolError => e
      outcome.merge!("status" => "clientError", "message" => first_line(e.message))
    rescue StandardError => e
      outcome.merge!("status" => "error", "message" => first_line(e.message))
    end
    outcome
  end

  # UTF-8 explicitly. Ruby's stdout takes the console codepage on Windows, and
  # one refusal message contains an em-dash — this output is compared byte for
  # byte against six other runners.
  $stdout.set_encoding(Encoding::UTF_8)
  $stdout.puts(pretty({
                        "protocolVersion" => welcome["protocolVersion"],
                        "results" => results,
                        "queries" => queries
                      }))
  0
end

exit(main(ARGV)) if $PROGRAM_NAME == __FILE__
