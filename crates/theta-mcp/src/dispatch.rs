//! Tool calls, onto the wire.
//!
//! Every call connects with the caller's own token and speaks the ordinary
//! protocol. **This server holds no privilege of its own** and cannot reach
//! anything the caller could not reach with the CLI — which is what makes the
//! omission of the human-only operations meaningful rather than cosmetic. If
//! this process could confirm a change, moving `confirm` out of the tool list
//! would only be hiding a capability it still had.

use serde_json::{json, Value};
use theta_cli::DataClient;
use theta_proto::{RequestBody, ResponseBody};

/// Where to connect and with what.
///
/// Read from the environment the CLI injects via `theta exec`, so a token never
/// reaches a file anybody manages.
pub struct Context {
    pub address: String,
    pub token: String,
}

impl Context {
    pub fn from_env() -> Result<Self, String> {
        let address = std::env::var("THETA_ADDRESS").map_err(|_| {
            "THETA_ADDRESS is not set. Run this server under `theta exec`, which \
             injects the address and token, or set both yourself."
                .to_string()
        })?;
        let token = std::env::var("THETA_TOKEN").map_err(|_| {
            "THETA_TOKEN is not set. Run this server under `theta exec` rather than \
             writing a token to a file."
                .to_string()
        })?;
        Ok(Self { address, token })
    }
}

/// How many rows a query may put into a model's context.
///
/// A query that matched fifty thousand rows must not return fifty thousand
/// rows. The agent asked a question; an answer that fills the context window is
/// worse than a truncated one that says it was truncated.
const ROW_LIMIT: usize = 100;

/// Run one tool call and return its MCP content payload.
pub async fn call(ctx: &Context, name: &str, args: &Value) -> Value {
    match dispatch(ctx, name, args).await {
        Ok(value) => text(&value, false),
        // Errors come back as tool *results* with `isError`, not as JSON-RPC
        // errors. The distinction matters to the model: a JSON-RPC error reads
        // as "the tool is broken", and a refusal by the Safety Layer is the tool
        // working. An agent told a change is gated should try a smaller change,
        // not retry the call.
        Err(message) => text(&json!({ "error": message }), true),
    }
}

fn text(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        }],
        "isError": is_error
    })
}

async fn dispatch(ctx: &Context, name: &str, args: &Value) -> Result<Value, String> {
    let mut client = DataClient::connect(&ctx.address, &ctx.token)
        .await
        .map_err(|e| e.to_string())?;

    // The tools take branch *names*, because that is what an agent knows; the
    // wire takes ids. Resolving here rather than exposing ids is the difference
    // between a tool an agent can use and one it has to be taught. A name that
    // does not resolve is refused — silently falling back to main is how a write
    // lands on the wrong branch.
    let branch = resolve(&mut client, args["branch"].as_str()).await?;

    let body = match name {
        "theta_describe" => RequestBody::Describe {
            table: string(args, "table").unwrap_or_default(),
            include_examples: args["include_examples"].as_bool().unwrap_or(false),
            example_limit: 0,
        },
        "theta_get" => RequestBody::Get {
            key: required(args, "key")?,
        },
        "theta_delete" => RequestBody::Delete {
            key: required(args, "key")?,
        },
        "theta_put" => RequestBody::Put {
            key: required(args, "key")?,
            value_json: encode_value(&args["value"])?,
            ttl: 0,
        },
        "theta_query" => {
            // The SQL travels as text and the *server* compiles it — this crate
            // links no parser. Bindings travel beside it and are never
            // interpolated, which is the property `specs/07` invariant 4 exists
            // to keep: there is no raw-string path into the execution plan.
            let mut context_vars = Vec::new();
            if let Some(params) = args["params"].as_array() {
                for (i, param) in params.iter().enumerate() {
                    context_vars.push((format!("p{i}"), encode_value(param)?));
                }
            }
            RequestBody::Query(theta_proto::wire::QueryPlanWire {
                plan_hash: 0,
                raw_query: required(args, "query")?,
                context_vars,
            })
        }
        "theta_propose_schema_change" => RequestBody::ProposeSchemaChange {
            change_json: args["change"].to_string(),
        },
        "theta_change_status" => RequestBody::ShowChange {
            change_id: required(args, "change_id")?,
        },
        "theta_review_queue" => RequestBody::ReviewQueue,
        "theta_branch_create" => RequestBody::CreateBranch {
            name: required(args, "name")?,
            from: resolve(&mut client, args["from"].as_str()).await?,
        },
        "theta_branch_list" => RequestBody::ListBranches,
        "theta_branch_merge" => RequestBody::Merge {
            source_branch: resolve(&mut client, Some(&required(args, "source")?)).await?,
            target_branch: resolve(&mut client, Some(&required(args, "target")?)).await?,
        },
        "theta_audit" => RequestBody::Audit {
            limit: args["limit"].as_u64().unwrap_or(20) as u32,
            min_risk: risk_of(args),
        },
        other => return Err(format!("unknown tool: {other}")),
    };

    match client.call(branch, body).await {
        Ok(response) => Ok(render(response)),
        Err(e) => Err(e.to_string()),
    }
}

