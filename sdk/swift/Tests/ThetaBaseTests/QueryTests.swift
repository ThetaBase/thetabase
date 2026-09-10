// What the builder has to guarantee, checked against the real core.
//
// Against the core rather than against the AST alone: the AST is only
// interesting because of what the core makes of it, and a test that asserted on
// JSON shape would pass just as happily if the core rejected every query.

import Foundation
import XCTest

@testable import ThetaBase

final class QueryTests: XCTestCase {

    private static func repoRoot() -> URL {
        var dir = URL(fileURLWithPath: #filePath)
        while !FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("sdk/conformance/cases.json").path)
        {
            dir = dir.deletingLastPathComponent()
        }
        return dir
    }

    private func core() throws -> Scribe {
        try Scribe(
            path: QueryTests.repoRoot().appendingPathComponent(
                "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))
    }

    private func sql(_ rendered: JSONValue) -> String {
        guard case .object(let fields) = rendered, case .string(let text)? = fields["sql"] else {
            return ""
        }
        return text
    }

    private func params(_ rendered: JSONValue) -> [String: JSONValue] {
        guard case .object(let fields) = rendered, case .object(let bound)? = fields["params"]
        else { return [:] }
        return bound
    }

    func testAValueNeverReachesTheQueryText() throws {
        // The property the whole builder exists for.
        let hostile = "'; DROP TABLE users; --"
        let rendered = try Query.table("users").filter(.eq("name", .string(hostile))).render(core())

        XCTAssertEqual(sql(rendered), "SELECT * FROM users WHERE name = $p0")
        XCTAssertFalse(sql(rendered).contains("DROP"), "the value reached the text")
        XCTAssertEqual(params(rendered)["p0"], .string("\"\(hostile)\""))
    }

    func testRepeatedFiltersAreAndedRatherThanReplaced() throws {
        let rendered = try Query.table("users")
            .filter(.eq("email", "a@example.com"))
            .filter(.gt("age", 30))
            .render(core())

        XCTAssertTrue(sql(rendered).contains(" AND "), "the second filter replaced the first")
        XCTAssertEqual(params(rendered).count, 2)
    }

    func testABuilderNeverMutatesWhatItWasDerivedFrom() throws {
        // Swift gives this for free — `Query` is a value type — where Go, Ruby
        // and TypeScript each had to copy a collection by hand. Asserted anyway,
        // because "value type" stops being true the moment someone reaches for a
        // class to avoid a copy.
        let core = try core()
        let base = Query.table("users").select("id")
        let left = base.select("email")
        let right = base.select("name")

        XCTAssertEqual(base.ast.columns.count, 1)
        XCTAssertEqual(sql(try left.render(core)), "SELECT id, email FROM users")
        XCTAssertEqual(sql(try right.render(core)), "SELECT id, name FROM users")
    }

    func testAnIdentifierThatCouldCarrySyntaxIsRefused() throws {
        // Refused rather than escaped. Escaping would mean deciding what is safe
        // to quote, and the safe answer is that a name needing quotes is not a
        // name this subset accepts.
        let core = try core()
        XCTAssertThrowsError(try Query.table("users; DROP TABLE x").render(core)) { error in
            XCTAssertTrue(error is ProtocolError, "wrong error type: \(error)")
        }
    }

    func testAQueryWithNoProjectionSelectsEverything() throws {
        // The Go SDK shipped this broken: an empty slice can be nil there and
        // marshals to `null`, which the core reads as a missing sequence. Pinned
        // in every binding rather than only in the one that got it wrong.
        let encoded = try JSONEncoder().encode(Query.table("users").ast)
        let decoded = try JSONDecoder().decode(JSONValue.self, from: encoded)
        guard case .object(let fields) = decoded else { return XCTFail("not an object") }

        XCTAssertEqual(fields["columns"], .array([]))
        XCTAssertEqual(fields["orderBy"], .array([]))
        XCTAssertEqual(sql(try Query.table("users").render(core())), "SELECT * FROM users")
    }

    func testALargeIntegerSurvivesJsonValue() throws {
        // `Int` is tried before `Double` in the decoder, and that order is the
        // point: above 2^53 a Double silently changes value, so `WHERE id = n`
        // would match the wrong row.
        let big: Int64 = 9_007_199_254_740_993
        let encoded = try JSONEncoder().encode(JSONValue.int(big))
        XCTAssertEqual(try JSONDecoder().decode(JSONValue.self, from: encoded), .int(big))
    }

    func testAGeneratedEnumCarriesItsWireSpelling() throws {
        let encoded = try JSONEncoder().encode(Gate.autoApply)
        XCTAssertEqual(String(decoding: encoded, as: UTF8.self), "\"autoApply\"")
        XCTAssertEqual(
            String(decoding: try JSONEncoder().encode(Status.upToDate), as: UTF8.self),
            "\"upToDate\"")
    }
}
