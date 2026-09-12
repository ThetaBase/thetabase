module github.com/ThetaBase/thetabase/sdk/go

go 1.25.0

// wazero rather than wasmtime-go or wasmer-go: it is pure Go with no cgo, so
// `go build` works on any platform Go targets and a user installing this SDK
// does not need a C toolchain. The Scribe core imports nothing, so none of the
// host-function machinery the other runtimes offer is needed here.
require github.com/tetratelabs/wazero v1.12.0

require golang.org/x/sys v0.44.0 // indirect
