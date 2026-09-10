//! `thetad` entry point.

use std::sync::Arc;

use clap::Parser;
use theta_identity::{PublicKeyset, WireKeyset};
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::sync::RwLock;

/// How long an instance may go without hearing from the Control Plane before it
/// stops authorizing. Several heartbeats, so one dropped packet is not an
/// outage, and far short of a token's lifetime.
const REVOCATION_STALENESS_LIMIT_MS: i64 = 15_000;

#[derive(Parser, Debug)]
#[command(name = "thetad", version, about = "ThetaBase storage engine daemon")]
struct Args {
    /// Project this instance serves. Fixed for the process lifetime.
    #[arg(long, env = "THETA_PROJECT")]
    project: String,

    /// Path to a JSON config file. Falls back to dev defaults when absent.
    #[arg(long, env = "THETA_CONFIG")]
    config: Option<std::path::PathBuf>,

    /// Address to listen on. Overrides the config file.
    #[arg(long, env = "THETA_LISTEN")]
    listen: Option<std::net::SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thetad=info,theta_storage=info,theta_safety=info".into()),
        )
        .init();

    let args = Args::parse();
    let mut config: Config = match &args.config {
        Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
        None => Config::dev_default(&args.project),
    };
    if let Some(listen) = args.listen {
        config.listen = listen;
    }

    let server_config = ServerConfig::from_config(&config);
    let environment = config.environment;
    let project = config.project_id.clone();
    let keyset_path = config.data_dir.join("keys.json");
    let engine = Engine::open(config)?;

    tracing::info!(
        project = %engine.config().project_id,
        environment = ?engine.config().environment,
        commits = engine.commits_applied(),
        "engine recovered"
    );

    // The keyset is placed here by the Control Plane when it provisions this
    // instance, not fetched by the instance at startup. Fetching would be
    // circular: authenticating a keyset fetch needs a keyset. The component
    // that brings the instance up is the one that can hand over the keys out of
    // band (`06-provisioning-identity-flow.md` §3).
    //
    // Public keys only, so an instance can verify tokens and never mint them.
    let keyset = load_public_keyset(&keyset_path, &project)?;

    // The engine holds them too: a signed safety policy is verified against the
    // same keys, and an instance with no keyset refuses to adopt one.
    let mut engine = engine;
    engine.install_keyset(keyset.clone());

    let handle = EngineHandle::spawn(engine, server_config.queue_depth);

    // Refuses every token until the first signed revocation list arrives.
    //
    // The alternative — treating an empty list as fresh as of startup — opens a
    // window on every restart in which a token revoked while this process was
    // down is honoured, and `is_stale` is false because the clock says the list
    // was "synced" at boot. A restart is not a sync.
    let authorizer = Arc::new(RwLock::new(Authorizer::awaiting_first_sync(
        keyset,
        environment,
        REVOCATION_STALENESS_LIMIT_MS,
    )));

    let (ready, bound) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        server_config,
        handle,
        Arc::clone(&authorizer),
        ready,
        async {
            let _ = tokio::signal::ctrl_c().await;
        },
    ));

    if let Ok(addr) = bound.await {
        tracing::info!(%addr, "ready");
    }

    server.await??;
    tracing::info!("shut down");
    Ok(())
}

/// Load this instance's public keys.
///
/// Public keys only: an instance that could mint its own tokens would make the
/// per-project isolation guarantee meaningless.
fn load_public_keyset(path: &std::path::Path, project_id: &str) -> anyhow::Result<PublicKeyset> {
    resolve_public_keyset(
        path,
        project_id,
        std::env::var(theta_core::PUBLIC_KEYSET_ENV).ok(),
    )
}

