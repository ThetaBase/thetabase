//! SLA latency gate.
//!
//! `docs/specs/09-sla-performance.md` §2 publishes latency targets, and
//! `08-test-validation-plan.md` §4 requires them asserted in CI rather than
//! measured once by hand. This is that assertion.
//!
//! # What this does and does not prove
//!
//! It measures a real engine over a real socket, so it catches an accidental
//! full scan on the `get` path or a lock held across an fsync. It does **not**
//! reproduce production: a CI runner is shared, unpredictable, and slower than
//! the single-region Pro-tier hardware the published targets describe.
//!
//! So the gate here is deliberately looser than the published SLA — see
//! [`CI_BUDGET_MULTIPLIER`]. A test that fails on a noisy neighbour teaches
//! people to re-run CI until it passes, which is worse than no test. The
//! published numbers stay a claim about production, verified against production
//! telemetry (`09-sla-performance.md` §5); this gate exists to catch the
//! regressions that are orders of magnitude, not percentages.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use theta_core::Value;
use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use theta_scribe::{Scribe, ScribeConfig, StaticToken};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};

/// How much slack CI gets over the published targets.
///
/// Generous on purpose. The failures worth catching — a scan where a lookup
/// belongs, a model call on the hot path — are hundreds of times over budget,
/// not tens of percent.
const CI_BUDGET_MULTIPLIER: u32 = 20;

/// Published p50 targets, in milliseconds (`09-sla-performance.md` §2).
mod target {
    pub const GET_P50_MS: u32 = 5;
    pub const PUT_P50_MS: u32 = 8;
    pub const QUERY_CACHED_P50_MS: u32 = 15;
    pub const QUERY_UNCACHED_P50_MS: u32 = 40;
    pub const BRANCH_CREATE_P50_MS: u32 = 50;
}

struct Harness {
    addr: SocketAddr,
    keys: ProjectKeys,
    _dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::dev_default("bench-project");
        config.data_dir = dir.path().to_path_buf();
        config.listen = "127.0.0.1:0".parse().expect("addr");
        // The breaker exists to stop runaway agents, not benchmarks.
        config.safety.breaker_row_ceiling = u64::MAX;

