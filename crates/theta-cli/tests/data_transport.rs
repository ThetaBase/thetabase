//! The data-plane client has to be able to reach a provisioned instance.
//!
//! The bug this file exists for: `DataClient::connect` dialled a bare
//! `TcpStream`. Every provisioned instance is reached at `<app>.fly.dev:443`,
//! where Fly's proxy terminates TLS — and a *shared* IPv4 is routed by the SNI
//! name in the handshake, so a plaintext connection to that port is not merely
//! unencrypted: it cannot be routed to the right app at all. Nothing built on
//! this client — the CLI, `theta-mcp`, every `theta exec` child — could talk to
//! anything the Control Plane had provisioned.
//!
//! The symptom was the worst available one. A plaintext frame sent to a TLS
//! listener is not rejected; the first four bytes are read as a ClientHello,
//! found malformed, and the connection dropped. So the failure looked like a
//! hang or a bare "connection closed", with nothing anywhere naming TLS.
//!
//! Tested in three layers: that the framing survives a real handshake, that
//! the rule choosing the transport can be steered in both directions, and
//! — under `THETA_LIVE_ADDRESS` — against an actual provisioned instance,
//! because a public certificate chain and an SNI-routed proxy are not things a
//! local listener can imitate.

use std::sync::Arc;

use theta_cli::data::DataClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Held for the whole of any test that touches `THETA_TLS` or `THETA_TLS_CA`.
///
/// Those are process-global and cargo runs these tests in parallel threads, so
/// without this a test that forces TLS can make another test's plaintext
/// connection attempt a handshake. The failure that produces is a *hang*, not
/// an assertion: a plaintext listener reads a ClientHello as a length prefix,
/// asks for two gigabytes and waits for ever -- which is the same symptom as
/// the bug this file exists for, arriving from the test harness instead.
///
/// A mutex rather than `--test-threads=1`, because a flag in a Makefile is a
/// flag somebody runs `cargo test` without.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set `THETA_TLS` and `THETA_TLS_CA` for the duration of one connection.
///
/// Restores whatever was there, so a developer running with the variables set
/// in their shell does not get different results from CI.
struct Env {
    _guard: std::sync::MutexGuard<'static, ()>,
    previous: Vec<(&'static str, Option<String>)>,
}

impl Env {
    fn set(pairs: &[(&'static str, Option<String>)]) -> Self {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let mut previous = Vec::new();
        for (name, value) in pairs {
            previous.push((*name, std::env::var(name).ok()));
            // SAFETY: the mutex above makes this the only thread touching
            // these variables, and `Drop` restores them.
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        Self {
            _guard: guard,
            previous,
        }
    }

    /// TLS on, with an optional CA file.
    fn tls(ca: Option<String>) -> Self {
        Self::set(&[("THETA_TLS", Some("1".into())), ("THETA_TLS_CA", ca)])
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            // SAFETY: still holding the mutex.
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// Wait for a frame, or fail saying it never arrived.
///
/// Bounded deliberately. The failure this whole file guards against produces
/// *silence*: a frame written to the raw socket instead of the encrypted stream
/// never reaches the server, and a plaintext frame sent to a TLS listener is
/// read as a ClientHello and discarded. An unbounded wait turns each of those
/// into a hung test run rather than a named failure -- which is what happened
/// when I planted them.
async fn expect_frame(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    what: &str,
) -> Vec<u8> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
        Ok(Some(frame)) => frame,
        Ok(None) => panic!("{what}: the listener closed without receiving a frame"),
        Err(_) => panic!("{what}: no frame arrived within five seconds"),
    }
}

// ---- a certificate, generated here -----------------------------------------

/// A CA and a leaf for `localhost`, written to a temporary directory.
///
/// Generated rather than checked in. A private key in the tree is a private key
/// in the tree, and one that exists only to be trusted by a test is exactly the
/// kind that gets copied somewhere it should not be.
struct Pki {
    dir: tempfile::TempDir,
    chain: Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>,
    key: tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>,
}

impl Pki {
    fn generate() -> Self {
        let mut ca_params =
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "thetabase test ca");
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let ca = ca_params.self_signed(&ca_key).expect("self-signed ca");

        let leaf_params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf = leaf_params
            .signed_by(&leaf_key, &ca, &ca_key)
            .expect("signed leaf");

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("ca.pem"), ca.pem()).expect("write ca");

        Self {
            dir,
            chain: vec![leaf.der().clone()],
            key: tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
                leaf_key.serialize_der().into(),
            ),
        }
    }

