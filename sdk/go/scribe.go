// The host half of Scribe: sockets, and nothing else.
//
// Every decision about the protocol — how a request is encoded, what a response
// means, when a cached read must be dropped — lives in the WebAssembly core
// (`crates/theta-scribe-wasm`). This file moves bytes.
//
// The split is deliberate, and it is the reason a third SDK is cheap. A Go SDK
// that built wire messages in Go would be a third implementation of the
// protocol, and the first time the schema moved two of the three would be
// wrong. What must be identical across languages is the protocol; what must
// differ is I/O, because WebAssembly has no sockets and the host runtimes do not
// agree on what a socket is.

package thetabase

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"

	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
)

// `[u32 length][u8 tag][bytes]`, as `abi.rs` lays it out.
const (
	resultHeader      = 5
	lengthPrefixBytes = 4
)

// noLength is what the core returns from theta_body_length for a frame this
// protocol does not allow.
const noLength = 0xffffffff

// ProtocolError is an error the core rejected, kept distinct from a transport
// failure.
//
// The distinction is the whole point: a caller can retry a transport failure and
// must not retry a protocol one, and collapsing them into a single error type
// would turn "you sent something invalid" into "try again", forever.
type ProtocolError struct {
	Message string
}

func (e *ProtocolError) Error() string { return e.Message }

// ScribeCore is the WebAssembly core, loaded once per process.
//
// Safe for concurrent use: the core owns a single linear memory and every call
// allocates into it, so two goroutines encoding at once would interleave
// allocations against one heap. The mutex is not a performance compromise being
// tolerated — encoding is microseconds and a connection is sequential anyway.
type ScribeCore struct {
	runtime wazero.Runtime
	module  api.Module
	ctx     context.Context
	mu      sync.Mutex

	alloc       api.Function
	free        api.Function
	freeResult  api.Function
	encode      api.Function
	decode      api.Function
	bodyLength  api.Function
	invalidate  api.Function
	encodeHello api.Function
	decodeHello api.Function
	renderQuery api.Function
}

// LoadCore instantiates the core from `wasm`.
//
// Pass nil to look for the module next to the SDK, which is what a developer in
// this repository wants and what a published package would embed.
func LoadCore(ctx context.Context, wasm []byte) (*ScribeCore, error) {
	if wasm == nil {
		found, err := defaultModule()
		if err != nil {
			return nil, err
		}
		wasm = found
	}

	runtime := wazero.NewRuntime(ctx)
	// No host imports: the core is pure computation, which is why it loads
	// identically here, in Node, in a browser and in an edge runtime.
	module, err := runtime.Instantiate(ctx, wasm)
	if err != nil {
		runtime.Close(ctx)
		return nil, fmt.Errorf("the Scribe core did not instantiate: %w", err)
	}

	core := &ScribeCore{runtime: runtime, module: module, ctx: ctx}
	for name, into := range map[string]*api.Function{
		"theta_alloc":          &core.alloc,
		"theta_free":           &core.free,
		"theta_free_result":    &core.freeResult,
		"theta_encode":         &core.encode,
		"theta_decode":         &core.decode,
		"theta_body_length":    &core.bodyLength,
		"theta_invalidation":   &core.invalidate,
		"theta_encode_hello":   &core.encodeHello,
		"theta_decode_welcome": &core.decodeHello,
		"theta_render_query":   &core.renderQuery,
	} {
		fn := module.ExportedFunction(name)
		if fn == nil {
			runtime.Close(ctx)
			// Named rather than deferred to a nil dereference at the first call:
			// a core built from a different revision is a real failure mode, and
			// "theta_render_query is missing" says which revision.
			return nil, fmt.Errorf("the Scribe core does not export %s — it was built from a different revision of the schema", name)
		}
		*into = fn
	}
	return core, nil
}

// Close releases the runtime.
func (c *ScribeCore) Close() error { return c.runtime.Close(c.ctx) }

// call copies `input` into the core's memory, invokes `fn`, and returns the
// result buffer.
func (c *ScribeCore) call(fn api.Function, input []byte, extra ...uint64) ([]byte, error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	length := uint64(len(input))
	allocated, err := fn0(c.ctx, c.alloc, length)
	if err != nil {
		return nil, err
	}
	defer c.free.Call(c.ctx, allocated, length)

	if !c.module.Memory().Write(uint32(allocated), input) {
		return nil, fmt.Errorf("the core's memory is smaller than the %d bytes just allocated in it", len(input))
	}

	ptr, err := fn0(c.ctx, fn, append([]uint64{allocated, length}, extra...)...)
	if err != nil {
		return nil, err
	}
	return c.take(uint32(ptr))
}

