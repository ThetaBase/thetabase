//! Emits the Ruby wire surface.
//!
//! **`Data.define` rather than `Struct` or a plain class.** `Data` is immutable
//! and has no setters at all, which is the same reason the Java and C# emitters
//! use records: a wire type with a setter is a wire type somebody mutates after
//! validation. `Struct` would give writers for free.
//!
//! **Names are snake_case here and camelCase on the wire, and the mapping is
//! carried rather than derived.** Each type holds its own frozen `wire` map and
//! gets `to_wire`/`from_wire` from it. Deriving the wire name from the Ruby one would
//! mean re-implementing `snake_case` in reverse, and `writeVolumeMB` is exactly
//! the case where that stops being reversible.
//!
//! **Enums are frozen string constants, not a type.** Ruby has no enums, and the
//! options are a constant, a symbol, or a hand-rolled value class. A constant is
//! what a Ruby caller expects and what compares correctly against what the
//! server sends; the value class would buy type safety a dynamic language will
//! not enforce anyway.

use crate::schema::{Enum, FieldType, Schema, Struct};

const HEADER: &str = "\
# frozen_string_literal: true
#
# Generated from crates/theta-proto/schema/theta.capnp by `make sdk`.
#
# DO NOT EDIT. Edit the schema and regenerate; `make sdk-check` fails the build
# if this file and the schema disagree.
#
# The prose explaining each field lives in the schema, which is the one place it
# can be read without a stale copy to compare against.
#
# Field names are snake_case here and camelCase on the wire. Each type carries
# the mapping in its own `wire` method rather than deriving it, because deriving
# it would mean reversing snake_case and `writeVolumeMB` does not survive that.

module ThetaBase
  module Wire
";

const FOOTER: &str = "  end\nend\n";

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
    out.push_str(FOOTER);
    out
}

fn render_enum(e: &Enum) -> String {
    let values: Vec<String> = e.values.iter().map(|v| format!("\"{v}\"")).collect();
    let members: Vec<String> = e
        .values
        .iter()
        .map(|v| format!("      {} = \"{v}\"", screaming(v)))
        .collect();

    format!(
        "    # One of {}.\n    module {}\n{}\n\n      ALL = [{}].freeze\n    end\n",
        values.join(", "),
        e.name,
        members.join("\n"),
        values.join(", ")
    )
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

    let members: Vec<String> = s
        .fields
        .iter()
        .map(|f| format!(":{}", ruby_name(&f.name)))
        .collect();
    let wire: Vec<String> = s
        .fields
        .iter()
        .map(|f| format!("          {}: \"{}\"", ruby_name(&f.name), f.name))
        .collect();
    let types: Vec<String> = s
        .fields
        .iter()
        .map(|f| {
            let ty = match &f.ty {
                FieldType::Group(_) => format!("{}Body", s.name),
                ty => ruby_type(ty, schema),
            };
            format!("    #   {} : {ty}", ruby_name(&f.name))
        })
        .collect();

    // An empty wire type is a real thing here — `ListBranchesRequest`,
    // `StatusRequest` and `OkResponse` all carry nothing — and the naive form of
    // this wrote `WIRE = {\n,\n}`, which does not parse. Handled rather than
    // special-cased away: a request with no fields is still a request.
    out.push_str(&match types.is_empty() {
        true => "    # No fields.\n".to_string(),
        false => format!("    # Fields:\n{}\n", types.join("\n")),
    });
    // `def self.wire` rather than a `WIRE` constant, and that is not a style
    // choice. A constant assigned inside a `Data.define ... do` block binds to
    // the *enclosing* lexical scope, not to the class — so every generated type
    // wrote to one `ThetaBase::Wire::WIRE` and the last one won. Every `to_wire`
    // in the file then used the last type's field list, which surfaced as a
    // `NoMethodError` naming a field belonging to an unrelated message.
    out.push_str(&format!(
        "    {} = Data.define({}) do\n\
         \x20     def self.wire\n\
         \x20       {{{}}}.freeze\n\
         \x20     end\n\n\
         \x20     def to_wire\n\
         \x20       self.class.wire.each_with_object({{}}) {{ |(name, on_wire), out| out[on_wire] = public_send(name) }}\n\
         \x20     end\n\n\
         \x20     def self.from_wire(hash)\n\
         \x20       new(**wire.each_with_object({{}}) {{ |(name, on_wire), out| out[name] = hash[on_wire] }})\n\
         \x20     end\n\
         \x20   end\n",
        s.name,
        members.join(", "),
        match wire.is_empty() {
            true => String::new(),
            false => format!("\n{},\n        ", wire.join(",\n")),
        }
    ));
    out
}