    fn ca_path(&self) -> String {
        self.dir.path().join("ca.pem").display().to_string()
    }
}

/// A TLS listener that reads one frame and answers with another.
///
/// Returns the bound port and a channel carrying what it read off the
/// *decrypted* stream — which is the assertion that matters. A transport that
/// negotiated TLS and then wrote the frame to the raw socket would complete a
/// handshake and deliver nothing here.
async fn tls_echo(pki: &Pki) -> (u16, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(pki.chain.clone(), pki.key.clone_key())
    .expect("server config");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();

    // An unbounded channel and a loop, rather than a oneshot and one accept.
    // A test that makes two attempts against this listener found a closed port
    // on the second and failed for a reason that had nothing to do with what it
    // was testing.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut tls = match acceptor.accept(socket).await {
                    Ok(tls) => tls,
                    // Expected, for the untrusted-certificate tests: the client
                    // refuses and drops the connection mid-handshake.
                    Err(_) => return,
                };

                let mut prefix = [0u8; 4];
                if tls.read_exact(&mut prefix).await.is_err() {
                    return;
                }
                let size = u32::from_le_bytes(prefix) as usize;
                let mut payload = vec![0u8; size];
                if tls.read_exact(&mut payload).await.is_err() {
                    return;
                }

                // Closed rather than answered. The client reports "closed
                // during handshake", which is a real response and proves the
                // round trip without this test having to encode a `Welcome`.
                let _ = tls.shutdown().await;
                let _ = tx.send(payload);
            });
        }
    });

    (port, rx)
}

// ---- the framing survives TLS ----------------------------------------------

#[tokio::test]
async fn tls_carries_the_framing() {
    let pki = Pki::generate();
    let (port, mut rx) = tls_echo(&pki).await;

    // `THETA_TLS` rather than port 443: binding 443 needs privileges a test
    // does not have, and the override is worth exercising anyway -- a
    // self-hosted instance behind a terminating proxy on another port is a real
    // deployment.
    let outcome = {
        let _env = Env::tls(Some(pki.ca_path()));
        DataClient::connect(&format!("localhost:{port}"), "tok_tls").await
    };

    let message = outcome.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        message.contains("closed during handshake"),
        "the TLS connection did not complete and deliver a frame: {message}"
    );

    // Bounded, because the failure being guarded against produces silence
    // rather than an error: a frame written to the wrong stream never arrives,
    // and an unbounded `recv` would hang instead of failing. I planted exactly
    // that and the run had to be killed rather than reporting anything.
    let hello = expect_frame(&mut rx, "the server received a frame over TLS").await;
    let hello = String::from_utf8_lossy(&hello);
    assert!(
        hello.contains("tok_tls"),
        "the frame that crossed TLS did not carry the session token, so the \
         transport wrote it somewhere other than the encrypted stream"
    );
}

#[tokio::test]
async fn an_untrusted_certificate_is_refused_by_name() {
    // The same listener without the CA. Refusing is correct; refusing *legibly*
    // is the point -- a certificate failure that surfaced as "connection
    // closed" is indistinguishable from the plaintext-to-TLS bug this file is
    // about.
    let pki = Pki::generate();
    let (port, _rx) = tls_echo(&pki).await;

    let outcome = {
        let _env = Env::tls(None);
        DataClient::connect(&format!("localhost:{port}"), "tok_tls").await
    };

    let message = outcome
        .err()
        .expect("an untrusted certificate must be refused")
        .to_string();
    assert!(
        message.contains("tls handshake"),
        "a certificate failure was not reported as a TLS failure: {message}"
    );
}

