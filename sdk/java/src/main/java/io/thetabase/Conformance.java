// Runs the shared conformance suite from Java, against a live thetad.
//
// Prints one JSON document on stdout. Every other runner prints the same
// document from the same cases, and `make conformance` diffs them: bindings
// that each pass their own suite prove nothing about whether they agree.

package io.thetabase;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

public final class Conformance {

    private static final ObjectMapper JSON = new ObjectMapper();

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: Conformance <host:port> <token>");
            System.exit(2);
        }
        Path root = repoRoot();
        JsonNode suite = JSON.readTree(Files.readAllBytes(root.resolve("sdk/conformance/cases.json")));

        try (Scribe core = Scribe.load(
                root.resolve("target/wasm32-unknown-unknown/wasm/theta_scribe_wasm.wasm"))) {

            // `Dial.open` rather than `new Socket(...)`, so the harness
            // exercises the transport a customer actually gets -- TLS, SNI and
            // hostname verification included. Dialling a raw socket here is
            // what let the SDK ship with no TLS at all: every test passed
            // against a local plaintext instance, which is the only kind the
            // harness ever started.
            Socket socket = Dial.open(args[0]);

            ObjectNode report = JSON.createObjectNode();
            ArrayNode results = JSON.createArrayNode();
            ArrayNode queries = JSON.createArrayNode();

            try (Scribe.Connection connection = new Scribe.Connection(core, socket)) {
                // Handshake first: the server refuses anything else until it has one.
                connection.write(core.encodeHello(args[1], "conformance-java"));
                JsonNode welcome = core.decodeWelcome(connection.frame());
                report.put("protocolVersion", welcome.get("protocolVersion").asInt());

                long requestId = 1;
                for (JsonNode testCase : suite.get("cases")) {
                    results.add(exchange(core, connection, testCase, requestId++));
                }
            }

            // The typed builder renders rather than calls, so these need no
            // server. Each binding builds with its own API; the rendered output
            // is compared.
            for (JsonNode testCase : suite.get("queries")) {
                queries.add(render(core, testCase));
            }

            report.set("results", results);
            report.set("queries", queries);
            // A UTF-8 stream rather than `System.out`, which on Windows encodes
            // with the platform codepage. One refusal message contains an
            // em-dash, and the harness compares this output byte for byte
            // against four other runners — so a `System.out.println` here fails
            // the gate over a character, not over the protocol.
            out().println(pretty(report, ""));
        }
    }

    private static ObjectNode exchange(
            Scribe core, Scribe.Connection connection, JsonNode testCase, long requestId) {
        ObjectNode outcome = JSON.createObjectNode();
        outcome.put("name", testCase.get("name").asText());
        try {
            JsonNode response = connection.exchange(testCase.get("request"), requestId, 0);
            outcome.put("status", "ok");
            outcome.set("response", redact(response, testCase.get("redact")));
        } catch (Scribe.ProtocolException e) {
            outcome.put("status", "clientError");
            outcome.put("message", firstLine(e.getMessage()));
        } catch (IOException e) {
            outcome.put("status", "transportError");
            outcome.put("message", firstLine(String.valueOf(e.getMessage())));
        }
        return outcome;
    }

    private static ObjectNode render(Scribe core, JsonNode testCase) {
        ObjectNode outcome = JSON.createObjectNode();
        outcome.put("name", testCase.get("name").asText());
        JsonNode spec = testCase.get("build");

        try {
            Query query = Query.table(spec.get("table").asText());
            for (JsonNode column : orEmpty(spec.get("select"))) {
                query = query.select(column.asText());
            }
            for (JsonNode clause : orEmpty(spec.get("where"))) {
                query = query.where(predicate(
                        clause.get(0).asText(),
                        clause.get(1).asText(),
                        JSON.convertValue(clause.get(2), Object.class)));
            }
            for (JsonNode clause : orEmpty(spec.get("orderBy"))) {
                query = query.orderBy(clause.get(0).asText(), clause.get(1).asBoolean());
            }
            if (spec.has("limit")) {
                query = query.limit(spec.get("limit").asLong());
            }
            if (spec.has("offset")) {
                query = query.offset(spec.get("offset").asLong());
            }
            outcome.put("status", "ok");
            outcome.set("rendered", query.render(core));
        } catch (Scribe.ProtocolException e) {
            outcome.put("status", "clientError");
            outcome.put("message", firstLine(e.getMessage()));
        } catch (RuntimeException e) {
            outcome.put("status", "error");
            outcome.put("message", firstLine(String.valueOf(e.getMessage())));
        }
        return outcome;
    }

    /**
     * Map a case's operator name onto the builder.
     *
     * <p>A switch rather than reflection, which is what the Node and Python
     * runners use. It has one advantage worth keeping: a case naming an operator
     * this SDK does not have fails here by name rather than three frames later.
     */
    private static Query.Predicate predicate(String op, String column, Object value) {
        return switch (op) {
            case "eq" -> Query.eq(column, value);
            case "ne" -> Query.ne(column, value);
            case "lt" -> Query.lt(column, value);
            case "lte" -> Query.lte(column, value);
            case "gt" -> Query.gt(column, value);
            case "gte" -> Query.gte(column, value);
            case "isNull" -> Query.isNull(column);
            case "notNull" -> Query.notNull(column);
            default -> throw new IllegalArgumentException(
                    "the suite names an operator this SDK does not have: \"" + op + "\"");
        };
    }

    /** Blank out values that legitimately differ between runs. */
    private static JsonNode redact(JsonNode response, JsonNode paths) {
        for (JsonNode dotted : orEmpty(paths)) {
            List<String> parts = List.of(dotted.asText().split("\\."));
            JsonNode node = response;
            for (String part : parts.subList(0, parts.size() - 1)) {
                node = node == null ? null : node.get(part);
            }
            if (node instanceof ObjectNode object
                    && object.has(parts.get(parts.size() - 1))) {
                object.put(parts.get(parts.size() - 1), "<redacted>");
            }
        }
        return response;
    }

    private static Iterable<JsonNode> orEmpty(JsonNode node) {
        return node == null || node.isNull() ? List.of() : node;
    }

    /**
     * Print the way `JSON.stringify(value, null, 2)` does.
     *
     * <p>Jackson's `DefaultPrettyPrinter` writes `"key" : value` with a space
     * before the colon and `{ }` for an empty object, where JavaScript writes
     * `"key": value` and `{}`. Every runner's output is compared byte for byte,
     * so the formatting is part of the contract — and configuring Jackson's
     * separators to match turned out to be more code than writing the twenty
     * lines that do it directly.
     *
     * <p>Scalars still go through Jackson, because string escaping is the part
     * that is easy to get subtly wrong.
     */
    private static String pretty(JsonNode node, String indent) {
        String inner = indent + "  ";
        if (node.isObject()) {
            if (node.isEmpty()) {
                return "{}";
            }
            StringBuilder out = new StringBuilder("{\n");
            var fields = node.properties().iterator();
            while (fields.hasNext()) {
                var field = fields.next();
                out.append(inner)
                        .append(scalar(JSON.getNodeFactory().textNode(field.getKey())))
                        .append(": ")
                        .append(pretty(field.getValue(), inner));
                out.append(fields.hasNext() ? ",\n" : "\n");
            }
            return out.append(indent).append("}").toString();
        }
        if (node.isArray()) {
            if (node.isEmpty()) {
                return "[]";
            }
            StringBuilder out = new StringBuilder("[\n");
            for (int i = 0; i < node.size(); i++) {
                out.append(inner).append(pretty(node.get(i), inner));
                out.append(i + 1 < node.size() ? ",\n" : "\n");
            }
            return out.append(indent).append("]").toString();
        }
        return scalar(node);
    }

    private static String scalar(JsonNode node) {
        try {
            return JSON.writeValueAsString(node);
        } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
            throw new IllegalStateException("a scalar that will not serialise", e);
        }
    }

    private static java.io.PrintStream out() {
        return new java.io.PrintStream(
                new java.io.FileOutputStream(java.io.FileDescriptor.out),
                true,
                java.nio.charset.StandardCharsets.UTF_8);
    }

    private static String firstLine(String text) {
        return text == null ? "" : text.split("\n", 2)[0];
    }

    private static Path repoRoot() {
        Path dir = Path.of("").toAbsolutePath();
        while (dir != null) {
            if (Files.isRegularFile(dir.resolve("sdk/conformance/cases.json"))) {
                return dir;
            }
            dir = dir.getParent();
        }
        throw new IllegalStateException("no ThetaBase workspace above the working directory");
    }

    private Conformance() {}
}