// take reads a result buffer out of the core's memory and frees it.
//
// The tag byte distinguishes a result from a refusal. Reading the payload before
// checking it would be the same mistake as parsing a request before
// authenticating it: the refusal path must not depend on the success path having
// been valid.
func (c *ScribeCore) take(ptr uint32) ([]byte, error) {
	memory := c.module.Memory()
	header, ok := memory.Read(ptr, resultHeader)
	if !ok {
		return nil, fmt.Errorf("the core returned a result outside its own memory")
	}
	length := binary.LittleEndian.Uint32(header[:4])
	tag := header[4]

	payload, ok := memory.Read(ptr+resultHeader, length)
	if !ok {
		return nil, fmt.Errorf("the core returned a result of %d bytes that does not fit in its memory", length)
	}
	// Copied before the free: `Read` hands back a view into linear memory, and
	// the next allocation would write over it.
	out := make([]byte, len(payload))
	copy(out, payload)

	if _, err := c.freeResult.Call(c.ctx, uint64(ptr)); err != nil {
		return nil, err
	}
	if tag != 0 {
		return nil, &ProtocolError{Message: string(out)}
	}
	return out, nil
}

// Encode frames a request.
func (c *ScribeCore) Encode(request any, requestID, branchID uint64) ([]byte, error) {
	body, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	return c.call(c.encode, body, requestID, branchID)
}

// DecodeRaw reads a response body and returns the core's JSON, unparsed.
//
// For a caller that needs the document exactly as the core wrote it — key order
// included. `map[string]any` cannot carry that, because Go marshals a map in
// sorted key order, and the conformance runner compares its output byte for byte
// against two bindings whose maps preserve insertion order.
func (c *ScribeCore) DecodeRaw(body []byte) ([]byte, error) {
	return c.call(c.decode, body)
}

// Decode reads a response body into a generic document.
func (c *ScribeCore) Decode(body []byte) (map[string]any, error) {
	out, err := c.DecodeRaw(body)
	if err != nil {
		return nil, err
	}
	var decoded map[string]any
	// UseNumber: a commit id or a row impact past 2^53 would silently round
	// through float64, which is the same trap the TypeScript binding avoids by
	// using bigint. A number that quietly changes value is worse than an
	// awkward type.
	if err := decodeJSONExact(out, &decoded); err != nil {
		return nil, err
	}
	return decoded, nil
}

// BodyLength reads the four-byte frame prefix.
func (c *ScribeCore) BodyLength(prefix []byte) (uint32, error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	allocated, err := fn0(c.ctx, c.alloc, lengthPrefixBytes)
	if err != nil {
		return 0, err
	}
	defer c.free.Call(c.ctx, allocated, lengthPrefixBytes)

	if !c.module.Memory().Write(uint32(allocated), prefix) {
		return 0, fmt.Errorf("the core's memory is smaller than the frame prefix")
	}
	length, err := fn0(c.ctx, c.bodyLength, allocated)
	if err != nil {
		return 0, err
	}
	if uint32(length) == noLength {
		return 0, &ProtocolError{Message: "the peer sent a length this protocol does not allow"}
	}
	return uint32(length), nil
}

// The server's reply to a handshake is [Welcome], from `generated.go`.
//
// It was written by hand here first, and the compiler refused the redeclaration
// — which was the right answer. A hand-written copy of a wire type is the exact
// drift this SDK's generated half exists to prevent: the TypeScript SDK carried
// one for a while, and its `ChangeDiff` was missing the field that says whether
// confirmation is even a route.

// EncodeHello frames the handshake.
//
// The protocol version is the core's to state, not the host's: a host that could
// name its own version could claim a compatibility it does not have, and the
// negotiation that refuses mismatched versions rather than guessing would be
// negotiating with itself.
func (c *ScribeCore) EncodeHello(token, clientName string) ([]byte, error) {
	body, err := json.Marshal(map[string]string{"token": token, "clientName": clientName})
	if err != nil {
		return nil, err
	}
	return c.call(c.encodeHello, body)
}

// DecodeWelcome reads the server's reply. A refusal is a ProtocolError.
func (c *ScribeCore) DecodeWelcome(body []byte) (Welcome, error) {
	out, err := c.call(c.decodeHello, body)
	if err != nil {
		return Welcome{}, err
	}
	var welcome Welcome
	return welcome, json.Unmarshal(out, &welcome)
}

