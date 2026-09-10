//! What an instance says about its own idle time, and why the Control Plane
//! believes it.
//!
//! Hibernation — stopping the task behind a project nobody is using — is the
//! difference between $7.21 and $0.22 a month per project
//! (`docs/business/PROFITABILITY.html`), and it is decided entirely on the
//! number tested here. Two ways to get it wrong, both of which look like
//! working software:
//!
//!   * never reset it, and every project hibernates while somebody is using it;
//!   * reset it on the Control Plane's own polling, and nothing ever hibernates
//!     at all, while the feature appears to be implemented.
//!
//! The second is the dangerous one. The courier visits every instance on a
//! timer to collect usage and push revocations, so counting its `Status` call
//! as customer traffic would keep the whole fleet awake for ever and the only
//! symptom would be the bill.

use theta_identity::keys::KeyId;
use theta_identity::TokenScope;
use theta_proto::{Request, RequestBody, ResponseBody};
use thetad::dispatch::dispatch;
use thetad::engine::Engine;
use thetad::Config;

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("idle-reporting");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn scope() -> TokenScope {
    TokenScope {
        token_id: "tok_test".into(),
        project_id: "idle-reporting".into(),
        environment: "dev".into(),
        session_id: "sess_test".into(),
        user_id: "u_test".into(),
        org_id: "org_test".into(),
        issued_at_ms: 0,
        expires_at_ms: i64::MAX,
        key_id: KeyId::new("idle-reporting-k1"),
        signing_key: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn send(engine: &mut Engine, body: RequestBody) -> ResponseBody {
    dispatch(
        engine,
        Request {
            request_id: 1,
            branch_id: 0,
            body,
        },
        &scope(),
        now_ms(),
    )
    .body
}

/// Read the instance's own idle figure the way the courier does.
fn reported_idle_ms(engine: &mut Engine) -> u64 {
    match send(engine, RequestBody::Status) {
        ResponseBody::Status(status) => status.idle_ms,
        other => panic!("expected a status response, got {other:?}"),
    }
}

/// Long enough that the assertions have room, short enough not to slow the
/// suite. Every comparison below leaves a wide margin either side, because a
/// timing test that is tight is a timing test that is flaky on a loaded
/// machine.
const QUIET: std::time::Duration = std::time::Duration::from_millis(120);

#[test]
fn an_instance_that_has_served_nothing_is_idle_from_when_it_started() {
    // Not zero. An instance woken by a request that then went away is exactly
    // what hibernation exists to stop paying for, and a permanent zero would
    // keep it running for ever.
    let (mut engine, _dir) = engine();
    std::thread::sleep(QUIET);

    assert!(
        reported_idle_ms(&mut engine) >= 100,
        "a freshly started instance reported itself as busy"
    );
}

#[test]
fn serving_a_request_resets_the_idle_clock() {
    let (mut engine, _dir) = engine();
    std::thread::sleep(QUIET);
    assert!(reported_idle_ms(&mut engine) >= 100);

    // Any customer request will do; a read is the cheapest.
    let _ = send(
        &mut engine,
        RequestBody::Get {
            key: "nobody".into(),
        },
    );

    assert!(
        reported_idle_ms(&mut engine) < 100,
        "the idle clock did not reset when a request was served"
    );
}

#[test]
fn the_control_planes_own_polling_does_not_keep_an_instance_awake() {
    // The one that decides whether hibernation saves anything. The courier
    // polls `Status` on a timer; if that counted as use, no instance would ever
    // look idle and the feature would be inert while appearing to work.
    let (mut engine, _dir) = engine();
    std::thread::sleep(QUIET);

    // Poll the way the courier does, repeatedly.
    for _ in 0..5 {
        let idle = reported_idle_ms(&mut engine);
        assert!(
            idle >= 100,
            "polling for status reset the idle clock; it reported {idle}ms"
        );
    }
}

#[test]
fn pushing_revocations_does_not_keep_an_instance_awake() {
    // Same argument as the status poll, and the same failure. Revocation
    // delivery is the Control Plane looking after the instance, not somebody
    // using the database.
    let (mut engine, _dir) = engine();
    std::thread::sleep(QUIET);

    // The push is refused without a keyset, which is fine — this is about
    // whether the attempt counted as activity, not whether it succeeded.
    let _ = send(
        &mut engine,
        RequestBody::PushRevocations {
            payload: b"{}".to_vec(),
            signature: vec![0u8; 64],
            key_id: "idle-reporting-k1".into(),
        },
    );

    assert!(
        reported_idle_ms(&mut engine) >= 100,
        "delivering revocations reset the idle clock"
    );
}

#[test]
fn a_refused_request_still_counts_as_somebody_using_the_database() {
    // Somebody is there, holding a connection and waiting on an answer. That
    // the answer was "no" does not make the instance idle, and hibernating it
    // would drop a caller who is plainly present.
    let (mut engine, _dir) = engine();
    std::thread::sleep(QUIET);
    assert!(reported_idle_ms(&mut engine) >= 100);

    // A write to a table with no schema is refused.
    let refused = send(
        &mut engine,
        RequestBody::Put {
            key: "orders:1".into(),
            value_json: "{}".into(),
            ttl: 0,
        },
    );
    assert!(
        matches!(refused, ResponseBody::Error(_)),
        "this test needs a refusal to be meaningful, got {refused:?}"
    );

    assert!(
        reported_idle_ms(&mut engine) < 100,
        "a refused request left the instance looking idle"
    );
}