/// Turn a wire response into something a model can read.
///
/// A gated proposal is **not** an error here. It is the product working, and the
/// rendering says so explicitly — an agent that reads "rejected" will try again,
/// and one that reads "this needs a person" will go and ask.
fn render(response: ResponseBody) -> Value {
    match response {
        ResponseBody::Propose(diff) => json!({
            "changeId": diff.change_id,
            "gate": format!("{:?}", diff.gate),
            "rowsAffected": diff.rows_affected,
            "destructive": diff.destructive,
            "reversible": diff.reversible,
            "reason": diff.reason,
            "whatHappensNext": match format!("{:?}", diff.gate).as_str() {
                "AutoApply" => "Applied. Nothing further is needed.",
                "Confirm" => "Proposed, not applied. A person has to confirm it. You \
                              cannot, and no tool here lets you — tell them the change id.",
                _ => "Proposed, not applied. Confirmation is not enough: this goes to a \
                      shadow branch to be validated, and a person promotes it.",
            }
        }),
        // Rows, not bytes. Arrow IPC is right for the wire and useless to a
        // model, and a tool that returned the byte array would be a tool no
        // agent could use.
        ResponseBody::Query {
            result_set,
            row_count,
            ..
        } => match crate::rows::decode(&result_set, ROW_LIMIT) {
            Ok(decoded) => decoded,
            Err(message) => json!({ "error": message, "rowCount": row_count }),
        },
        // Grouped, and each batch says which member drove its gate. "This batch
        // needs shadow validation" is not actionable on its own — the next
        // question is always *which one*.
        ResponseBody::ReviewQueue { batches } => json!({
            "waitingForAHuman": batches.len(),
            "batches": batches.iter().map(|b| json!({
                "table": b.key,
                "gate": match b.gate { 0 => "auto_apply", 1 => "confirm", _ => "shadow_validate" },
                "changes": b.changes.iter().map(|c| json!({
                    "changeId": c.change_id,
                    "reason": c.reason,
                    "rowsAffected": c.rows_affected,
                })).collect::<Vec<_>>(),
                "rowsAffected": b.rows_affected,
                "why": b.reason,
            })).collect::<Vec<_>>(),
            "note": "You cannot answer these. Tell the person you are working with the change ids."
        }),
        ResponseBody::Error(e) => json!({
            "refused": e.message,
            "code": format!("{:?}", e.code),
        }),
        other => serde_json::to_value(RenderDebug(other)).unwrap_or(Value::Null),
    }
}

/// Fallback rendering for the response shapes that have no bespoke view yet.
struct RenderDebug(ResponseBody);

impl serde::Serialize for RenderDebug {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{:?}", self.0))
    }
}

/// Turn a branch name into the id the wire wants.
///
/// `None` means the default branch, which is main. A name that does not resolve
/// is an error rather than a fallback: a write that silently lands on main
/// because a branch name was misspelled is the failure this whole product is
/// about, and it would be a strange one to introduce here.
async fn resolve(client: &mut DataClient, name: Option<&str>) -> Result<u64, String> {
    let Some(name) = name else {
        return Ok(0);
    };
    if name == "main" {
        return Ok(0);
    }

    match client.call(0, RequestBody::ListBranches).await {
        Ok(ResponseBody::Branches { branches }) => branches
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.branch_id)
            .ok_or_else(|| {
                let known: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
                format!("no branch named `{name}`. There is: {}", known.join(", "))
            }),
        Ok(other) => Err(format!("listing branches returned {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

fn string(args: &Value, key: &str) -> Option<String> {
    args[key].as_str().map(str::to_string)
}

fn required(args: &Value, key: &str) -> Result<String, String> {
    string(args, key).ok_or_else(|| format!("`{key}` is required"))
}

fn risk_of(args: &Value) -> u8 {
    match args["min_risk"].as_str().unwrap_or("info") {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Encode a row the way the wire expects it.
///
/// `Value`'s own serde representation, not raw JSON — the same encoding `put`
/// uses everywhere else. The two diverged once already and the failure mode was
/// a signature check reporting a mismatch with nothing pointing at the encoding.
fn encode_value(value: &Value) -> Result<String, String> {
    let parsed: theta_core::Value = serde_json::from_value(value.clone()).map_err(|e| {
        format!(
            "the row is not in ThetaBase's value encoding: {e}. Columns are tagged, \
             e.g. {{\"kind\":\"map\",\"value\":{{\"n\":{{\"kind\":\"int\",\"value\":1}}}}}}"
        )
    })?;
    serde_json::to_string(&parsed).map_err(|e| e.to_string())
}
