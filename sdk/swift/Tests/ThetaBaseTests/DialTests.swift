// The transport, and the Apple-only half of it.
//
// `Dial.swift` has two arms. The `#else` arm refuses, and it is the one that
// compiles on Windows and Linux — so building the package on those platforms
// proves nothing about the arm that actually carries traffic. The
// `canImport(Network)` arm was written and never compiled anywhere, which is
// exactly the state every SDK's TLS was in before this week: plausible, unrun,
// and wrong in a way that only shows up against a real endpoint.
//
// So these tests exist to be run on macOS in CI. The address rules run
// everywhere; the TLS dial runs only where `Network` exists, and it dials a
// real host rather than a local listener, because what broke every other
// client was SNI routing and public certificate verification — neither of
// which a loopback socket can imitate.

import Foundation
import XCTest

@testable import ThetaBase

final class DialTests: XCTestCase {

    // MARK: - the address rules, everywhere

    /// Port 443 means TLS, and the override beats it both ways.
    ///
    /// The rule has to be identical in all eight SDKs, because the address
    /// travels through `THETA_ADDRESS` into every one of them and a client that
    /// disagreed about what an address means would fail as a hang rather than
    /// an error.
    func testPort443MeansTLSAndTheOverrideBeatsItBothWays() throws {
        // The override is passed, not set. `setenv` does not exist on Windows
        // Swift, and mutating a global that every other test in the binary
        // reads is the shape that turned a failing assertion into a hung run in
        // this project's Rust suite.
        XCTAssertTrue(
            Scribe.wantsTLS(port: 443, override: nil),
            "a hosted instance's address would be dialled in the clear, which cannot "
                + "even be routed -- a shared IPv4 is routed by the name in the TLS "
                + "handshake")
        XCTAssertFalse(Scribe.wantsTLS(port: 7700, override: nil))
        XCTAssertFalse(Scribe.wantsTLS(port: 7701, override: nil))

        // A developer tunnelling a local plaintext process to 443.
        for off in ["0", "false", "off", "no", "OFF"] {
            XCTAssertFalse(
                Scribe.wantsTLS(port: 443, override: off),
                "`THETA_TLS=\(off)` did not turn TLS off")
        }

        // A self-hosted instance behind its own terminating proxy.
        for on in ["1", "true", "require", "yes", "TRUE"] {
            XCTAssertTrue(
                Scribe.wantsTLS(port: 9443, override: on),
                "`THETA_TLS=\(on)` did not turn TLS on")
        }

        // An unrecognised value falls through to the port rule rather than
        // being treated as either answer.
        XCTAssertTrue(Scribe.wantsTLS(port: 443, override: "maybe"))
        XCTAssertFalse(Scribe.wantsTLS(port: 7700, override: "maybe"))
    }

    /// Split from the last colon, not the first.
    ///
    /// An IPv6 literal is full of colons. Splitting on the first yields a host
    /// of `[2a09` and a port of nonsense — and the consequence is not a parse
    /// error, it is a plaintext dial to a hosted address.
    func testAnIPv6AddressSplitsAtTheRightColon() throws {
        let (host, port) = try Scribe.splitAddress("[2a09:8280:1::1]:443")
        XCTAssertEqual(host, "2a09:8280:1::1")
        XCTAssertEqual(port, 443)

        let (name, plain) = try Scribe.splitAddress("db.example.com:7700")
        XCTAssertEqual(name, "db.example.com")
        XCTAssertEqual(plain, 7700)

        // A URL by mistake is the common hand-written error, and it has to be
        // refused rather than parsed into something.
        XCTAssertThrowsError(try Scribe.splitAddress("https://db.example.com"))
        XCTAssertThrowsError(try Scribe.splitAddress("db.example.com"))
    }

    // MARK: - TLS, where there is a TLS stack

    #if canImport(Network)

