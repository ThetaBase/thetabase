# frozen_string_literal: true

# The host half of Scribe: sockets, and nothing else.
#
# Every decision about the protocol — how a request is encoded, what a response
# means, when a cached read must be dropped — lives in the WebAssembly core
# (`crates/theta-scribe-wasm`). This file moves bytes.
#
# The split is why a seventh SDK is cheap. A Ruby SDK that built wire messages in
# Ruby would be a seventh implementation of the protocol, and the first time the
# schema moved six of the seven would be wrong. What must be identical across
# languages is the protocol; what must differ is I/O.

require "json"
require "socket"
require "wasmtime"

module ThetaBase
  # A protocol error the core rejected, kept distinct from a transport failure.
  #
  # The distinction is the whole point: a caller may retry a transport failure
  # and must not retry a protocol one.
  class ProtocolError < StandardError; end

  # The WebAssembly core, loaded once per process.
  class Scribe
    # `[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out.
    RESULT_HEADER = 5
    LENGTH_PREFIX_BYTES = 4

    # What the core returns for a frame length this protocol does not allow.
    NO_LENGTH = 0xffffffff

    def initialize(wasm)
      engine = Wasmtime::Engine.new
      # No host imports: the core is pure computation, which is why it loads
      # identically here, in Node, in a browser, in Go, on the JVM and in .NET.
      @store = Wasmtime::Store.new(engine)
      module_ = Wasmtime::Module.new(engine, wasm)
      @instance = Wasmtime::Instance.new(@store, module_)
      @memory = @instance.export("memory")&.to_memory
      raise ProtocolError, "this Scribe core exports no memory" if @memory.nil?

      @mutex = Mutex.new
    end

    def self.load(path)
      new(File.binread(path))
    end

    # ---- the core's surface -------------------------------------------------

    # Frame a request.
    def encode(request, request_id, branch_id)
      call("theta_encode", JSON.generate(request), request_id, branch_id)
    end

    # Read a response body into a document.
    def decode(body)
      JSON.parse(call("theta_decode", body))
    end

    # Read the four-byte frame prefix.
    def body_length(prefix)
      @mutex.synchronize do
        pointer = invoke("theta_alloc", LENGTH_PREFIX_BYTES)
        begin
          @memory.write(pointer, prefix)
          length = invoke("theta_body_length", pointer) & 0xffffffff
          if length == NO_LENGTH
            raise ProtocolError, "the peer sent a length this protocol does not allow"
          end

          length
        ensure
          invoke("theta_free", pointer, LENGTH_PREFIX_BYTES)
        end
      end
    end

    # Frame the handshake.
    #
    # The protocol version is the core's to state, not the host's: a host that
    # could name its own version could claim a compatibility it does not have,
    # and the negotiation that refuses mismatched versions rather than guessing
    # would be negotiating with itself.
    def encode_hello(token, client_name)
      call("theta_encode_hello", JSON.generate({ token: token, clientName: client_name }))
    end

    # Read the server's reply to a handshake. A refusal raises.
    def decode_welcome(body)
      JSON.parse(call("theta_decode_welcome", body))
    end

    # Render a typed query AST to SQL-subset source and bound parameters.
    #
    # In the core rather than here, so a query built the same way in Python
    # renders to the same bytes — and so the rule that a value never becomes
    # query text has one implementation.
    def render_query(ast)
      JSON.parse(call("theta_render_query", JSON.generate(ast)))
    end

    # What a request invalidates: one key, everything, or nothing.
    def invalidation(request)
      JSON.parse(call("theta_invalidation", JSON.generate(request)))
    end

    private

    def invoke(name, *args)
      export = @instance.export(name)&.to_func
      # Named rather than deferred to a nil error at the first call: a core built
      # from a different revision is a real failure mode, and
      # "theta_render_query is missing" says which revision.
      raise ProtocolError, "#{name} is not exported by this Scribe core" if export.nil?

      export.call(*args)
    end

    def call(name, input, *extra)
      # The core owns one linear memory and every call allocates into it, so two
      # threads encoding at once would interleave allocations against one heap.
      @mutex.synchronize do
        bytes = input.dup.force_encoding(Encoding::BINARY)
        pointer = invoke("theta_alloc", bytes.bytesize)
        begin
          @memory.write(pointer, bytes)
          take(invoke(name, pointer, bytes.bytesize, *extra))
        ensure
          invoke("theta_free", pointer, bytes.bytesize)
        end
      end
    end

    # Read a result buffer out of the core's memory and free it.
    #
    # The tag byte distinguishes a result from a refusal, and it is checked
    # before the payload is used — the refusal path must not depend on the
    # success path having been valid.
    def take(pointer)
      header = @memory.read(pointer, RESULT_HEADER)
      length = header[0, 4].unpack1("V")
      tag = header.getbyte(4)
      payload = @memory.read(pointer + RESULT_HEADER, length)
      invoke("theta_free_result", pointer)

      raise ProtocolError, payload.force_encoding(Encoding::UTF_8) unless tag.zero?

      payload.force_encoding(Encoding::UTF_8)
    end
  end

  # A framed connection to `thetad`.
  class Connection
    def initialize(core, socket)
      @core = core
      @socket = socket
    end

    def write(bytes)
      @socket.write(bytes)
    end

    # Read exactly +n+ bytes, or raise if the peer closes first.
    def read_fully(n)
      # A socket delivers whatever arrived, not what was asked for, and a short
      # read here would frame the next message wrong.
      out = @socket.read(n)
      raise IOError, "the server closed the connection" if out.nil? || out.bytesize < n

      out
    end

    # Read one length-prefixed frame.
    def frame
      read_fully(@core.body_length(read_fully(Scribe::LENGTH_PREFIX_BYTES)))
    end

    # One request/response exchange.
    #
    # Sequential by construction: a caller that needs concurrency opens more
    # connections. Multiplexing on one socket would need request-id correlation
    # on the read side, and correlating replies to the wrong request is a
    # data-corruption bug rather than a performance one.
    def exchange(request, request_id, branch_id)
      write(@core.encode(request, request_id, branch_id))
      @core.decode(frame)
    end

    def close
      @socket.close
    end
  end
end