/// The half of [`load_public_keyset`] that does not touch global state.
///
/// Split out so the two delivery routes can be tested against each other.
/// Reading the environment inside the decision would make every test of it a
/// test of process-wide mutable state, and `set_var` is unsound to call while
/// other threads run — which a test harness always does.
fn resolve_public_keyset(
    path: &std::path::Path,
    project_id: &str,
    from_env: Option<String>,
) -> anyhow::Result<PublicKeyset> {
    // Two routes, and which one applies is a fact about the deployment rather
    // than a preference. Where the Control Plane shares a filesystem with this
    // process it writes the keyset into the data directory; on a machine
    // platform it cannot, and delivers it in the environment instead.
    //
    // The environment is checked first because it is the route that only the
    // Control Plane can have set for *this* boot, whereas the file may be a
    // leftover on a volume that outlived a reprovision.
    let (source, bytes) = match from_env {
        Some(json) if !json.trim().is_empty() => {
            (theta_core::PUBLIC_KEYSET_ENV.to_string(), json.into_bytes())
        }
        _ => {
            let read = std::fs::read(path).map_err(|e| {
                anyhow::anyhow!(
                    "cannot read the public keyset at {} and {} is unset: {e}. \
                     The control plane writes the file when it provisions an \
                     instance on a host it shares, and sets the variable when it \
                     does not; if you started this process by hand, copy the \
                     project's keyset from `GET /v1/projects/<project_id>/keys`. \
                     thetad will not serve without one — an instance that cannot \
                     verify tokens cannot safely accept any request.",
                    path.display(),
                    theta_core::PUBLIC_KEYSET_ENV,
                )
            })?;
            (path.display().to_string(), read)
        }
    };

    let wire: WireKeyset = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("the public keyset from {source} is not readable: {e}"))?;

    anyhow::ensure!(
        wire.project_id == project_id,
        "the keyset from {} is for project `{}`, but this instance serves `{}`",
        source,
        wire.project_id,
        project_id
    );

    Ok(PublicKeyset::from_wire(&wire)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_identity::ProjectKeys;

    fn wire(project_id: &str) -> WireKeyset {
        ProjectKeys::generate(project_id).public_keyset().to_wire()
    }

    #[test]
    fn a_keyset_delivered_in_the_environment_is_used_when_no_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = serde_json::to_string(&wire("proj")).expect("serialises");

        // The machine-platform case: nothing on the volume, everything in the
        // environment. Before this route existed, an instance here started and
        // then refused every request.
        resolve_public_keyset(&dir.path().join("keys.json"), "proj", Some(json))
            .expect("the environment is a delivery route, not a fallback");
    }

    #[test]
    fn the_environment_wins_over_a_file_left_behind_on_the_volume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.json");

        // A volume outlives a reprovision, so the file may be for a project this
        // instance no longer serves. The Control Plane set the variable for
        // *this* boot; the file is only evidence of some earlier one.
        std::fs::write(
            &path,
            serde_json::to_vec(&wire("an-older-project")).expect("serialises"),
        )
        .expect("write");

        let json = serde_json::to_string(&wire("proj")).expect("serialises");
        resolve_public_keyset(&path, "proj", Some(json))
            .expect("the stale file must not win over what was delivered for this boot");
    }

    #[test]
    fn a_file_is_still_read_when_nothing_was_delivered_in_the_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&wire("proj")).expect("serialises"),
        )
        .expect("write");

        resolve_public_keyset(&path, "proj", None)
            .expect("the shared-filesystem route must keep working");
    }

    #[test]
    fn an_empty_variable_is_not_a_delivery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&wire("proj")).expect("serialises"),
        )
        .expect("write");

        // A platform that sets every declared variable, empty ones included,
        // must not thereby hide a perfectly good keyset on disk.
        resolve_public_keyset(&path, "proj", Some("   ".into()))
            .expect("an empty variable should fall through to the file");
    }

    #[test]
    fn a_keyset_for_another_project_is_refused_whichever_route_it_came_by() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("keys.json");
        let wrong = serde_json::to_string(&wire("someone-else")).expect("serialises");

        let err = resolve_public_keyset(&path, "proj", Some(wrong))
            .expect_err("a keyset for another project authorizes the wrong tokens");
        let message = err.to_string();
        assert!(
            message.contains("someone-else") && message.contains("proj"),
            "the error should name both projects, said: {message}"
        );
    }

    #[test]
    fn no_keyset_at_all_names_both_routes_rather_than_only_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");

        let err = resolve_public_keyset(&dir.path().join("keys.json"), "proj", None)
            .expect_err("an instance with no keys cannot serve");
        let message = err.to_string();
        assert!(
            message.contains(theta_core::PUBLIC_KEYSET_ENV),
            "an operator told only about the file will look in the wrong place on \
             a machine platform, said: {message}"
        );
    }
}
