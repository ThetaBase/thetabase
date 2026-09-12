// Runs the shared conformance suite from Swift, against a live thetad.
//
// Prints one JSON document on stdout. Every other runner prints the same
// document from the same cases, and `make conformance` diffs them: bindings that
// each pass their own suite prove nothing about whether they agree.

import Foundation
import ThetaBase

#if canImport(Glibc)
    import Glibc
#elseif canImport(Darwin)
    import Darwin
#elseif canImport(WinSDK)
    import WinSDK
#endif

// MARK: - the report

/// One case's outcome.
///
/// An ordered list of pairs rather than a dictionary, because `JSONEncoder`
/// writes a dictionary in whatever order it likes and the other seven runners
/// emit `name`, `status`, then the body. The comparison is byte for byte, so key
/// order is part of the contract.
typealias Document = [(String, JSONValue)]

func pretty(_ value: JSONValue, _ indent: String = "") -> String {
    let inner = indent + "  "
    switch value {
    case .object(let fields):
        // Reached only for values that came back from the core, which are
        // already the shape the core chose; the report's own ordering is handled
        // by `prettyDocument`.
        if fields.isEmpty { return "{}" }
        let body = fields.keys.sorted().map { key in
            "\(inner)\(scalar(.string(key))): \(pretty(fields[key]!, inner))"
        }
        return "{\n" + body.joined(separator: ",\n") + "\n" + indent + "}"
    case .array(let items):
        if items.isEmpty { return "[]" }
        return "[\n" + items.map { inner + pretty($0, inner) }.joined(separator: ",\n") + "\n"
            + indent + "]"
    default:
        return scalar(value)
    }
}

func prettyDocument(_ document: Document, _ indent: String = "") -> String {
    let inner = indent + "  "
    if document.isEmpty { return "{}" }
    let body = document.map { key, value -> String in
        // A `.string` whose contents already look like JSON is a pre-rendered
        // sub-document, spliced rather than quoted. Ugly, and the alternative is
        // an order-preserving `JSONValue` — a hand-written `Codable` conformance
        // in the SDK proper, added to serve a test harness.
        if case .string(let text) = value, text.hasPrefix("{") || text.hasPrefix("[") {
            return "\(inner)\(scalar(.string(key))): \(text)"
        }
        return "\(inner)\(scalar(.string(key))): \(pretty(value, inner))"
    }
    return "{\n" + body.joined(separator: ",\n") + "\n" + indent + "}"
}

func scalar(_ value: JSONValue) -> String {
    let encoder = JSONEncoder()
    // `withoutEscapingSlashes` because `JSON.stringify` does not escape them and
    // this output is compared byte for byte. Non-ASCII is passed through by
    // default, which is what the other runners do too.
    encoder.outputFormatting = [.withoutEscapingSlashes]
    guard let data = try? encoder.encode(value) else { return "null" }
    return String(decoding: data, as: UTF8.self)
}

// MARK: - a socket

/// A blocking TCP socket, in the two dialects the C sockets API comes in.
///
/// Winsock and POSIX disagree about almost every type here: a handle is a
/// `UInt64` on one and an `Int32` on the other, and `send`/`recv` take and
/// return `Int32` against `Int`. The differences are pushed into three small
/// shims at the top rather than sprinkled through the methods, so the framing
/// logic below reads the same on both.
///
/// Hand-rolled rather than NIO because this is a test harness: pulling a server
/// framework in would make the SDK's own dependency graph a claim about what a
/// user needs, and they need neither.
final class TCPSocket: FramedSocket {
    #if canImport(WinSDK)
        private typealias Handle = SOCKET
    #else
        private typealias Handle = Int32
    #endif

    private let handle: Handle

    init(host: String, port: UInt16) throws {
        #if canImport(WinSDK)
            var wsa = WSADATA()
            _ = WSAStartup(0x0202, &wsa)
            handle = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP.rawValue)
            guard handle != INVALID_SOCKET else { throw ProtocolError("could not open a socket") }
        #elseif canImport(Darwin)
            // `SOCK_STREAM` is a plain `Int32` here. On Linux it is
            // `__socket_type`, which is why the arm below has to unwrap a
            // `rawValue` that does not exist on Darwin -- and why this file did
            // not compile for macOS or iOS at all, on a package that declares
            // both.
            handle = socket(AF_INET, SOCK_STREAM, 0)
            guard handle >= 0 else { throw ProtocolError("could not open a socket") }
        #else
            handle = socket(AF_INET, Int32(SOCK_STREAM.rawValue), 0)
            guard handle >= 0 else { throw ProtocolError("could not open a socket") }
        #endif

