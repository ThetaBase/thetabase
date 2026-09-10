// The host half of Scribe: sockets, and nothing else.
//
// Every decision about the protocol — how a request is encoded, what a response
// means, when a cached read must be dropped — lives in the WebAssembly core
// (`crates/theta-scribe-wasm`). This file moves bytes.
//
// The split is why an eighth SDK is cheap. A Swift SDK that built wire messages
// in Swift would be an eighth implementation of the protocol, and the first time
// the schema moved seven of the eight would be wrong. What must be identical
// across languages is the protocol; what must differ is I/O.

import Foundation
import WasmKit

/// A protocol error the core rejected, kept distinct from a transport failure.
///
/// The distinction is the whole point: a caller may retry a transport failure
/// and must not retry a protocol one. Collapsing them would turn "you sent
/// something invalid" into "try again", forever.
public struct ProtocolError: Error, CustomStringConvertible {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// The WebAssembly core, loaded once per process.
public final class Scribe {

    /// `[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out.
    private static let resultHeader = 5

    static let lengthPrefixBytes = 4

    /// What the core returns for a frame length this protocol does not allow.
    private static let noLength: UInt32 = 0xffff_ffff

    private let engine: Engine
    private let store: Store
    private let instance: Instance
    private let memory: Memory

    public init(wasm: [UInt8]) throws {
        let module = try parseWasm(bytes: wasm)
        // No host imports: the core is pure computation, which is why it loads
        // identically here, in Node, in a browser, in Go, on the JVM and in
        // .NET.
        self.engine = Engine()
        self.store = Store(engine: engine)
        self.instance = try module.instantiate(store: store)
        guard let memory = instance.exports[memory: "memory"] else {
            throw ProtocolError(
                "this Scribe core exports no memory — it was built from a different revision"
            )
        }
        self.memory = memory
    }

    public convenience init(path: URL) throws {
        try self.init(wasm: [UInt8](Data(contentsOf: path)))
    }

    // MARK: - the core's surface

    /// Frame a request.
    public func encode(
        _ request: some Encodable,
        requestId: UInt64,
        branchId: UInt64
    ) throws -> [UInt8] {
        try call("theta_encode", json(request), .i64(requestId), .i64(branchId))
    }

    /// Read a response body into a document.
    public func decode(_ body: [UInt8]) throws -> JSONValue {
        try parse(call("theta_decode", body))
    }

    /// Read the four-byte frame prefix.
    public func bodyLength(_ prefix: [UInt8]) throws -> Int {
        let pointer = try allocate(Scribe.lengthPrefixBytes)
        defer { try? free(pointer, Scribe.lengthPrefixBytes) }

        try write(prefix, at: pointer)
        guard case .i32(let length) = try invoke("theta_body_length", [.i32(UInt32(pointer))])
        else {
            throw ProtocolError("theta_body_length did not return an i32")
        }
        if length == Scribe.noLength {
            throw ProtocolError("the peer sent a length this protocol does not allow")
        }
        return Int(length)
    }

    /// Frame the handshake.
    ///
    /// The protocol version is the core's to state, not the host's: a host that
    /// could name its own version could claim a compatibility it does not have,
    /// and the negotiation that refuses mismatched versions rather than guessing
    /// would be negotiating with itself.
    public func encodeHello(token: String, clientName: String) throws -> [UInt8] {
        try call("theta_encode_hello", json(["token": token, "clientName": clientName]))
    }

    /// Read the server's reply to a handshake. A refusal throws.
    public func decodeWelcome(_ body: [UInt8]) throws -> JSONValue {
        try parse(call("theta_decode_welcome", body))
    }

    /// Render a typed query AST to SQL-subset source and bound parameters.
    ///
    /// In the core rather than here, so a query built the same way in Python
    /// renders to the same bytes — and so the rule that a value never becomes
    /// query text has one implementation.
    public func renderQuery(_ ast: some Encodable) throws -> JSONValue {
        try parse(call("theta_render_query", json(ast)))
    }

    /// What a request invalidates: one key, everything, or nothing.
    public func invalidation(_ request: some Encodable) throws -> JSONValue {
        try parse(call("theta_invalidation", json(request)))
    }

    // MARK: - the byte-buffer boundary

    private func function(_ name: String) throws -> Function {
        guard let function = instance.exports[function: name] else {
            // Named rather than deferred to a crash at the first call: a core
            // built from a different revision is a real failure mode, and
            // "theta_render_query is missing" says which revision.
            throw ProtocolError(
                "\(name) is not exported by this Scribe core — it was built from a "
                    + "different revision of the schema"
            )
        }
        return function
    }

