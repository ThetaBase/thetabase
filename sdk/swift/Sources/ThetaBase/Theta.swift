// The client a caller holds.
//
// Everything about *what* a message means lives in the WebAssembly core, which
// `Scribe` loads. This file decides when to send one, unwraps the answer, and
// turns a refusal into an error a caller can act on. It builds no wire
// messages, which is why a capability added to the protocol reaches every
// binding at once.
//
// No socket is created here. `FramedSocket` is supplied by the caller for the
// reason `Scribe.swift` gives: a server uses NIO, an iOS app uses
// `Network.framework`, and an SDK should force neither on the other.

import Foundation

/// A refusal or a fault the server reported.
public enum ThetaError: Error, Equatable {
    /// The Safety Layer refused a change. Carries the diff to act on.
    ///
    /// An *answer*, not a fault: the change was classified and found to need a
    /// human, and the diff is what that human reads.
    case safetyGate(message: String, diff: JSONValue?)

    /// The blast-radius breaker is open. A limit this project set, not a bad
    /// request — retrying unchanged will fail identically until the window
    /// rolls.
    case circuitBreaker(message: String, windowRows: Int64, ceiling: Int64)

    /// The server sent something this build does not understand, which means it
    /// is newer than this client.
    case unknownResponse(String)

    /// Anything else the server refused, with its message.
    case refused(String)

    /// The connection is not usable.
    case transport(String)
}

/// A precondition on a row's current state.
public enum Expect: Equatable, Sendable {
    /// The row must not exist. Create-only — this is how a caller makes a
    /// uniqueness rule the storage layer enforces, by putting the unique tuple
    /// in the key.
    case absent
    /// The row must be at exactly this version.
    case version(UInt64)

    var wire: JSONValue {
        switch self {
        case .absent:
            return .object(["kind": .string("absent")])
        case .version(let v):
            return .object(["kind": .string("version"), "value": .int(Int64(v))])
        }
    }
}

/// One operation inside a transaction.
public struct TxOp: Equatable, Sendable {
    public let key: String
    /// `nil` means unconditional, which is the common case.
    public let expect: Expect?
    /// `nil` deletes the row.
    ///
    /// Distinguished from writing a null value by `isDelete` rather than by
    /// this being `nil`: null is a value a caller may legitimately store, and a
    /// client that treated storing it as a delete would silently drop rows.
    public let value: JSONValue?
    public let isDelete: Bool

    public static func put(_ key: String, _ value: JSONValue, expect: Expect? = nil) -> TxOp {
        TxOp(key: key, expect: expect, value: value, isDelete: false)
    }

    public static func delete(_ key: String, expect: Expect? = nil) -> TxOp {
        TxOp(key: key, expect: expect, value: nil, isDelete: true)
    }

    var wire: JSONValue {
        var fields: [String: JSONValue] = ["key": .string(key)]
        if let expect { fields["expect"] = expect.wire }
        if isDelete {
            fields["action"] = .string("delete")
        } else {
            fields["action"] = .string("put")
            fields["value"] = value ?? .null
        }
        return .object(fields)
    }
}

/// A connection to one ThetaBase project.
///
/// No connection string, no API key, no `.env`: the project is named and the
/// scoped token is resolved and injected by the toolchain. There is
/// deliberately no initialiser taking an address and a token — accepting one
/// would undo the property that keeps credentials out of application code.
public final class Theta {
    private let core: Scribe
    private let socket: FramedSocket
    private var nextRequestId: UInt64 = 1
    private var branchId: UInt64 = 0

    public let project: String

    /// Perform the handshake over an already-connected socket.
    ///
    /// The token comes from `THETA_TOKEN`, which `theta exec` injects.
    public init(core: Scribe, socket: FramedSocket, project: String? = nil) throws {
        guard let token = ProcessInfo.processInfo.environment["THETA_TOKEN"],
            !token.isEmpty
        else {
            throw ThetaError.transport(
                "THETA_TOKEN is not set. Run this process under `theta exec`, "
                    + "which resolves a scoped token for the project and injects it."
            )
        }

        self.core = core
        self.socket = socket
        self.project =
            project ?? ProcessInfo.processInfo.environment["THETA_PROJECT"] ?? ""

        // The handshake, before anything else travels. `thetad` also
        // re-authorises on every request, so this is the introduction rather
        // than the whole of the authentication.
        try socket.write(core.encodeHello(token: token, clientName: "thetabase-swift"))
        _ = try core.decodeWelcome(core.frame(from: socket))
    }

    /// Operate on a named branch for subsequent calls.
    public func useBranch(_ id: UInt64) {
        branchId = id
    }

    public func close() {
        socket.close()
    }

    /// Point lookup. Hot path: no model call, ever.
    public func get(_ key: String) throws -> JSONValue? {
        let value = try expect(
            call(.object(["op": .string("get"), "key": .string(key)])),
            "get"
        )
        // `found: false` is a successful answer meaning the row is not there,
        // and it is distinct from a null value that is.
        guard case .object(let fields) = value ?? .null,
            case .bool(true) = fields["found"] ?? .null
        else { return nil }
        return fields["value"]
    }