        var address = sockaddr_in()
        // `sa_family_t` on POSIX, `ADDRESS_FAMILY` on Windows, and different
        // widths between macOS and Linux. Written through the field's own type
        // so none of that has to be named.
        address.sin_family = numericCast(AF_INET)
        address.sin_port = port.bigEndian
        let raw = TCPSocket.networkOrder(host)
        #if canImport(WinSDK)
            address.sin_addr.S_un.S_addr = raw
        #else
            address.sin_addr.s_addr = raw
        #endif

        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(handle, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard connected >= 0 else { throw ProtocolError("could not connect to \(host):\(port)") }
    }

    /// A dotted quad as `s_addr` expects it.
    ///
    /// Built by hand rather than through `inet_addr`, whose return type and
    /// header differ between the two platforms for no benefit here — the harness
    /// only ever connects to a numeric loopback address.
    private static func networkOrder(_ host: String) -> UInt32 {
        let name = host == "localhost" ? "127.0.0.1" : host
        let parts = name.split(separator: ".").compactMap { UInt32($0) }
        guard parts.count == 4 else { return 0x0100_007F }
        return (parts[3] << 24) | (parts[2] << 16) | (parts[1] << 8) | parts[0]
    }

    /// The C `send`, not this class's.
    ///
    /// Module-qualified in every arm because the method below is also called
    /// `send`: an unqualified call would recurse into it. That is necessary and
    /// was not the bug -- naming `Glibc` in the non-Windows arm was, since the
    /// module is `Darwin` on Apple platforms and this file claims to support
    /// them.
    private func send(_ pointer: UnsafeRawPointer, _ count: Int) -> Int {
        #if canImport(WinSDK)
            return Int(
                WinSDK.send(handle, pointer.assumingMemoryBound(to: CChar.self), Int32(count), 0))
        #elseif canImport(Darwin)
            return Darwin.send(handle, pointer, count, 0)
        #else
            return Glibc.send(handle, pointer, count, 0)
        #endif
    }

    private func receive(_ pointer: UnsafeMutableRawPointer, _ count: Int) -> Int {
        #if canImport(WinSDK)
            return Int(
                WinSDK.recv(handle, pointer.assumingMemoryBound(to: CChar.self), Int32(count), 0))
        #elseif canImport(Darwin)
            return Darwin.recv(handle, pointer, count, 0)
        #else
            return Glibc.recv(handle, pointer, count, 0)
        #endif
    }

    func write(_ bytes: [UInt8]) throws {
        var sent = 0
        while sent < bytes.count {
            let n = bytes.withUnsafeBytes { buffer in
                send(buffer.baseAddress!.advanced(by: sent), bytes.count - sent)
            }
            guard n > 0 else { throw ProtocolError("the connection closed while writing") }
            sent += n
        }
    }

    func readFully(_ count: Int) throws -> [UInt8] {
        // A socket delivers whatever arrived, not what was asked for, and a
        // short read here would frame the next message wrong.
        var out = [UInt8](repeating: 0, count: count)
        var read = 0
        while read < count {
            let n = out.withUnsafeMutableBytes { buffer in
                receive(buffer.baseAddress!.advanced(by: read), count - read)
            }
            guard n > 0 else { throw ProtocolError("the server closed the connection") }
            read += n
        }
        return out
    }

    func close() {
        #if canImport(WinSDK)
            closesocket(handle)
        #elseif canImport(Darwin)
            _ = Darwin.close(handle)
        #else
            _ = Glibc.close(handle)
        #endif
    }
}

// MARK: - the run

func repoRoot() throws -> URL {
    var dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
    while dir.path != "/" && dir.path.count > 3 {
        if FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("sdk/conformance/cases.json").path)
        {
            return dir
        }
        dir = dir.deletingLastPathComponent()
    }
    throw ProtocolError("no ThetaBase workspace above the working directory")
}

func firstLine(_ text: String) -> String {
    text.split(separator: "\n", maxSplits: 1).first.map(String.init) ?? ""
}

/// Blank out values that legitimately differ between runs.
func redact(_ value: JSONValue, _ paths: JSONValue?) -> JSONValue {
    guard case .array(let list) = paths ?? .null else { return value }
    var out = value
    for entry in list {
        guard case .string(let dotted) = entry else { continue }
        out = redactOne(out, dotted.split(separator: ".").map(String.init))
    }
    return out
}

func redactOne(_ value: JSONValue, _ path: [String]) -> JSONValue {
    guard case .object(var fields) = value, let head = path.first else { return value }
    if path.count == 1 {
        if fields[head] != nil { fields[head] = .string("<redacted>") }
    } else if let child = fields[head] {
        fields[head] = redactOne(child, Array(path.dropFirst()))
    }
    return .object(fields)
}

