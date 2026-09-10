//! Emits the Swift wire surface.
//!
//! **Almost no `CodingKeys`.** Swift's convention is camelCase and so is the
//! wire's, so for once the two agree and `Codable`'s default synthesis is
//! already right. The generator emits a `CodingKeys` block only when a field
//! name had to be escaped — which keeps the file short and, more importantly,
//! means a reader who sees one knows something unusual happened there.
//!
//! **Backticks rather than a renamed property.** Swift escapes a keyword with
//! `` `default` ``, so the property keeps the wire's own name instead of
//! becoming `default_` the way Python and Ruby must. That is strictly better:
//! there is one fewer name in play, and the `CodingKeys` entry exists to make
//! the equivalence explicit rather than to repair a mismatch.
//!
//! **Enums are `String`-backed.** `enum Gate: String, Codable` gets encoding,
//! decoding and exhaustive `switch` for free, and an unknown value from a newer
//! server is a decode error rather than a silently-wrong case. That last part is
//! a deliberate trade: the alternative — an `unknown(String)` arm — would let a
//! client carry on past a value it does not understand, which is the kind of
//! quiet degradation this project refuses everywhere else.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.
//
// Field names match the wire's, because Swift and the wire are both camelCase.
// A `CodingKeys` block appears only where a name had to be escaped.

import Foundation
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
    out
}

fn render_enum(e: &Enum) -> String {
    let mut out = format!("public enum {}: String, Codable, Sendable {{\n", e.name);
    for value in &e.values {
        out.push_str(&format!("    case {} = \"{value}\"\n", swift_name(value)));
    }
    out.push_str("}\n");
    out
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

    out.push_str(&format!(
        "public struct {}: Codable, Equatable, Sendable {{\n",
        s.name
    ));
    for field in &s.fields {
        let ty = match &field.ty {
            FieldType::Group(_) => format!("{}Body", s.name),
            ty => swift_type(ty, schema),
        };
        out.push_str(&format!(
            "    public let {}: {ty}\n",
            swift_name(&field.name)
        ));
    }

    // A public memberwise initialiser, because Swift's synthesised one is
    // `internal` — a consumer outside this module could decode these types and
    // not construct them, which makes the whole surface read-only for the
    // people it is for.
    let parameters: Vec<String> = s
        .fields
        .iter()
        .map(|field| {
            let ty = match &field.ty {
                FieldType::Group(_) => format!("{}Body", s.name),
                ty => swift_type(ty, schema),
            };
            format!("{}: {ty}", swift_name(&field.name))
        })
        .collect();
    out.push_str(&format!(
        "\n    public init({}) {{\n",
        parameters.join(", ")
    ));
    for field in &s.fields {
        let name = swift_name(&field.name);
        out.push_str(&format!("        self.{name} = {name}\n"));
    }
    out.push_str("    }\n");

    // Only where a name was escaped. Synthesised `CodingKeys` are already right
    // everywhere else, and emitting them anyway would bury the one case that
    // matters in fifty that do not.
    let escaped: Vec<&crate::schema::Field> = s
        .fields
        .iter()
        .filter(|f| swift_name(&f.name) != f.name)
        .collect();
    if !escaped.is_empty() {
        out.push_str("\n    // Only the escaped names need saying; the rest match the wire.\n");
        out.push_str("    private enum CodingKeys: String, CodingKey {\n");
        for field in &s.fields {
            out.push_str(&format!(
                "        case {} = \"{}\"\n",
                swift_name(&field.name),
                field.name
            ));
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    out
}

fn render_union(owner: &str, group: &Struct) -> String {
    let mut arms = String::new();
    for field in &group.union_fields {
        arms.push_str(&format!(
            "    case {} = \"{}\"\n",
            swift_name(&field.name),
            field.name
        ));
    }

    format!(
        "/// `{owner}`'s union: exactly one arm is present, and `kind` says which.\n\
         public enum {owner}BodyKind: String, Codable, Sendable {{\n{arms}}}\n\n\
         public struct {owner}Body: Codable, Equatable, Sendable {{\n\
         \x20   public let kind: {owner}BodyKind\n\
         \x20   /// The arm's payload. `JSONValue` rather than a per-arm associated\n\
         \x20   /// value: the arms carry unrelated shapes, and every other binding\n\
         \x20   /// sends this as an opaque document.\n\
         \x20   public let value: JSONValue?\n\n\
         \x20   public init(kind: {owner}BodyKind, value: JSONValue? = nil) {{\n\
         \x20       self.kind = kind\n\
         \x20       self.value = value\n\
         \x20   }}\n\
         }}\n\n"
    )
}

fn swift_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "Bool".into(),
        FieldType::Int32 => "Int32".into(),
        FieldType::Int64 => "Int64".into(),
        FieldType::Float => "Double".into(),
        // Optional, because a field the server omits has to decode rather than
        // throw. Scalars stay non-optional: capnp gives them a zero default and
        // the wire always carries them.
        FieldType::Text => "String?".into(),
        FieldType::Data => "Data?".into(),
        FieldType::List(inner) => format!("[{}]?", swift_type(inner, schema).trim_end_matches('?')),
        FieldType::Named(id) | FieldType::Group(id) => match schema.name_of(*id) {
            // An enum is a value type with no absent state; a struct may be
            // missing.
            Some(name) if schema.enums.iter().any(|e| e.name == name) => name.to_string(),
            Some(name) => format!("{name}?"),
            None => "JSONValue?".into(),
        },
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("JSONValue? /* {what} */"),
    }
}