        let server_config = ServerConfig::from_config(&config);
        let keys = ProjectKeys::generate("bench-project");
        let authorizer = std::sync::Arc::new(tokio::sync::RwLock::new(Authorizer::new(
            keys.public_keyset(),
            Environment::Dev,
            // An hour, because this harness never sends a heartbeat.
            //
            // It used to be 60s and the suite ran for 60.07s. That was harmless
            // while revocation staleness was only checked at handshake; now it
            // is checked on every request (R1-01), so the list went stale
            // mid-run and the instance started refusing — correctly. A latency
            // harness is not modelling Control Plane connectivity, so it should
            // not be the thing that decides when this expires.
            60 * 60 * 1_000,
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

    fn scribe(&self) -> Scribe {
        let mut config = ScribeConfig::new(self.addr.to_string());
        // Measuring the engine, not the cache.
        config.cache_ttl_ms = 0;

        let token = SessionToken::mint(
            &self.keys,
            &TokenScope {
                token_id: "tok_bench".into(),
                project_id: "bench-project".into(),
                environment: "dev".into(),
                session_id: "sess_bench".into(),
                user_id: "u_bench".into(),
                org_id: "org_bench".into(),
                issued_at_ms: 0,
                expires_at_ms: i64::MAX,
                key_id: KeyId::new("bench-project-k1"),
                // This session does not sign its writes.
                signing_key: None,
            },
        );
        Scribe::connect(
            config,
            std::sync::Arc::new(StaticToken(token.into_string())),
        )
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

/// Median of `samples`, which is what a p50 target is about.
fn p50(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn p99(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[(samples.len() * 99 / 100).min(samples.len() - 1)]
}

/// Assert a measured median against a published target, with CI slack.
fn assert_within(what: &str, measured: Duration, target_ms: u32, samples: &[Duration]) {
    let budget = Duration::from_millis((target_ms * CI_BUDGET_MULTIPLIER) as u64);
    // Printed so a run that passes still shows how much headroom it had — a
    // gate that only speaks when it fails hides a slow slide toward the limit.
    println!(
        "{what}: p50 {:?}, p99 {:?} (target {target_ms}ms, CI budget {budget:?})",
        measured,
        p99(samples.to_vec())
    );
    assert!(
        measured <= budget,
        "{what}: p50 {measured:?} exceeds the CI budget of {budget:?} \
         (published target {target_ms}ms x{CI_BUDGET_MULTIPLIER}); \
         p99 {:?}, {} samples",
        p99(samples.to_vec()),
        samples.len()
    );
}

async fn seed(scribe: &Scribe, rows: usize) {
    let writes: Vec<(String, Value)> = (0..rows)
        .map(|i| {
            (
                format!("users:{i:06}"),
                Value::Map(std::collections::BTreeMap::from([
                    ("name".to_string(), Value::Text(format!("user-{i}"))),
                    ("age".to_string(), Value::Int((20 + i % 50) as i64)),
                    (
                        "plan".to_string(),
                        Value::Text(if i % 3 == 0 { "pro" } else { "free" }.into()),
                    ),
                ])),
            )
        })
        .collect();

    for chunk in writes.chunks(500) {
        scribe.put_many(chunk).await.expect("seed");
    }
}

#[tokio::test]
async fn get_meets_its_latency_target() {
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 2_000).await;

    // Warm up, so the first-call costs of a fresh connection are not measured
    // as if they were steady state.
    for _ in 0..20 {
        scribe.get("users:000001").await.expect("get");
    }

    let mut samples = Vec::new();
    for i in 0..200 {
        let key = format!("users:{:06}", i % 2_000);
        let start = Instant::now();
        scribe.get(&key).await.expect("get");
        samples.push(start.elapsed());
    }

    assert_within("get", p50(samples.clone()), target::GET_P50_MS, &samples);
}

#[tokio::test]
async fn put_meets_its_latency_target_including_durability() {
    let h = Harness::start().await;
    let scribe = h.scribe();

    for i in 0..20 {
        scribe
            .put(&format!("warm:{i}"), &Value::Int(i))
            .await
            .expect("warm");
    }

    // Every one of these is fsynced before it is acknowledged, so this measures
    // real durability rather than a buffered write.
    let mut samples = Vec::new();
    for i in 0..200i64 {
        let start = Instant::now();
        scribe
            .put(&format!("bench:{i}"), &Value::Int(i))
            .await
            .expect("put");
        samples.push(start.elapsed());
    }

    assert_within("put", p50(samples.clone()), target::PUT_P50_MS, &samples);
}

#[tokio::test]
async fn a_repeated_query_meets_the_cached_plan_target() {
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 2_000).await;

    let sql = "SELECT * FROM users WHERE plan = 'pro' LIMIT 20";
    for _ in 0..10 {
        scribe.query(sql).await.expect("warm");
    }

    let mut samples = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        let result = scribe.query(sql).await.expect("query");
        samples.push(start.elapsed());
        assert_eq!(result.row_count, 20);
    }

    assert_within(
        "cached query",
        p50(samples.clone()),
        target::QUERY_CACHED_P50_MS,
        &samples,
    );
}

#[tokio::test]
async fn a_first_run_query_meets_the_uncached_target() {
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 2_000).await;

    // A distinct statement each time, so parse and planning are always paid.
    let mut samples = Vec::new();
    for i in 0..100 {
        let sql = format!("SELECT * FROM users WHERE age > {} LIMIT 10", 20 + i % 40);
        let start = Instant::now();
        scribe.query(&sql).await.expect("query");
        samples.push(start.elapsed());
    }

    assert_within(
        "uncached query",
        p50(samples.clone()),
        target::QUERY_UNCACHED_P50_MS,
        &samples,
    );
}

#[tokio::test]
async fn creating_a_branch_is_a_pointer_copy_not_a_data_copy() {
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 5_000).await;

    // The claim is O(1) in the data (`01-system-architecture.md` §3.2), so this
    // measures branching over a table large enough that copying it would show.
    let mut samples = Vec::new();
    for i in 0..50 {
        let start = Instant::now();
        scribe
            .create_branch(&format!("bench-{i}"), 0)
            .await
            .expect("branch");
        samples.push(start.elapsed());
    }

    assert_within(
        "branch create",
        p50(samples.clone()),
        target::BRANCH_CREATE_P50_MS,
        &samples,
    );
}

#[tokio::test]
async fn a_point_lookup_does_not_get_slower_as_the_table_grows() {
    // The shape that matters more than any absolute number: if `get` were
    // scanning, this ratio would grow with the table.
    let h = Harness::start().await;
    let scribe = h.scribe();

    seed(&scribe, 200).await;
    let small = measure_gets(&scribe, 200).await;

    seed(&scribe, 5_000).await;
    let large = measure_gets(&scribe, 5_000).await;

    let ratio = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);
    assert!(
        ratio < 5.0,
        "point lookups scaled with table size (25x the rows, {ratio:.1}x the time) \
         — that is a scan, not a lookup"
    );
}

