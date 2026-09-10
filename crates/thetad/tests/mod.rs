//! Shared scaffolding for the end-to-end suites.
//!
//! Extracted from `server.rs` when a second suite needed it. Each integration
//! test file is its own crate, so the alternative was a second copy — and two
//! copies of a test harness drift, which means two suites that appear to be
//! testing the same server against the same client and are not.

#![allow(dead_code)]

use std::net::SocketAddr;

use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use theta_proto::frame;
use theta_proto::wire::WireError;
use theta_proto::{Hello, Request, RequestBody, Response, ResponseBody, Welcome, PROTOCOL_VERSION};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A running server plus the scaffolding that keeps it alive.
pub struct Harness {
    pub addr: SocketAddr,
    /// The project's signing keys, so tests can mint tokens the server will
    /// accept — and ones it must reject.
    pub keys: ProjectKeys,
    pub _dir: tempfile::TempDir,
    pub shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// A server whose safety policy is production-strict.
    ///
    /// The shadow threshold is what decides whether a drop is confirmable, so a
    /// test about the gate that confirmation cannot clear has to say which
    /// policy it means.
    pub async fn start_protected() -> Self {
        Self::build(theta_safety::SafetyPolicy::protected(), |_| {}).await
    }

    pub async fn start_with(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::build(thetad::config::Environment::Dev.default_policy(), tweak).await
    }

    pub async fn build(
        safety: theta_safety::SafetyPolicy,
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::dev_default("test-project");
        config.data_dir = dir.path().to_path_buf();
        config.safety = safety;
        // Port zero: the OS picks a free port, so tests never collide.
        config.listen = "127.0.0.1:0".parse().expect("addr");

        let mut server_config = ServerConfig::from_config(&config);
        tweak(&mut server_config);

        let keys = ProjectKeys::generate("test-project");
        // The server gets public keys only — it can verify, never mint. The
        // authorizer is seeded as freshly synced, standing in for the Control
        // Plane heartbeat these tests do not run.
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

        let addr = bound.await.expect("server bound");
        Self {
            addr,
            keys,
            _dir: dir,
            shutdown: Some(shutdown),
        }
    }

    /// A token this server will accept.
    pub fn token(&self) -> String {
        SessionToken::mint(&self.keys, &scope("test-project", "dev")).into_string()
    }

    /// A connected client, for the common case where the handshake is not what
    /// is under test.
    pub async fn client(&self) -> Client {
        Client::connect(self.addr, self.token())
            .await
            .expect("handshake")
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn scope(project: &str, environment: &str) -> TokenScope {
    TokenScope {
        token_id: "tok_test".into(),
        project_id: project.into(),
        environment: environment.into(),
        session_id: "sess_test".into(),
        user_id: "u_test".into(),
        org_id: "org_test".into(),
        issued_at_ms: 0,
        expires_at_ms: i64::MAX,
        key_id: KeyId::new(format!("{project}-k1")),
        // This session does not sign its writes.
        signing_key: None,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// A connected client that has completed the handshake.
#[derive(Debug)]
pub struct Client {
    pub stream: TcpStream,
    next_id: u64,
}

impl Client {
    pub async fn connect(addr: SocketAddr, token: String) -> Result<Self, Box<Response>> {
        Self::connect_with(addr, PROTOCOL_VERSION, token)
            .await
            .map(|(c, _)| c)
    }

    pub async fn connect_with(
        addr: SocketAddr,
        version: u32,
        session_token: String,
    ) -> Result<(Self, Welcome), Box<Response>> {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let hello = Hello {
            protocol_version: version,
            session_token,
            client_name: "test-client".into(),
        };
        write(&mut stream, &hello.encode()).await;

        let payload = read(&mut stream).await.expect("server replied to Hello");
        match Welcome::decode(&payload) {
            Ok(welcome) => Ok((Self { stream, next_id: 1 }, welcome)),
            // A refusal comes back as an error Response on the same connection.
            Err(_) => Err(Box::new(
                Response::decode(&payload).expect("refusal is a Response"),
            )),
        }
    }

    pub async fn call(&mut self, body: RequestBody) -> ResponseBody {
        self.call_on(0, body).await
    }

    pub async fn call_on(&mut self, branch_id: u64, body: RequestBody) -> ResponseBody {
        let request_id = self.next_id;
        self.next_id += 1;

        let request = Request {
            request_id,
            branch_id,
            body,
        };
        write(&mut self.stream, &request.encode()).await;

        let payload = read(&mut self.stream).await.expect("response");
        let response = Response::decode(&payload).expect("decode response");
        assert_eq!(
            response.request_id, request_id,
            "response echoed the wrong request id"
        );
        response.body
    }
}

pub async fn write(stream: &mut TcpStream, payload: &[u8]) {
    let framed = frame::frame(payload).expect("frame");
    stream.write_all(&framed).await.expect("write");
    stream.flush().await.expect("flush");
}

pub async fn read(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).await.ok()?;
    let size = frame::decode_length(prefix).ok()?;
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

pub fn expect_error(body: ResponseBody) -> WireError {
    match body {
        ResponseBody::Error(e) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

pub fn json(value: i64) -> String {
    serde_json::to_string(&theta_core::Value::Int(value)).expect("encode")
}
