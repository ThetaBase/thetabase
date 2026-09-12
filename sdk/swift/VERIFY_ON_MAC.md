# Verifying the Swift SDK's TLS on a Mac

CI does this automatically — the `swift-apple` job in `.github/workflows/ci.yml`
runs on `macos-14` and is the authoritative check. This file is for running the
same thing by hand, which is worth doing once before trusting the CI result, and
is the fallback if the macOS runner is unavailable.

## Why a Mac is needed at all

`Sources/ThetaBase/Dial.swift` has two arms:

```swift
#if canImport(Network)
    // NWConnection, TLS, SNI, system trust evaluation.
#else
    // Refuses, and says swift-nio-ssl is what would be needed.
#endif
```

`Network.framework` is Apple-only. Windows and Linux both take the `#else` arm,
so building or testing the package on either compiles **none** of the code that
actually carries traffic. Until the macOS job existed, that arm had been written
and compiled nowhere.

That is not a hypothetical concern. Every SDK in this repository shipped with no
TLS at all — a hosted instance is `<app>.fly.dev:443` behind a proxy that routes
by the SNI name, so a plaintext socket could not reach one even in principle —
and none of it was visible until somebody tried.

## What to run

Requires Xcode command line tools and a Rust toolchain with the
`wasm32-unknown-unknown` target.

```sh
git clone https://github.com/ThetaBase/thetabase.git
cd thetabase

# The Scribe core. `QueryTests` loads it, so the package's tests cannot run
# without it.
make wasm

cd sdk/swift
swift build
swift build --build-tests   # separately, so a compile failure in the
                            # Apple-only arm reads as a build failure
swift test
```

Expect `DialTests` to report **four** tests on macOS, against three on Windows
or Linux. The two extra are the point of the exercise:

| Test | What it establishes |
|---|---|
| `testPort443MeansTLSAndTheOverrideBeatsItBothWays` | The address rule, identical in all eight SDKs. Runs everywhere. |
| `testAnIPv6AddressSplitsAtTheRightColon` | Splitting from the last colon. Runs everywhere. |
| `testATLSDialReachesAPublicHost` | **macOS only.** A real handshake against a public certificate chain, routed by SNI. |
| `testAPrivateCAIsRefusedRatherThanIgnored` | **macOS only.** `THETA_TLS_CA` is refused rather than silently dropped. |

On Windows or Linux the third and fourth are replaced by
`testAnUnsupportedPlatformRefusesRatherThanDowngrading`, which asserts the
`#else` arm throws rather than returning a plaintext socket.

## What a failure means

`testATLSDialReachesAPublicHost` dials `thetabase-control.fly.dev:443` and
asserts the connection reaches `.ready` — meaning the handshake completed and
`Network.framework` was configured correctly. It cannot succeed at the *protocol*
level, because that host speaks HTTP rather than the frame protocol, so the
assertion is about which failure arrives.

- **Skipped** (`no network egress`): the machine cannot reach the host. Not a
  transport problem.
- **Failed** with a certificate or `sec_protocol_options` error: a real bug in
  the Apple arm. This is what the test exists to catch.
- **Hung**: the worst outcome and the one the original bug produced. A plaintext
  frame sent to a TLS listener is read as a ClientHello, found malformed, and
  discarded — so a client that failed to negotiate waits for an answer that
  never comes. If this hangs, `Scribe.wantsTLS` is returning `false` for port
  443.

To point it at a different endpoint:

```sh
THETA_LIVE_ADDRESS=your-app.fly.dev:443 swift test --filter DialTests
```

## The known gap

`THETA_TLS_CA` — adding a deployment's own CA to the trust store — is **not
implemented** in the Swift SDK. It is refused with an explanation rather than
ignored, because ignoring it would connect under a laxer trust policy than the
operator chose.

Implementing it means replacing `Network.framework`'s verify block with a
hand-written `SecTrust` evaluation, and a hand-written one that is subtly wrong
is worse than not offering the option. The other seven SDKs support it. A
deployment that needs a private CA should reach it from one of those, or install
the CA in the system keychain so no override is needed.
