//! Emits the Python wire surface.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
# Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
#
# DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
# if this file and the schema disagree.
#
# The prose explaining each field lives in the schema, which is the one place it
# can be read without a stale copy to compare against.
#
# Field names are snake_case here and camelCase on the wire. The mapping is
# mechanical and total, so a name can always be recovered in either direction.

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal, Union
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
    let arms: Vec<String> = e.values.iter().map(|name| format!("\"{name}\"")).collect();
    format!("{} = Literal[{}]\n", e.name, arms.join(", "))
}

fn render_struct(s: &Struct, schema: &Schema) -> String {
    let mut out = String::new();

    // A union arrives as a capnp group. Exactly one arm is present, so it is
    // modelled as a tag plus a payload rather than a record of optionals: a
    // record would let a caller set two arms at once, which the wire cannot
    // represent.
    let union_group = s.fields.iter().find_map(|f| match &f.ty {
        FieldType::Group(id) => schema.groups.get(id),
        _ => None,
    });

    if let Some(group) = union_group {
        let arms: Vec<String> = group
            .union_fields
            .iter()
            .map(|f| format!("\"{}\"", f.name))
            .collect();
        out.push_str(&format!(
            "# Exactly one arm is present; `{}_kind` says which.\n{}Kind = Literal[{}]\n\n",
            py_field(&group.name),
            s.name,
            arms.join(", ")
        ));
    }

    out.push_str("@dataclass(frozen=True)\n");
    out.push_str(&format!("class {}:\n", s.name));

    let mut wrote_field = false;
    for f in &s.fields {
        match &f.ty {
            // Named after the group rather than a bare `kind`/`value` pair.
            //
            // A bare `value` collides with any struct that already has a field
            // of that name, and `value` is the most common field name in this
            // schema. `PutIfRequest` was the first struct to have both: two
            // `value` annotations in one dataclass silently reordered its
            // fields and made the whole generated module fail to import.
            //
            // Prefixing with the group's own name is collision-free by
            // construction, because capnp already forbids two fields of one
            // struct sharing a name. TypeScript reached the same shape from the
            // other direction (`expect: PutIfRequestBody`), so both bindings now
            // name a union after its group.
            FieldType::Group(_) => {
                let group = py_field(&f.name);
                out.push_str(&format!("    {group}_kind: {}Kind\n", s.name));
                out.push_str(&format!("    {group}_value: object | None = None\n"));
            }
            ty => out.push_str(&format!(
                "    {}: {}\n",
                py_field(&f.name),
                py_type(ty, schema)
            )),
        }
        wrote_field = true;
    }
    if !wrote_field {
        out.push_str("    pass\n");
    }
    out
}

fn py_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "bool".into(),
        // Python integers are arbitrary precision, so the 64-bit split that
        // TypeScript needs has no equivalent here.
        FieldType::Int32 | FieldType::Int64 => "int".into(),
        FieldType::Float => "float".into(),
        FieldType::Text => "str".into(),
        FieldType::Data => "bytes".into(),
        FieldType::List(inner) => format!("list[{}]", py_type(inner, schema)),
        FieldType::Named(id) | FieldType::Group(id) => {
            format!("\"{}\"", schema.name_of(*id).unwrap_or("object"))
        }
        FieldType::Unhandled(what) => format!("object  # {what}"),
    }
}

/// Python keywords a wire field could collide with.
///
/// `BranchRequest.from` is the live case: a perfectly good wire name and a
/// syntax error in Python. A trailing underscore is the convention PEP 8 gives
/// for exactly this, and it is what the hand-written SDK already used.
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

/// A wire field name as a Python attribute.
pub fn py_field(name: &str) -> String {
    let snake = snake_case(name);
    match PYTHON_KEYWORDS.contains(&snake.as_str()) {
        true => format!("{snake}_"),
        false => snake,
    }
}

/// `camelCase` to `snake_case`.
///
/// Mechanical and total, so a name can be recovered in either direction — which
/// matters because the wire name is what a debugger and a packet capture show.
pub fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_name_that_is_a_python_keyword_is_escaped() {
        // `BranchRequest.from` is a fine wire name and a syntax error in Python.
        assert_eq!(py_field("from"), "from_");
        assert_eq!(py_field("class"), "class_");
        // And an ordinary name is left alone.
        assert_eq!(py_field("changeId"), "change_id");
    }

    #[test]
    fn wire_names_convert_to_python_names_predictably() {
        assert_eq!(snake_case("changeId"), "change_id");
        assert_eq!(snake_case("rowsAffected"), "rows_affected");
        assert_eq!(snake_case("branch"), "branch");
        assert_eq!(snake_case("writeVolumeMB"), "write_volume_m_b");
    }
}
