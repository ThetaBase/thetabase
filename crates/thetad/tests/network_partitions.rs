//! Network partitions — the half of the M1 gate that was blocked on there
//! being a server (`docs/specs/08-test-validation-plan.md` §2, and the
//! `ROADMAP` M1/M2 entries naming a fault-injecting transport).
//!
//! The faults are injected *in front of* the server rather than inside it. A
//! partition is a property of the network, not of the process, and modelling it
//! in the transport would mean the production read/write path carried code that
//! exists only for tests — on a hot path whose dependency closure is itself
//! asserted (`crates/thetad/tests/no_llm_on_hot_path.rs`). So these tests drive
//! a real `thetad` over a real socket, through a proxy that can hold or sever
//! traffic in either direction.
//!
//! The distinction the proxy draws, and the reason it is not simply a `drop`:
//!
//! * **Blackhole** holds bytes without closing. The peer sees silence, which is
//!   what a partition looks like and is *not* what a closed socket looks like.
//!   A client can react to a close; it can only time out on silence.
//! * **Sever** closes. This is the partition that outlives TCP's patience.
//! * **DropServerToClient** is asymmetric: the request lands, the
//!   acknowledgement never comes back. This is the case that decides whether an
//!   unacknowledged write was applied, and it is the one most likely to be got
//!   wrong.
//!
//! The randomized test is deterministic on purpose. A Jepsen-style suite that
//! cannot reproduce its own failure reports a bug nobody can fix.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use theta_proto::frame;
use theta_proto::{Hello, Request, RequestBody, Response, ResponseBody, Welcome, PROTOCOL_VERSION};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---- the fault-injecting transport -----------------------------------------

const OPEN: u8 = 0;
const BLACKHOLE: u8 = 1;
const DROP_S2C: u8 = 2;
const SEVER: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    ClientToServer,
    ServerToClient,
}

/// A TCP proxy that can partition the link it carries.
struct Partitioner {
    addr: SocketAddr,
    fault: Arc<AtomicU8>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Partitioner {
    async fn in_front_of(upstream: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let fault = Arc::new(AtomicU8::new(OPEN));
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();

        let accept_fault = Arc::clone(&fault);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => return,
                    accepted = listener.accept() => {
                        let Ok((downstream, _)) = accepted else { return };
                        let fault = Arc::clone(&accept_fault);
                        tokio::spawn(async move {
                            let Ok(up) = TcpStream::connect(upstream).await else { return };
                            let (dr, dw) = downstream.into_split();
                            let (ur, uw) = up.into_split();
                            let a = tokio::spawn(pump(
                                dr, uw, Arc::clone(&fault), Direction::ClientToServer,
                            ));
                            let b = tokio::spawn(pump(
                                ur, dw, fault, Direction::ServerToClient,
                            ));
                            let _ = a.await;
                            let _ = b.await;
                        });
                    }
                }
            }
        });

        Self {
            addr,
            fault,
            _shutdown: shutdown,
        }
    }

    fn heal(&self) {
        self.fault.store(OPEN, Ordering::SeqCst);
    }
    fn blackhole(&self) {
        self.fault.store(BLACKHOLE, Ordering::SeqCst);
    }
    fn drop_responses(&self) {
        self.fault.store(DROP_S2C, Ordering::SeqCst);
    }
    fn sever(&self) {
        self.fault.store(SEVER, Ordering::SeqCst);
    }
}

