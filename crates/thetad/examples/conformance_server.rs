//! A live `thetad` for the SDK conformance suite.
//!
//! The M6 gate is "generated SDKs compile and pass a shared conformance suite
//! run against a live `thetad`". Against a *live* server is the point: a suite
//! run against a mock proves the SDKs agree with the mock.
//!
//! Starts an instance on an ephemeral port, mints a token for it, and prints
//! both as JSON on stdout so a test runner in any language can connect. Stays
//! up until stdin closes, which is how the runner shuts it down without signals.

use std::sync::Arc;

use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::sync::RwLock;

const PROJECT: &str = "conformance";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = Config::dev_default(PROJECT);
    config.data_dir = dir.path().to_path_buf();
    config.listen = "127.0.0.1:0".parse()?;

    let keys = ProjectKeys::generate(PROJECT);
    let mut engine = Engine::open(config.clone())?;
    engine.install_keyset(keys.public_keyset());

    let now = now_ms();
    let token = SessionToken::mint(
        &keys,
        &TokenScope {
            token_id: "tok_conformance".into(),
            project_id: PROJECT.into(),
            environment: "dev".into(),
            session_id: "sess_conformance".into(),
            user_id: "conformance".into(),
            org_id: "org_conformance".into(),
            issued_at_ms: now,
            // Long enough that a slow suite cannot fail on expiry, short enough
            // that a leaked harness token is not a lasting credential.
            expires_at_ms: now + 3_600_000,
            key_id: keys.active.clone(),
            // This session does not sign its writes.
            signing_key: None,
        },
    )
    .into_string();

    let server_config = ServerConfig::from_config(&config);
    let handle = EngineHandle::spawn(engine, server_config.queue_depth);
    let authorizer = Arc::new(RwLock::new(Authorizer::new(
        keys.public_keyset(),
        Environment::Dev,
        15_000,
        now,
    )));

    let (ready, bound) = tokio::sync::oneshot::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve(server_config, handle, authorizer, ready, async {
        let _ = stop_rx.await;
    }));

    let addr = bound.await?;
    println!(
        "{}",
        serde_json::json!({
            "address": addr.to_string(),
            "token": token,
            "projectId": PROJECT,
        })
    );
    // The runner reads one line, so it must not wait on a buffer.
    use std::io::Write;
    std::io::stdout().flush()?;

    // Stdin closing is the shutdown signal: no signal handling, and no orphan
    // if the runner dies. Blocking read on its own thread, since the tokio
    // stdin feature is not enabled here and this thread has nothing else to do.
    tokio::task::spawn_blocking(|| {
        let mut buf = [0u8; 1];
        let _ = std::io::Read::read(&mut std::io::stdin(), &mut buf);
    })
    .await
    .ok();
    let _ = stop_tx.send(());
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
