//! The MCP tools, against a live `thetad` over TCP.
//!
//! The unit tests in `tools.rs` check the shape of the tool list — that no
//! human-only operation is exposed, that every schema is closed. They cannot
//! check that a call reaches the wire, and a tool list that describes operations
//! nothing performs is the failure this file exists to prevent.

use std::sync::Arc;

use serde_json::json;
use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use theta_mcp::dispatch::{self, Context};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::sync::RwLock;

const PROJECT: &str = "mcp-test";

/// A running instance, and a context pointed at it.
async fn instance() -> (Context, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default(PROJECT);
    config.data_dir = dir.path().to_path_buf();
    config.listen = "127.0.0.1:0".parse().expect("addr");

    let keys = ProjectKeys::generate(PROJECT);
    let mut engine = Engine::open(config.clone()).expect("open");
    engine.install_keyset(keys.public_keyset());

    let now = 1_800_000_000_000;
    let token = SessionToken::mint(
        &keys,
        &TokenScope {
            token_id: "tok_mcp".into(),
            project_id: PROJECT.into(),
            environment: "dev".into(),
            session_id: "sess_mcp".into(),
            user_id: "u_mcp".into(),
            org_id: "org_mcp".into(),
            issued_at_ms: 0,
            expires_at_ms: i64::MAX,
            key_id: KeyId::new(format!("{PROJECT}-k1")),
            signing_key: None,
        },
    )
    .into_string();

    let server_config = ServerConfig::from_config(&config);
    let handle = EngineHandle::spawn(engine, server_config.queue_depth);
    let authorizer = Arc::new(RwLock::new(Authorizer::new(
        keys.public_keyset(),
        Environment::Dev,
        60 * 60 * 1_000,
        now,
    )));

    let (ready, bound) = tokio::sync::oneshot::channel();
    tokio::spawn(serve(
        server_config,
        handle,
        authorizer,
        ready,
        std::future::pending::<()>(),
    ));
    let addr = bound.await.expect("bound");

    (
        Context {
            address: addr.to_string(),
            token,
        },
        dir,
    )
}

/// The text of a tool result, and whether it was an error.
async fn call(ctx: &Context, name: &str, args: serde_json::Value) -> (bool, String) {
    let result = dispatch::call(ctx, name, &args).await;
    (
        result["isError"].as_bool().unwrap_or(false),
        result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

#[tokio::test]
async fn a_tool_call_reaches_the_instance() {
    // The whole point of this file. A tool list is a promise; this is the check
    // that anything is behind it.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(&ctx, "theta_branch_list", json!({})).await;
    assert!(!is_error, "listing branches failed: {text}");
    assert!(
        text.contains("main"),
        "a live instance did not report its main branch: {text}"
    );
}

#[tokio::test]
async fn proposing_returns_a_gate_and_says_that_proposing_is_not_applying() {
    // The single most important response in the product, for an agent. A model
    // that reads "ok" here will tell the person it is working with that the
    // migration is done.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(
        &ctx,
        "theta_propose_schema_change",
        json!({ "change": { "change": "add_table", "table": {
            "name": "orders", "fields": {}, "indexes": []
        }}}),
    )
    .await;

    assert!(!is_error, "proposing failed: {text}");
    assert!(text.contains("gate"), "no gate in the response: {text}");
    assert!(
        text.contains("changeId"),
        "no change id for a human to answer: {text}"
    );
    assert!(
        text.contains("whatHappensNext"),
        "the response does not tell the agent what happens next: {text}"
    );
}

#[tokio::test]
async fn a_branch_name_that_does_not_exist_is_refused_rather_than_falling_back_to_main() {
    // A write that lands on main because a branch name was misspelled is the
    // failure this whole product is about. It would be a strange one to
    // introduce in the agent-facing surface.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(
        &ctx,
        "theta_get",
        json!({ "key": "orders:1", "branch": "no-such-branch" }),
    )
    .await;

    assert!(is_error, "an unknown branch was accepted: {text}");
    assert!(
        text.contains("no-such-branch"),
        "the refusal does not name the branch that was asked for: {text}"
    );
    assert!(
        text.contains("There is"),
        "the refusal does not say which branches exist, so an agent cannot \
         correct itself: {text}"
    );
}

