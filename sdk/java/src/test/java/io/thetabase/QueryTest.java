package io.thetabase;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.file.Files;
import java.nio.file.Path;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/**
 * What the builder has to guarantee, checked against the real core.
 *
 * <p>Against the core rather than against the AST alone: the AST is only
 * interesting because of what the core makes of it, and a test that asserted on
 * JSON shape would pass just as happily if the core rejected every query.
 */
class QueryTest {

    private static Scribe core;

    @BeforeAll
    static void loadCore() throws Exception {
        Path root = Path.of("").toAbsolutePath();
        while (!Files.isRegularFile(root.resolve("sdk/conformance/cases.json"))) {
            root = root.getParent();
        }
        core = Scribe.load(root.resolve("target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"));
    }

    @Test
    void aValueNeverReachesTheQueryText() {
        // The property the whole builder exists for.
        String hostile = "'; DROP TABLE users; --";
        JsonNode rendered = Query.table("users").where(Query.eq("name", hostile)).render(core);

        assertEquals("SELECT * FROM users WHERE name = $p0", rendered.get("sql").asText());
        assertFalse(rendered.get("sql").asText().contains("DROP"), "the value reached the text");
        assertEquals("\"" + hostile + "\"", rendered.get("params").get("p0").asText());
    }

    @Test
    void repeatedFiltersAreAndedRatherThanReplaced() {
        JsonNode rendered = Query.table("users")
                .where(Query.eq("email", "a@example.com"))
                .where(Query.gt("age", 30))
                .render(core);
        assertTrue(rendered.get("sql").asText().contains(" AND "),
                "the second filter replaced the first: " + rendered.get("sql").asText());
        assertEquals(2, rendered.get("params").size());
    }

    @Test
    void aBuilderNeverMutatesWhatItWasDerivedFrom() {
        // Two queries from one base is the case that matters. The records are
        // immutable and the lists copied, so this cannot regress silently — but
        // "cannot" is what the Go SDK's slices looked like too.
        Query base = Query.table("users").select("id");
        Query left = base.select("email");
        Query right = base.select("name");

        assertEquals(1, base.ast().columns().size());
        assertEquals("SELECT id, email FROM users", left.render(core).get("sql").asText());
        assertEquals("SELECT id, name FROM users", right.render(core).get("sql").asText());
    }

    @Test
    void anIdentifierThatCouldCarrySyntaxIsRefused() {
        // Refused rather than escaped. Escaping would mean deciding what is safe
        // to quote, and the safe answer is that a name needing quotes is not a
        // name this subset accepts.
        assertThrows(Scribe.ProtocolException.class,
                () -> Query.table("users; DROP TABLE x").render(core));
    }

    @Test
    void aQueryWithNoProjectionSelectsEverything() {
        // The Go SDK shipped this broken: an empty slice can be nil there and
        // marshals to `null`, which the core reads as a missing sequence. Java's
        // `List.of()` cannot be null, but the case is worth pinning across every
        // binding rather than in the one that got it wrong.
        assertEquals("SELECT * FROM users", Query.table("users").render(core).get("sql").asText());
    }

    @Test
    void anEmptyProjectionSerialisesAsAListRatherThanNull() {
        JsonNode ast = new ObjectMapper().valueToTree(Query.table("users").ast());
        assertNotNull(ast.get("columns"));
        assertTrue(ast.get("columns").isArray(), "columns is not an array: " + ast.get("columns"));
        assertTrue(ast.get("orderBy").isArray());
    }
}
