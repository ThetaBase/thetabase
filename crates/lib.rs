//! Scribe's protocol core, compiled to WebAssembly.
//!
//! `01-system-architecture.md` §1 puts Scribe in the caller's app runtime as
//! WASM, and `02-api-wire-protocol.md` §3 says every binding compiles from one
//! source of truth. This is the half of Scribe that must not be written three
//! times.
//!
//! # What is here and what is not
//!
//! Here: framing, request encoding, response decoding, and the read-cache rule.
//! Not here: sockets. WebAssembly has no networking of its own, and the host
//! runtimes differ too much to pretend otherwise — Node has `net`, a browser
//! has none, an edge runtime has `fetch` and a socket API that is not Node's.
//!
//! The split is not a workaround, it is the right seam. What must be identical
//! across languages is the protocol and the cache invariants; what must differ
//! is I/O. A JavaScript SDK that re-encoded requests in JavaScript would be a
//! second implementation of the wire, and a Python one a third — and the first
//! time the schema moved, two of the three would be wrong.
//!
//! # The interface is bytes and JSON
//!
//! Every exported function takes and returns a length-prefixed byte buffer in
//! linear memory. Callers write arguments in, call, and read results out; there
//! is no `wasm-bindgen`, no JS glue to keep in sync, and no per-language ABI.
//! Requests and responses cross the boundary as JSON, which every host can
//! already build and parse, and which the Rust side turns into typed wire
//! messages — so the *typed* step happens once, here.

use std::collections::HashMap;

use theta_proto::frame;
use theta_proto::{Request, RequestBody, Response, ResponseBody};

mod abi;
mod cache;
pub mod query;
mod translate;

pub use cache::ReadCache;

/// What a conditional write requires of the row it replaces.
///
/// Tagged rather than a nullable version: "no version given" and "this row must
/// not exist" are different requests, and the second is a much stronger claim
/// than a host makes by omitting a field.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expect {
    Absent,
    Version { value: u64 },
}

/// What the host asked for, before it becomes a wire message.
///
/// JSON rather than a binary ABI: a host has to be able to build this without a
/// generated binding, or the WASM module would need per-language glue and we
/// would be back to code that drifts.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]

pub enum HostRequest {
    Get {
        key: String,
    },
    Put {
        key: String,
        value: serde_json::Value,
    },
    /// A write with a precondition on the row's current version (M10.5).
    ///
    /// `expect` is `{"kind": "absent"}` or `{"kind": "version", "value": n}` —
    /// the shape the generated bindings produce for the wire union, so a host
    /// with those types can hand one straight through.
    PutIf {
        key: String,
        value: serde_json::Value,
        expect: Expect,
    },
    Delete {
        key: String,
    },
    Query {
        sql: String,
        #[serde(default)]
        params: HashMap<String, serde_json::Value>,
    },
    Explain {
        sql: String,
        #[serde(default)]
        params: HashMap<String, serde_json::Value>,
    },
    ProposeSchemaChange {
        change: serde_json::Value,
    },
    ApplySchemaChange {
        change_id: String,
        confirm: bool,
    },
    ShowChange {
        change_id: String,
    },
    PromoteChange {
        change_id: String,
    },
    RejectChange {
        change_id: String,
        reason: String,
    },
    CreateBranch {
        name: String,
        from: Option<String>,
    },
    ListBranches,
    DiscardBranch {
        name: String,
    },
    Merge {
        source: String,
        into: String,
    },
    Audit {
        limit: u32,
        min_risk: u32,
    },
    Status,
}

/// Encode one host request as a framed wire message.
///
/// The request id and branch are the host's to choose: it owns the connection,
/// so it owns the correlation. Everything else is decided here.
pub fn encode(request_id: u64, branch_id: u64, host: &HostRequest) -> Result<Vec<u8>, String> {
    let body = translate::to_wire(host)?;
    let request = Request {
        request_id,
        branch_id,
        body,
    };
    frame::frame(&request.encode()).map_err(|e| e.to_string())
}

