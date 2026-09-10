//! Emits the TypeScript wire surface.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
// Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
//
// DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
// if this file and the schema disagree.
//
// The prose explaining each field lives in the schema, which is the one place
// it can be read without a stale copy to compare against.
//
// 64-bit integers are `bigint`. Above 2^53 a JavaScript `number` silently
// rounds, and a commit id or row impact that quietly changes value would be
// worse than the inconvenience of a distinct type.
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
    let arms: Vec<String> = e
        .values
        .iter()
        .map(|name| format!("  | \"{name}\""))
        .collect();
    format!("export type {} =\n{};\n", e.name, arms.join("\n"))
}

fn render_struct(s: &Struct, schema: &Schema) -> String {
    let mut out = String::new();

    // A union is a discriminated union, not a record: modelling it as a record
    // with every arm optional would let a caller construct two arms at once,
    // which the wire cannot represent.
    let union_group = s.fields.iter().find_map(|f| match &f.ty {
        FieldType::Group(id) => schema.groups.get(id).map(|g| (f.name.clone(), g)),
        _ => None,
    });

    if let Some((field_name, group)) = &union_group {
        out.push_str(&render_union(&s.name, field_name, group, schema));
    }

    out.push_str(&format!("export interface {} {{\n", s.name));
    for field in &s.fields {
        let ty = match &field.ty {
            FieldType::Group(_) => format!("{}Body", s.name),
            ty => ts_type(ty, schema),
        };
        out.push_str(&format!("  {}: {};\n", field.name, ty));
    }
    out.push_str("}\n");
    out
}

fn render_union(owner: &str, field_name: &str, group: &Struct, schema: &Schema) -> String {
    let arms: Vec<String> = group
        .union_fields
        .iter()
        .map(|field| {
            let payload = match &field.ty {
                FieldType::Unhandled(v) if v == "void" => String::new(),
                ty => format!("; value: {}", ts_type(ty, schema)),
            };
            format!("  | {{ kind: \"{}\"{payload} }}", field.name)
        })
        .collect();

    format!(
        "/** `{owner}.{field_name}`: exactly one arm is present. */\nexport type {owner}Body =\n{};\n\n",
        arms.join("\n")
    )
}

fn ts_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "boolean".into(),
        FieldType::Int32 | FieldType::Float => "number".into(),
        FieldType::Int64 => "bigint".into(),
        FieldType::Text => "string".into(),
        FieldType::Data => "Uint8Array".into(),
        FieldType::List(inner) => format!("{}[]", ts_type(inner, schema)),
        FieldType::Named(id) => schema.name_of(*id).unwrap_or("unknown").to_string(),
        // A group outside the body position: name it after its owner, which is
        // the only stable name it has.
        FieldType::Group(id) => schema.name_of(*id).unwrap_or("unknown").to_string(),
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("unknown /* {what} */"),
    }
}
