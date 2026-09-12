// Runs the shared conformance suite from Go, against a live thetad.
//
// Prints one JSON document on stdout. The Node and Python runners print the same
// document from the same cases, and `make conformance` diffs all three: three
// SDKs that each pass their own suite prove nothing about whether they agree.
//
// Built as a binary rather than run from source, matching the Node runner's
// reason for importing the built SDK — a suite that passed against sources and
// failed against the published package would be testing the wrong artifact.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	thetabase "github.com/ThetaBase/thetabase/sdk/go"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

type testCase struct {
	Name    string          `json:"name"`
	Request json.RawMessage `json:"request"`
	Redact  []string        `json:"redact"`
}

type queryCase struct {
	Name  string `json:"name"`
	Build struct {
		Table   string   `json:"table"`
		Select  []string `json:"select"`
		Where   [][]any  `json:"where"`
		OrderBy [][]any  `json:"orderBy"`
		Limit   *int     `json:"limit"`
		Offset  *int     `json:"offset"`
	} `json:"build"`
}

type suite struct {
	Cases   []testCase  `json:"cases"`
	Queries []queryCase `json:"queries"`
}

func run() error {
	if len(os.Args) < 3 {
		return fmt.Errorf("usage: conformance <host:port> <token>")
	}
	address, token := os.Args[1], os.Args[2]

	root, err := repoRoot()
	if err != nil {
		return err
	}

	ctx := context.Background()
	wasm, err := os.ReadFile(filepath.Join(root, "target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))
	if err != nil {
		return fmt.Errorf("the Scribe core is not built — run `make wasm`: %w", err)
	}
	core, err := thetabase.LoadCore(ctx, wasm)
	if err != nil {
		return err
	}
	defer core.Close()

	raw, err := os.ReadFile(filepath.Join(root, "sdk/conformance/cases.json"))
	if err != nil {
		return err
	}
	var cases suite
	if err := json.Unmarshal(raw, &cases); err != nil {
		return err
	}

	// `thetabase.Dial` rather than `net.Dial`, so the harness exercises the
	// transport a customer actually gets -- TLS and SNI included. Dialling raw
	// TCP here is what let the SDK ship with no TLS at all: every test passed
	// against a local plaintext instance, which is the only kind the harness
	// ever started.
	socket, err := thetabase.Dial(address)
	if err != nil {
		return err
	}

	// Handshake first: the server refuses anything else until it has one.
	hello, err := core.EncodeHello(token, "conformance-go")
	if err != nil {
		return err
	}
	if err := socket.Write(hello); err != nil {
		return err
	}
	welcomeBody, err := readFrame(core, socket)
	if err != nil {
		return err
	}
	welcome, err := core.DecodeWelcome(welcomeBody)
	if err != nil {
		return err
	}

	results := []any{}
	requestID := uint64(1)
	for _, testCase := range cases.Cases {
		var request any
		if err := json.Unmarshal(testCase.Request, &request); err != nil {
			return err
		}
		results = append(results, exchange(core, socket, testCase, request, requestID))
		requestID++
	}
	socket.Close()

	// The typed builder renders rather than calls, so these cases need no
	// server. Each binding builds with its own API; the rendered output is
	// compared.
	queries := []any{}
	for _, testCase := range cases.Queries {
		queries = append(queries, render(core, testCase))
	}

	report := newDocument()
	report.set("protocolVersion", welcome.ProtocolVersion)
	report.set("results", results)
	report.set("queries", queries)

	out, err := indent(report)
	if err != nil {
		return err
	}
	fmt.Println(string(out))
	return nil
}

func exchange(core *thetabase.ScribeCore, socket thetabase.Socket, testCase testCase, request any, requestID uint64) *document {
	outcome := newDocument()
	outcome.set("name", testCase.Name)

	framed, err := core.Encode(request, requestID, 0)
	if err == nil {
		err = socket.Write(framed)
	}
	var body []byte
	if err == nil {
		body, err = readFrame(core, socket)
	}
	var decoded []byte
	if err == nil {
		decoded, err = core.DecodeRaw(body)
	}
	if err != nil {
		return failure(outcome, err)
	}

	parsed, err := parse(decoded)
	if err != nil {
		return failure(outcome, err)
	}
	outcome.set("status", "ok")
	outcome.set("response", redact(parsed, testCase.Redact))
	return outcome
}

func render(core *thetabase.ScribeCore, testCase queryCase) *document {
	outcome := newDocument()
	outcome.set("name", testCase.Name)

	query, err := build(testCase)
	var rendered []byte
	if err == nil {
		rendered, err = core.RenderQueryRaw(query.Ast())
	}
	if err != nil {
		// The other two runners report a refused render as `clientError` and
		// anything else as `error`; the distinction is the same one
		// ProtocolError draws.
		outcome.set("status", kind(err, "error"))
		outcome.set("message", firstLine(err.Error()))
		return outcome
	}

	parsed, err := parse(rendered)
	if err != nil {
		outcome.set("status", "error")
		outcome.set("message", firstLine(err.Error()))
		return outcome
	}
	outcome.set("status", "ok")
	outcome.set("rendered", parsed)
	return outcome
}

func build(testCase queryCase) (thetabase.Query, error) {
	spec := testCase.Build
	query := thetabase.Table(spec.Table)
	for _, column := range spec.Select {
		query = query.Select(column)
	}
	for _, clause := range spec.Where {
		if len(clause) != 3 {
			return query, fmt.Errorf("a where clause with %d parts", len(clause))
		}
		op, _ := clause[0].(string)
		column, _ := clause[1].(string)
		predicate, err := predicateFor(op, column, clause[2])
		if err != nil {
			return query, err
		}
		query = query.Where(predicate)
	}
	for _, clause := range spec.OrderBy {
		if len(clause) != 2 {
			return query, fmt.Errorf("an orderBy clause with %d parts", len(clause))
		}
		column, _ := clause[0].(string)
		descending, _ := clause[1].(bool)
		query = query.OrderBy(column, descending)
	}
	if spec.Limit != nil {
		query = query.Limit(*spec.Limit)
	}
	if spec.Offset != nil {
		query = query.Offset(*spec.Offset)
	}
	return query, nil
}

// predicateFor maps a case's operator name onto the builder.
//
// A switch rather than reflection, which is what the Node and Python runners use
// (`qb[op]`, `getattr(qb, op)`). The switch is what Go offers, and it has one
// advantage worth keeping: a case naming an operator this SDK does not have
// fails here by name rather than by nil dereference three frames later.
func predicateFor(op, column string, value any) (thetabase.Predicate, error) {
	switch op {
	case "eq":
		return thetabase.Eq(column, value), nil
	case "ne":
		return thetabase.Ne(column, value), nil
	case "lt":
		return thetabase.Lt(column, value), nil
	case "lte":
		return thetabase.Lte(column, value), nil
	case "gt":
		return thetabase.Gt(column, value), nil
	case "gte":
		return thetabase.Gte(column, value), nil
	case "isNull":
		return thetabase.IsNull(column), nil
	case "notNull":
		return thetabase.NotNull(column), nil
	default:
		return thetabase.Predicate{}, fmt.Errorf("the suite names an operator this SDK does not have: %q", op)
	}
}

func failure(outcome *document, err error) *document {
	outcome.set("status", kind(err, "transportError"))
	outcome.set("message", firstLine(err.Error()))
	return outcome
}

// kind separates what the core refused from what the transport did.
//
// The distinction is the one the other two runners draw, and it is worth drawing
// carefully: a caller may retry a transport failure and must not retry a
// protocol one.
func kind(err error, otherwise string) string {
	var protocol *thetabase.ProtocolError
	if errors.As(err, &protocol) {
		return "clientError"
	}
	return otherwise
}

// redact blanks out values that legitimately differ between runs.
func redact(value any, paths []string) any {
	for _, dotted := range paths {
		parts := strings.Split(dotted, ".")
		node, ok := value.(*document)
		if !ok {
			continue
		}
		for _, part := range parts[:len(parts)-1] {
			next, present := node.get(part)
			if !present {
				node = nil
				break
			}
			node, ok = next.(*document)
			if !ok {
				node = nil
				break
			}
		}
		last := parts[len(parts)-1]
		if node != nil {
			if _, present := node.get(last); present {
				node.set(last, "<redacted>")
			}
		}
	}
	return value
}

func indent(value any) ([]byte, error) {
	raw, err := marshal(value)
	if err != nil {
		return nil, err
	}
	var out bytes.Buffer
	// Two spaces, matching `JSON.stringify(x, null, 2)` and `indent=2`.
	if err := json.Indent(&out, raw, "", "  "); err != nil {
		return nil, err
	}
	return out.Bytes(), nil
}

func firstLine(text string) string { return strings.SplitN(text, "\n", 2)[0] }

func readFrame(core *thetabase.ScribeCore, socket thetabase.Socket) ([]byte, error) {
	prefix, err := socket.ReadFull(4)
	if err != nil {
		return nil, err
	}
	length, err := core.BodyLength(prefix)
	if err != nil {
		return nil, err
	}
	return socket.ReadFull(int(length))
}


// repoRoot walks up until it finds the workspace.
func repoRoot() (string, error) {
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "sdk/conformance/cases.json")); err == nil {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("no ThetaBase workspace above the working directory")
		}
		dir = parent
	}
}