/// Decode a wire response into JSON the host can hand back to its caller.
///
/// Errors from the server arrive as a response body rather than as a transport
/// failure, and stay that way here: a gate refusal is an *answer*, and
/// flattening it into a connection error would lose the diff the caller has to
/// act on.
pub fn decode(payload: &[u8]) -> Result<serde_json::Value, String> {
    let response = Response::decode(payload).map_err(|e| e.to_string())?;
    Ok(translate::from_wire(&response))
}

/// Encode the handshake.
///
/// The protocol version is fixed by the core, not chosen by the host: a host
/// that could name its own version could claim compatibility it does not have,
/// and the negotiation that refuses mismatched versions rather than guessing
/// (`02-api-wire-protocol.md` §5) would be negotiating with itself.
pub fn encode_hello(session_token: &str, client_name: &str) -> Result<Vec<u8>, String> {
    let hello = theta_proto::Hello {
        protocol_version: theta_proto::PROTOCOL_VERSION,
        session_token: session_token.to_string(),
        client_name: client_name.to_string(),
    };
    frame::frame(&hello.encode()).map_err(|e| e.to_string())
}

/// Decode the server's reply to a handshake.
///
/// A refusal arrives as an error `Response` rather than a `Welcome`, and says
/// so plainly: "the server refused the handshake" is actionable, and a decode
/// failure on a message that decoded fine is not.
pub fn decode_welcome(payload: &[u8]) -> Result<serde_json::Value, String> {
    if let Ok(welcome) = theta_proto::Welcome::decode(payload) {
        return Ok(serde_json::json!({
            "protocolVersion": welcome.protocol_version,
            "projectId": welcome.project_id,
            "serverName": welcome.server_name,
        }));
    }

    match Response::decode(payload) {
        Ok(Response {
            body: ResponseBody::Error(e),
            ..
        }) => Err(format!("the server refused the handshake: {}", e.message)),
        _ => Err("the server sent neither a welcome nor an error".to_string()),
    }
}

/// The length prefix a host must read before the body of a reply.
pub const LENGTH_PREFIX_BYTES: usize = frame::LENGTH_PREFIX_BYTES;

/// How many bytes of body follow a length prefix.
pub fn body_length(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Result<usize, String> {
    frame::decode_length(prefix).map_err(|e| e.to_string())
}

/// Whether a request invalidates a cached key, and which.
///
/// Read-your-writes is the one cache rule that cannot be got wrong
/// (`03-data-model-consistency.md` §3.1), so the decision lives here rather
/// than in each host: a write must drop the key locally before the write is
/// acknowledged, or a subsequent read can serve a value the caller has already
/// replaced.
pub fn invalidates(host: &HostRequest) -> Option<&str> {
    match host {
        HostRequest::Put { key, .. } | HostRequest::Delete { key } => Some(key),
        // A merge or a schema change can move rows this Scribe has cached and
        // cannot name them, so the honest answer is to drop everything. Handled
        // by the host through `invalidates_everything`.
        _ => None,
    }
}

/// Whether a request makes every cached read untrustworthy.
pub fn invalidates_everything(host: &HostRequest) -> bool {
    matches!(
        host,
        HostRequest::Merge { .. }
            | HostRequest::ApplySchemaChange { .. }
            | HostRequest::PromoteChange { .. }
            | HostRequest::CreateBranch { .. }
            | HostRequest::DiscardBranch { .. }
    )
}

/// Whether a response is a server-side error rather than a result.
pub fn is_error(response: &Response) -> bool {
    matches!(response.body, ResponseBody::Error(_))
}

/// The wire body a host request becomes. Exposed for tests.
pub fn wire_body(host: &HostRequest) -> Result<RequestBody, String> {
    translate::to_wire(host)
}
