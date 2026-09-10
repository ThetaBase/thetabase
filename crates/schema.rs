//! Reading `theta.capnp` into a shape a generator can walk.
//!
//! The schema is compiled by the same embedded Cap'n Proto compiler that
//! produces the Rust bindings, and this reads the IR that compilation emits. One
//! compilation, one IR, every binding — which is the whole claim
//! `02-api-wire-protocol.md` §3 makes about generated SDKs staying in lockstep.
//! A hand-written parser here would be a second reading of the schema, and two
//! readings are exactly how bindings drift from the wire.

use anyhow::{anyhow, Context, Result};
use capnp::schema_capnp::{field, node, type_ as capnp_type};
use std::collections::HashMap;

/// A scalar or composite type, as far as a generator needs to care.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Bool,
    /// Fits in a JS `number` without loss: 32 bits or fewer.
    Int32,
    /// Needs `bigint` in TypeScript — beyond 2^53 a JS `number` silently
    /// rounds, and a commit count or a row impact that quietly changes value is
    /// worse than an awkward type.
    Int64,
    Float,
    Text,
    Data,
    /// Named struct or enum, by its schema id.
    Named(u64),
    List(Box<FieldType>),
    /// A capnp group. An anonymous union inside a struct arrives this way, and
    /// it is how the wire's `Request`/`Response` bodies are shaped — so a
    /// generator that skipped groups would emit a body field of no useful type.
    Group(u64),
    /// A field the generator does not model. Carried rather than dropped so an
    /// unsupported type is a visible `unknown` instead of a missing field.
    Unhandled(String),
}

