package thetabase

import (
	"encoding/json"
	"testing"
)

// The bug the conformance harness found on the Go SDK's first run.
//
// `clone` used `append([]string(nil), ...)`, and appending nothing to a nil
// slice yields nil. A nil slice marshals to `null`; an empty one marshals to
// `[]`. The core reads `columns` as a sequence and refuses null, so every query
// with no explicit projection — the common case, `SELECT *` — failed to render
// with a message about line 1 column 31.
//
// Here as a unit test as well as in the harness, because the harness needs a
// live server and a built core and this needs neither. A bug that can only be
// caught by the expensive gate is a bug that gets caught late.
func TestAQueryWithNoProjectionMarshalsColumnsAsAnEmptyList(t *testing.T) {
	encoded, err := json.Marshal(Table("users").Ast())
	if err != nil {
		t.Fatal(err)
	}

	var document map[string]any
	if err := json.Unmarshal(encoded, &document); err != nil {
		t.Fatal(err)
	}
	for _, field := range []string{"columns", "orderBy"} {
		value, present := document[field]
		if !present {
			t.Fatalf("%s is missing from the AST entirely", field)
		}
		if value == nil {
			t.Fatalf("%s marshalled to null; the core reads it as a sequence and refuses null", field)
		}
	}
}

func TestABuilderNeverMutatesWhatItWasDerivedFrom(t *testing.T) {
	// Two queries from one base is the case that matters: `append` on a slice
	// with spare capacity writes in place, so the second `Select` would
	// otherwise overwrite the first's column rather than adding to its own.
	base := Table("users").Select("id")
	left := base.Select("email")
	right := base.Select("name")

	if got := len(base.Ast().Columns); got != 1 {
		t.Fatalf("the base query grew to %d columns", got)
	}
	if got := left.Ast().Columns[1]; got != "email" {
		t.Fatalf("the left branch has %q where email was expected", got)
	}
	if got := right.Ast().Columns[1]; got != "name" {
		t.Fatalf("the right branch has %q where name was expected — one branch overwrote the other", got)
	}
}

func TestRepeatedFiltersAreAnded(t *testing.T) {
	ast := Table("users").
		Where(Eq("email", "a@example.com")).
		Where(Gt("age", 30)).
		Ast()

	if ast.Filter == nil {
		t.Fatal("no filter survived two Where calls")
	}
	if ast.Filter.Kind != "and" {
		t.Fatalf("two filters combined into %q rather than an and — the second replaced the first", ast.Filter.Kind)
	}
	if got := len(ast.Filter.Terms); got != 2 {
		t.Fatalf("the and has %d terms", got)
	}
}

func TestAValueIsNeverPartOfTheQueryShape(t *testing.T) {
	// The property the whole builder exists for. Whatever a caller passes as a
	// value lands in the `value` slot and nowhere else — there is no path from
	// here to the query text, because the text is written by the core from the
	// AST's structure.
	hostile := "'; DROP TABLE users; --"
	predicate := Eq("name", hostile)

	if predicate.Column != "name" || predicate.Value != hostile {
		t.Fatalf("the value did not land in the value slot: %+v", predicate)
	}

	encoded, err := json.Marshal(predicate)
	if err != nil {
		t.Fatal(err)
	}
	var document map[string]any
	if err := json.Unmarshal(encoded, &document); err != nil {
		t.Fatal(err)
	}
	if document["column"] != "name" {
		t.Fatalf("the column moved: %v", document["column"])
	}
	if document["value"] != hostile {
		t.Fatalf("the value was altered on the way out: %v", document["value"])
	}
}

func TestAnAstRoundTripsThroughJsonUnchanged(t *testing.T) {
	// The AST crosses a JSON boundary into the core, so a field the tags do not
	// carry is a field the core never sees. `omitempty` on the wrong field would
	// silently drop a limit of zero, or a filter, and the query would still run.
	limit := 10
	original := Table("users").
		Select("id", "email").
		Where(Or(IsNull("deleted_at"), NotNull("verified_at"))).
		OrderBy("email", true).
		Limit(limit).
		Offset(5).
		Ast()

	encoded, err := json.Marshal(original)
	if err != nil {
		t.Fatal(err)
	}
	var back QueryAst
	if err := json.Unmarshal(encoded, &back); err != nil {
		t.Fatal(err)
	}

	if back.Table != original.Table || len(back.Columns) != 2 || len(back.OrderBy) != 1 {
		t.Fatalf("the shape changed crossing JSON: %+v", back)
	}
	if back.Filter == nil || back.Filter.Kind != "or" || len(back.Filter.Terms) != 2 {
		t.Fatalf("the filter did not survive: %+v", back.Filter)
	}
	if back.Limit == nil || *back.Limit != limit {
		t.Fatalf("the limit did not survive: %+v", back.Limit)
	}
	if !back.OrderBy[0].Descending {
		t.Fatal("descending was dropped — `omitempty` on a bool would silently reverse the sort")
	}
}
