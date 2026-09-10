// swift-tools-version:5.9

import PackageDescription

let package = Package(
    name: "ThetaBase",
    platforms: [.macOS(.v13), .iOS(.v16)],
    products: [
        .library(name: "ThetaBase", targets: ["ThetaBase"]),
        .executable(name: "thetabase-conformance", targets: ["Conformance"]),
    ],
    dependencies: [
        // WasmKit rather than Wasmtime's C bindings: pure Swift, no native
        // library to ship per platform, so the package builds anywhere Swift
        // does — including iOS, where loading a C runtime would not be an
        // option at all. The Scribe core imports nothing, so none of the WASI
        // machinery the alternatives offer is needed. Same reasoning as Go's
        // wazero and Java's Chicory.
        .package(url: "https://github.com/swiftwasm/WasmKit.git", from: "0.1.5"),
    ],
    targets: [
        .target(
            name: "ThetaBase",
            dependencies: [.product(name: "WasmKit", package: "WasmKit")]
        ),
        .executableTarget(name: "Conformance", dependencies: ["ThetaBase"]),
        .testTarget(name: "ThetaBaseTests", dependencies: ["ThetaBase"]),
    ]
)