#[derive(Debug, Clone)]
pub struct Field {
    /// As written in the schema: camelCase.
    pub name: String,
    pub ty: FieldType,
    /// Which union arm this field belongs to. `None` for an ordinary field.
    ///
    /// Read by [`read_fields`] to sort fields into the two lists; kept on the
    /// field because a generator emitting arms in wire order needs the tag, and
    /// dropping it would mean re-deriving it from position.
    #[allow(dead_code)]
    pub union_tag: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Struct {
    /// The schema id, which is how fields refer to this node.
    #[allow(dead_code)]
    pub id: u64,
    pub name: String,
    /// Fields outside any union.
    pub fields: Vec<Field>,
    /// Fields inside the struct's anonymous union, if it has one.
    pub union_fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub struct Enum {
    #[allow(dead_code)]
    pub id: u64,
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Schema {
    pub structs: Vec<Struct>,
    pub enums: Vec<Enum>,
    /// Every named node, so a field referring to one can be resolved.
    pub names: HashMap<u64, String>,
    /// Groups, by id. Not emitted as types of their own — they are expanded
    /// into the struct that contains them.
    pub groups: HashMap<u64, Struct>,
}

impl Schema {
    pub fn name_of(&self, id: u64) -> Option<&str> {
        self.names.get(&id).map(|s| s.as_str())
    }
}

/// Compile `path` and read the result.
pub fn load(path: &std::path::Path, src_prefix: &std::path::Path) -> Result<Schema> {
    let ir = capnpc_embedded::CompileCommand::new()
        .src_prefix(src_prefix)
        .file(path)
        .compile()
        .map_err(|e| anyhow!("{path:?} failed to compile: {e}"))?;

    let message = capnp::serialize::read_message_from_flat_slice(
        &mut ir.as_slice(),
        capnp::message::ReaderOptions::new(),
    )
    .context("the compiler's output is not a Cap'n Proto message")?;

    let request: capnp::schema_capnp::code_generator_request::Reader = message
        .get_root()
        .context("unreadable CodeGeneratorRequest")?;

    let mut schema = Schema::default();
    let nodes: Vec<node::Reader> = request.get_nodes()?.iter().collect();

    // Names first: a field can refer to a node declared later in the file.
    for node in &nodes {
        if let Some(name) = short_name(node)? {
            schema.names.insert(node.get_id(), name);
        }
    }

    for node in &nodes {
        let Some(name) = schema.name_of(node.get_id()).map(str::to_string) else {
            continue;
        };
        match node.which()? {
            node::Struct(s) => {
                // Groups are how capnp models a named union; the anonymous
                // union inside a struct is what the wire protocol uses, so that
                // is what is modelled here.
                let (fields, union_fields) = read_fields(s.get_fields()?)?;
                let parsed = Struct {
                    id: node.get_id(),
                    name,
                    fields,
                    union_fields,
                };
                match s.get_is_group() {
                    true => {
                        schema.groups.insert(node.get_id(), parsed);
                    }
                    false => schema.structs.push(parsed),
                }
            }
            node::Enum(e) => {
                let mut values = Vec::new();
                for value in e.get_enumerants()?.iter() {
                    values.push(value.get_name()?.to_str()?.to_string());
                }
                schema.enums.push(Enum {
                    id: node.get_id(),
                    name,
                    values,
                });
            }
            _ => {}
        }
    }

    // Deterministic output: the generator must produce byte-identical files for
    // an unchanged schema, or the drift check reports a change on every run.
    schema.structs.sort_by(|a, b| a.name.cmp(&b.name));
    schema.enums.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(schema)
}

fn read_fields(
    fields: capnp::struct_list::Reader<field::Owned>,
) -> Result<(Vec<Field>, Vec<Field>)> {
    let mut plain = Vec::new();
    let mut union = Vec::new();

    for f in fields.iter() {
        let name = f.get_name()?.to_str()?.to_string();
        // `noDiscriminant` marks a field outside the struct's union.
        let tag = match f.get_discriminant_value() {
            capnp::schema_capnp::field::NO_DISCRIMINANT => None,
            value => Some(value),
        };

        let ty = match f.which()? {
            field::Slot(slot) => field_type(slot.get_type()?)?,
            // A group inside a union arm: modelled by name so the emitted
            // surface still carries the arm, rather than silently dropping it.
            field::Group(g) => FieldType::Group(g.get_type_id()),
        };

        let field = Field {
            name,
            ty,
            union_tag: tag,
        };
        match tag {
            Some(_) => union.push(field),
            None => plain.push(field),
        }
    }
    Ok((plain, union))
}

fn field_type(ty: capnp_type::Reader) -> Result<FieldType> {
    Ok(match ty.which()? {
        capnp_type::Void(()) => FieldType::Unhandled("void".into()),
        capnp_type::Bool(()) => FieldType::Bool,
        capnp_type::Int8(())
        | capnp_type::Int16(())
        | capnp_type::Int32(())
        | capnp_type::Uint8(())
        | capnp_type::Uint16(())
        | capnp_type::Uint32(()) => FieldType::Int32,
        capnp_type::Int64(()) | capnp_type::Uint64(()) => FieldType::Int64,
        capnp_type::Float32(()) | capnp_type::Float64(()) => FieldType::Float,
        capnp_type::Text(()) => FieldType::Text,
        capnp_type::Data(()) => FieldType::Data,
        capnp_type::List(list) => FieldType::List(Box::new(field_type(list.get_element_type()?)?)),
        capnp_type::Enum(e) => FieldType::Named(e.get_type_id()),
        capnp_type::Struct(s) => FieldType::Named(s.get_type_id()),
        capnp_type::Interface(_) => FieldType::Unhandled("interface".into()),
        capnp_type::AnyPointer(_) => FieldType::Unhandled("anyPointer".into()),
    })
}

/// The last segment of a node's scoped name, e.g. `ChangeDiff`.
fn short_name(node: &node::Reader) -> Result<Option<String>> {
    let display = node.get_display_name()?.to_str()?;
    let start = node.get_display_name_prefix_length() as usize;
    let name = display.get(start..).unwrap_or_default();
    if name.is_empty() || name.contains([':']) {
        return Ok(None);
    }
    // A group's display name is `Parent.field`; the last segment is its own
    // name and the prefix is what it belongs to.
    Ok(Some(name.rsplit('.').next().unwrap_or(name).to_string()))
}
