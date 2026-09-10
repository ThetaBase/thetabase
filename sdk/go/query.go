// The typed query builder.
//
// Chainable in Go, because that is what makes a query pleasant to write. What it
// produces is an AST, and the AST is rendered to SQL-subset source and bound
// parameters by the WebAssembly core — so the same query built in TypeScript or
// Python reaches the server as the same bytes, and the rule that a value never
// becomes query text has one implementation rather than one per language.
//
// Nothing here interpolates. `Where(Eq("name", userInput))` puts `userInput` in
// the parameter map and `$p0` in the text, whatever `userInput` contains.

package thetabase

// CompareOp is a binary comparison. The set is closed on purpose: an open one
// would eventually carry an operator the renderer does not know, and the only
// place left to put it would be the query text.
type CompareOp string

const (
	OpEq  CompareOp = "eq"
	OpNe  CompareOp = "ne"
	OpLt  CompareOp = "lt"
	OpLte CompareOp = "lte"
	OpGt  CompareOp = "gt"
	OpGte CompareOp = "gte"
)

// Predicate is one node of a filter tree.
//
// One struct with a Kind rather than an interface with an implementation per
// arm, because the AST crosses a JSON boundary into the core. An interface would
// need custom marshalling on both sides, and the shape that survives the round
// trip unchanged is the one the other two SDKs already send.
type Predicate struct {
	Kind   string      `json:"kind"`
	Column string      `json:"column,omitempty"`
	Op     CompareOp   `json:"op,omitempty"`
	Value  any         `json:"value,omitempty"`
	Values []any       `json:"values,omitempty"`
	Terms  []Predicate `json:"terms,omitempty"`
	Term   *Predicate  `json:"term,omitempty"`
}

// OrderBy is one sort key.
type OrderBy struct {
	Column     string `json:"column"`
	Descending bool   `json:"descending"`
}

// QueryAst is what the core renders. Exported so a caller can inspect or send
// one without going through the builder.
type QueryAst struct {
	Table   string     `json:"table"`
	Columns []string   `json:"columns"`
	Filter  *Predicate `json:"filter,omitempty"`
	OrderBy []OrderBy  `json:"orderBy"`
	Limit   *int       `json:"limit,omitempty"`
	Offset  *int       `json:"offset,omitempty"`
}

func compare(column string, op CompareOp, value any) Predicate {
	return Predicate{Kind: "compare", Column: column, Op: op, Value: value}
}

// Eq keeps rows where `column` equals `value`.
func Eq(column string, value any) Predicate { return compare(column, OpEq, value) }

// Ne keeps rows where `column` does not equal `value`.
func Ne(column string, value any) Predicate { return compare(column, OpNe, value) }

// Lt keeps rows where `column` is less than `value`.
func Lt(column string, value any) Predicate { return compare(column, OpLt, value) }

// Lte keeps rows where `column` is at most `value`.
func Lte(column string, value any) Predicate { return compare(column, OpLte, value) }

// Gt keeps rows where `column` is greater than `value`.
func Gt(column string, value any) Predicate { return compare(column, OpGt, value) }

// Gte keeps rows where `column` is at least `value`.
func Gte(column string, value any) Predicate { return compare(column, OpGte, value) }

// In keeps rows where `column` is one of `values`.
func In(column string, values []any) Predicate {
	return Predicate{Kind: "in", Column: column, Values: values}
}

// IsNull keeps rows where `column` is null.
func IsNull(column string) Predicate { return Predicate{Kind: "isNull", Column: column} }

// NotNull keeps rows where `column` is not null.
func NotNull(column string) Predicate { return Predicate{Kind: "notNull", Column: column} }

// And keeps rows matching every term.
func And(terms ...Predicate) Predicate { return Predicate{Kind: "and", Terms: terms} }

// Or keeps rows matching any term.
func Or(terms ...Predicate) Predicate { return Predicate{Kind: "or", Terms: terms} }

// Not inverts a term.
func Not(term Predicate) Predicate { return Predicate{Kind: "not", Term: &term} }

// Query is a typed plan under construction.
type Query struct {
	ast QueryAst
}

// Table starts a query against `name`.
func Table(name string) Query {
	return Query{ast: QueryAst{Table: name, Columns: []string{}, OrderBy: []OrderBy{}}}
}

// Select adds columns to the projection.
//
// Every method here returns a new Query rather than mutating the receiver. A
// builder that mutated would make a shared base query change under whoever else
// was holding it — and Go's value semantics do not save it, because the slices
// inside would still be shared.
func (q Query) Select(columns ...string) Query {
	next := q.clone()
	next.ast.Columns = append(next.ast.Columns, columns...)
	return next
}

// Where keeps rows matching `predicate`. Repeated calls are ANDed.
func (q Query) Where(predicate Predicate) Query {
	next := q.clone()
	if next.ast.Filter != nil {
		combined := And(*next.ast.Filter, predicate)
		next.ast.Filter = &combined
	} else {
		next.ast.Filter = &predicate
	}
	return next
}

// OrderBy adds a sort key.
func (q Query) OrderBy(column string, descending bool) Query {
	next := q.clone()
	next.ast.OrderBy = append(next.ast.OrderBy, OrderBy{Column: column, Descending: descending})
	return next
}

// Limit caps the number of rows returned.
func (q Query) Limit(count int) Query {
	next := q.clone()
	next.ast.Limit = &count
	return next
}

// Offset skips rows before returning any.
func (q Query) Offset(count int) Query {
	next := q.clone()
	next.ast.Offset = &count
	return next
}

// Ast returns the plan, for a caller that wants to inspect or send it.
func (q Query) Ast() QueryAst { return q.clone().ast }

// Render renders through the core. Fails if an identifier could carry syntax.
func (q Query) Render(core *ScribeCore) (RenderedQuery, error) {
	return core.RenderQuery(q.ast)
}

// clone deep-copies the parts a later call would otherwise append into.
//
// `append` on a slice with spare capacity writes in place, so two queries
// derived from one base would share — and the second `Select` would overwrite
// the first's column rather than adding to its own.
func (q Query) clone() Query {
	next := q
	// `make` + `copy` rather than `append(nil, ...)`: appending nothing to a nil
	// slice yields nil, and a nil slice marshals to `null` where an empty one
	// marshals to `[]`. The core reads `columns` as a sequence and refuses null,
	// so every query with no explicit projection — the common case — failed to
	// render. Found by the conformance harness on the first Go run, which is
	// exactly the divergence it exists to catch.
	next.ast.Columns = make([]string, len(q.ast.Columns))
	copy(next.ast.Columns, q.ast.Columns)
	next.ast.OrderBy = make([]OrderBy, len(q.ast.OrderBy))
	copy(next.ast.OrderBy, q.ast.OrderBy)
	if q.ast.Filter != nil {
		filter := *q.ast.Filter
		next.ast.Filter = &filter
	}
	if q.ast.Limit != nil {
		limit := *q.ast.Limit
		next.ast.Limit = &limit
	}
	if q.ast.Offset != nil {
		offset := *q.ast.Offset
		next.ast.Offset = &offset
	}
	return next
}