/// Swift keywords a wire name could collide with.
///
/// Escaped with backticks rather than renamed, so the property keeps the wire's
/// own name. `default`, `class`, `where` and `in` are all plausible column-ish
/// words; none is in this schema today, and the rule exists so that a schema
/// which adds one produces a file that compiles rather than one that does not.
const SWIFT_KEYWORDS: &[&str] = &[
    "associatedtype",
    "as",
    "break",
    "case",
    "catch",
    "class",
    "continue",
    "default",
    "defer",
    "deinit",
    "do",
    "else",
    "enum",
    "extension",
    "fallthrough",
    "false",
    "fileprivate",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "inout",
    "internal",
    "is",
    "let",
    "nil",
    "operator",
    "private",
    "protocol",
    "public",
    "repeat",
    "rethrows",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "subscript",
    "super",
    "switch",
    "throw",
    "throws",
    "true",
    "try",
    "typealias",
    "var",
    "where",
    "while",
    "Any",
    "Protocol",
    "Type",
];

/// A wire name as a Swift identifier.
pub fn swift_name(name: &str) -> String {
    match SWIFT_KEYWORDS.contains(&name) {
        true => format!("`{name}`"),
        false => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_name_that_is_a_swift_keyword_is_backticked() {
        assert_eq!(swift_name("default"), "`default`");
        assert_eq!(swift_name("where"), "`where`");
        assert_eq!(swift_name("in"), "`in`");
    }

    #[test]
    fn an_ordinary_name_is_left_exactly_as_the_wire_spells_it() {
        // The point of backticks over a rename: Swift and the wire are both
        // camelCase, so for once there is only one name in play. Python and Ruby
        // cannot have this — they need `from_`, and then two names exist.
        for name in ["changeId", "rowsAffected", "from", "value", "writeVolumeMB"] {
            assert_eq!(swift_name(name), name);
        }
    }

    #[test]
    fn a_list_element_is_not_doubly_optional() {
        // `[String?]?` would say the wire can carry a null inside a list, which
        // it cannot — capnp lists have no null element.
        let schema = Schema::default();
        assert_eq!(
            swift_type(&FieldType::List(Box::new(FieldType::Text)), &schema),
            "[String]?"
        );
    }
}