/// Map a case's operator name onto the builder.
// `ThetaBase.Predicate`, qualified. Foundation ships a `Predicate` of its own, so
// the bare name is ambiguous wherever both modules are imported — which, in
// Swift, is everywhere. Naming the module is the idiomatic answer and it is
// cheaper than diverging from the seven other bindings that call this a
// predicate.
func predicate(_ op: String, _ column: String, _ value: JSONValue) throws -> ThetaBase.Predicate {
    switch op {
    case "eq": return .eq(column, value)
    case "ne": return .ne(column, value)
    case "lt": return .lt(column, value)
    case "lte": return .lte(column, value)
    case "gt": return .gt(column, value)
    case "gte": return .gte(column, value)
    case "isNull": return .isNull(column)
    case "notNull": return .notNull(column)
    default:
        throw ProtocolError("the suite names an operator this SDK does not have: \"\(op)\"")
    }
}

func field(_ value: JSONValue, _ name: String) -> JSONValue? {
    guard case .object(let fields) = value else { return nil }
    return fields[name]
}

func string(_ value: JSONValue?) -> String {
    guard case .string(let text) = value ?? .null else { return "" }
    return text
}

func array(_ value: JSONValue?) -> [JSONValue] {
    guard case .array(let items) = value ?? .null else { return [] }
    return items
}

func build(_ spec: JSONValue) throws -> Query {
    var query = Query.table(string(field(spec, "table")))
    for column in array(field(spec, "select")) {
        query = query.select(string(column))
    }
    for clause in array(field(spec, "where")) {
        let parts = array(clause)
        query = query.filter(
            try predicate(string(parts[0]), string(parts[1]), parts.count > 2 ? parts[2] : .null))
    }
    for clause in array(field(spec, "orderBy")) {
        let parts = array(clause)
        var descending = false
        if case .bool(let flag) = parts[1] { descending = flag }
        query = query.orderBy(string(parts[0]), descending: descending)
    }
    if case .int(let limit) = field(spec, "limit") ?? .null { query = query.limit(limit) }
    if case .int(let offset) = field(spec, "offset") ?? .null { query = query.offset(offset) }
    return query
}

