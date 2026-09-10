//! The tool list, and what each one does.
//!
//! Every tool here is something an agent may do on its own authority. See
//! [`crate::HUMAN_ONLY`] for what is deliberately absent and why.

use serde_json::{json, Value};

/// One exposed tool.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

/// The tools, in the order an agent meets them.
///
/// `describe` is first deliberately. An agent arriving at an unfamiliar database
/// asks what is in it, and every step it has to take before it can ask is a step
/// it can get wrong.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "theta_describe",
        description: "What is in this database and where it came from: tables, columns, \
                      types, and when each was declared. Ask this first. Row examples are \
                      off by default because an example is customer data.",
        schema: describe_schema,
    },
    Tool {
        name: "theta_query",
        description: "Run a typed query. Returns rows. Parameters are bound, never \
                      interpolated — there is no raw-SQL escape hatch, by design.",
        schema: query_schema,
    },
    Tool {
        name: "theta_get",
        description: "Read one row by key, with the version it is at. Keys are \
                      `table:id`.",
        schema: key_schema,
    },
    Tool {
        name: "theta_put",
        description: "Write one row. Type-checked against the declared schema; a value \
                      that does not fit is refused rather than coerced.",
        schema: put_schema,
    },
    Tool {
        name: "theta_delete",
        description: "Delete one row. It leaves this branch's view; the log keeps the entry that removed it, so the history is still readable and the row is not.",
        schema: key_schema,
    },
    Tool {
        name: "theta_propose_schema_change",
        description: "Propose a schema change. Returns the change id, the gate it was \
                      assigned, the number of rows it would affect, and why. \
                      **Proposing is not applying.** If the gate is `confirm` or \
                      `shadow_validate`, a person has to answer it — you cannot, and no \
                      tool here lets you.",
        schema: propose_schema,
    },
    Tool {
        name: "theta_change_status",
        description: "The current state of a proposal you made: its gate, its diff, and \
                      what validating it found if it went to a shadow branch.",
        schema: change_id_schema,
    },
    Tool {
        name: "theta_review_queue",
        description: "What is waiting for a human, grouped so it can be answered \
                      together. Read-only. Useful for telling somebody what you need \
                      from them.",
        schema: empty_schema,
    },
    Tool {
        name: "theta_branch_create",
        description: "Create a branch. Cheap — creation copies no data, and reads do not \
                      get slower as branches deepen. Work on a branch rather than on main.",
        schema: branch_create_schema,
    },
    Tool {
        name: "theta_branch_list",
        description: "List branches, with each one's head and whether it is protected.",
        schema: empty_schema,
    },
    Tool {
        name: "theta_branch_merge",
        description: "Merge a branch into another. CRDT-typed fields converge \
                      automatically; anything else that conflicts is reported as a \
                      conflict for a human, never auto-resolved.",
        schema: branch_merge_schema,
    },
    Tool {
        name: "theta_audit",
        description: "The forensic trail, ranked by risk: what was attempted, by whom, \
                      and what the Safety Layer did about it.",
        schema: audit_schema,
    },
];

pub fn list() -> Value {
    json!({
        "tools": TOOLS
            .iter()
            .map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            }))
            .collect::<Vec<_>>()
    })
}

// ---- schemas ---------------------------------------------------------------

fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn branch_field() -> Value {
    json!({
        "type": "string",
        "description": "Branch name. Defaults to the current context's branch."
    })
}

fn describe_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "table": { "type": "string", "description": "One table. Omit for all." },
            "branch": branch_field(),
            "include_examples": {
                "type": "boolean",
                "default": false,
                "description": "Draw example values from the branch. Off by default: an \
                                example is customer data, and this call is one you make \
                                to learn the shape."
            }
        },
        "additionalProperties": false
    })
}

fn query_schema() -> Value {
    json!({
        "type": "object",
        "required": ["query"],
        "properties": {
            "query": { "type": "string", "description": "The query, in ThetaBase's SQL front end." },
            "params": {
                "type": "array",
                "description": "Bound parameters. Never interpolated into the text.",
                "items": {}
            },
            "branch": branch_field()
        },
        "additionalProperties": false
    })
}

