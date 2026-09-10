// The typed query builder.
//
// Chainable in Java, because that is what makes a query pleasant to write. What
// it produces is an AST, and the AST is rendered to SQL-subset source and bound
// parameters by the WebAssembly core — so the same query built in TypeScript,
// Python, Go or Rust reaches the server as the same bytes, and the rule that a
// value never becomes query text has one implementation rather than one per
// language.
//
// Nothing here interpolates. `where(eq("name", userInput))` puts `userInput` in
// the parameter map and `$p0` in the text, whatever `userInput` contains.

package io.thetabase;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.JsonNode;
import java.util.ArrayList;
import java.util.List;

/** A typed plan under construction. */
public final class Query {

    /** A binary comparison. */
    public enum Op {
        EQ("eq"),
        NE("ne"),
        LT("lt"),
        LTE("lte"),
        GT("gt"),
        GTE("gte");

        private final String wire;

        Op(String wire) {
            this.wire = wire;
        }

        @com.fasterxml.jackson.annotation.JsonValue
        public String wire() {
            return wire;
        }
    }

    /**
     * One node of a filter tree.
     *
     * <p>One record with a kind rather than a sealed hierarchy with a subtype
     * per arm. The AST crosses a JSON boundary into the core; a hierarchy would
     * need a custom serialiser on this side and a matching reader on the other,
     * and the shape that survives the round trip unchanged is the one the other
     * four SDKs already send.
     */
    @JsonInclude(JsonInclude.Include.NON_NULL)
    public record Predicate(
            @JsonProperty("kind") String kind,
            @JsonProperty("column") String column,
            @JsonProperty("op") Op op,
            @JsonProperty("value") Object value,
            @JsonProperty("values") List<Object> values,
            @JsonProperty("terms") List<Predicate> terms,
            @JsonProperty("term") Predicate term) {}

    /** One sort key. */
    public record Sort(
            @JsonProperty("column") String column,
            @JsonProperty("descending") boolean descending) {}

    /** What the core renders. */
    @JsonInclude(JsonInclude.Include.NON_NULL)
    public record Ast(
            @JsonProperty("table") String table,
            @JsonProperty("columns") List<String> columns,
            @JsonProperty("filter") Predicate filter,
            @JsonProperty("orderBy") List<Sort> orderBy,
            @JsonProperty("limit") Long limit,
            @JsonProperty("offset") Long offset) {}

    private static Predicate compare(String column, Op op, Object value) {
        return new Predicate("compare", column, op, value, null, null, null);
    }

    /** Keep rows where {@code column} equals {@code value}. */
    public static Predicate eq(String column, Object value) {
        return compare(column, Op.EQ, value);
    }

    /** Keep rows where {@code column} does not equal {@code value}. */
    public static Predicate ne(String column, Object value) {
        return compare(column, Op.NE, value);
    }

    /** Keep rows where {@code column} is less than {@code value}. */
    public static Predicate lt(String column, Object value) {
        return compare(column, Op.LT, value);
    }

    /** Keep rows where {@code column} is at most {@code value}. */
    public static Predicate lte(String column, Object value) {
        return compare(column, Op.LTE, value);
    }

    /** Keep rows where {@code column} is greater than {@code value}. */
    public static Predicate gt(String column, Object value) {
        return compare(column, Op.GT, value);
    }

    /** Keep rows where {@code column} is at least {@code value}. */
    public static Predicate gte(String column, Object value) {
        return compare(column, Op.GTE, value);
    }

    /** Keep rows where {@code column} is one of {@code values}. */
    public static Predicate in(String column, List<Object> values) {
        return new Predicate("in", column, null, null, List.copyOf(values), null, null);
    }

    /** Keep rows where {@code column} is null. */
    public static Predicate isNull(String column) {
        return new Predicate("isNull", column, null, null, null, null, null);
    }

    /** Keep rows where {@code column} is not null. */
    public static Predicate notNull(String column) {
        return new Predicate("notNull", column, null, null, null, null, null);
    }

    /** Keep rows matching every term. */
    public static Predicate and(Predicate... terms) {
        return new Predicate("and", null, null, null, null, List.of(terms), null);
    }

    /** Keep rows matching any term. */
    public static Predicate or(Predicate... terms) {
        return new Predicate("or", null, null, null, null, List.of(terms), null);
    }

    /** Invert a term. */
    public static Predicate not(Predicate term) {
        return new Predicate("not", null, null, null, null, null, term);
    }

    private final Ast ast;

    private Query(Ast ast) {
        this.ast = ast;
    }

    /** Start a query against {@code name}. */
    public static Query table(String name) {
        return new Query(new Ast(name, List.of(), null, List.of(), null, null));
    }

    /**
     * Add columns to the projection.
     *
     * <p>Every method here returns a new Query rather than mutating this one. A
     * builder that mutated would make a shared base query change under whoever
     * else was holding it.
     */
    public Query select(String... columns) {
        List<String> next = new ArrayList<>(ast.columns());
        next.addAll(List.of(columns));
        return new Query(new Ast(
                ast.table(), List.copyOf(next), ast.filter(), ast.orderBy(),
                ast.limit(), ast.offset()));
    }

    /** Keep rows matching {@code predicate}. Repeated calls are ANDed. */
    public Query where(Predicate predicate) {
        Predicate filter = ast.filter() == null ? predicate : and(ast.filter(), predicate);
        return new Query(new Ast(
                ast.table(), ast.columns(), filter, ast.orderBy(), ast.limit(), ast.offset()));
    }

    /** Add a sort key. */
    public Query orderBy(String column, boolean descending) {
        List<Sort> next = new ArrayList<>(ast.orderBy());
        next.add(new Sort(column, descending));
        return new Query(new Ast(
                ast.table(), ast.columns(), ast.filter(), List.copyOf(next),
                ast.limit(), ast.offset()));
    }

    /** Cap the number of rows returned. */
    public Query limit(long count) {
        return new Query(new Ast(
                ast.table(), ast.columns(), ast.filter(), ast.orderBy(), count, ast.offset()));
    }

    /** Skip rows before returning any. */
    public Query offset(long count) {
        return new Query(new Ast(
                ast.table(), ast.columns(), ast.filter(), ast.orderBy(), ast.limit(), count));
    }

    /** The plan, for a caller that wants to inspect or send it. */
    public Ast ast() {
        return ast;
    }

    /** Render through the core. Throws if an identifier could carry syntax. */
    public JsonNode render(Scribe core) {
        return core.renderQuery(ast);
    }
}