    /// Call an export that returns one value.
    private func invoke(_ name: String, _ arguments: [Value]) throws -> Value {
        guard let first = try function(name).invoke(arguments).first else {
            throw ProtocolError("\(name) returned nothing")
        }
        return first
    }

    /// Call an export that returns nothing.
    ///
    /// Separate from `invoke` rather than folded into it with an ignored result:
    /// `theta_free` and `theta_free_result` genuinely return void, and a single
    /// helper demanding a value made every deallocation throw. It was invisible
    /// because the frees sit in `defer` blocks behind `try?` — so the SDK leaked
    /// the core's memory on every call and said nothing.
    private func invokeVoid(_ name: String, _ arguments: [Value]) throws {
        _ = try function(name).invoke(arguments)
    }

    private func allocate(_ length: Int) throws -> Int {
        guard case .i32(let pointer) = try invoke("theta_alloc", [.i32(UInt32(length))]) else {
            throw ProtocolError("theta_alloc did not return a pointer")
        }
        return Int(pointer)
    }

    private func free(_ pointer: Int, _ length: Int) throws {
        try invokeVoid("theta_free", [.i32(UInt32(pointer)), .i32(UInt32(length))])
    }

    private func write(_ bytes: [UInt8], at pointer: Int) throws {
        try memory.withUnsafeMutableBufferPointer(offset: UInt(pointer), count: bytes.count) {
            buffer in
            buffer.copyBytes(from: bytes)
        }
    }

    private func read(_ pointer: Int, _ count: Int) throws -> [UInt8] {
        try memory.withUnsafeMutableBufferPointer(offset: UInt(pointer), count: count) { buffer in
            Array(buffer)
        }
    }

    private func call(_ name: String, _ input: [UInt8], _ extra: Value...) throws -> [UInt8] {
        let pointer = try allocate(input.count)
        defer { try? free(pointer, input.count) }

        try write(input, at: pointer)
        let arguments: [Value] = [.i32(UInt32(pointer)), .i32(UInt32(input.count))] + extra
        guard case .i32(let result) = try invoke(name, arguments) else {
            throw ProtocolError("\(name) did not return a pointer")
        }
        return try take(Int(result))
    }

    /// Read a result buffer out of the core's memory and free it.
    ///
    /// The tag byte distinguishes a result from a refusal, and it is checked
    /// before the payload is used — the refusal path must not depend on the
    /// success path having been valid.
    private func take(_ pointer: Int) throws -> [UInt8] {
        let header = try read(pointer, Scribe.resultHeader)
        let length =
            Int(header[0]) | Int(header[1]) << 8 | Int(header[2]) << 16 | Int(header[3]) << 24
        let tag = header[4]
        let payload = try read(pointer + Scribe.resultHeader, length)

        try invokeVoid("theta_free_result", [.i32(UInt32(pointer))])

        if tag != 0 {
            throw ProtocolError(String(decoding: payload, as: UTF8.self))
        }
        return payload
    }

    private func json(_ value: some Encodable) throws -> [UInt8] {
        [UInt8](try JSONEncoder().encode(value))
    }

    private func parse(_ bytes: [UInt8]) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: Data(bytes))
    }
}

/// A framed connection to `thetad`.
///
/// A protocol rather than a concrete socket, because Swift runs where
/// `Foundation`'s socket story differs — a server uses NIO, an iOS app uses
/// `Network.framework`, and neither should be forced on the other by an SDK.
public protocol FramedSocket {
    func write(_ bytes: [UInt8]) throws
    /// Return exactly `count` bytes, or throw if the peer closes first.
    func readFully(_ count: Int) throws -> [UInt8]
    func close()
}

extension Scribe {
    /// Read one length-prefixed frame from `socket`.
    public func frame(from socket: FramedSocket) throws -> [UInt8] {
        try socket.readFully(bodyLength(socket.readFully(Scribe.lengthPrefixBytes)))
    }

    /// One request/response exchange.
    ///
    /// Sequential by construction: a caller that needs concurrency opens more
    /// connections. Multiplexing on one socket would need request-id correlation
    /// on the read side, and correlating replies to the wrong request is a
    /// data-corruption bug rather than a performance one.
    public func exchange(
        _ request: some Encodable,
        over socket: FramedSocket,
        requestId: UInt64,
        branchId: UInt64
    ) throws -> JSONValue {
        try socket.write(encode(request, requestId: requestId, branchId: branchId))
        return try decode(frame(from: socket))
    }
}
