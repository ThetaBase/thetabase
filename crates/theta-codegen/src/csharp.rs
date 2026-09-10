//! Emits the C# wire surface.
//!
//! **Positional records.** Immutable, structurally equal, and the constructor is
//! the only way to make one — so a wire type cannot be half-built, and cannot
//! acquire a setter that lets somebody mutate it after validation. The same
//! reason the Java emitter uses records.
//!
//! **`[property: JsonPropertyName]` on every component.** C# properties are
//! PascalCase by convention and the wire is camelCase, so unlike Java the two
//! genuinely differ and the attribute is load-bearing on every field rather than
//! only on the escaped ones. `System.Text.Json` has a camelCase naming policy
//! that would cover most of them, and "most" is the problem: a policy silently
//! produces a *different* wrong name for `writeVolumeMB`, where an explicit
//! attribute is either right or absent.
//!
//! **Enums carry `[JsonStringEnumMemberName]`.** The wire spells them
//! `autoApply`; C# spells them `AutoApply`. Without the attribute the enum
//! serialises as an integer, which the server does not read — and that failure
//! looks like a schema mismatch rather than a naming one.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place it
// can be read without a stale copy to compare against.

using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace ThetaBase;
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
    let mut out = format!(
        "[JsonConverter(typeof(JsonStringEnumConverter<{}>))]\npublic enum {} {{\n",
        e.name, e.name
    );
    for value in &e.values {
        out.push_str(&format!(
            "    [JsonStringEnumMemberName(\"{value}\")]\n    {},\n",
            pascal_case(value)
        ));
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

    let components: Vec<String> = s
        .fields
        .iter()
        .map(|field| {
            let ty = match &field.ty {
                FieldType::Group(_) => format!("{}Body", s.name),
                ty => cs_type(ty, schema),
            };
            format!(
                "    [property: JsonPropertyName(\"{}\")] {ty} {}",
                field.name,
                pascal_case(&field.name)
            )
        })
        .collect();

    out.push_str(&format!("public sealed record {}(\n", s.name));
    out.push_str(&components.join(",\n"));
    out.push_str("\n);\n");
    out
}

fn render_union(owner: &str, group: &Struct) -> String {
    let mut arms = String::new();
    for field in &group.union_fields {
        arms.push_str(&format!(
            "    [JsonStringEnumMemberName(\"{}\")]\n    {},\n",
            field.name,
            pascal_case(&field.name)
        ));
    }

    format!(
        "/// <summary>`{owner}`'s union: exactly one arm is present, and Kind says which.</summary>\n\
         [JsonConverter(typeof(JsonStringEnumConverter<{owner}BodyKind>))]\n\
         public enum {owner}BodyKind {{\n{arms}}}\n\n\
         public sealed record {owner}Body(\n\
         \x20   [property: JsonPropertyName(\"kind\")] {owner}BodyKind Kind,\n\
         \x20   [property: JsonPropertyName(\"value\")] object? Value\n\
         );\n\n"
    )
}

fn cs_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "bool".into(),
        FieldType::Int32 => "int".into(),
        FieldType::Int64 => "long".into(),
        FieldType::Float => "double".into(),
        // Nullable, because the wire's absent-string and empty-string are the
        // same bytes and C#'s nullable analysis would otherwise promise
        // something the protocol does not.
        FieldType::Text => "string?".into(),
        FieldType::Data => "byte[]?".into(),
        FieldType::List(inner) => format!("IReadOnlyList<{}>?", cs_type(inner, schema)),
        FieldType::Named(id) | FieldType::Group(id) => {
            match schema.name_of(*id) {
                // A named struct is a reference type and may be absent; an enum
                // is a value type and cannot be.
                Some(name) if schema.enums.iter().any(|e| e.name == name) => name.to_string(),
                Some(name) => format!("{name}?"),
                None => "object?".into(),
            }
        }
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("object? /* {what} */"),
    }
}

/// A wire name as a C# identifier.
///
/// Capitalise the first letter and leave the rest, so the mapping stays
/// mechanical and total. C# keywords need no escaping here because every
/// identifier this emits is capitalised and every C# keyword is lowercase —
/// which is the same argument the Go emitter makes, and is asserted rather than
/// assumed.
pub fn pascal_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_name_becomes_a_pascal_case_property() {
        assert_eq!(pascal_case("changeId"), "ChangeId");
        assert_eq!(pascal_case("from"), "From");
        assert_eq!(pascal_case("writeVolumeMB"), "WriteVolumeMB");
    }

    #[test]
    fn a_csharp_keyword_cannot_collide_with_a_property_name() {
        // Every C# keyword is lowercase and every emitted name is capitalised,
        // so the escaping Python needs has no equivalent here. A test rather
        // than a comment, because it stops being true the moment somebody emits
        // an unexported member.
        for keyword in [
            "class",
            "record",
            "int",
            "string",
            "object",
            "namespace",
            "default",
        ] {
            assert!(pascal_case(keyword)
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase()));
        }
    }
}