/// Drive the client a customer holds, against a live instance.
///
/// The conformance suite exercises the protocol core through this binding's
/// shim. Nothing there touches `Theta`, so without this the Swift client could
/// be wired and broken at once — which is exactly what the Python and
/// TypeScript clients turned out to be.
func smoke(root: URL, address: String, token: String) throws -> Int32 {
    // The client reads the token from the environment and refuses to take it
    // as a parameter, so a test has to put it there the way `theta exec` does.
    // `setenv` is POSIX and absent on Windows, where the ucrt spelling is
    // `_putenv_s`.
    #if os(Windows)
        _ = "THETA_TOKEN".withCString { name in
            token.withCString { value in _putenv_s(name, value) }
        }
    #else
        setenv("THETA_TOKEN", token, 1)
    #endif

    let core = try Scribe(
        path: root.appendingPathComponent(
            "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))

    let hostPort = address.split(separator: ":")
    let socket = try TCPSocket(
        host: String(hostPort[0]), port: UInt16(String(hostPort[1])) ?? 0)

    let theta = try Theta(core: core, socket: socket, project: "conformance")
    defer { theta.close() }

    var failures: [String] = []
    func check(_ name: String, _ held: Bool) {
        print("  \(held ? "ok  " : "FAIL")  \(name)")
        if !held { failures.append(name) }
    }

    check("get of an absent row is nil", try theta.get("sw/absent") == nil)

    _ = try theta.put("sw/a", .int(1))
    check("put then get round-trips", try theta.get("sw/a") == .int(1))

    let created = try theta.putIf("sw/b", .int(2), expect: .absent)
    check("a create-only write lands when the row is absent", created != nil)

    let again = try theta.putIf("sw/b", .int(3), expect: .absent)
    check("the same write is refused once the row exists", again == nil)
    check("the refused write changed nothing", try theta.get("sw/b") == .int(2))

    let committed = try theta.transaction([
        .put("sw/tx1", .int(10)),
        .put("sw/tx2", .int(20)),
    ])
    check("a transaction commits", committed != nil)
    check("both of its writes are visible", try theta.get("sw/tx2") == .int(20))

    // The precondition fails on the *second* operation; the first must not
    // survive it.
    let refused = try theta.transaction([
        .put("sw/tx3", .int(30)),
        .put("sw/b", .int(99), expect: .absent),
    ])
    check("a transaction with a failing precondition is refused", refused == nil)
    check("its other write did not land", try theta.get("sw/tx3") == nil)

    _ = try theta.delete("sw/a")
    check("delete removes the row", try theta.get("sw/a") == nil)

    if !failures.isEmpty {
        let report = failures.joined(separator: "; ")
        FileHandle.standardError.write(report.data(using: .utf8)!)
        return 1
    }
    return 0
}

func run() throws -> Int32 {
    let arguments = Array(CommandLine.arguments.dropFirst())
    guard arguments.count >= 2 else {
        FileHandle.standardError.write("usage: thetabase-conformance <host:port> <token>\n".data(using: .utf8)!)
        return 2
    }

    let root = try repoRoot()

    // A second mode in this runner rather than a second target: the conformance
    // build is already in the gate, and a separate executable would be a second
    // thing to keep built. `--smoke` drives the high-level `Theta` client; the
    // default drives the protocol core.
    if arguments.contains("--smoke") {
        return try smoke(root: root, address: arguments[0], token: arguments[1])
    }

    let suite = try JSONDecoder().decode(
        JSONValue.self,
        from: Data(contentsOf: root.appendingPathComponent("sdk/conformance/cases.json"))
    )
    let core = try Scribe(
        path: root.appendingPathComponent(
            "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))

    let hostPort = arguments[0].split(separator: ":")
    let socket = try TCPSocket(
        host: String(hostPort[0]), port: UInt16(String(hostPort[1])) ?? 0)

    // Handshake first: the server refuses anything else until it has one.
    try socket.write(core.encodeHello(token: arguments[1], clientName: "conformance-swift"))
    let welcome = try core.decodeWelcome(core.frame(from: socket))

    var results: [JSONValue] = []
    for (index, testCase) in array(field(suite, "cases")).enumerated() {
        var outcome: [String: JSONValue] = ["name": field(testCase, "name") ?? .null]
        var ordered: Document = [("name", field(testCase, "name") ?? .null)]
        do {
            let response = try core.exchange(
                field(testCase, "request") ?? JSONValue.null,
                over: socket,
                requestId: UInt64(index + 1),
                branchId: 0
            )
            ordered.append(("status", .string("ok")))
            ordered.append(("response", redact(response, field(testCase, "redact"))))
        } catch let error as ProtocolError {
            ordered.append(("status", .string("clientError")))
            ordered.append(("message", .string(firstLine(error.message))))
        } catch {
            ordered.append(("status", .string("transportError")))
            ordered.append(("message", .string(firstLine("\(error)"))))
        }
        outcome.removeAll()
        results.append(.string(prettyDocument(ordered, "    ")))
    }
    socket.close()

    // The typed builder renders rather than calls, so these need no server.
    var queries: [JSONValue] = []
    for testCase in array(field(suite, "queries")) {
        var ordered: Document = [("name", field(testCase, "name") ?? .null)]
        do {
            let rendered = try build(field(testCase, "build") ?? .null).render(core)
            ordered.append(("status", .string("ok")))
            // Rebuilt in `sql`, `params` order rather than printed as it decoded.
            // `JSONValue.object` is a Dictionary, so the core's key order does
            // not survive the parse — and every other response in this document
            // happens to be alphabetical, which hid it until the one object that
            // is not turned up.
            ordered.append(
                (
                    "rendered",
                    .string(
                        prettyDocument(
                            [
                                ("sql", field(rendered, "sql") ?? .null),
                                ("params", field(rendered, "params") ?? .null),
                            ],
                            "      "
                        ))
                ))
        } catch let error as ProtocolError {
            ordered.append(("status", .string("clientError")))
            ordered.append(("message", .string(firstLine(error.message))))
        } catch {
            ordered.append(("status", .string("error")))
            ordered.append(("message", .string(firstLine("\(error)"))))
        }
        queries.append(.string(prettyDocument(ordered, "    ")))
    }

    // The two arrays hold pre-rendered text rather than values, because the
    // per-case key order has to survive and `JSONValue.object` is a dictionary.
    // Assembled by hand here rather than given to an encoder.
    let body =
        "{\n  \"protocolVersion\": \(scalar(field(welcome, "protocolVersion") ?? .null)),\n"
        + "  \"results\": " + block(results) + ",\n"
        + "  \"queries\": " + block(queries) + "\n}"

    FileHandle.standardOutput.write((body + "\n").data(using: .utf8)!)
    return 0
}

/// Join pre-rendered case documents into a JSON array at two-space indent.
func block(_ entries: [JSONValue]) -> String {
    if entries.isEmpty { return "[]" }
    let body = entries.map { entry -> String in
        guard case .string(let text) = entry else { return "null" }
        return "    " + text
    }
    return "[\n" + body.joined(separator: ",\n") + "\n  ]"
}

do {
    exit(try run())
} catch {
    FileHandle.standardError.write("\(error)\n".data(using: .utf8)!)
    exit(1)
}
