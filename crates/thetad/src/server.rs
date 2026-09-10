//! The TCP server: framing, handshake, and backpressure.
//!
//! Everything about what a request *means* lives in [`crate::dispatch`]. This
//! module owns only what needs a network.
//!
//! Two limits, for two different failure modes:
//!
//! * **Connection limit** — bounds memory and file descriptors. A client over
//!   the limit is refused at accept, immediately, rather than accepted and
//!   starved.
//! * **Queue depth** (in [`crate::service`]) — bounds pending work. A caller
//!   over that limit gets a `Busy` response and can retry.

use std::net::SocketAddr;
use std::sync::Arc;

use theta_proto::frame::{self, LENGTH_PREFIX_BYTES};
use theta_proto::wire::WireError;
use theta_proto::{
    negotiate, Hello, Request, RequestBody, Response, ResponseBody, StatusCode, Welcome,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::now_ms;
use crate::service::EngineHandle;
use crate::session::Authorizer;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Connections served at once. Beyond this, new connections are refused.
    pub max_connections: usize,
    /// Requests that may be queued for the engine before callers see `Busy`.
    pub queue_depth: usize,
    pub project_id: String,
    pub environment: crate::config::Environment,
}

impl ServerConfig {
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            listen: config.listen,
            max_connections: 512,
            queue_depth: 1_024,
            project_id: config.project_id.clone(),
            environment: config.environment,
        }
    }
}

/// Serve until `shutdown` resolves.
///
/// Returns the address actually bound, via `ready`, before serving — so a test
/// can bind port 0 and still know where to connect.
pub async fn serve(
    config: ServerConfig,
    engine: EngineHandle,
    authorizer: Arc<tokio::sync::RwLock<Authorizer>>,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    let bound = listener.local_addr()?;
    let _ = ready.send(bound);

    tracing::info!(
        %bound,
        project = %config.project_id,
        environment = ?config.environment,
        max_connections = config.max_connections,
        "thetad listening"
    );

    let permits = Arc::new(Semaphore::new(config.max_connections));
    let config = Arc::new(config);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown requested; no longer accepting connections");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        continue;
                    }
                };

                // Refuse rather than queue: a connection accepted and then
                // starved is worse than one that was never accepted, because
                // the client cannot tell the difference from a hang.
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::warn!(%peer, "connection limit reached; refusing");
                    drop(stream);
                    continue;
                };

                let engine = engine.clone();
                let config = Arc::clone(&config);
                let authorizer = Arc::clone(&authorizer);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, engine, &config, &authorizer).await {
                        tracing::debug!(%peer, error = %e, "connection ended");
                    }
                    drop(permit);
                });
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    engine: EngineHandle,
    config: &ServerConfig,
    authorizer: &tokio::sync::RwLock<Authorizer>,
) -> std::io::Result<()> {
    // Nagle's algorithm batches small writes, which is exactly wrong for a
    // request/response protocol chasing single-digit-millisecond latencies.
    stream.set_nodelay(true)?;

    // The guard is taken per check and released immediately, so a heartbeat
    // updating the revocation list is never blocked behind a long-lived
    // connection.
    let authorized = match handshake(&mut stream, config, &*authorizer.read().await).await {
        Ok(authorized) => authorized,
        Err(refusal) => {
            // Say why, then close. A silent drop is indistinguishable from a
            // network fault and sends the client into a retry loop.
            let _ = write_frame(&mut stream, &refusal.encode()).await;
            return Ok(());
        }
    };

    loop {
        let payload = match read_frame(&mut stream).await {
            Ok(Some(payload)) => payload,
            Ok(None) => return Ok(()), // clean close
            Err(e) => {
                tracing::debug!(error = %e, "framing error; closing connection");
                return Ok(());
            }
        };

        let response = match Request::decode(&payload) {
            // A revocation push is answered here rather than by the engine: it
            // changes what the *connection layer* will accept, and routing it
            // through the engine queue would let a backlog of writes delay a
            // revocation.
            Ok(Request {
                request_id,
                body:
                    RequestBody::PushRevocations {
                        payload,
                        signature,
                        key_id,
                    },
                ..
            }) => apply_revocations(authorizer, request_id, payload, signature, key_id).await,

            // **Re-authorized before every request, not once at handshake.**
            //
            // The wire protocol is a persistent connection, so authorizing only
            // at handshake meant a token stayed good for the life of the
            // connection: revocation reached the instance, updated the shared
            // list, and was never consulted again. A leaked token plus one held
            // connection defeated both "revocation propagates within one
            // heartbeat" and "short token lifetime limits the exposure window"
            // (`04-threat-model-security.md` 2 and 4). An external review
            // demonstrated it committing a write after revocation.
            //
            // The full check runs, not just the revocation lookup: expiry and
            // the signing key's validity change over a connection's life too,
            // and a partial re-check is a list somebody has to keep in sync with
            // `authorize`.
            Ok(request) => {
                let request_id = request.request_id;
                match authorizer
                    .read()
                    .await
                    .authorize(&authorized.token, now_ms())
                {
                    Ok(scope) => engine.call(request, scope, now_ms()).await,
                    Err(e) => {
                        // The credential is no longer good, so the connection
                        // does not continue. Answering this one request and
                        // leaving the socket open would let the holder keep
                        // trying, and every attempt would be one more request
                        // served after revocation.
                        tracing::warn!(error = %e, "closing a connection whose token stopped being valid");
                        let refusal = Response {
                            request_id,
                            body: ResponseBody::Error(refusal_for(&e)),
                        };
                        let _ = write_frame(&mut stream, &refusal.encode()).await;
                        return Ok(());
                    }
                }
            }
            Err(e) => Response {
                // A request that will not decode has no id to echo, so zero
                // stands for "this connection sent something unparseable".
                request_id: 0,
                body: ResponseBody::Error(WireError {
                    code: StatusCode::Rejected,
                    message: format!("malformed request: {e}"),
                    diff: None,
                }),
            },
        };

        write_frame(&mut stream, &response.encode()).await?;
    }
}

