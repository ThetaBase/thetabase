//! Emits the Java wire surface.
//!
//! One file holding one class per wire type, as nested records. Java's usual
//! shape would be one file per public type, which for this schema is fifty-odd
//! files a generator owns — and a directory a generator owns is a directory
//! somebody eventually edits by hand, because nothing about a lone
//! `ChangeDiff.java` says it is machine output. A single `Generated.java` with
//! `DO NOT EDIT` at the top says it on every screen.
//!
//! **Records rather than classes.** A record is immutable, gets its equality and
//! `toString` for free, and cannot acquire a setter — which matters more than it
//! looks: a wire type with a setter is a wire type somebody mutates after
//! validation.
//!
//! **`@JsonProperty` on every component.** The Scribe core reads camelCase and
//! Java's convention is camelCase too, so the annotation is usually redundant.
//! Usually is not always: `record Foo(String value)` is fine, `record
//! Foo(String default)` is a syntax error, and a keyword-escaped component would
//! silently serialise under the wrong name. Annotating everything means the
//! escaping rule and the wire name can never disagree.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.

package io.thetabase;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonValue;
import java.util.List;

/** Every wire type, generated from `theta.capnp`. */
public final class Generated {
    private Generated() {}
";

pub fn render(schema: &Schema) -> String {
    let mut out = String::from(HEADER);

    for e in &schema.enums {
        out.push('\n');
        out.push_str(&render_enum(e));
    }
    for s in &schema.structs {
        out.push('\n');
        out.push_str(&render_struct(s, schema));
    }
    out.push_str("}\n");
    out
}

fn render_enum(e: &Enum) -> String {
    let mut out = format!("    public enum {} {{\n", e.name);
    let arms: Vec<String> = e
        .values
        .iter()
        .map(|value| format!("        {}(\"{value}\")", screaming(value)))
        .collect();
    out.push_str(&arms.join(",\n"));
    out.push_str(";\n\n");
    out.push_str(
        "        private final String wire;\n\n\
         \x20       PLACEHOLDER(String wire) { this.wire = wire; }\n\n\
         \x20       /** The name on the wire, which is not the Java constant. */\n\
         \x20       @JsonValue\n\
         \x20       public String wire() { return wire; }\n\
         \x20   }\n",
    );
    out.replace("PLACEHOLDER", &e.name)
}

fn render_struct(s: &Struct, schema: &Schema) -> String {
    let mut out = String::new();

    // A union arrives as a capnp group. Exactly one arm is present, so it is a
    // tag plus a payload rather than a record of optionals — a record of
    // optionals would let a caller set two arms at once, which the wire cannot
    // represent.
    let union_group = s.fields.iter().find_map(|f| match &f.ty {
        FieldType::Group(id) => schema.groups.get(id),
        _ => None,
    });

    if let Some(group) = union_group {
        out.push_str(&render_union(&s.name, group));
    }

    let components: Vec<String> = s
        .fields
        .iter()
        .map(|field| {
            let ty = match &field.ty {
                FieldType::Group(_) => format!("{}Body", s.name),
                ty => java_type(ty, schema),
            };
            format!(
                "            @JsonProperty(\"{}\") {ty} {}",
                field.name,
                java_name(&field.name)
            )
        })
        .collect();

    out.push_str(&format!("    public record {}(\n", s.name));
    out.push_str(&components.join(",\n"));
    out.push_str("\n    ) {}\n");
    out
}

fn render_union(owner: &str, group: &Struct) -> String {
    let arms: Vec<String> = group
        .union_fields
        .iter()
        .map(|field| format!("            {}(\"{}\")", screaming(&field.name), field.name))
        .collect();

    format!(
        "    /** `{owner}`'s union: exactly one arm is present, and kind says which. */\n\
         \x20   public enum {owner}BodyKind {{\n{}\n;\n\n\
         \x20       private final String wire;\n\n\
         \x20       {owner}BodyKind(String wire) {{ this.wire = wire; }}\n\n\
         \x20       @JsonValue\n\
         \x20       public String wire() {{ return wire; }}\n\
         \x20   }}\n\n\
         \x20   public record {owner}Body(\n\
         \x20           @JsonProperty(\"kind\") {owner}BodyKind kind,\n\
         \x20           @JsonProperty(\"value\") Object value\n\
         \x20   ) {{}}\n\n",
        arms.join(",\n")
    )
}

fn java_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        // Primitives, not their boxes. A `Boolean` can be null and a `boolean`
        // cannot, and a wire field that is absent is a decode error rather than
        // a null the caller has to check for at every use.
        FieldType::Bool => "boolean".into(),
        FieldType::Int32 => "int".into(),
        FieldType::Int64 => "long".into(),
        FieldType::Float => "double".into(),
        FieldType::Text => "String".into(),
        FieldType::Data => "byte[]".into(),
        // `List<T>` needs the boxed element type — Java has no `List<int>`.
        FieldType::List(inner) => format!("List<{}>", boxed(&java_type(inner, schema))),
        FieldType::Named(id) | FieldType::Group(id) => {
            schema.name_of(*id).unwrap_or("Object").to_string()
        }
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("Object /* {what} */"),
    }
}

fn boxed(ty: &str) -> String {
    match ty {
        "boolean" => "Boolean".into(),
        "int" => "Integer".into(),
        "long" => "Long".into(),
        "double" => "Double".into(),
        other => other.to_string(),
    }
}

/// Java keywords a wire field could collide with.
///
/// No field in this schema hits one today — `from`, which forced Python to have
/// an escaping rule at all, is not a Java keyword and is emitted as written.
/// Present anyway, because the alternative to escaping in advance is a schema
/// change that emits a file which will not compile, and the person who makes
/// that change is not the person who would know why.
///
/// A trailing underscore rather than a prefix, matching what Python does for
/// the same reason — and the `@JsonProperty` above it keeps the wire name
/// authoritative either way.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
    "record",
    "var",
    "yield",
];

/// A wire field name as a Java identifier.
pub fn java_name(name: &str) -> String {
    match JAVA_KEYWORDS.contains(&name) {
        true => format!("{name}_"),
        false => name.to_string(),
    }
}

/// A wire name as a Java enum constant: `autoApply` becomes `AUTO_APPLY`.
pub fn screaming(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_name_that_is_a_java_keyword_is_escaped() {
        assert_eq!(java_name("default"), "default_");
        assert_eq!(java_name("class"), "class_");
        assert_eq!(java_name("new"), "new_");
        assert_eq!(java_name("changeId"), "changeId");
    }

    #[test]
    fn from_needs_no_escaping_here_even_though_python_escapes_it() {
        // `BranchRequest.from` is the field that forced the Python emitter to
        // have an escaping rule at all. It is not a Java keyword, so escaping it
        // would produce `from_` in Java for no reason — and a binding whose
        // field names differ from the wire's for no reason is one more thing a
        // reader has to hold. Asserted so nobody copies Python's list wholesale.
        assert_eq!(java_name("from"), "from");
    }

    #[test]
    fn an_enum_constant_is_screaming_snake_case() {
        assert_eq!(screaming("autoApply"), "AUTO_APPLY");
        assert_eq!(screaming("ok"), "OK");
        assert_eq!(screaming("upToDate"), "UP_TO_DATE");
    }

    #[test]
    fn a_list_element_is_boxed_because_java_has_no_list_of_int() {
        assert_eq!(boxed("int"), "Integer");
        assert_eq!(boxed("long"), "Long");
        assert_eq!(boxed("String"), "String");
    }
}
