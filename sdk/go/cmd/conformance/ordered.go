package main

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// document is a JSON object that remembers the order its keys arrived in.
//
// Go's `map[string]any` does not, and marshals in sorted key order. The Node and
// Python runners both round-trip the core's output through structures that
// preserve insertion order, so a Go runner using a plain map would print the
// same document with its keys rearranged — and `make conformance` would report a
// disagreement between bindings that agree perfectly.
//
// Worth the ~60 lines because the alternative is weakening the comparison to be
// order-insensitive, and then it would no longer catch a binding that really did
// send its fields in a different order.
type document struct {
	keys   []string
	values map[string]any
}

func newDocument() *document {
	return &document{values: map[string]any{}}
}

func (d *document) set(key string, value any) {
	if _, seen := d.values[key]; !seen {
		d.keys = append(d.keys, key)
	}
	d.values[key] = value
}

func (d *document) get(key string) (any, bool) {
	value, ok := d.values[key]
	return value, ok
}

func (d *document) MarshalJSON() ([]byte, error) {
	var out bytes.Buffer
	out.WriteByte('{')
	for i, key := range d.keys {
		if i > 0 {
			out.WriteByte(',')
		}
		name, err := marshal(key)
		if err != nil {
			return nil, err
		}
		out.Write(name)
		out.WriteByte(':')
		value, err := marshal(d.values[key])
		if err != nil {
			return nil, err
		}
		out.Write(value)
	}
	out.WriteByte('}')
	return out.Bytes(), nil
}

// marshal encodes without Go's default HTML escaping.
//
// `encoding/json` turns `<`, `>` and `&` into `<` and friends, on the
// assumption the output lands in a web page. JavaScript's `JSON.stringify` does
// not, and one of the conformance cases is a value carrying `'; DROP TABLE
// users; --`. Escaping would make the Go runner disagree with the other two over
// punctuation rather than over protocol.
func marshal(value any) ([]byte, error) {
	var out bytes.Buffer
	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	// Encode appends a newline.
	return bytes.TrimRight(out.Bytes(), "\n"), nil
}

// parse reads JSON into `document`s rather than maps, preserving key order and
// leaving numbers as the literals the core wrote.
func parse(data []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	// Numbers stay as written. A commit id past 2^53 would silently round
	// through float64, which is the same trap the TypeScript binding avoids by
	// using bigint — and here it would also reformat the literal and fail the
	// comparison for a reason that has nothing to do with the protocol.
	decoder.UseNumber()

	value, err := parseValue(decoder)
	if err != nil {
		return nil, err
	}
	return value, nil
}

func parseValue(decoder *json.Decoder) (any, error) {
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	return parseFrom(decoder, token)
}

func parseFrom(decoder *json.Decoder, token json.Token) (any, error) {
	delim, isDelim := token.(json.Delim)
	if !isDelim {
		return token, nil
	}

	switch delim {
	case '{':
		out := newDocument()
		for decoder.More() {
			key, err := decoder.Token()
			if err != nil {
				return nil, err
			}
			name, ok := key.(string)
			if !ok {
				return nil, fmt.Errorf("an object key that is not a string: %v", key)
			}
			value, err := parseValue(decoder)
			if err != nil {
				return nil, err
			}
			out.set(name, value)
		}
		_, err := decoder.Token() // closing brace
		return out, err
	case '[':
		// An empty array must marshal as `[]` and not `null`, which is what a
		// nil slice would produce — and the suite has cases that return one.
		out := []any{}
		for decoder.More() {
			value, err := parseValue(decoder)
			if err != nil {
				return nil, err
			}
			out = append(out, value)
		}
		_, err := decoder.Token() // closing bracket
		return out, err
	default:
		return nil, fmt.Errorf("unexpected %v", delim)
	}
}