/// Naming a private CA must not stop the client reaching a provisioned instance.
///
/// `THETA_TLS_CA` *adds* to the public roots; it does not replace them. A
/// deployment with one self-hosted instance and one provisioned instance
/// reaches both from the same process, and a client that swapped the root set
/// would break the provisioned half the moment the private CA was configured.
///
/// This needs a publicly signed peer, because that is the only thing that can
/// tell "added" from "replaced" -- two local CAs cannot, since neither is in
/// the public set. So it dials the Control Plane's own host. A connection
/// failure skips (the machine is offline); a *handshake* failure is a real
/// failure, which is the distinction that makes the test worth having.
///
/// I planted the replacement and every other test here stayed green, which is
/// why this exists.
#[tokio::test]
async fn a_private_ca_is_added_to_the_public_roots_not_substituted_for_them() {
    const PUBLIC: &str = "thetabase-control.fly.dev:443";

    // Reachability first, outside the guard, so an offline machine skips
    // rather than holding the lock through a DNS timeout.
    if tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(PUBLIC),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .is_none()
    {
        eprintln!("skipping: {PUBLIC} is not reachable from here");
        return;
    }

    let pki = Pki::generate();

    let outcome = {
        let _env = Env::tls(Some(pki.ca_path()));
        DataClient::connect(PUBLIC, "tok_x").await
    };

    // It cannot succeed -- that host speaks HTTP, not the frame protocol -- so
    // the assertion is about *which* failure. A TLS handshake error means the
    // private CA displaced the public roots.
    let message = outcome.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        !message.contains("tls handshake"),
        "naming a private CA broke TLS to a publicly signed host, so a          deployment that configures THETA_TLS_CA can no longer reach any          provisioned instance: {message}"
    );
}

// ---- the rule ---------------------------------------------------------------

#[tokio::test]
async fn a_local_address_is_dialled_in_the_clear() {
    // The default has to stay plaintext. Every local instance is on 7700 and
    // upwards, and a rule that dialled TLS everywhere would break development
    // to fix production.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut prefix = [0u8; 4];
        if socket.read_exact(&mut prefix).await.is_err() {
            return;
        }
        let size = u32::from_le_bytes(prefix) as usize;
        let mut payload = vec![0u8; size];
        let _ = socket.read_exact(&mut payload).await;
        let _ = tx.send(payload);
    });

    let outcome = {
        // The guard even though nothing is being set: another test holding
        // `THETA_TLS=1` would otherwise turn this plaintext attempt into a
        // handshake against a plaintext listener, which hangs.
        let _env = Env::set(&[("THETA_TLS", None), ("THETA_TLS_CA", None)]);
        DataClient::connect(&addr.to_string(), "tok_plain").await
    };
    assert!(
        outcome.is_err(),
        "the fake listener never answers, so this cannot succeed"
    );

    let hello = expect_frame(&mut rx, "a local address received a plaintext frame").await;
    assert!(
        String::from_utf8_lossy(&hello).contains("tok_plain"),
        "a local address did not receive a plaintext frame"
    );
}

/// The rule itself, on addresses a test cannot bind.
///
/// Every connection-level check here runs against an ephemeral port, where the
/// rule and its negation both produce a plaintext connection -- so a test of
/// `THETA_TLS=0` through `connect` passes whether or not the override is
/// honoured. I planted the override's removal and the suite stayed green, which
/// is why this test exists and why `wants_tls` is public.
#[test]
fn port_443_means_tls_and_the_override_beats_it_both_ways() {
    // One guard per scope, never nested. `ENV` is a plain `std::sync::Mutex`
    // and those are not reentrant, so taking it again on the same thread
    // deadlocks -- which is what an earlier version of this test did, and it
    // hung the whole binary rather than failing.
    {
        let _env = Env::set(&[("THETA_TLS", None), ("THETA_TLS_CA", None)]);

        // The default, and the one that was broken: a provisioned address.
        assert!(
            theta_cli::data::wants_tls("storagegenie-dev-e0def69a3d28.fly.dev:443"),
            "a provisioned instance's address would be dialled in the clear,              which cannot even be routed -- a shared IPv4 is routed by SNI"
        );
        // And a local one, which must stay plaintext or development breaks to
        // fix production.
        assert!(!theta_cli::data::wants_tls("127.0.0.1:7700"));
        assert!(!theta_cli::data::wants_tls("localhost:7701"));

        // An IPv6 literal, because splitting on the first colon rather than the
        // last would read the port as nonsense and answer plaintext for a
        // provisioned address.
        assert!(theta_cli::data::wants_tls("[2a09:8280:1::1]:443"));
    }

    // The override, in the direction the port rule does not give for free: a
    // developer tunnelling a local plaintext process to 443.
    {
        let _env = Env::set(&[("THETA_TLS", Some("0".into())), ("THETA_TLS_CA", None)]);
        assert!(
            !theta_cli::data::wants_tls("anything.fly.dev:443"),
            "`THETA_TLS=0` did not turn TLS off, so a developer tunnelling 443              to a local plaintext process cannot connect"
        );
    }

    // And the other direction: a self-hosted instance behind a terminating
    // proxy on some other port.
    {
        let _env = Env::set(&[("THETA_TLS", Some("1".into())), ("THETA_TLS_CA", None)]);
        assert!(
            theta_cli::data::wants_tls("db.example.com:9443"),
            "`THETA_TLS=1` did not turn TLS on, so a self-hosted instance              behind its own terminating proxy cannot be reached"
        );
    }
}