/// Is this direction currently held?
fn blocked(fault: u8, direction: Direction) -> bool {
    match fault {
        BLACKHOLE => true,
        DROP_S2C => direction == Direction::ServerToClient,
        _ => false,
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    fault: Arc<AtomicU8>,
    direction: Direction,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        // Poll for a sever even while no bytes are moving. A partition that only
        // takes effect on the next byte is not a partition — the connection this
        // test wants closed would sit open until someone happened to write.
        let n = loop {
            if fault.load(Ordering::SeqCst) == SEVER {
                return;
            }
            tokio::select! {
                // `read` is cancel-safe, so losing this race drops no bytes.
                r = from.read(&mut buf) => match r {
                    Ok(0) | Err(_) => return,
                    Ok(n) => break n,
                },
                _ = tokio::time::sleep(Duration::from_millis(5)) => continue,
            }
        };

        // Hold, do not discard. TCP retransmits across a heartbeat-length
        // partition, so a link that heals delivers what was in flight; a link
        // that is severed never gets here.
        loop {
            let f = fault.load(Ordering::SeqCst);
            if f == SEVER {
                return;
            }
            if blocked(f, direction) {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
            break;
        }

        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

// ---- server harness ---------------------------------------------------------

struct Harness {
    addr: SocketAddr,
    keys: ProjectKeys,
    _dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    async fn start_with(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::dev_default("test-project");
        config.data_dir = dir.path().to_path_buf();
        config.listen = "127.0.0.1:0".parse().expect("addr");

        let mut server_config = ServerConfig::from_config(&config);
        tweak(&mut server_config);

        let keys = ProjectKeys::generate("test-project");
        let authorizer = Arc::new(tokio::sync::RwLock::new(Authorizer::new(
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

    fn token(&self) -> String {
        SessionToken::mint(&self.keys, &scope("test-project", "dev")).into_string()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn scope(project: &str, environment: &str) -> TokenScope {
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

fn json(value: i64) -> String {
    serde_json::to_string(&theta_core::Value::Int(value)).expect("encode")
}

// ---- client -----------------------------------------------------------------

struct Client {
    stream: TcpStream,
    next_id: u64,
}

impl Client {
    async fn connect(addr: SocketAddr, token: String) -> Option<Self> {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            session_token: token,
            client_name: "partition-test".into(),
        };
        write(&mut stream, &hello.encode()).await.ok()?;
        let payload = read(&mut stream).await?;
        Welcome::decode(&payload).ok()?;
        Some(Self { stream, next_id: 1 })
    }

    /// `None` means no response arrived — which under partition is the answer,
    /// not a failure.
    ///
    /// **Not cancel-safe.** Dropping this future after the request is written
    /// leaves a response that nobody will claim, and the next call on this
    /// connection reads it and finds a request id that does not match. A caller
    /// that abandons a call — every `silenced(...)` here does — must abandon the
    /// connection with it.
    async fn call(&mut self, body: RequestBody) -> Option<ResponseBody> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = Request {
            request_id,
            branch_id: 0,
            body,
        };
        write(&mut self.stream, &request.encode()).await.ok()?;
        let payload = read(&mut self.stream).await?;
        let response = Response::decode(&payload).ok()?;
        assert_eq!(response.request_id, request_id, "wrong request id echoed");
        Some(response.body)
    }

    async fn put(&mut self, key: &str, value: i64) -> Option<ResponseBody> {
        self.call(RequestBody::Put {
            key: key.into(),
            value_json: json(value),
            ttl: 0,
        })
        .await
    }

    async fn get(&mut self, key: &str) -> Option<(bool, String)> {
        match self.call(RequestBody::Get { key: key.into() }).await? {
            ResponseBody::Get {
                found, value_json, ..
            } => Some((found, value_json)),
            other => panic!("expected a get response, got {other:?}"),
        }
    }
}

async fn write(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let framed = frame::frame(payload).expect("frame");
    stream.write_all(&framed).await?;
    stream.flush().await
}

async fn read(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).await.ok()?;
    let size = frame::decode_length(prefix).ok()?;
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

/// Two timeouts, because the two questions are not the same question.
///
/// `SILENCE` bounds how long to wait before concluding a partition swallowed a
/// call. It is short: waiting longer only makes the suite slower, and a call
/// that was going to be swallowed stays swallowed.
///
/// `PATIENCE` bounds a call that *must* succeed. It is long, and deliberately
/// so — these tests run twelve servers at once inside a gate that runs beside
/// clippy on a shared CI runner, and a healthy request there can take orders of
/// magnitude longer than a healthy request on an idle laptop. Using one timeout
/// for both questions is what makes a partition suite flaky, and a gate that
/// fails under load teaches people to re-run CI until it passes.
const SILENCE: Duration = Duration::from_millis(400);
const PATIENCE: Duration = Duration::from_secs(20);

/// Expect a partition to swallow this. `None` is the pass.
async fn silenced<T>(f: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(SILENCE, f).await.ok()
}

/// Expect this to complete. `None` means it genuinely did not.
async fn patiently<T>(f: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(PATIENCE, f).await.ok()
}

/// Poll until `f` yields a value or `PATIENCE` runs out.
///
/// Replaces sleeping for a fixed interval and hoping. What these call sites are
/// waiting for — the server noticing a peer is gone and releasing its permit —
/// has no upper bound that a constant can honestly express.
async fn eventually<T, F, Fut>(mut f: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if let Some(value) = f().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---- tests ------------------------------------------------------------------

#[tokio::test]
async fn an_acknowledged_write_survives_a_partition_that_severs_the_connection() {
    let h = Harness::start().await;
    let net = Partitioner::in_front_of(h.addr).await;
    let mut client = Client::connect(net.addr, h.token()).await.expect("connect");

    let ack = client.put("survivor", 42).await;
    assert!(
        matches!(ack, Some(ResponseBody::Put { .. })),
        "the write was not acknowledged: {ack:?}"
    );

    // Acknowledged, then the network fails underneath it.
    net.sever();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        silenced(client.get("survivor")).await.flatten().is_none(),
        "a severed link must not deliver a response"
    );

    net.heal();
    let mut reconnected = Client::connect(net.addr, h.token())
        .await
        .expect("reconnect after heal");
    let (found, value) = reconnected.get("survivor").await.expect("read after heal");

    // Read-your-writes for the writing session, across a partition: an
    // acknowledgement that does not survive the network is not an
    // acknowledgement (`specs/08` §2).
    assert!(found, "an acknowledged write was lost across a partition");
    assert_eq!(value, json(42));
}

#[tokio::test]
async fn a_write_whose_acknowledgement_the_network_swallowed_was_still_applied() {
    let h = Harness::start().await;
    let net = Partitioner::in_front_of(h.addr).await;
    let mut client = Client::connect(net.addr, h.token()).await.expect("connect");

    // Asymmetric: the request lands, the acknowledgement never returns. The
    // client cannot distinguish this from a write that never arrived, so the
    // durable state is the only thing that can answer.
    net.drop_responses();
    assert!(
        silenced(client.put("unacked", 7)).await.flatten().is_none(),
        "the acknowledgement should not have arrived"
    );

    // Abandon that connection entirely, so nothing buffered behind the
    // partition can be mistaken for a fresh reply.
    drop(client);
    net.sever();
    tokio::time::sleep(Duration::from_millis(50)).await;
    net.heal();

    let mut fresh = Client::connect(net.addr, h.token())
        .await
        .expect("reconnect");
    let (found, value) = fresh.get("unacked").await.expect("read after heal");
    assert!(
        found,
        "a request the server received was lost because its reply was not delivered"
    );
    assert_eq!(value, json(7));
}

#[tokio::test]
async fn a_blackholed_client_does_not_stop_the_server_serving_everyone_else() {
    let h = Harness::start().await;
    let net = Partitioner::in_front_of(h.addr).await;

    let mut partitioned = Client::connect(net.addr, h.token()).await.expect("connect");
    // Straight to the server: this client's network is fine, and it must stay
    // fine. Head-of-line blocking across connections would show up here.
    let mut healthy = Client::connect(h.addr, h.token()).await.expect("connect");

    net.blackhole();
    assert!(
        silenced(partitioned.put("stuck", 1))
            .await
            .flatten()
            .is_none(),
        "a blackholed call must not complete"
    );

    let ack = patiently(healthy.put("unaffected", 2))
        .await
        .flatten()
        .expect("an unpartitioned client must keep working");
    assert!(matches!(ack, ResponseBody::Put { .. }), "got {ack:?}");

    let (found, _) = patiently(healthy.get("unaffected"))
        .await
        .flatten()
        .expect("read back");
    assert!(found);
}

#[tokio::test]
async fn severed_connections_do_not_leak_the_connection_permits_they_held() {
    // Small on purpose: a leak of even a few permits is fatal at this size, and
    // invisible at the default of 512.
    const LIMIT: usize = 4;
    let h = Harness::start_with(|c| c.max_connections = LIMIT).await;
    let net = Partitioner::in_front_of(h.addr).await;

    // Fill every slot, then kill the network under all of them.
    let mut doomed = Vec::new();
    for _ in 0..LIMIT {
        doomed.push(
            Client::connect(net.addr, h.token())
                .await
                .expect("connect within the limit"),
        );
    }
    net.sever();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(doomed);
    net.heal();

    // The server has to notice the peer is gone and release the permit. How
    // long that takes is not a constant — polling to a deadline says "this must
    // happen" without also asserting how fast a loaded machine manages it.
    for i in 0..LIMIT {
        let mut c = eventually(|| Client::connect(net.addr, h.token()))
            .await
            .unwrap_or_else(|| panic!("connection {i} refused: permits leaked on partition"));
        let ack = patiently(c.put(&format!("after-partition-{i}"), i as i64))
            .await
            .flatten()
            .unwrap_or_else(|| panic!("connection {i} accepted but unusable"));
        assert!(matches!(ack, ResponseBody::Put { .. }), "got {ack:?}");
    }
}

#[tokio::test]
async fn a_partition_during_the_handshake_leaves_no_half_open_session() {
    let h = Harness::start_with(|c| c.max_connections = 2).await;
    let net = Partitioner::in_front_of(h.addr).await;

    net.blackhole();
    // The handshake cannot complete through a partition, and must not hang the
    // slot forever once the client gives up.
    assert!(
        silenced(Client::connect(net.addr, h.token()))
            .await
            .flatten()
            .is_none(),
        "a handshake must not complete through a blackhole"
    );

    net.sever();
    tokio::time::sleep(Duration::from_millis(150)).await;
    net.heal();

    let mut c = eventually(|| Client::connect(net.addr, h.token()))
        .await
        .expect("a healed link must accept a fresh handshake");
    let ack = patiently(c.put("post-handshake-partition", 1))
        .await
        .flatten()
        .expect("usable after the partition healed");
    assert!(matches!(ack, ResponseBody::Put { .. }), "got {ack:?}");
}

/// Independent partition storms, run at once.
///
/// The storm is dominated by waiting for acknowledgements that a partition is
/// swallowing, so it is bound by timeouts rather than by the CPU. Running the
/// scenarios concurrently rather than in sequence buys an order of magnitude
/// more interleavings for the same wall clock, which is what `specs/08` §2 asks
/// for ("thousands of randomized interleaving scenarios") and what a gate run
/// on every change can actually afford.
#[tokio::test(flavor = "multi_thread")]
async fn randomized_partitions_never_lose_an_acknowledged_write() {
    const STORMS: u64 = 12;

    let mut running = Vec::new();
    for storm in 0..STORMS {
        // Distinct seeds, all derived from one constant, so the whole suite is
        // reproducible from a single number.
        running.push(tokio::spawn(partition_storm(
            0x05EE_D1CE_u64.wrapping_mul(storm + 1).wrapping_add(storm),
        )));
    }

    let mut total = 0usize;
    for (storm, handle) in running.into_iter().enumerate() {
        total += handle
            .await
            .unwrap_or_else(|e| panic!("storm {storm} panicked: {e}"));
    }

    assert!(
        total > 200,
        "only {total} writes were acknowledged across {STORMS} storms — the \
         suite partitioned so aggressively it proved nothing"
    );
    eprintln!("{total} acknowledged writes verified across {STORMS} partition storms");
}

/// One storm. Returns how many writes were acknowledged and then verified.
async fn partition_storm(seed: u64) -> usize {
    const ROUNDS: usize = 120;

    let h = Harness::start().await;
    let net = Partitioner::in_front_of(h.addr).await;

    let mut rng = Lcg::new(seed);
    let mut acknowledged: Vec<(String, i64)> = Vec::new();
    let mut client = Client::connect(net.addr, h.token()).await.expect("connect");

    for round in 0..ROUNDS {
        // A quarter of rounds change the state of the network.
        match rng.next_u32() % 8 {
            0 => net.blackhole(),
            1 => net.drop_responses(),
            2 => {
                net.sever();
                tokio::time::sleep(Duration::from_millis(10)).await;
                net.heal();
                // The connection did not survive that; the client reconnects,
                // as a real one would.
                match patiently(Client::connect(net.addr, h.token()))
                    .await
                    .flatten()
                {
                    Some(fresh) => client = fresh,
                    None => continue,
                }
            }
            _ => net.heal(),
        }

        let key = format!("k{round}");
        let value = round as i64;
        match silenced(client.put(&key, value)).await {
            Some(Some(ResponseBody::Put { .. })) => acknowledged.push((key, value)),
            // Answered, just not with an acknowledgement. The connection is
            // still in step, so it can carry the next request.
            Some(Some(_)) => {}
            // Either the socket died, or we stopped waiting while the request
            // was still on the wire. The second is the subtle one: the response
            // arrives eventually, and a connection holding an unclaimed response
            // hands it to the *next* request. Reconnecting is the only way to
            // stay in step - reusing this one would compare a fresh request
            // against a stale reply and report a corruption that never happened.
            //
            // No acknowledgement also means the write may or may not have
            // landed, and this test claims nothing about it: only acknowledged
            // writes are promises.
            Some(None) | None => {
                net.heal();
                match patiently(Client::connect(net.addr, h.token()))
                    .await
                    .flatten()
                {
                    Some(fresh) => client = fresh,
                    None => break,
                }
            }
        }
    }

    net.heal();

    let mut verifier = eventually(|| Client::connect(net.addr, h.token()))
        .await
        .unwrap_or_else(|| panic!("reconnect after the storm (seed {seed:#x})"));
    for (key, value) in &acknowledged {
        let (found, stored) = patiently(verifier.get(key))
            .await
            .flatten()
            .unwrap_or_else(|| panic!("read after the storm (seed {seed:#x})"));
        assert!(
            found,
            "acknowledged write `{key}` did not survive the partition storm \
             (seed {seed:#x})"
        );
        assert_eq!(
            stored,
            json(*value),
            "acknowledged write `{key}` came back changed (seed {seed:#x})"
        );
    }

    acknowledged.len()
}

/// A small deterministic PRNG, so a failing interleaving can be replayed.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes' LCG constants: adequate for choosing between four
        // branches, and not used for anything that needs more.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
}
