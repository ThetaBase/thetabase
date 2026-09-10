// A `Codable` any-JSON value.
//
// Swift has no `Any` that `Codable` will encode, and this SDK needs one twice
// over: a query's bound values are whatever the caller passed, and a wire
// union's payload is a different shape per arm. Both cross a JSON boundary into
// the Scribe core, so both need a type the encoder will accept.
//
// Deliberately not `AnyCodable` from a package. It is thirty lines, it is the
// only dependency this SDK would take that is not the WebAssembly runtime, and
// a JSON value is not a thing whose definition should be able to change
// underneath a wire protocol.

import Foundation

public enum JSONValue: Codable, Equatable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Int64.self) {
            // Int before Double, and that order is the whole point. Above 2^53 a
            // Double silently changes value, so `WHERE id = 9007199254740993`
            // would match the wrong row — the same trap the TypeScript binding
            // avoids with bigint.
            self = .int(value)
        } else if let value = try? container.decode(Double.self) {
            self = .double(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
        } else {
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "not a JSON value"
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case .bool(let value): try container.encode(value)
        case .int(let value): try container.encode(value)
        case .double(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        }
    }
}

extension JSONValue: ExpressibleByStringLiteral,
                     ExpressibleByIntegerLiteral,
                     ExpressibleByFloatLiteral,
                     ExpressibleByBooleanLiteral,
                     ExpressibleByNilLiteral {
    // So a caller writes `Query.eq("email", "alice@example.com")` rather than
    // `Query.eq("email", .string("alice@example.com"))`. The ceremony would be
    // at every call site, which is where a builder is judged.
    public init(stringLiteral value: String) { self = .string(value) }
    public init(integerLiteral value: Int64) { self = .int(value) }
    public init(floatLiteral value: Double) { self = .double(value) }
    public init(booleanLiteral value: Bool) { self = .bool(value) }
    public init(nilLiteral: ()) { self = .null }
}