#[tokio::test]
async fn the_override_can_force_plaintext() {
    // `THETA_TLS=0` is the developer tunnelling a local plaintext process to
    // 443. Without it the port rule would be a rule with no escape.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut prefix = [0u8; 4];
        if socket.read_exact(&mut prefix).await.is_err() {
            return;
        }
        let size = u32::from_le_bytes(prefix) as usize;
        let mut payload = vec![0u8; size];
        let _ = socket.read_exact(&mut payload).await;
        let _ = tx.send(payload);
    });

    {
        let _env = Env::set(&[("THETA_TLS", Some("0".into())), ("THETA_TLS_CA", None)]);
        let _ = DataClient::connect(&addr.to_string(), "tok_forced").await;
    }

    let hello = expect_frame(
        &mut rx,
        "`THETA_TLS=0` must produce a plaintext connection",
    )
    .await;
    assert!(String::from_utf8_lossy(&hello).contains("tok_forced"));
}

#[tokio::test]
async fn a_trust_file_that_is_wrong_is_refused_rather_than_ignored() {
    // An operator who set `THETA_TLS_CA` has said "trust this CA". Continuing
    // without it because the path was wrong would silently connect under a
    // different, laxer trust policy than the one they chose.
    let pki = Pki::generate();
    let (port, _rx) = tls_echo(&pki).await;

    let empty = pki.dir.path().join("empty.pem");
    std::fs::write(&empty, b"").expect("write");

    for (label, path) in [
        ("a path that does not exist", "/nonexistent/ca.pem".to_string()),
        ("a file with no certificates", empty.display().to_string()),
    ] {
        let outcome = {
            let _env = Env::tls(Some(path.clone()));
            DataClient::connect(&format!("localhost:{port}"), "tok_x").await
        };

        let message = outcome
            .err()
            .unwrap_or_else(|| panic!("{label} was accepted"))
            .to_string();
        assert!(
            message.contains("THETA_TLS_CA"),
            "{label} did not produce an error naming the variable that caused \
             it: {message}"
        );
    }
}

// ---- live, against a provisioned instance -----------------------------------

/// The only test that proves the thing that was broken.
///
/// A local listener cannot imitate what actually failed: a certificate chain
/// from a public CA, and a *shared* IPv4 where the proxy decides which app to
/// route to from the SNI name alone. Set `THETA_LIVE_ADDRESS=<app>.fly.dev:443`
/// to run it, and `THETA_LIVE_TOKEN` if a real session token is to hand.
///
/// A wrong token is still a useful run. The instance answers a refusal, and a
/// refusal that arrives at all is proof the frame crossed TLS, was routed by
/// SNI to the right app, and reached `thetad`.
#[tokio::test]
async fn a_provisioned_instance_answers_over_tls() {
    let Ok(address) = std::env::var("THETA_LIVE_ADDRESS") else {
        eprintln!(
            "skipping: set THETA_LIVE_ADDRESS=<app>.fly.dev:443 to test against \
             a provisioned instance"
        );
        return;
    };
    let token = std::env::var("THETA_LIVE_TOKEN").unwrap_or_else(|_| "tok_not_a_real_one".into());

    let outcome = {
        let _env = Env::set(&[("THETA_TLS", None), ("THETA_TLS_CA", None)]);
        DataClient::connect(&address, &token).await
    };

    match outcome {
        Ok(client) => {
            assert!(
                !client.project_id.is_empty(),
                "the instance welcomed the connection without naming a project"
            );
            eprintln!("connected to {address}, serving `{}`", client.project_id);
        }
        Err(e) => {
            let message = e.to_string();
            assert!(
                !message.contains("tls handshake"),
                "the TLS handshake with a provisioned instance failed: {message}"
            );
            assert!(
                !message.contains("cannot reach"),
                "a provisioned instance was unreachable, which is the failure \
                 this transport exists to fix: {message}"
            );
            eprintln!("the instance answered over TLS: {message}");
        }
    }
}