async fn measure_gets(scribe: &Scribe, rows: usize) -> Duration {
    for _ in 0..20 {
        scribe.get("users:000001").await.expect("warm");
    }
    let mut samples = Vec::new();
    for i in 0..100 {
        let key = format!("users:{:06}", i % rows);
        let start = Instant::now();
        scribe.get(&key).await.expect("get");
        samples.push(start.elapsed());
    }
    p50(samples)
}

#[tokio::test]
async fn no_model_call_happens_on_any_measured_path() {
    // `no_llm_on_hot_path` proves this structurally, by dependency closure.
    // This is the behavioural half: EXPLAIN reports the call count for the plan
    // that actually runs, and it must be zero.
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 100).await;

    for sql in [
        "SELECT * FROM users",
        "SELECT * FROM users WHERE age > 30",
        "SELECT COUNT(*) FROM users GROUP BY plan",
    ] {
        let explain = scribe.explain(sql).await.expect("explain");
        assert_eq!(
            explain.get("llmCalls").and_then(|v| v.as_u64()),
            Some(0),
            "`{sql}` reported a model call on the hot path"
        );
    }
}

#[tokio::test]
async fn explain_reports_estimates_drawn_from_real_statistics() {
    let h = Harness::start().await;
    let scribe = h.scribe();
    seed(&scribe, 500).await;

    let explain = scribe
        .explain("SELECT * FROM users WHERE plan = 'pro'")
        .await
        .expect("explain");

    assert_eq!(
        explain
            .get("estimatesFromStatistics")
            .and_then(|v| v.as_bool()),
        Some(true),
        "estimates must be measured once the table has rows"
    );

    let rows = explain
        .get("estimatedRows")
        .and_then(|v| v.as_u64())
        .expect("estimatedRows");
    // A third of rows have plan = 'pro'. The estimate need not be exact, but it
    // must be in the right neighbourhood or it is not worth reporting.
    assert!(
        (50..=400).contains(&rows),
        "estimate of {rows} rows is not close to the ~167 that match"
    );
}

#[tokio::test]
async fn an_unanalyzed_table_says_its_estimates_are_unmeasured() {
    let h = Harness::start().await;
    let scribe = h.scribe();

    let explain = scribe
        .explain("SELECT * FROM never_written")
        .await
        .expect("explain");
    assert_eq!(
        explain
            .get("estimatesFromStatistics")
            .and_then(|v| v.as_bool()),
        Some(false),
        "an estimate nobody can tell is a guess is worse than no estimate"
    );
}
