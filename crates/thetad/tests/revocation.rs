//! Revocation propagation, end to end over a real connection.
//!
//! The gate `04-threat-model-security.md` §2 sets: a leaked token must be
//! revocable instantly, reaching instances within one heartbeat (target: under
//! five seconds). These tests measure that rather than assuming it.

use std::net::SocketAddr;
use std::time::Instant;

use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, RevocationList, SessionToken, SignedRevocationList, TokenScope};
use theta_proto::{
    frame, Hello, Request, RequestBody, Response, ResponseBody, StatusCode, Welcome,
};
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
                // This session does not sign its writes.
                signing_key: None,
            },
        )
    }

    /// Push a signed revocation list, as the Control Plane would.
    async fn push(&self, list: &RevocationList) -> ResponseBody {
        let signed = SignedRevocationList::sign(&self.keys, list);
        let mut client = Client::connect(self.addr, self.token_for("tok_admin", "sess_admin"))
            .await
            .expect("connect");
        client
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

#[derive(Debug)]
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
            client_name: "revocation-test".into(),
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

// ---- the gate ---------------------------------------------------------------

#[tokio::test]
async fn a_revoked_token_stops_working_within_the_heartbeat_budget() {
    let h = Harness::start().await;
    let token = h.token_for("tok_leaked", "sess_1");

    // It works before the revocation.
    assert!(Client::connect(h.addr, token.clone()).await.is_ok());

    let started = Instant::now();
    let mut list = RevocationList::new();
    list.revoke_token("tok_leaked");
    let accepted = h.push(&list).await;

    assert!(
        matches!(accepted, ResponseBody::Revocations { accepted: true, .. }),
        "the push was not accepted: {accepted:?}"
    );

    // And it is refused immediately afterwards.
    let refusal = Client::connect(h.addr, token)
        .await
        .expect_err("a revoked token must not connect");
    let elapsed = started.elapsed();

    match refusal.body {
        ResponseBody::Error(e) => assert_eq!(e.code, StatusCode::Unauthorized),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "revocation took {elapsed:?}, past the 5s propagation budget"
    );
}

#[tokio::test]
async fn revoking_a_session_covers_every_token_it_issued() {
    let h = Harness::start().await;
    let first = h.token_for("tok_a", "sess_compromised");
    let second = h.token_for("tok_b", "sess_compromised");

    let mut list = RevocationList::new();
    list.revoke_session("sess_compromised");
    h.push(&list).await;

    // An agent session going bad withdraws every credential it holds, without
    // having to enumerate them.
    assert!(Client::connect(h.addr, first).await.is_err());
    assert!(Client::connect(h.addr, second).await.is_err());
}

#[tokio::test]
async fn revoking_an_org_covers_tokens_for_its_projects() {
    let h = Harness::start().await;
    let token = h.token_for("tok_x", "sess_x");

    let mut list = RevocationList::new();
    list.revoke_org("org_test");
    h.push(&list).await;

    assert!(Client::connect(h.addr, token).await.is_err());
}

#[tokio::test]
async fn an_unrevoked_token_keeps_working_after_a_push() {
    let h = Harness::start().await;
    let survivor = h.token_for("tok_fine", "sess_fine");

    let mut list = RevocationList::new();
    list.revoke_token("tok_someone_else");
    h.push(&list).await;

    assert!(
        Client::connect(h.addr, survivor).await.is_ok(),
        "a revocation must not withdraw unrelated credentials"
    );
}

#[tokio::test]
async fn a_list_signed_by_another_project_is_refused() {
    // Otherwise anyone holding any project key could push an empty list to
    // every instance and un-revoke the world.
    let h = Harness::start().await;
    let attacker = ProjectKeys::generate("attacker-project");

    let mut list = RevocationList::new();
    list.revoke_token("tok_anything");
    let signed = SignedRevocationList::sign(&attacker, &list);

    let mut client = Client::connect(h.addr, h.token_for("tok_admin", "sess_admin"))
        .await
        .expect("connect");
    let response = client
        .call(RequestBody::PushRevocations {
            payload: signed.payload,
            signature: signed.signature,
            key_id: signed.key_id.0,
        })
        .await;

    match response {
        ResponseBody::Error(e) => assert_eq!(e.code, StatusCode::Unauthorized),
        other => panic!("a foreign revocation list was accepted: {other:?}"),
    }
}

#[tokio::test]
async fn a_tampered_list_is_refused() {
    let h = Harness::start().await;

    let mut list = RevocationList::new();
    list.revoke_token("tok_leaked");
    let signed = SignedRevocationList::sign(&h.keys, &list);

    // An attacker in the middle stripping the revocation.
    let emptied = serde_json::to_vec(&RevocationList::new()).expect("encode");

    let mut client = Client::connect(h.addr, h.token_for("tok_admin", "sess_admin"))
        .await
        .expect("connect");
    let response = client
        .call(RequestBody::PushRevocations {
            payload: emptied,
            signature: signed.signature,
            key_id: signed.key_id.0,
        })
        .await;

    assert!(
        matches!(response, ResponseBody::Error(_)),
        "an edited revocation list was accepted: {response:?}"
    );
}

#[tokio::test]
async fn an_older_list_cannot_undo_a_revocation() {
    let h = Harness::start().await;
    let token = h.token_for("tok_leaked", "sess_1");

    let mut current = RevocationList::new();
    current.revoke_token("tok_leaked");
    current.version = 10;
    h.push(&current).await;
    assert!(Client::connect(h.addr, token.clone()).await.is_err());

    // An out-of-order delivery of an older, empty list must not resurrect it.
    let mut older = RevocationList::new();
    older.version = 2;
    let response = h.push(&older).await;
    match response {
        ResponseBody::Revocations { accepted, version } => {
            assert!(!accepted, "an older list must not be applied");
            assert_eq!(version, 10, "the instance keeps the newer list");
        }
        other => panic!("expected a revocation response, got {other:?}"),
    }

    assert!(
        Client::connect(h.addr, token).await.is_err(),
        "an older list un-revoked a token"
    );
}

#[tokio::test]
async fn the_instance_reports_the_version_it_holds_so_propagation_is_measurable() {
    let h = Harness::start().await;

    let mut list = RevocationList::new();
    list.revoke_token("a");
    list.revoke_token("b");
    let expected = list.version;

    match h.push(&list).await {
        ResponseBody::Revocations { version, accepted } => {
            assert!(accepted);
            // A caller polls until instances report the version it revoked at,
            // which is what turns propagation into something measurable rather
            // than assumed.
            assert_eq!(version, expected);
        }
        other => panic!("expected a revocation response, got {other:?}"),
    }
}