        /// A real TLS handshake, against a real certificate, routed by SNI.
        ///
        /// This is the assertion the `#else` arm can never make and the reason
        /// this file needs a macOS runner. It dials the Control Plane's own
        /// host: a public certificate chain, and a shared address where the
        /// proxy decides which app to route to from the SNI name alone.
        ///
        /// The connection cannot *succeed* at the protocol level — that host
        /// speaks HTTP, not the frame protocol — so the assertion is about
        /// which failure arrives. Reaching the point of sending a frame means
        /// the handshake completed and `Network.framework` was configured
        /// correctly.
        ///
        /// Skipped rather than failed when the network is unavailable, because
        /// a CI runner without egress should not report a transport bug.
        func testATLSDialReachesAPublicHost() throws {
            let address = ProcessInfo.processInfo.environment["THETA_LIVE_ADDRESS"]
                ?? "thetabase-control.fly.dev:443"

            let socket: NetworkSocket
            do {
                socket = try NetworkSocket.dial(address, timeout: 20)
            } catch {
                let message = "\(error)"
                // A name that will not resolve, or no route: the runner has no
                // egress. Not a transport failure.
                if message.contains("cannot reach") || message.contains("timed out") {
                    throw XCTSkip("no network egress to \(address): \(message)")
                }
                // Anything else -- a certificate rejection, a misconfigured
                // `sec_protocol_options` -- is the failure this test is for.
                XCTFail(
                    "the TLS dial to \(address) failed in a way that is not a network "
                        + "problem, which is what this test exists to catch: \(message)")
                return
            }
            defer { socket.close() }

            // The handshake completed. Writing a frame and reading the answer
            // must not hang: the peer speaks HTTP, so it will close or reply
            // with something that is not a frame, and either is a legible
            // outcome. A hang here would be the exact symptom the transport bug
            // produced in every other client.
            do {
                try socket.write([0x04, 0x00, 0x00, 0x00, 0x7B, 0x7D, 0x0A, 0x0A])
                _ = try socket.readFully(4)
                // A reply of any kind is fine. An HTTP error page is bytes.
            } catch {
                // A close or a decode failure is the expected shape.
            }
        }

        /// A private CA is refused rather than silently ignored.
        ///
        /// The Swift SDK cannot yet add a root to Network.framework's
        /// evaluation, and the honest response to `THETA_TLS_CA` is to say so.
        /// Ignoring it would connect under a laxer trust policy than the
        /// operator chose — and doing it silently is worse than not supporting
        /// it at all.
        func testAPrivateCAIsRefusedRatherThanIgnored() throws {
            // Set through the platform's own API rather than `setenv`, which
            // Windows Swift does not have. This arm only compiles on Apple
            // platforms, but keeping the helper portable means the file does
            // not grow a second way of doing the same thing later.
            setEnvironment("THETA_TLS_CA", "/tmp/some-ca.pem")
            defer { setEnvironment("THETA_TLS_CA", nil) }

            XCTAssertThrowsError(
                try NetworkSocket.dial("thetabase-control.fly.dev:443", timeout: 5)
            ) { error in
                XCTAssertTrue(
                    "\(error)".contains("THETA_TLS_CA"),
                    "the refusal does not name the variable that caused it, so an "
                        + "operator cannot tell why their CA was not used: \(error)")
            }
        }

    #else

        /// The unsupported platform refuses, and never downgrades.
        ///
        /// The one thing that must not happen off Apple platforms is a
        /// plaintext fallback: it would put a session token on the open
        /// internet, and it would not even connect, because a shared address is
        /// routed by the name in the TLS handshake.
        func testAnUnsupportedPlatformRefusesRatherThanDowngrading() throws {
            XCTAssertThrowsError(
                try NetworkSocket.dial("thetabase-control.fly.dev:443", timeout: 5)
            ) { error in
                let message = "\(error)"
                XCTAssertTrue(
                    message.contains("swift-nio-ssl") || message.contains("no TLS transport"),
                    "the refusal does not say what is missing or what would fix it: "
                        + message)
            }
        }

    #endif
}

/// Set or clear an environment variable.
///
/// Scoped to the platforms that have `Network`, which are the Apple ones, and
/// they all have POSIX `setenv`. Windows Swift has neither, and putting the
/// helper behind the same condition as its only caller is cheaper than a
/// `WinSDK` import for an arm that never compiles there.
///
/// Only the TLS tests need this at all. The address rules take their override
/// as a parameter precisely so they do not touch process state.
#if canImport(Network)
    private func setEnvironment(_ name: String, _ value: String?) {
        switch value {
        case let .some(value): setenv(name, value, 1)
        case .none: unsetenv(name)
        }
    }
#endif
