// Dialling `thetad`, with TLS where the address calls for it.
//
// This file exists because of a gap, not a feature. The SDK took a
// `FramedSocket` from the caller and left the dialling to them, and the only
// implementation anywhere -- the conformance harness in this repository -- was
// a hand-rolled loopback socket with no TLS. A provisioned instance is at
// `<app>.fly.dev:443`, where Fly's proxy terminates TLS, and a *shared* IPv4 is
// routed by the SNI name in the handshake: a plaintext connection to that port
// is not merely unencrypted, it cannot be routed to the right app at all.
//
// Leaving that to the caller means asking every user of this SDK to get SNI,
// certificate verification and a private-CA escape hatch right on their own.
// Nobody will, and the failure when they do not is a hang rather than an error.
//
// # Why Network.framework, and what that costs
//
// `NWConnection` is in the platform SDK on every Apple target, terminates TLS
// with the system trust store, and sends SNI without being asked. The
// alternative is `swift-nio-ssl`, which would work on Linux too and would add a
// BoringSSL build to a package whose whole dependency argument is that it needs
// no native library (see `Package.swift` on WasmKit). The package declares
// `.macOS(.v13)` and `.iOS(.v16)` and nothing else, so taking the platform
// framework matches what the package already promises.
//
// On a non-Apple platform `dial` refuses, in as many words, rather than
// returning a plaintext socket. A client that quietly downgraded would put a
// session token on the open internet, and the refusal names swift-nio-ssl as
// what would have to be added.

import Foundation

#if canImport(Network)
    import Network
#endif

extension Scribe {
    /// Whether a connection to this port should be dialled with TLS.
    ///
    /// `THETA_TLS` wins when it is set, so a self-hosted instance behind a
    /// terminating proxy on some other port can say so -- and so can a
    /// developer tunnelling 443 to a local plaintext process.
    ///
    /// Otherwise: port 443 means TLS. That is the port Fly's proxy listens on
    /// and the port the Control Plane hands out, and it is the only port in this
    /// product's vocabulary that implies a terminator in front of `thetad`.
    /// Local instances are on 7700 and upwards.
    ///
    /// Inferred from the address rather than carried beside it, deliberately,
    /// and identically to every other client. The address travels through
    /// `THETA_ADDRESS` into every SDK; a second variable that had to agree with
    /// it would be a second thing to get wrong, and the symptom of getting it
    /// wrong is a *hang* rather than an error -- a plaintext frame sent to a TLS
    /// listener is read as a ClientHello, found malformed, and discarded.
    public static func wantsTLS(port: UInt16) -> Bool {
        wantsTLS(port: port, override: ProcessInfo.processInfo.environment["THETA_TLS"])
    }

    /// The same rule, with the override supplied rather than read.
    ///
    /// Separate so a test can exercise the rule without mutating process
    /// environment. That is not a convenience: `setenv` does not exist on
    /// Windows Swift, and a test that mutates a global read by every other test
    /// in the binary is the shape that turned a failing assertion into a hung
    /// run in this project's Rust suite.
    public static func wantsTLS(port: UInt16, override: String?) -> Bool {
        switch override?.lowercased() {
        case "1", "true", "require", "yes": return true
        case "0", "false", "off", "no": return false
        default: break
        }
        return port == 443
    }

    /// Split `host:port`, the way every other client splits it.
    ///
    /// From the last colon, not the first: an IPv6 address is full of them and
    /// splitting on the first yields a host of "[2a09" and a port of nonsense.
    public static func splitAddress(_ address: String) throws -> (host: String, port: UInt16) {
        guard let colon = address.lastIndex(of: ":") else {
            throw ProtocolError(
                "`\(address)` is not `host:port`. THETA_ADDRESS is set by `theta exec`; "
                    + "a hand-written value is usually a URL by mistake.")
        }
        let host = String(address[address.startIndex..<colon])
            .replacingOccurrences(of: "[", with: "")
            .replacingOccurrences(of: "]", with: "")
        let tail = address[address.index(after: colon)...].trimmingCharacters(
            in: .whitespaces)
        guard !host.isEmpty, let port = UInt16(tail) else {
            throw ProtocolError("`\(address)` does not end in a port number")
        }
        return (host, port)
    }
}

