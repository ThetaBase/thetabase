//! Emits the Go wire surface.
//!
//! Two things differ from the TypeScript and Python emitters, and both come from
//! Go rather than from the schema.
//!
//! **Every field carries a JSON tag.** Go exports a field by capitalising it, so
//! `changeId` has to become `ChangeId` to be visible outside the package — and
//! `encoding/json` would then marshal it under *that* name. The Scribe core
//! reads camelCase, so the tag is what keeps the wire name authoritative and the
//! Go name a local convenience. Without it the binding would compile, typecheck,
//! and send a document the server does not recognise.
//!
//! **Names are capitalised rather than idiomatised.** `changeId` becomes
//! `ChangeId`, not the `ChangeID` a Go reviewer would write by hand. The rule
//! this emitter follows everywhere is that the mapping between a wire name and a
//! binding name is mechanical and total, so a name can be recovered in either
//! direction — which matters because the wire name is what a packet capture and
//! a server log show. An initialism table would be neither.

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
// Field names are exported (capitalised) here and camelCase on the wire; the
// JSON tag on each field is what keeps the wire name authoritative.

package thetabase
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
    let rows: Vec<Vec<String>> = e
        .values
        .iter()
        .map(|value| {
            vec![
                format!("{}{}", e.name, pascal_case(value)),
                e.name.clone(),
                format!("= \"{value}\""),
            ]
        })
        .collect();

    format!("type {} string\n\nconst (\n{}\n)\n", e.name, aligned(&rows))
}

/// Lay rows out the way `gofmt` does: one leading tab, then each column padded
/// with spaces to the width of the widest cell in it.
///
/// Reproduced here rather than shelling out to `gofmt`, because the drift check
/// compares this generator's output byte for byte against what is committed. A
/// formatter run afterwards would make the committed file differ from what the
/// generator produces, and `--check` would then report drift on every run — and
/// making the check depend on a Go toolchain being installed would make its
/// answer depend on the machine.
fn aligned(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(String::len)
                .max()
                .unwrap_or(0)
        })
        .collect();

    rows.iter()
        .map(|row| {
            let mut line = String::from("\t");
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                // Every column but the last is padded out; the last is not, so
                // no line ends in trailing whitespace.
                if i + 1 < row.len() {
                    line.push_str(&" ".repeat(widths[i] - cell.len() + 1));
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_struct(s: &Struct, schema: &Schema) -> String {
    let mut out = String::new();

    // A union arrives as a capnp group. Exactly one arm is present, so it is a
    // tag plus a payload rather than a record of optionals — a record would let
    // a caller set two arms at once, which the wire cannot represent.
    let union_group = s.fields.iter().find_map(|f| match &f.ty {
        FieldType::Group(id) => schema.groups.get(id),
        _ => None,
    });

    if let Some(group) = union_group {
        out.push_str(&render_union(&s.name, group));
    }

    let rows: Vec<Vec<String>> = s
        .fields
        .iter()
        .map(|field| {
            let ty = match &field.ty {
                FieldType::Group(_) => format!("{}Body", s.name),
                ty => go_type(ty, schema),
            };
            vec![
                pascal_case(&field.name),
                ty,
                format!("`json:\"{}\"`", field.name),
            ]
        })
        .collect();

    out.push_str(&format!("type {} struct {{\n", s.name));
    if !rows.is_empty() {
        out.push_str(&aligned(&rows));
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

fn render_union(owner: &str, group: &Struct) -> String {
    let arms: Vec<Vec<String>> = group
        .union_fields
        .iter()
        .map(|field| {
            vec![
                format!("{owner}BodyKind{}", pascal_case(&field.name)),
                format!("{owner}BodyKind"),
                format!("= \"{}\"", field.name),
            ]
        })
        .collect();

    let body = [
        vec![
            "Kind".to_string(),
            format!("{owner}BodyKind"),
            "`json:\"kind\"`".to_string(),
        ],
        // `any` rather than a generated per-arm type. The arms carry unrelated
        // payloads and Go has no sum type, so the alternatives are an interface
        // with one method per arm — which the core's JSON boundary would not use
        // — or a struct with every arm optional, which is the shape the comment
        // above rules out.
        vec![
            "Value".to_string(),
            "any".to_string(),
            "`json:\"value,omitempty\"`".to_string(),
        ],
    ];

    format!(
        "// {owner}Body is `{owner}`'s union: exactly one arm is present, and\n\
         // Kind says which.\n\
         type {owner}BodyKind string\n\n\
         const (\n{}\n)\n\n\
         type {owner}Body struct {{\n{}\n}}\n\n",
        aligned(&arms),
        aligned(&body)
    )
}

fn go_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "bool".into(),
        // Go has fixed-width integers, so the wire's width is carried exactly
        // rather than widened. Unlike TypeScript there is nothing to work
        // around: an int64 is an int64 and does not silently lose precision.
        FieldType::Int32 => "int32".into(),
        FieldType::Int64 => "int64".into(),
        FieldType::Float => "float64".into(),
        FieldType::Text => "string".into(),
        FieldType::Data => "[]byte".into(),
        FieldType::List(inner) => format!("[]{}", go_type(inner, schema)),
        FieldType::Named(id) | FieldType::Group(id) => {
            schema.name_of(*id).unwrap_or("any").to_string()
        }
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("any /* {what} */"),
    }
}

/// A wire name as an exported Go identifier.
///
/// Capitalise the first letter and leave the rest. Deliberately not an
/// initialism-aware conversion: this has to be reversible, and `ChangeID` is not
/// recoverable to `changeId` without a table of special cases that would then be
/// the thing drifting.
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
    fn a_wire_name_becomes_an_exported_go_name() {
        assert_eq!(pascal_case("changeId"), "ChangeId");
        assert_eq!(pascal_case("from"), "From");
        assert_eq!(pascal_case("writeVolumeMB"), "WriteVolumeMB");
    }

    #[test]
    fn the_conversion_is_reversible() {
        // The property that rules out an initialism table. A wire name has to be
        // recoverable from a Go name, because the wire name is what a packet
        // capture shows and what a server log names in an error.
        for wire in ["changeId", "rowsAffected", "planHash", "from", "value"] {
            let go = pascal_case(wire);
            let back: String = go
                .chars()
                .enumerate()
                .map(|(i, c)| if i == 0 { c.to_ascii_lowercase() } else { c })
                .collect();
            assert_eq!(back, wire);
        }
    }

    #[test]
    fn a_go_keyword_cannot_collide_with_a_field_name() {
        // Every Go keyword is lowercase and every emitted field is capitalised,
        // so the escaping Python needs for `from` has no equivalent here. Stated
        // as a test rather than assumed, because it is the kind of thing that
        // stops being true when someone adds an unexported field.
        for keyword in ["type", "func", "range", "select", "map", "chan", "go"] {
            assert!(pascal_case(keyword)
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase()));
        }
    }
}
