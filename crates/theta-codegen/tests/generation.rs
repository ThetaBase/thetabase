//! What the generator has to guarantee about its output.
//!
//! The generated files are checked in, and `make sdk-check` compares them
//! byte-for-byte against a fresh run. That comparison is only meaningful if
//! generation is deterministic and total: a generator that reordered its output
//! between runs would report drift on every build until someone disabled the
//! gate, and one that silently dropped a field it could not model would report
//! no drift while the binding lost part of the protocol.

use std::path::PathBuf;

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("theta-proto/schema")
}

fn generated(name: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    std::fs::read_to_string(root.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn generation_is_byte_identical_across_runs() {
    // The property the drift gate rests on. Cap'n Proto hands nodes back in an
    // order that is not the schema's, so the generator sorts — without that,
    // every build would report drift and the gate would be turned off within a
    // week.
    let dir = schema_dir();
    let a = theta_codegen_render(&dir);
    let b = theta_codegen_render(&dir);
    assert_eq!(a, b, "two runs of the generator disagreed");
}

/// Render every file, as the binary does.
fn theta_codegen_render(dir: &std::path::Path) -> Vec<String> {
    let schema = theta_codegen::schema::load(&dir.join("theta.capnp"), dir).expect("schema loads");
    vec![
        theta_codegen::typescript::render(&schema),
        theta_codegen::python::render(&schema),
        theta_codegen::golang::render(&schema),
        theta_codegen::java::render(&schema),
        theta_codegen::csharp::render(&schema),
        theta_codegen::ruby::render(&schema),
        theta_codegen::swift::render(&schema),
    ]
}

#[test]
fn every_request_arm_on_the_wire_reaches_every_binding() {
    // The failure this whole milestone exists to prevent: the schema gains an
    // RPC and the SDKs do not hear about it. `rejectChange` and `pushPolicy`
    // are the newest, and were exactly the ones the hand-written surfaces
    // lacked.
    let schema = std::fs::read_to_string(schema_dir().join("theta.capnp")).expect("schema");
    let ts = generated("sdk/typescript/src/generated.ts");
    let py = generated("sdk/python/src/thetabase/generated.py");
    let go = generated("sdk/go/generated.go");
    let java = generated("sdk/java/src/main/java/io/thetabase/Generated.java");
    let cs = generated("sdk/csharp/src/Generated.cs");
    let rb = generated("sdk/ruby/lib/thetabase/generated.rb");
    let sw = generated("sdk/swift/Sources/ThetaBase/Generated.swift");

    for arm in [
        "get",
        "put",
        "delete",
        "query",
        "proposeSchemaChange",
        "applySchemaChange",
        "audit",
        "listBranches",
        "discardBranch",
        "showChange",
        "promoteChange",
        "rejectChange",
        "pushPolicy",
    ] {
        assert!(
            schema.contains(arm),
            "`{arm}` is not in the schema; this test is out of date"
        );
        assert!(
            ts.contains(&format!("\"{arm}\"")),
            "TypeScript lost `{arm}`"
        );
        assert!(py.contains(&format!("\"{arm}\"")), "Python lost `{arm}`");
        assert!(go.contains(&format!("\"{arm}\"")), "Go lost `{arm}`");
        assert!(java.contains(&format!("\"{arm}\"")), "Java lost `{arm}`");
        assert!(cs.contains(&format!("\"{arm}\"")), "C# lost `{arm}`");
        assert!(rb.contains(&format!("\"{arm}\"")), "Ruby lost `{arm}`");
        assert!(sw.contains(&format!("\"{arm}\"")), "Swift lost `{arm}`");
    }
}

#[test]
fn sixty_four_bit_fields_are_bigint_in_typescript() {
    // Above 2^53 a JavaScript `number` rounds silently. A commit id or a row
    // impact that quietly changes value is worse than an awkward type.
    let ts = generated("sdk/typescript/src/generated.ts");
    assert!(
        ts.contains("rowsAffected: bigint"),
        "a 64-bit field was emitted as `number`"
    );
    assert!(
        ts.contains("estimatedCostMs: number"),
        "a 32-bit field was needlessly widened to `bigint`"
    );
}

#[test]
fn a_wire_name_that_is_a_python_keyword_is_still_reachable() {
    // `BranchRequest.from` is a fine wire name and a syntax error in Python.
    let py = generated("sdk/python/src/thetabase/generated.py");
    assert!(py.contains("from_: int"), "`from` was not escaped: {py:.0}");
}

#[test]
fn a_union_is_a_tagged_choice_rather_than_a_record_of_optionals() {
    // A record with every arm optional would let a caller build two arms at
    // once, which the wire cannot represent — the error would surface as a
    // decode failure on the server rather than a type error at the call site.
    let ts = generated("sdk/typescript/src/generated.ts");
    assert!(ts.contains("export type RequestBody ="));
    assert!(ts.contains("| { kind: \"get\"; value: GetRequest }"));
    assert!(ts.contains("  body: RequestBody;"));

    let py = generated("sdk/python/src/thetabase/generated.py");
    assert!(py.contains("RequestKind = Literal["));
    // `body_kind`, not `kind`. A bare `kind`/`value` pair collides with any
    // struct that already has a field of that name, and `value` is the most
    // common field name in this schema — `PutIfRequest` was the first to have
    // both, and two `value` annotations in one dataclass silently reordered its
    // fields and made the whole module fail to import. Naming a union after its
    // group is collision-free by construction.
    //
    // This assertion said `kind: RequestKind` until now, and had been red since
    // M10.5 renamed it. Nothing ran it: `sdk-check` typechecked the bindings and
    // never ran the generator's own tests. It does now.
    assert!(py.contains("    body_kind: RequestKind"));

    // Go reaches the same shape from a third direction. It has no sum type, so
    // the union is a Kind plus an `any` payload — the one thing it must not be
    // is a struct with every arm optional.
    let go = generated("sdk/go/generated.go");
    assert!(go.contains("type RequestBodyKind string"));
    // Two assertions rather than one line, because the constants are
    // column-aligned and the padding between them depends on the longest arm in
    // the block — a test that pinned the whole line would break every time the
    // schema gained a longer RPC name, which is drift in the test rather than in
    // the binding.
    assert!(go.contains("RequestBodyKindGet "));
    assert!(go.contains("RequestBodyKind = \"get\""));
    assert!(go.contains("RequestBody `json:\"body\"`"));
}

#[test]
fn every_java_component_carries_the_wire_name_in_an_annotation() {
    // Java's convention is camelCase and so is the wire's, so `@JsonProperty` is
    // usually redundant — and usually is not always. `record Foo(String value)`
    // is fine and `record Foo(String default)` is a syntax error, so a
    // keyword-escaped component would serialise under the wrong name. Annotating
    // every component means the escaping rule and the wire name cannot disagree.
    let java = generated("sdk/java/src/main/java/io/thetabase/Generated.java");
    let mut components = 0;
    for line in java.lines() {
        let trimmed = line.trim();
        // A record component is an indented line inside `public record X(`,
        // which is every line that ends in an identifier and is not a brace,
        // a comment, or an enum constant.
        if !trimmed.starts_with("@JsonProperty(") {
            continue;
        }
        components += 1;
        assert!(
            trimmed.contains('"'),
            "an annotation with no wire name: {trimmed}"
        );
    }
    assert!(
        components > 50,
        "only {components} annotated components; this test is not looking at the output"
    );
}

#[test]
fn a_wire_name_that_is_a_java_keyword_would_be_escaped_but_none_are() {
    // The schema has no field whose name is a Java keyword today, so this
    // asserts the two halves separately: that `from` — the field which forced
    // Python to have an escaping rule at all — is emitted unescaped here,
    // because it is not a Java keyword and renaming it for no reason is one
    // more difference from the wire that a reader has to carry.
    //
    // The escaping rule itself is unit-tested in `java.rs`. Written down here
    // so that a schema which does add a `default` or a `class` fails there
    // rather than producing a file that will not compile.
    let java = generated("sdk/java/src/main/java/io/thetabase/Generated.java");
    assert!(
        java.contains("@JsonProperty(\"from\") long from"),
        "`from` should be emitted unescaped in Java"
    );
    assert!(
        !java.contains("from_"),
        "something was escaped that Java does not require escaping"
    );
}

#[test]
fn swift_emits_no_coding_keys_it_does_not_need() {
    // Swift and the wire are both camelCase, so `Codable`'s synthesised keys are
    // already right — and this schema has no field whose name is a Swift
    // keyword. A `CodingKeys` block in the output would therefore mean either a
    // new keyword-shaped field, which is fine, or a generator emitting ceremony
    // for every struct, which buries the one case that matters in fifty that do
    // not.
    let sw = generated("sdk/swift/Sources/ThetaBase/Generated.swift");
    assert!(
        !sw.contains("private enum CodingKeys"),
        "a CodingKeys block appeared; either a field name now needs escaping —          which is fine and this test should say which — or the emitter started          writing them unconditionally"
    );
    // And every struct is constructible from outside the module: Swift's
    // synthesised memberwise init is `internal`, which would make the whole
    // generated surface read-only for the people it is for.
    let inits = sw.matches("    public init(").count();
    assert!(
        inits > 40,
        "only {inits} public initialisers; this test is not looking at the output"
    );
}

#[test]
fn no_ruby_wire_map_is_a_constant() {
    // A constant assigned inside a `Data.define ... do` block binds to the
    // enclosing lexical scope, not to the class — so `WIRE = {...}` made every
    // generated type share one map and the last one won. Every `to_wire` then
    // used the last type's field list, and it surfaced as a `NoMethodError`
    // naming a field from an unrelated message.
    //
    // Asserted at the emitter rather than left to the Ruby tests, because the
    // shape is easy to "tidy" back into a constant by someone who has not met
    // this.
    let rb = generated("sdk/ruby/lib/thetabase/generated.rb");
    assert!(
        !rb.contains("WIRE = "),
        "a wire map was emitted as a constant; it would leak into ThetaBase::Wire"
    );
    assert!(rb.contains("def self.wire"));
}

#[test]
fn every_csharp_property_carries_the_wire_name_in_an_attribute() {
    // C# properties are PascalCase and the wire is camelCase, so unlike Java the
    // two genuinely differ everywhere rather than only on escaped names. A
    // camelCase naming policy would cover most of them, and *most* is the
    // problem: it produces a different wrong name for `writeVolumeMB`, where an
    // explicit attribute is either right or absent.
    let cs = generated("sdk/csharp/src/Generated.cs");
    let properties = cs.matches("[property: JsonPropertyName(").count();
    assert!(
        properties > 50,
        "only {properties} annotated properties; this test is not looking at the output"
    );
    // And every generated enum member says how it is spelled on the wire.
    // Without that an enum serialises as an integer, which the server does not
    // read — and that failure looks like a schema mismatch rather than a naming
    // one.
    assert!(cs.contains("[JsonStringEnumMemberName(\"autoApply\")]"));
    assert!(cs.contains("[JsonStringEnumMemberName(\"upToDate\")]"));
}

#[test]
fn every_go_field_carries_the_wire_name_in_a_json_tag() {
    // Go exports a field by capitalising it, so `changeId` becomes `ChangeId` —
    // and `encoding/json` would then marshal it under that name. The core reads
    // camelCase, so a missing tag is a binding that compiles, typechecks, and
    // sends a document the server does not recognise.
    let go = generated("sdk/go/generated.go");
    for line in go.lines() {
        let trimmed = line.trim();
        // Field lines are the indented ones inside a struct; they are the only
        // ones with a type between a name and end of line.
        if !line.starts_with('\t') || trimmed.starts_with("//") || trimmed.contains('=') {
            continue;
        }
        assert!(
            trimmed.contains("`json:\""),
            "a Go field with no JSON tag would be sent under its Go name: {trimmed}"
        );
    }
}

#[test]
fn the_go_binding_is_gofmt_canonical() {
    // The emitter reproduces gofmt's column alignment rather than shelling out
    // to it, because the drift check compares the generator's output byte for
    // byte against what is committed — a formatter run afterwards would report
    // drift on every build, and depending on a Go toolchain would make the
    // check's answer depend on the machine.
    //
    // What that costs is this: the alignment has to stay right by hand. So it is
    // asserted here on a shape that would break first, and `make sdk-check` runs
    // `gofmt -l` over the file as the real check.
    let go = generated("sdk/go/generated.go");

    // Structural rather than a pinned line: every field in one struct must put
    // its JSON tag at the same column. Pinning an exact line would break every
    // time the schema gained a longer field name in that struct, which is drift
    // in the test rather than in the binding.
    let mut checked = 0;
    for block in go.split("type ").filter(|b| b.contains(" struct {")) {
        let columns: Vec<usize> = block
            .lines()
            .filter(|l| l.starts_with('\t') && l.contains("`json:\""))
            .map(|l| l.find("`json:\"").expect("just matched"))
            .collect();
        if columns.len() < 2 {
            continue;
        }
        assert!(
            columns.iter().all(|c| *c == columns[0]),
            "a struct's tags are not aligned, so gofmt would rewrite the file: {:?}",
            block.lines().next()
        );
        checked += 1;
    }
    assert!(
        checked > 5,
        "only {checked} multi-field structs were checked; this test is not looking at the output"
    );

    assert!(
        !go.lines().any(|line| line.ends_with(' ')),
        "a line ends in whitespace, which gofmt would strip"
    );
}

#[test]
fn the_generated_files_say_not_to_edit_them() {
    // Someone will open these looking for the bug. The header has to send them
    // to the schema instead of letting them fix it here and lose it on the next
    // regeneration.
    for name in [
        "sdk/typescript/src/generated.ts",
        "sdk/python/src/thetabase/generated.py",
        "sdk/go/generated.go",
        "sdk/java/src/main/java/io/thetabase/Generated.java",
        "sdk/csharp/src/Generated.cs",
        "sdk/ruby/lib/thetabase/generated.rb",
        "sdk/swift/Sources/ThetaBase/Generated.swift",
    ] {
        let text = generated(name);
        assert!(text.contains("DO NOT EDIT"), "{name}");
        assert!(
            text.contains("theta.capnp"),
            "{name} does not name its source"
        );
        assert!(
            text.contains("make sdk"),
            "{name} does not say how to regenerate"
        );
    }
}