#if canImport(Network)

    /// A framed connection to `thetad`, plaintext or TLS.
    ///
    /// One class for both, because the transport is a property of the
    /// `NWConnection`'s parameters rather than of the framing -- so choosing it
    /// happens once, in `dial`, and every read and write below is unchanged.
    public final class NetworkSocket: FramedSocket {
        private let connection: NWConnection
        /// Bytes that have arrived and not yet been asked for.
        ///
        /// A framed protocol asks for an exact number of bytes and TCP delivers
        /// whatever it likes, so the two have to be reconciled somewhere.
        private var buffered = [UInt8]()
        private var failure: Error?
        private let lock = NSLock()

        /// Open a connection to `address`, with TLS if the address calls for it.
        ///
        /// Synchronous, like the rest of this SDK's socket surface: `Scribe`'s
        /// exchange is sequential by construction, and an async dial in front of
        /// a blocking protocol would only move the waiting.
        public static func dial(_ address: String, timeout: TimeInterval = 30) throws
            -> NetworkSocket
        {
            let (host, port) = try Scribe.splitAddress(address)
            let endpoint = NWEndpoint.hostPort(
                host: NWEndpoint.Host(host),
                port: NWEndpoint.Port(rawValue: port)!)

            let parameters: NWParameters
            if Scribe.wantsTLS(port: port) {
                let options = NWProtocolTLS.Options()
                // The SNI name, and on a shared address it is what the proxy
                // routes on -- so getting it wrong does not produce a
                // certificate error, it produces a connection to the wrong
                // instance or to none.
                sec_protocol_options_set_tls_server_name(
                    options.securityProtocolOptions, host)
                sec_protocol_options_set_min_tls_protocol_version(
                    options.securityProtocolOptions, .TLSv12)

                // Verification is the system's, against the system trust store,
                // and there is deliberately no option to turn it off. A flag
                // that disables certificate checking is a flag that ends up set
                // in production, and the whole reason this transport exists is
                // that a session token was about to cross the open internet.
                //
                // `THETA_TLS_CA` is refused rather than ignored here. Adding a
                // root to Network.framework's evaluation means replacing the
                // whole verify block with a hand-written `SecTrust` evaluation,
                // and a hand-written one that is subtly wrong is worse than not
                // offering the option -- so a deployment that needs a private CA
                // is told plainly that this SDK cannot do it yet, rather than
                // being silently connected under the wrong trust policy.
                if let ca = ProcessInfo.processInfo.environment["THETA_TLS_CA"],
                    !ca.isEmpty
                {
                    throw ProtocolError(
                        "THETA_TLS_CA is set, and the Swift SDK cannot yet add a "
                            + "private CA to the system trust store. It will not "
                            + "silently connect without it. Reach a self-hosted "
                            + "instance from another SDK, or add the CA to the "
                            + "system keychain so no override is needed.")
                }

                parameters = NWParameters(tls: options, tcp: Self.tcpOptions())
            } else {
                parameters = NWParameters(tls: nil, tcp: Self.tcpOptions())
            }

            let connection = NWConnection(to: endpoint, using: parameters)
            let socket = NetworkSocket(connection: connection)

            let ready = DispatchSemaphore(value: 0)
            var stateError: Error?
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    ready.signal()
                case .failed(let error), .waiting(let error):
                    // `.waiting` too: a TLS failure surfaces there rather than
                    // `.failed`, and waiting for a `.ready` that will never
                    // arrive is the hang this whole file is about.
                    stateError = error
                    ready.signal()
                case .cancelled:
                    stateError = ProtocolError("the connection was cancelled")
                    ready.signal()
                default:
                    break
                }
            }
            connection.start(queue: .global(qos: .userInitiated))

            guard ready.wait(timeout: .now() + timeout) == .success else {
                connection.cancel()
                throw ProtocolError("timed out connecting to \(address)")
            }
            if let error = stateError {
                connection.cancel()
                throw ProtocolError(
                    "cannot reach the instance at \(address): \(error). If this is a "
                        + "provisioned instance, it presents a publicly signed "
                        + "certificate and is reached by hostname -- an IP address "
                        + "cannot say which instance is wanted.")
            }

            socket.receiveLoop()
            return socket
        }

        private static func tcpOptions() -> NWProtocolTCP.Options {
            let tcp = NWProtocolTCP.Options()
            // Nagle off. Every exchange is one small frame and then a wait for
            // the answer, which is the exact shape Nagle delays.
            tcp.noDelay = true
            return tcp
        }

        private init(connection: NWConnection) {
            self.connection = connection
        }

        /// Pull bytes as they arrive, so `readFully` can hand out exact counts.
        private func receiveLoop() {
            connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
                [weak self] data, _, complete, error in
                guard let self else { return }
                self.lock.lock()
                if let data, !data.isEmpty {
                    self.buffered.append(contentsOf: data)
                }
                if let error {
                    self.failure = error
                } else if complete {
                    self.failure = ProtocolError(
                        "the connection closed while a response was outstanding. "
                            + "`thetad` closes a connection it cannot authorise, so the "
                            + "usual cause is an expired or revoked token rather than a "
                            + "network fault.")
                }
                let done = self.failure != nil
                self.lock.unlock()
                if !done {
                    self.receiveLoop()
                }
            }
        }

        public func write(_ bytes: [UInt8]) throws {
            let sent = DispatchSemaphore(value: 0)
            var failed: Error?
            connection.send(
                content: Data(bytes),
                completion: .contentProcessed { error in
                    failed = error
                    sent.signal()
                })
            guard sent.wait(timeout: .now() + 30) == .success else {
                throw ProtocolError("timed out sending a frame")
            }
            if let failed {
                throw ProtocolError("could not send a frame: \(failed)")
            }
        }

        public func readFully(_ count: Int) throws -> [UInt8] {
            let deadline = Date().addingTimeInterval(30)
            while true {
                lock.lock()
                if buffered.count >= count {
                    let out = Array(buffered.prefix(count))
                    buffered.removeFirst(count)
                    lock.unlock()
                    return out
                }
                let failure = self.failure
                lock.unlock()

                if let failure {
                    throw ProtocolError("\(failure)")
                }
                guard Date() < deadline else {
                    throw ProtocolError(
                        "timed out waiting for \(count) bytes from the instance")
                }
                // A short sleep rather than a condition variable: this SDK's
                // exchange is sequential, so there is exactly one waiter and the
                // wait is bounded by the request it is answering.
                Thread.sleep(forTimeInterval: 0.002)
            }
        }

        public func close() {
            connection.cancel()
        }
    }

#else

    /// Not available here, and it says so rather than downgrading.
    ///
    /// `Network.framework` is Apple-only, and the package declares no other
    /// platform. Reaching a provisioned instance from Linux or Windows Swift
    /// needs `swift-nio-ssl`, which would add a BoringSSL build to a package
    /// whose dependency argument is that it needs no native library -- a
    /// decision worth making deliberately rather than as a side effect.
    ///
    /// What must not happen is a plaintext fallback: that would put a session
    /// token on the open internet, and it would not even connect, because a
    /// shared address is routed by the name in the TLS handshake.
    public enum NetworkSocket {
        public static func dial(_ address: String, timeout: TimeInterval = 30) throws
            -> Never
        {
            throw ProtocolError(
                "this platform has no TLS transport in the ThetaBase Swift SDK. "
                    + "Network.framework is Apple-only, and reaching a provisioned "
                    + "instance from Linux or Windows Swift needs swift-nio-ssl. "
                    + "Supply your own FramedSocket, or use another SDK.")
        }
    }

#endif