    @discardableResult
    public func put(_ key: String, _ value: JSONValue) throws -> String {
        try commitId(
            call(.object(["op": .string("put"), "key": .string(key), "value": value]))
        )
    }

    @discardableResult
    public func delete(_ key: String) throws -> String {
        try commitId(call(.object(["op": .string("delete"), "key": .string(key)])))
    }

    /// A write conditional on the row's current state.
    ///
    /// Returns `nil` when the condition was not met. That is not an error: the
    /// request was well formed and the server did what it was asked, and a
    /// lost-update retry that threw would make a contended key look like a
    /// fault.
    public func putIf(_ key: String, _ value: JSONValue, expect condition: Expect) throws
        -> String?
    {
        let response = try call(
            .object([
                "op": .string("putIf"),
                "key": .string(key),
                "value": value,
                "expect": condition.wire,
            ])
        )
        if kind(of: response) == "preconditionFailed" { return nil }
        return try commitId(response)
    }

    /// Several writes that land as one commit, or none of them.
    ///
    /// Unlike sending several writes, this batches the durability boundary and
    /// not merely the network. Every operation's precondition is checked before
    /// any write is applied, so a transaction that would violate one changes
    /// nothing.
    public func transaction(_ ops: [TxOp]) throws -> String? {
        let response = try call(
            .object(["op": .string("transaction"), "ops": .array(ops.map(\.wire))])
        )
        if kind(of: response) == "preconditionFailed" { return nil }
        return try commitId(response)
    }

    /// Run a typed plan. Rendered to SQL by the core, never here.
    public func query(_ plan: Query) throws -> JSONValue? {
        try run(plan, kind: "query")
    }

    /// EXPLAIN without executing — what a reviewer reads before approving.
    public func explain(_ plan: Query) throws -> JSONValue? {
        try run(plan, kind: "explain")
    }

    public func status() throws -> JSONValue? {
        try expect(call(.object(["op": .string("status")])), "status")
    }

    /// What is in here, and where it came from.
    public func describe() throws -> JSONValue? {
        try expect(call(.object(["op": .string("describe")])), "description")
    }

    private func run(_ plan: Query, kind name: String) throws -> JSONValue? {
        let rendered = try plan.render(core)
        guard case .object(let fields) = rendered else {
            throw ThetaError.transport("the core did not render a query object")
        }
        let response = try call(
            .object([
                "op": .string(name),
                "sql": fields["sql"] ?? .null,
                "params": fields["params"] ?? .object([:]),
            ])
        )
        return try expect(response, name)
    }

    private func call(_ request: JSONValue) throws -> JSONValue {
        let id = nextRequestId
        nextRequestId += 1
        return try core.exchange(request, over: socket, requestId: id, branchId: branchId)
    }

    private func kind(of response: JSONValue) -> String? {
        guard case .object(let fields) = response,
            case .string(let k) = fields["kind"] ?? .null
        else { return nil }
        return k
    }

    private func commitId(_ response: JSONValue) throws -> String {
        guard case .object(let fields) = try expect(response, "commit") ?? .null,
            case .string(let id) = fields["commitId"] ?? .null
        else {
            throw ThetaError.transport("a commit response carried no commit id")
        }
        return id
    }

    /// Unwrap a response of the expected kind, or throw something actionable.
    ///
    /// The three named failures stay distinct. A gate refusal carries the diff
    /// the caller has to act on and is an answer rather than a fault; an open
    /// breaker is a limit this project set rather than a bad request; and
    /// everything else is an error with a message. Collapsing them into one
    /// case would make the first two unactionable, which is why the wire
    /// separates them.
    private func expect(_ response: JSONValue, _ wanted: String) throws -> JSONValue? {
        guard case .object(let fields) = response else {
            throw ThetaError.transport("the core returned something that is not an object")
        }
        let actual = kind(of: response)
        if actual == wanted { return fields["value"] }

        if actual == "error" {
            guard case .object(let value) = fields["value"] ?? .object([:]) else {
                throw ThetaError.refused("the server refused the request")
            }
            var message = "the server refused the request"
            if case .string(let m) = value["message"] ?? .null { message = m }
            var code = ""
            if case .string(let c) = value["code"] ?? .null { code = c }

            switch code {
            case "ConfirmationRequired":
                throw ThetaError.safetyGate(message: message, diff: value["diff"])
            case "BreakerOpen":
                var rows: Int64 = 0
                var ceiling: Int64 = 0
                if case .int(let r) = value["windowRows"] ?? .null { rows = r }
                if case .int(let c) = value["ceiling"] ?? .null { ceiling = c }
                throw ThetaError.circuitBreaker(
                    message: message, windowRows: rows, ceiling: ceiling)
            default:
                throw ThetaError.refused(message)
            }
        }

        // What the core produces for a response this build does not know.
        // Reported as a version problem because that is what it is, and
        // upgrading is the remedy.
        if actual == "raw" {
            throw ThetaError.unknownResponse(
                "the server sent a response this SDK does not understand; it is "
                    + "newer than this client. Upgrade the SDK."
            )
        }

        throw ThetaError.transport(
            "expected a `\(wanted)` response, got `\(actual ?? "nothing")`"
        )
    }
}
