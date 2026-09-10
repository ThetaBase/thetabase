//! Scribe against a real `thetad`.
//!
//! The cache is the risky part of this crate — a cache that can serve a value
//! the caller already overwrote breaks read-your-writes, which is a guarantee
//! the database makes and the client must not quietly withdraw. Most of what
//! follows is about that.

use std::sync::Arc;

use theta_core::schema::SchemaChange;
use theta_core::Value;
use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, SessionToken, TokenScope};
use theta_proto::StatusCode;
use theta_scribe::{Scribe, ScribeConfig, ScribeError, StaticToken};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};

struct Harness {
    addr: String,
    keys: ProjectKeys,
    _dir: tempfile::TempDir,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Harness {
    async fn start() -> Self {
        Self::with_policy(thetad::config::Environment::Dev.default_policy()).await
    }

    async fn with_policy(safety: theta_safety::SafetyPolicy) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::dev_default("test-project");
        config.data_dir = dir.path().to_path_buf();
        config.listen = "127.0.0.1:0".parse().expect("addr");
        config.safety = safety;

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

        let addr = bound.await.expect("bound").to_string();
        Self {
            addr,
            keys,
            _dir: dir,
            shutdown: Some(shutdown),
        }
    }

    /// A token this server will accept.
    fn token(&self) -> String {
        SessionToken::mint(&self.keys, &scope()).into_string()
    }

    fn scribe(&self) -> Scribe {
        self.scribe_with(|_| {})
    }

    fn scribe_with(&self, tweak: impl FnOnce(&mut ScribeConfig)) -> Scribe {
        let mut config = ScribeConfig::new(&self.addr);
        tweak(&mut config);
        Scribe::connect(config, Arc::new(StaticToken(self.token())))
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

fn scope() -> TokenScope {
    TokenScope {
        token_id: "tok_scribe".into(),
        project_id: "test-project".into(),
        environment: "dev".into(),
        session_id: "sess_scribe".into(),
        user_id: "u_test".into(),
        org_id: "org_test".into(),
        issued_at_ms: 0,
        expires_at_ms: i64::MAX,
        key_id: KeyId::new("test-project-k1"),
        // This session does not sign its writes.
        signing_key: None,
    }
}

#[tokio::test]
async fn a_value_round_trips_through_the_edge_runtime() {
    let h = Harness::start().await;
    let scribe = h.scribe();

    scribe
        .put("user:1", &Value::Text("Alice".into()))
        .await
        .expect("put");
    assert_eq!(
        scribe.get("user:1").await.expect("get"),
        Some(Value::Text("Alice".into()))
    );
}

#[tokio::test]
async fn a_missing_key_returns_none_rather_than_an_error() {
    let h = Harness::start().await;
    assert_eq!(h.scribe().get("nope").await.expect("get"), None);
}

#[tokio::test]
async fn a_repeated_read_is_served_from_the_cache() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    scribe.put("k", &Value::Int(1)).await.expect("put");
    for _ in 0..5 {
        assert_eq!(scribe.get("k").await.expect("get"), Some(Value::Int(1)));
    }

    let stats = scribe.cache_stats().await;
    assert_eq!(
        stats.hits, 4,
        "only the first read should cross the network"
    );
    assert_eq!(stats.misses, 1);
}

#[tokio::test]
async fn the_cache_never_serves_a_value_the_caller_already_overwrote() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    scribe.put("k", &Value::Int(1)).await.expect("put");
    assert_eq!(scribe.get("k").await.expect("get"), Some(Value::Int(1)));

    // Read-your-writes: the second write must invalidate the cached first.
    scribe.put("k", &Value::Int(2)).await.expect("put");
    assert_eq!(
        scribe.get("k").await.expect("get"),
        Some(Value::Int(2)),
        "the cache served a value the caller had already replaced"
    );
}

#[tokio::test]
async fn a_delete_is_visible_immediately_to_the_deleter() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    scribe.put("k", &Value::Int(1)).await.expect("put");
    scribe.get("k").await.expect("warm the cache");
    scribe.delete("k").await.expect("delete");

    assert_eq!(scribe.get("k").await.expect("get"), None);
}

#[tokio::test]
async fn a_zero_ttl_means_every_read_crosses_the_network() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 0);

    scribe.put("k", &Value::Int(1)).await.expect("put");
    for _ in 0..3 {
        scribe.get("k").await.expect("get");
    }
    assert_eq!(
        scribe.cache_stats().await.hits,
        0,
        "a disabled cache must never serve"
    );
}

#[tokio::test]
async fn connections_are_reused_rather_than_reopened() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 0);

    for i in 0..20 {
        scribe
            .put(&format!("k{i}"), &Value::Int(i))
            .await
            .expect("put");
    }

    // A sequential caller needs exactly one connection; the pool must hold it
    // rather than paying a TCP plus protocol handshake per request.
    assert_eq!(scribe.idle_connections().await, 1);
}

#[tokio::test]
async fn a_batch_writes_every_key_in_one_round_trip() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 0);

    let writes: Vec<(String, Value)> = (0..25)
        .map(|i| (format!("batch:{i}"), Value::Int(i)))
        .collect();
    let commits = scribe.put_many(&writes).await.expect("batch");

    assert_eq!(commits.len(), 25);
    // Every commit is distinct: batching the network does not merge the writes.
    let unique: std::collections::HashSet<_> = commits.iter().collect();
    assert_eq!(unique.len(), 25);

    for (key, value) in &writes {
        assert_eq!(scribe.get(key).await.expect("get"), Some(value.clone()));
    }
}

