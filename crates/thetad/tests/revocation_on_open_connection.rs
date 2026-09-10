//! Review finding R1-01 — revocation and expiry are checked only at handshake.
//!
//! `04-threat-model-security.md` §2 promises a leaked token is revocable within
//! one heartbeat (<5s). The existing suite (`revocation.rs`) proves that only for
//! the *reconnect* path: every case calls `Client::connect` again, i.e. a fresh
//! handshake. This file holds a single connection open across a revocation and
//! issues an in-band request on it — the case the guarantee actually has to cover,
//! because the wire protocol is a persistent connection.
//!
//! **Fixed.** When this file arrived, the secure test below failed and a companion
//! `demonstrates_the_live_vulnerability` passed, having captured a revoked token
//! reading *and committing a write* — `Put { commit_id: "cdb8eb19…" }` — on a
//! connection that was open when the revocation landed.
//!
//! `server.rs` now re-authorizes before every request rather than once at
//! handshake, so the characterisation test began failing and has been retired as
//! its author instructed. What it demonstrated is recorded here because the
//! commit hash is the whole argument: this was not a theoretical gap.

use std::net::SocketAddr;

use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, RevocationList, SessionToken, SignedRevocationList, TokenScope};
use theta_proto::{frame, Hello, Request, RequestBody, Response, ResponseBody, Welcome};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Harness {
    addr: SocketAddr,
    keys: ProjectKeys,
    _dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::dev_default("test-project");
        config.data_dir = dir.path().to_path_buf();
        config.listen = "127.0.0.1:0".parse().expect("addr");

        let server_config = ServerConfig::from_config(&config);
        let keys = ProjectKeys::generate("test-project");
        let authorizer = std::sync::Arc::new(tokio::sync::RwLock::new(Authorizer::new(
            keys.public_keyset(),
            Environment::Dev,
            60_000,
            now_ms(),
        )));

        let engine = Engine::open(config).expect("open engine");
        let handle = EngineHandle::spawn(engine, server_config.queue_depth);

        let (ready, bound) = tokio::sync::oneshot::channel();
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(serve(
            server_config,
            handle,
            authorizer,
            ready,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        Self {
            addr: bound.await.expect("bound"),
            keys,
            _dir: dir,
            shutdown: Some(shutdown),
        }
    }

    fn token_for(&self, token_id: &str, session_id: &str) -> SessionToken {
        SessionToken::mint(
            &self.keys,
            &TokenScope {
                token_id: token_id.into(),
                project_id: "test-project".into(),
                environment: "dev".into(),
                session_id: session_id.into(),
                user_id: "u_test".into(),
                org_id: "org_test".into(),
                issued_at_ms: 0,
                expires_at_ms: i64::MAX,
                key_id: KeyId::new("test-project-k1"),
                signing_key: None,
            },
        )
    }

    async fn push_revoke_token(&self, token_id: &str) -> ResponseBody {
        let mut list = RevocationList::new();
        list.revoke_token(token_id);
        let signed = SignedRevocationList::sign(&self.keys, &list);
        // The push comes in on its own connection, exactly as the Control Plane
        // heartbeat does; it updates the shared Authorizer.
        let mut admin = Client::connect(self.addr, self.token_for("tok_admin", "sess_admin"))
            .await
            .expect("admin connect");
        admin
            .call(RequestBody::PushRevocations {
                payload: signed.payload,
                signature: signed.signature,
                key_id: signed.key_id.0,
            })
            .await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

struct Client {
    stream: TcpStream,
    next_id: u64,
}

impl Client {
    async fn connect(addr: SocketAddr, token: SessionToken) -> Result<Self, Box<Response>> {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let hello = Hello {
            protocol_version: theta_proto::PROTOCOL_VERSION,
            session_token: token.into_string(),
            client_name: "r1-01-test".into(),
        };
        write(&mut stream, &hello.encode()).await;
        let payload = read(&mut stream).await.expect("reply");
        match Welcome::decode(&payload) {
            Ok(_) => Ok(Self { stream, next_id: 1 }),
            Err(_) => Err(Box::new(Response::decode(&payload).expect("a refusal"))),
        }
    }

    async fn call(&mut self, body: RequestBody) -> ResponseBody {
        let request_id = self.next_id;
        self.next_id += 1;
        write(
            &mut self.stream,
            &Request {
                request_id,
                branch_id: 0,
                body,
            }
            .encode(),
        )
        .await;
        let payload = read(&mut self.stream).await.expect("response");
        Response::decode(&payload).expect("decode").body
    }
}

async fn write(stream: &mut TcpStream, payload: &[u8]) {
    stream
        .write_all(&frame::frame(payload).expect("frame"))
        .await
        .expect("write");
    stream.flush().await.expect("flush");
}

async fn read(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).await.ok()?;
    let size = frame::decode_length(prefix).ok()?;
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn served(body: &ResponseBody) -> bool {
    !matches!(body, ResponseBody::Error(_))
}

/// The secure property `04-threat-model-security.md` §2 claims, now guarded.
///
/// Holds one connection open across a revocation and issues an in-band request
/// on it — the case the guarantee has to cover, because the wire protocol is a
/// persistent connection and the existing `revocation.rs` suite proves the
/// property only for the reconnect path.
#[tokio::test]
async fn a_revocation_stops_an_open_connection_within_the_budget() {
    let h = Harness::start().await;
    let token = h.token_for("tok_leaked2", "sess_2");

    let mut conn = Client::connect(h.addr, token).await.expect("connect");
    assert!(served(
        &conn.call(RequestBody::Get { key: "k".into() }).await
    ));

    h.push_revoke_token("tok_leaked2").await;

    let after = conn.call(RequestBody::Get { key: "k".into() }).await;
    assert!(
        matches!(after, ResponseBody::Error(_)),
        "a revoked token was still served on an already-open connection: {after:?} \
         (R1-01: revocation is only checked at handshake, never per-request)"
    );
}