// RenderedQuery is a query as text with placeholders, and the values they stand
// for. Nothing here ever becomes query text.
type RenderedQuery struct {
	SQL    string            `json:"sql"`
	Params map[string]string `json:"params"`
}

// RenderQuery renders a typed query AST to SQL-subset source and bound
// parameters.
//
// In the core rather than in this file, so a query built the same way in
// TypeScript renders to the same bytes — and so the rule that a value never
// becomes query text has one implementation rather than one per language.
func (c *ScribeCore) RenderQuery(ast any) (RenderedQuery, error) {
	out, err := c.RenderQueryRaw(ast)
	if err != nil {
		return RenderedQuery{}, err
	}
	var rendered RenderedQuery
	return rendered, json.Unmarshal(out, &rendered)
}

// RenderQueryRaw renders and returns the core's JSON, unparsed. See
// [ScribeCore.DecodeRaw] for why the unparsed form is worth having.
func (c *ScribeCore) RenderQueryRaw(ast any) ([]byte, error) {
	body, err := json.Marshal(ast)
	if err != nil {
		return nil, err
	}
	return c.call(c.renderQuery, body)
}

// Invalidation says what a request invalidates: one key, everything, or nothing.
type Invalidation struct {
	Key *string `json:"key,omitempty"`
	All *bool   `json:"all,omitempty"`
}

// Invalidation asks the core what a request invalidates.
func (c *ScribeCore) Invalidation(request any) (Invalidation, error) {
	body, err := json.Marshal(request)
	if err != nil {
		return Invalidation{}, err
	}
	out, err := c.call(c.invalidate, body)
	if err != nil {
		return Invalidation{}, err
	}
	var invalidation Invalidation
	return invalidation, json.Unmarshal(out, &invalidation)
}

// Socket is a framed connection to `thetad`.
type Socket interface {
	Write(bytes []byte) error
	// ReadFull returns exactly n bytes, or an error if the peer closes first.
	ReadFull(n int) ([]byte, error)
	Close() error
}

// Exchange performs one request/response exchange over a socket.
//
// Sequential by construction: a caller that needs concurrency opens more
// connections. Multiplexing on one socket would need request-id correlation on
// the read side, and correlating replies to the wrong request is a
// data-corruption bug rather than a performance one.
func Exchange(core *ScribeCore, socket Socket, request any, requestID, branchID uint64) (map[string]any, error) {
	framed, err := core.Encode(request, requestID, branchID)
	if err != nil {
		return nil, err
	}
	if err := socket.Write(framed); err != nil {
		return nil, err
	}

	prefix, err := socket.ReadFull(lengthPrefixBytes)
	if err != nil {
		return nil, err
	}
	length, err := core.BodyLength(prefix)
	if err != nil {
		return nil, err
	}
	body, err := socket.ReadFull(int(length))
	if err != nil {
		return nil, err
	}
	return core.Decode(body)
}

// fn0 calls a wasm function that returns exactly one value.
func fn0(ctx context.Context, fn api.Function, args ...uint64) (uint64, error) {
	results, err := fn.Call(ctx, args...)
	if err != nil {
		return 0, err
	}
	if len(results) != 1 {
		return 0, fmt.Errorf("the core returned %d values where one was expected", len(results))
	}
	return results[0], nil
}

// decodeJSONExact unmarshals without turning large integers into float64.
func decodeJSONExact(data []byte, into any) error {
	decoder := json.NewDecoder(newReader(data))
	decoder.UseNumber()
	return decoder.Decode(into)
}

func newReader(data []byte) io.Reader { return &sliceReader{data: data} }

type sliceReader struct {
	data []byte
	at   int
}

func (r *sliceReader) Read(p []byte) (int, error) {
	if r.at >= len(r.data) {
		return 0, io.EOF
	}
	n := copy(p, r.data[r.at:])
	r.at += n
	return n, nil
}

// defaultModule looks for the core next to the SDK, then in the repository's
// build output. Reached only when the caller did not pass a module.
func defaultModule() ([]byte, error) {
	candidates := []string{
		"theta_scribe_wasm.wasm",
		filepath.Join("..", "..", "target", "wasm32-unknown-unknown", "wasm", "theta_scribe_wasm.wasm"),
	}
	for _, candidate := range candidates {
		if bytes, err := os.ReadFile(candidate); err == nil {
			return bytes, nil
		}
	}
	return nil, fmt.Errorf("the Scribe core was not found next to the SDK — run `make wasm` to build it, or pass the module to LoadCore")
}