/// Exchange `Hello`/`Welcome`, or produce the refusal to send back.
///
/// The refusal is boxed because it is the rare path: `Response` carries a
/// schema diff and is far larger than a `TokenScope`, so returning it unboxed
/// would make every successful handshake pay for the shape of a failed one.
/// A credential that passed the handshake.
///
/// Holds the *token* and deliberately **not** the scope it produced. A scope is
/// the answer to a question asked at one instant; keeping it is what made
/// authorization a one-time event, because every later request then had an
/// answer available without asking. The token is what lets the question be asked
/// again, and the connection loop asks it every time.
struct Authorized {
    token: String,
}

/// How an authorization failure is reported to a caller.
///
/// Shared by the handshake and the per-request re-check so the two cannot drift:
/// a caller must not be able to tell "refused at handshake" from "refused
/// mid-connection" by the shape of the answer, and an operator must not have to
/// read two mappings to know what a code means.
fn refusal_for(error: &crate::session::AuthError) -> WireError {
    let (code, message) = match error {
        // The instance cannot currently tell whether the token was revoked,
        // which is an infrastructure problem the client should retry through
        // rather than a credential problem it should re-authenticate for.
        crate::session::AuthError::RevocationStale { .. } => (
            StatusCode::Internal,
            "revocation state is stale; refusing to authorize",
        ),
        _ => (StatusCode::Unauthorized, "invalid session token"),
    };
    WireError {
        code,
        message: message.to_string(),
        diff: None,
    }
}

async fn handshake(
    stream: &mut TcpStream,
    config: &ServerConfig,
    authorizer: &Authorizer,
) -> Result<Authorized, Box<Response>> {
    let refuse = |code: StatusCode, message: String| {
        Box::new(Response {
            request_id: 0,
            body: ResponseBody::Error(WireError {
                code,
                message,
                diff: None,
            }),
        })
    };

    let payload = match read_frame(stream).await {
        Ok(Some(payload)) => payload,
        _ => {
            return Err(refuse(
                StatusCode::Rejected,
                "expected a Hello frame".into(),
            ))
        }
    };

    let hello = Hello::decode(&payload)
        .map_err(|e| refuse(StatusCode::Rejected, format!("malformed Hello: {e}")))?;

    // Refuse an incompatible pairing rather than guessing at a shared subset
    // (`02-api-wire-protocol.md` §5).
    negotiate(hello.protocol_version).map_err(|e| refuse(StatusCode::Rejected, e.to_string()))?;

    // Signature, environment scope and revocation, in that order. A token that
    // does not verify has told us nothing, so nothing in its payload is acted
    // on — see `session::Authorizer`.
    let scope = authorizer
        .authorize(&hello.session_token, now_ms())
        .map_err(|e| {
            // Logged in full, returned terse. A detailed reason tells an
            // attacker which half of a guessed token was wrong; an operator
            // reading logs needs the detail.
            tracing::warn!(error = %e, "rejected a session token");
            let wire = refusal_for(&e);
            refuse(wire.code, wire.message)
        })?;

    // The scope is what proved the token good just now; it is not carried
    // forward. The next request re-derives it from the token, against the
    // revocation list as it stands then.
    drop(scope);
    let authorized = Authorized {
        token: hello.session_token.clone(),
    };

    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        project_id: config.project_id.clone(),
        server_name: concat!("thetad/", env!("CARGO_PKG_VERSION")).to_string(),
    };
    write_frame(stream, &welcome.encode())
        .await
        .map_err(|e| refuse(StatusCode::Internal, e.to_string()))?;

    Ok(authorized)
}

/// Read one frame. `Ok(None)` means the peer closed cleanly between frames.
async fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    match stream.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    // Checked before allocating, so a four-byte prefix cannot make the server
    // reserve four gigabytes.
    let size = frame::decode_length(prefix)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let framed = frame::frame(payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    stream.write_all(&framed).await?;
    stream.flush().await
}

/// Verify and apply a pushed revocation list.
///
/// The list is signed with the project key, so this needs no separate
/// authentication: an instance verifies it with the same public key it uses for
/// tokens. That is also why the instance needs no HTTP client — the Control
/// Plane pushes, and the hot-path dependency guard stays intact.
async fn apply_revocations(
    authorizer: &tokio::sync::RwLock<Authorizer>,
    request_id: u64,
    payload: Vec<u8>,
    signature: Vec<u8>,
    key_id: String,
) -> Response {
    let signed = theta_identity::SignedRevocationList {
        payload,
        signature,
        key_id: theta_identity::KeyId::new(key_id),
    };

    let mut guard = authorizer.write().await;
    let list = match guard.verify_revocations(&signed) {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "rejected a pushed revocation list");
            return Response {
                request_id,
                body: ResponseBody::Error(WireError {
                    code: StatusCode::Unauthorized,
                    message: "revocation list signature is not valid".into(),
                    diff: None,
                }),
            };
        }
    };

    let version = list.version;
    let accepted = guard.revocations_mut().apply(list, now_ms());
    tracing::info!(version, accepted, "applied a pushed revocation list");

    Response {
        request_id,
        body: ResponseBody::Revocations {
            version: guard.revocations().version(),
            accepted,
        },
    }
}
