//! Model Context Protocol server for ThetaBase.
//!
//! # What this is for
//!
//! ThetaBase's primary user is an agent, and MCP is how an agent reaches a tool.
//! A CLI requires something to decide to shell out and parse the result; an MCP
//! server puts the operations in the model's tool list, which is the difference
//! between a database an agent *can* use and one it *does*.
//!
//! # The one decision that matters
//!
//! **This server exposes what an agent may do, and deliberately omits what only
//! a human may do.**
//!
//! The Safety Layer's whole design is that a destructive change is proposed by
//! an agent and answered by a person (`07-agent-safety-layer.md` §4–5). The
//! engine's `apply_schema_change` takes a `confirmed: bool`. Exposing that as an
//! MCP tool would hand the agent the key to its own gate: it would propose a
//! change, be told it needs confirmation, and confirm it — and every gate in the
//! product would become a two-call formality.
//!
//! So there is no confirm tool, no promote tool, no reject tool, and no revoke
//! tool. Not "not yet" — those are human authority, and an MCP server is by
//! definition the agent's hands. The list of what is absent is as much the
//! design as the list of what is present, which is why [`HUMAN_ONLY`] exists and
//! is asserted against the tool list in the tests.
//!
//! An agent that wants a gated change to land asks a person. That is the
//! product.
//!
//! # Transport
//!
//! JSON-RPC 2.0 over stdio, newline-delimited, which is what an agent runtime
//! spawns. No network listener: this process is started by the agent, talks to
//! it over pipes, and reaches `thetad` with the caller's own token.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod dispatch;
pub mod rows;
pub mod tools;

/// The protocol version this server implements.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Operations that exist in the product and are **not** exposed here, with the
/// reason each is withheld.
///
/// Asserted against the tool list by
/// `no_human_only_operation_is_exposed_as_a_tool`. A future contributor adding
/// one of these as a tool fails a test that says why, rather than shipping a
/// hole with a plausible-looking commit message.
pub const HUMAN_ONLY: &[(&str, &str)] = &[
    (
        "confirm",
        "answering a gate is the human half of the Safety Layer; an agent that \
         could confirm its own proposal would make every gate a formality",
    ),
    (
        "promote",
        "promoting a validated shadow branch is the act the shadow gate exists \
         to require a person for",
    ),
    (
        "reject",
        "rejecting is a review decision, and a review an agent performs on its \
         own work is not a review",
    ),
    (
        "revoke",
        "revocation is how a human stops an agent; an agent that can revoke can \
         stop the thing stopping it",
    ),
    (
        "push_policy",
        "the policy sets every threshold the gates turn on, so writing it is the \
         most privileged act on a project and arrives signed by the Control \
         Plane (`07-agent-safety-layer.md` §7)",
    ),
    (
        "discard_branch",
        "discarding destroys work that may be under review; it is cheap to ask \
         and expensive to undo",
    ),
];

#[derive(Debug, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Absent for a notification, which expects no reply.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// The server's declared capabilities and identity.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "thetabase", "version": env!("CARGO_PKG_VERSION") },
        // Read by the model before it calls anything. It is worth spending
        // words here: an agent that understands the gate will propose better
        // changes, and one that does not will treat every refusal as an error
        // to route around.
        "instructions": "ThetaBase is a branchable database with a rule-based Safety Layer.\n\n\
            Schema changes are classified into one of three gates:\n\
            - auto_apply: applied immediately.\n\
            - confirm: needs a human to answer. You cannot answer it yourself.\n\
            - shadow_validate: confirmation is not enough. The change is applied to a \
              shadow branch and validated, and a human promotes it.\n\n\
            The classifier reads the KIND of change, the number of rows affected, whether \
            the change is reversible, and whether the branch is protected. It does not read \
            your identifiers, so renaming a column will never change its gate. Do not try.\n\n\
            If a change is gated, the useful response is to make it smaller or make it \
            reversible, or to ask the person you are working with. Splitting one destructive \
            change into many small ones to get under a threshold is detected and is treated \
            as an attack.\n\n\
            Branches are cheap and reads do not get slower as they deepen. Work on a branch."
    })
}