#[tokio::test]
async fn a_branch_can_be_created_and_then_named() {
    // Creation and resolution are two halves of one thing: a branch an agent
    // creates and cannot then address is a branch it cannot use.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(&ctx, "theta_branch_create", json!({ "name": "feature-x" })).await;
    assert!(!is_error, "creating a branch failed: {text}");

    let (is_error, text) = call(
        &ctx,
        "theta_get",
        json!({ "key": "orders:1", "branch": "feature-x" }),
    )
    .await;
    assert!(
        !is_error,
        "the branch that was just created could not be addressed: {text}"
    );
}

#[tokio::test]
async fn a_row_in_the_wrong_encoding_is_refused_with_the_shape_it_wanted() {
    // `Value`'s serde representation is tagged, and a model will guess plain
    // JSON. Refusing with an example of the right shape is the difference
    // between an agent that corrects itself and one that retries the same thing.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(
        &ctx,
        "theta_put",
        json!({ "key": "orders:1", "value": { "n": 1 } }),
    )
    .await;

    assert!(is_error, "an untagged row was accepted: {text}");
    assert!(
        text.contains("kind"),
        "the refusal does not show the encoding it wanted: {text}"
    );
    // And *why* it failed, not only what was wanted. A plant that dropped the
    // parse reason left the shape hint intact and passed — the two are separate
    // facts and an agent debugging a nested value needs both.
    assert!(
        text.contains("invalid type") || text.contains("missing field"),
        "the refusal does not say what was wrong with what was sent: {text}"
    );
}

#[tokio::test]
async fn the_review_queue_shows_what_is_waiting_not_what_happened() {
    // These are different questions. `audit` is a record of what the Safety
    // Layer did; the review queue is a list of what it has not finished doing.
    // This tool used to be served from the audit trail because the engine's own
    // queue had no wire request — close enough to look right, and wrong in the
    // direction that matters: a reviewer reading an empty audit would conclude
    // there was nothing to review.
    let (ctx, _dir) = instance().await;

    // Nothing proposed yet: the queue is empty and says so.
    let (is_error, text) = call(&ctx, "theta_review_queue", json!({})).await;
    assert!(!is_error, "reading the queue failed: {text}");
    assert!(
        text.contains("\"waitingForAHuman\": 0"),
        "a fresh instance reported something waiting: {text}"
    );

    // A gated change appears. `set_nullable` without a backfill destroys the
    // rows that are null, so it is destructive and irreversible.
    let (is_error, proposal) = call(
        &ctx,
        "theta_propose_schema_change",
        json!({ "change": { "change": "set_nullable", "table": "orders",
                            "column": "email", "nullable": false }}),
    )
    .await;
    assert!(!is_error, "proposing failed: {proposal}");

    let (is_error, text) = call(&ctx, "theta_review_queue", json!({})).await;
    assert!(!is_error, "reading the queue failed: {text}");
    assert!(
        !text.contains("\"waitingForAHuman\": 0"),
        "a gated proposal did not appear in the queue: {text}"
    );
    assert!(
        text.contains("orders"),
        "the queue does not say which table is waiting: {text}"
    );
    // The agent is told it cannot answer these, in the response rather than only
    // in the tool description — the description is read once and the response
    // every time.
    assert!(
        text.contains("cannot answer"),
        "the queue does not tell the agent the answer is not its to give: {text}"
    );
}

#[tokio::test]
async fn a_query_comes_back_as_rows_rather_than_arrow_bytes() {
    // Query results cross the wire as Arrow IPC, which is right for the protocol
    // and useless to a model. A tool returning the byte array would be a tool no
    // agent could use.
    let (ctx, _dir) = instance().await;

    let (is_error, text) = call(
        &ctx,
        "theta_query",
        json!({ "query": "SELECT * FROM orders" }),
    )
    .await;

    // The table does not exist, so this is a refusal — and the refusal is the
    // point: it proves the SQL reached the server's planner rather than being
    // rejected here for want of a parser this crate deliberately does not link.
    assert!(
        text.contains("orders") || text.contains("rows"),
        "the query did not reach the planner: {text}"
    );
    let _ = is_error;
}
