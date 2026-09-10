//! `theta-mcp` — the MCP server binary.
//!
//! Speaks JSON-RPC 2.0 over stdio, newline-delimited. Started by an agent
//! runtime, not by an operator, so it takes its context from the environment the
//! CLI already injects (`theta exec`) rather than from flags a human would type.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use theta_mcp::{dispatch, initialize_result, tools, Request, Response};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // stderr, never stdout. stdout is the protocol channel, and a stray log line
    // on it is a parse error at the other end — the kind that presents as "the
    // MCP server is broken" rather than "something logged".
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                // -32700 is the JSON-RPC parse error. Answered with a null id
                // because there is no id to echo.
                let response = Response::err(Value::Null, -32700, format!("parse error: {e}"));
                write(&mut stdout, &response).await?;
                continue;
            }
        };

        // A notification has no id and expects no reply. `initialized` is the
        // one the handshake sends.
        let Some(id) = request.id.clone() else {
            tracing::debug!(method = %request.method, "notification");
            continue;
        };

        let response = handle(&request, id).await;
        write(&mut stdout, &response).await?;
    }

    Ok(())
}

async fn handle(request: &Request, id: Value) -> Response {
    match request.method.as_str() {
        "initialize" => Response::ok(id, initialize_result()),
        "tools/list" => Response::ok(id, tools::list()),
        "tools/call" => call(request, id).await,
        // -32601 is method-not-found. Named rather than silently ignored: an
        // agent runtime probing for a capability should be told it is absent.
        other => Response::err(id, -32601, format!("method not found: {other}")),
    }
}

async fn call(request: &Request, id: Value) -> Response {
    let name = request.params["name"].as_str().unwrap_or_default();

    let known = tools::TOOLS.iter().any(|t| t.name == name);
    if !known {
        // The refusal names the human-only operation where that is what was
        // asked for, because "unknown tool" is a confusing answer to a
        // reasonable question. An agent that asked to confirm should be told
        // that confirming is not its to do — not that the tool is missing.
        if let Some((op, reason)) = theta_mcp::HUMAN_ONLY
            .iter()
            .find(|(op, _)| name.contains(op))
        {
            return Response::err(
                id,
                -32601,
                format!(
                    "`{name}` is not available to an agent. {op}: {reason}. Ask the \
                     person you are working with."
                ),
            );
        }
        return Response::err(id, -32601, format!("unknown tool: {name}"));
    }

    let ctx = match dispatch::Context::from_env() {
        Ok(ctx) => ctx,
        Err(message) => {
            // A tool *result* carrying isError, not a JSON-RPC error. The
            // difference matters to the model: a JSON-RPC error reads as "the
            // server is broken", and this is a configuration the agent's own
            // runtime can fix.
            return Response::ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": message }],
                    "isError": true
                }),
            );
        }
    };

    let arguments = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Connects with the caller's own token and speaks the ordinary protocol.
    // This server holds no privilege of its own and cannot reach anything the
    // caller could not — which is what makes omitting the human-only operations
    // meaningful rather than cosmetic.
    Response::ok(id, dispatch::call(&ctx, name, &arguments).await)
}

async fn write(out: &mut tokio::io::Stdout, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await
}