fn render_union(owner: &str, group: &Struct) -> String {
    let arms: Vec<String> = group
        .union_fields
        .iter()
        .map(|f| format!("\"{}\"", f.name))
        .collect();
    let members: Vec<String> = group
        .union_fields
        .iter()
        .map(|f| format!("      {} = \"{}\"", screaming(&f.name), f.name))
        .collect();

    format!(
        "    # `{owner}`'s union: exactly one arm is present, and `kind` says which.\n\
         \x20   module {owner}BodyKind\n{}\n\n      ALL = [{}].freeze\n    end\n\n\
         \x20   {owner}Body = Data.define(:kind, :value) do\n\
         \x20     def to_wire = {{ \"kind\" => kind, \"value\" => value }}\n\
         \x20     def self.from_wire(hash) = new(kind: hash[\"kind\"], value: hash[\"value\"])\n\
         \x20   end\n\n",
        members.join("\n"),
        arms.join(", ")
    )
}

fn ruby_type(ty: &FieldType, schema: &Schema) -> String {
    match ty {
        FieldType::Bool => "Boolean".into(),
        // Ruby integers are arbitrary precision, so the 64-bit split TypeScript
        // needs has no equivalent here — the same reason the Python emitter
        // collapses them.
        FieldType::Int32 | FieldType::Int64 => "Integer".into(),
        FieldType::Float => "Float".into(),
        FieldType::Text => "String".into(),
        FieldType::Data => "String (bytes)".into(),
        FieldType::List(inner) => format!("Array<{}>", ruby_type(inner, schema)),
        FieldType::Named(id) | FieldType::Group(id) => {
            schema.name_of(*id).unwrap_or("Object").to_string()
        }
        // Surfaced rather than dropped: an unmodelled type has to be visible in
        // the output, or the binding quietly loses a field the wire carries.
        FieldType::Unhandled(what) => format!("Object # {what}"),
    }
}

/// Names a generated accessor must not take.
///
/// Ruby keywords, plus the `Object` methods that a `Data` member would silently
/// override. `class` is the sharp one — a field named `class` would give every
/// instance an accessor shadowing `Object#class`, and the failure would surface
/// somewhere else entirely as a `NoMethodError` on something unrelated.
const RUBY_RESERVED: &[&str] = &[
    "alias",
    "and",
    "begin",
    "break",
    "case",
    "class",
    "def",
    "defined",
    "do",
    "else",
    "elsif",
    "end",
    "ensure",
    "false",
    "for",
    "if",
    "in",
    "module",
    "next",
    "nil",
    "not",
    "or",
    "redo",
    "rescue",
    "retry",
    "return",
    "self",
    "super",
    "then",
    "true",
    "undef",
    "unless",
    "until",
    "when",
    "while",
    "yield", // and the Object methods worth protecting
    "class",
    "method",
    "send",
    "freeze",
    "frozen",
    "hash",
    "inspect",
    "display",
    "format",
    "object_id",
    "tap",
    "then",
    "dup",
    "clone",
    "to_s",
    "methods",
];

/// A wire field name as a Ruby accessor.
pub fn ruby_name(name: &str) -> String {
    let snake = snake_case(name);
    match RUBY_RESERVED.contains(&snake.as_str()) {
        true => format!("{snake}_"),
        false => snake,
    }
}

/// `camelCase` to `snake_case`.
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

/// A wire name as a Ruby constant: `autoApply` becomes `AUTO_APPLY`.
pub fn screaming(name: &str) -> String {
    snake_case(name).to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_name_becomes_a_snake_case_accessor() {
        assert_eq!(ruby_name("changeId"), "change_id");
        assert_eq!(ruby_name("from"), "from");
        assert_eq!(ruby_name("rowsAffected"), "rows_affected");
    }

    #[test]
    fn a_name_that_would_shadow_an_object_method_is_escaped() {
        // `class` is the sharp one. A `Data` member named `class` gives every
        // instance an accessor shadowing `Object#class`, and the failure surfaces
        // somewhere else entirely as a NoMethodError on something unrelated.
        assert_eq!(ruby_name("class"), "class_");
        assert_eq!(ruby_name("hash"), "hash_");
        assert_eq!(ruby_name("send"), "send_");
        assert_eq!(ruby_name("end"), "end_");
    }

    #[test]
    fn a_constant_is_screaming_snake_case() {
        assert_eq!(screaming("autoApply"), "AUTO_APPLY");
        assert_eq!(screaming("upToDate"), "UP_TO_DATE");
        assert_eq!(screaming("ok"), "OK");
    }
}