fn key_schema() -> Value {
    json!({
        "type": "object",
        "required": ["key"],
        "properties": {
            "key": { "type": "string", "description": "Row key, `table:id`." },
            "branch": branch_field()
        },
        "additionalProperties": false
    })
}

fn put_schema() -> Value {
    json!({
        "type": "object",
        "required": ["key", "value"],
        "properties": {
            "key": { "type": "string", "description": "Row key, `table:id`." },
            "value": { "type": "object", "description": "The row, as an object of columns." },
            "branch": branch_field()
        },
        "additionalProperties": false
    })
}

fn propose_schema() -> Value {
    json!({
        "type": "object",
        "required": ["change"],
        "properties": {
            "change": {
                "type": "object",
                "description": "The schema change: add_table, drop_table, add_column, \
                                drop_column, alter_column_type, set_nullable, add_index, \
                                drop_index, rename_column, set_crdt."
            },
            "branch": branch_field()
        },
        "additionalProperties": false
    })
}

fn change_id_schema() -> Value {
    json!({
        "type": "object",
        "required": ["change_id"],
        "properties": { "change_id": { "type": "string" } },
        "additionalProperties": false
    })
}

fn branch_create_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": { "type": "string" },
            "from": { "type": "string", "description": "Parent branch. Defaults to current." }
        },
        "additionalProperties": false
    })
}

fn branch_merge_schema() -> Value {
    json!({
        "type": "object",
        "required": ["source", "target"],
        "properties": {
            "source": { "type": "string" },
            "target": { "type": "string" }
        },
        "additionalProperties": false
    })
}

fn audit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "default": 20 },
            "min_risk": {
                "type": "string",
                "enum": ["info", "low", "medium", "high"],
                "default": "info"
            }
        },
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HUMAN_ONLY;

    #[test]
    fn no_human_only_operation_is_exposed_as_a_tool() {
        // The load-bearing test in this crate.
        //
        // An MCP server is the agent's hands. Exposing `confirm` here would mean
        // an agent proposes a destructive change, is told it needs a human, and
        // is the human — and every gate in the product becomes a two-call
        // formality. The same argument applies to promote, reject, revoke,
        // policy push and branch discard.
        //
        // Adding one of these should fail a test that says why, rather than ship
        // behind a commit message that sounds reasonable.
        for (op, reason) in HUMAN_ONLY {
            for tool in TOOLS {
                assert!(
                    !tool.name.contains(op),
                    "`{}` exposes the human-only operation `{op}` — {reason}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn every_tool_says_what_it_is_for() {
        // The description is what the model reads to decide whether to call it.
        // A tool with a thin description is one an agent uses wrongly, and the
        // failure looks like the database being confusing rather than the
        // description being short.
        for tool in TOOLS {
            assert!(
                tool.description.len() > 40,
                "`{}` has a description too short to choose by",
                tool.name
            );
            assert!(
                tool.name.starts_with("theta_"),
                "`{}` is not namespaced; an agent with several MCP servers \
                 attached needs to tell them apart",
                tool.name
            );
        }
    }

    #[test]
    fn every_schema_is_a_closed_object() {
        // `additionalProperties: false` so a model that invents an argument is
        // told, rather than having it silently dropped — a dropped argument is
        // how an agent comes to believe it set a branch it did not set.
        for tool in TOOLS {
            let schema = (tool.schema)();
            assert_eq!(
                schema["additionalProperties"],
                serde_json::json!(false),
                "`{}` accepts unknown arguments",
                tool.name
            );
        }
    }

    #[test]
    fn the_proposal_tool_says_that_proposing_is_not_applying() {
        // The single most important sentence in the tool list. An agent that
        // believes `propose` applies will report success for a change sitting in
        // a review queue, and the person it is reporting to will believe it.
        let propose = TOOLS
            .iter()
            .find(|t| t.name == "theta_propose_schema_change")
            .expect("the proposal tool exists");
        assert!(
            propose.description.contains("not applying"),
            "the proposal tool does not tell the agent that proposing is not applying"
        );
    }
}