#[tokio::test]
async fn a_batch_invalidates_every_key_it_touched() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    scribe.put("a", &Value::Int(1)).await.expect("put");
    scribe.get("a").await.expect("warm");

    scribe
        .put_many(&[("a".into(), Value::Int(9)), ("b".into(), Value::Int(8))])
        .await
        .expect("batch");

    assert_eq!(scribe.get("a").await.expect("get"), Some(Value::Int(9)));
}

#[tokio::test]
async fn an_empty_batch_does_nothing() {
    let h = Harness::start().await;
    assert!(h.scribe().put_many(&[]).await.expect("batch").is_empty());
}

#[tokio::test]
async fn a_gated_change_surfaces_as_a_rejection_carrying_its_diff() {
    // Production strictness: the shadow threshold is what decides whether a
    // drop is confirmable, and this test is about the case where it is not.
    // Under dev defaults the same 200 rows are genuinely low-impact.
    let h = Harness::with_policy(theta_safety::SafetyPolicy::protected()).await;
    let scribe = h.scribe();

    // Rows first: the server measures the impact against its own view, so a
    // drop on an empty table is genuinely a low-impact change and would come
    // back confirmable rather than gated.
    let rows: Vec<(String, theta_core::Value)> = (0..200)
        .map(|i| {
            let mut row = std::collections::BTreeMap::new();
            row.insert(
                "email".to_string(),
                theta_core::Value::Text(format!("u{i}@example.com")),
            );
            (format!("users:{i}"), theta_core::Value::Map(row))
        })
        .collect();
    scribe.put_many(&rows).await.expect("seed rows");

    let change = SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    };
    let diff = scribe.propose(&change).await.expect("propose");
    assert_eq!(diff.rows_affected, 200, "the server did its own counting");
    assert!(diff.destructive && diff.requires_confirm);

    let err = scribe
        .apply(&diff.change_id, true)
        .await
        .expect_err("confirmation alone must not clear this");

    match err {
        ScribeError::Rejected {
            code,
            diff: Some(returned),
            ..
        } => {
            assert_eq!(code, StatusCode::ConfirmationRequired);
            // The diff comes back with the refusal, so the caller need not ask
            // again to find out what was blocked.
            assert_eq!(returned.change_id, diff.change_id);
        }
        other => panic!("expected a rejection carrying a diff, got {other:?}"),
    }
}

#[tokio::test]
async fn branch_scribes_do_not_share_a_cache() {
    let h = Harness::start().await;
    let main = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    main.put("shared", &Value::Int(1)).await.expect("put");
    let branch_id = main.create_branch("feature", 0).await.expect("branch");
    let feature = main.on_branch(branch_id);

    // Warm both caches, then diverge.
    assert_eq!(main.get("shared").await.expect("get"), Some(Value::Int(1)));
    assert_eq!(
        feature.get("shared").await.expect("get"),
        Some(Value::Int(1))
    );

    feature.put("shared", &Value::Int(2)).await.expect("put");

    // One cache serving both branches would return whichever was read last.
    assert_eq!(
        feature.get("shared").await.expect("get"),
        Some(Value::Int(2))
    );
    assert_eq!(main.get("shared").await.expect("get"), Some(Value::Int(1)));
}

#[tokio::test]
async fn a_merge_clears_the_cache_so_merged_values_are_visible() {
    let h = Harness::start().await;
    let main = h.scribe_with(|c| c.cache_ttl_ms = 60_000);

    main.put("base", &Value::Int(1)).await.expect("put");
    let branch_id = main.create_branch("feature", 0).await.expect("branch");
    let feature = main.on_branch(branch_id);

    feature
        .put("from-feature", &Value::Int(7))
        .await
        .expect("put");
    // Warm main's cache with the absence, which the merge is about to change.
    assert_eq!(main.get("from-feature").await.expect("get"), None);

    let result = main.merge(branch_id, 0).await.expect("merge");
    assert_eq!(result.status, theta_proto::wire::MergeStatus::Ok);

    assert_eq!(
        main.get("from-feature").await.expect("get"),
        Some(Value::Int(7)),
        "a cached absence survived a merge that filled it in"
    );
}

#[tokio::test]
async fn an_invalid_token_is_refused_rather_than_retried_forever() {
    let h = Harness::start().await;
    let scribe = Scribe::connect(
        ScribeConfig::new(&h.addr),
        // A static source cannot refresh, so one refusal is final.
        Arc::new(StaticToken("not a token".into())),
    );

    match scribe.get("k").await.expect_err("must be refused") {
        ScribeError::Refused { code, .. } => assert_eq!(code, StatusCode::Unauthorized),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn status_comes_back_through_the_runtime() {
    let h = Harness::start().await;
    let scribe = h.scribe();

    scribe.put("k", &Value::Int(1)).await.expect("put");
    let status = scribe.status().await.expect("status");

    assert_eq!(status.project_id, "test-project");
    assert_eq!(status.branch, "main");
    assert_eq!(status.commits_applied, 1);
}

#[tokio::test]
async fn concurrent_callers_share_one_pool_safely() {
    let h = Harness::start().await;
    let scribe = h.scribe_with(|c| {
        c.pool_size = 4;
        c.cache_ttl_ms = 0;
    });

    let mut tasks = Vec::new();
    for w in 0..12u64 {
        let scribe = scribe.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..8u64 {
                scribe
                    .put(&format!("w{w}:k{i}"), &Value::Int((w * 100 + i) as i64))
                    .await
                    .expect("put");
            }
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }

    let status = scribe.status().await.expect("status");
    assert_eq!(status.commits_applied, 96, "a concurrent write was lost");
    // The pool is bounded: concurrency must not leak connections.
    assert!(scribe.idle_connections().await <= 4);
}
