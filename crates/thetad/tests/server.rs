//! End-to-end tests over a real socket.
//!
//! These close the M1 gate clause that was blocked on there being a server at
//! all (`docs/specs/08-test-validation-plan.md` §2, "concurrent conflicting
//! writes"), and cover the M2 surface: handshake, dispatch, and backpressure.

use std::time::Duration;

use theta_identity::{ProjectKeys, SessionToken};
use theta_proto::{Hello, RequestBody, Response, ResponseBody, StatusCode, PROTOCOL_VERSION};
use thetad::config::Environment;
use thetad::session::Authorizer;
use thetad::{serve, Config, Engine, EngineHandle, ServerConfig};
use tokio::net::TcpStream;

mod harness;
use harness::{expect_error, json, now_ms, read, scope, write, Client, Harness};

// ---- handshake --------------------------------------------------------------

#[tokio::test]
async fn a_matching_client_completes_the_handshake() {
    let h = Harness::start().await;
    let (_client, welcome) = Client::connect_with(h.addr, PROTOCOL_VERSION, h.token())
        .await
        .expect("welcome");
    assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
    assert_eq!(welcome.project_id, "test-project");
}

#[tokio::test]
async fn a_client_from_the_future_is_refused_with_a_reason() {
    let h = Harness::start().await;
    let refusal = Client::connect_with(h.addr, PROTOCOL_VERSION + 1, h.token())
        .await
        .expect_err("must be refused");
    // Refused, and told why — a silent drop is indistinguishable from a network
    // fault and sends clients into retry loops.
    let err = expect_error(refusal.body);
    assert_eq!(err.code, StatusCode::Rejected);
    assert!(err.message.contains("newer"), "got: {}", err.message);
}

#[tokio::test]
async fn a_token_for_another_project_cannot_open_a_connection() {
    let h = Harness::start().await;

    // Minted with a different project's key — the real attack, not an edited
    // field. The signature cannot verify against this instance's key.
    let foreign_keys = ProjectKeys::generate("some-other-project");
    let foreign = SessionToken::mint(&foreign_keys, &scope("some-other-project", "dev"));

    let refusal = Client::connect_with(h.addr, PROTOCOL_VERSION, foreign.into_string())
        .await
        .expect_err("must be refused");
    let err = expect_error(refusal.body);
    assert_eq!(err.code, StatusCode::Unauthorized);
    // The message must not confirm which half was wrong, or it becomes an
    // oracle for guessing tokens (`04-threat-model-security.md` §2).
    assert_eq!(err.message, "invalid session token");
    assert!(!err.message.contains("some-other-project"));
}

#[tokio::test]
async fn an_expired_token_is_refused() {
    let h = Harness::start().await;

    let mut expired = scope("test-project", "dev");
    expired.expires_at_ms = 1;
    let token = SessionToken::mint(&h.keys, &expired);

    let refusal = Client::connect_with(h.addr, PROTOCOL_VERSION, token.into_string())
        .await
        .expect_err("must be refused");
    assert_eq!(expect_error(refusal.body).code, StatusCode::Unauthorized);
}

#[tokio::test]
async fn a_token_whose_payload_was_edited_is_refused() {
    // Editing the payload to extend its life keeps the signature that no longer
    // matches it, which is what the signature is for.
    let h = Harness::start().await;

    let mut short = scope("test-project", "dev");
    short.expires_at_ms = 1;
    let token = SessionToken::mint(&h.keys, &short);

    let mut extended = short.clone();
    extended.expires_at_ms = i64::MAX;
    let payload = serde_json::to_vec(&extended).expect("encode");

    let parts: Vec<&str> = token.as_str().split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], base64_url(&payload), parts[2]);

    let refusal = Client::connect_with(h.addr, PROTOCOL_VERSION, forged)
        .await
        .expect_err("must be refused");
    assert_eq!(expect_error(refusal.body).code, StatusCode::Unauthorized);
}

/// URL-safe base64 without padding, matching the token format.
fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[tokio::test]
async fn garbage_in_place_of_a_hello_is_refused_not_crashed_on() {
    let h = Harness::start().await;
    let mut stream = TcpStream::connect(h.addr).await.expect("connect");
    write(&mut stream, b"this is not a Hello").await;

    let payload = read(&mut stream).await.expect("server replied");
    let response = Response::decode(&payload).expect("a Response");
    assert_eq!(expect_error(response.body).code, StatusCode::Rejected);
}

// ---- round trips ------------------------------------------------------------

#[tokio::test]
async fn a_write_is_readable_by_the_session_that_made_it() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let put = client
        .call(RequestBody::Put {
            key: "user:1".into(),
            value_json: json(42),
            ttl: 0,
        })
        .await;
    assert!(matches!(put, ResponseBody::Put { .. }), "got {put:?}");

    // Read-your-writes: acknowledged means visible, immediately.
    match client
        .call(RequestBody::Get {
            key: "user:1".into(),
        })
        .await
    {
        ResponseBody::Get {
            found, value_json, ..
        } => {
            assert!(found);
            assert_eq!(value_json, json(42));
        }
        other => panic!("expected a get response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_key_reports_not_found_rather_than_erroring() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");
    match client.call(RequestBody::Get { key: "nope".into() }).await {
        ResponseBody::Get { found, .. } => assert!(!found),
        other => panic!("expected a get response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_delete_removes_the_key() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "k".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;
    client.call(RequestBody::Delete { key: "k".into() }).await;

    match client.call(RequestBody::Get { key: "k".into() }).await {
        ResponseBody::Get { found, .. } => assert!(!found, "delete did not take effect"),
        other => panic!("expected a get response, got {other:?}"),
    }
}

#[tokio::test]
async fn status_reports_real_engine_state() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    for i in 0..5 {
        client
            .call(RequestBody::Put {
                key: format!("k{i}"),
                value_json: json(i),
                ttl: 0,
            })
            .await;
    }

    match client.call(RequestBody::Status).await {
        ResponseBody::Status(status) => {
            assert_eq!(status.project_id, "test-project");
            assert_eq!(status.branch, "main");
            assert_eq!(status.protocol_version, PROTOCOL_VERSION);
            assert_eq!(
                status.commits_applied, 5,
                "status must reflect real commits"
            );
            assert!(!status.circuit_breaker_tripped);
        }
        other => panic!("expected a status response, got {other:?}"),
    }
}

#[tokio::test]
async fn branching_and_merging_work_over_the_wire() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "shared".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;

    let branch_id = match client
        .call(RequestBody::CreateBranch {
            name: "feature".into(),
            from: 0,
        })
        .await
    {
        ResponseBody::Branch { branch_id } => branch_id,
        other => panic!("expected a branch response, got {other:?}"),
    };
    assert_ne!(branch_id, 0);

    // The new branch inherits what main had.
    match client
        .call_on(
            branch_id,
            RequestBody::Get {
                key: "shared".into(),
            },
        )
        .await
    {
        ResponseBody::Get { found, .. } => assert!(found, "branch did not inherit main's state"),
        other => panic!("expected a get response, got {other:?}"),
    }

    client
        .call_on(
            branch_id,
            RequestBody::Put {
                key: "only-on-feature".into(),
                value_json: json(7),
                ttl: 0,
            },
        )
        .await;

    match client
        .call(RequestBody::Merge {
            source_branch: branch_id,
            target_branch: 0,
        })
        .await
    {
        ResponseBody::Merge(result) => {
            assert_eq!(result.status, theta_proto::wire::MergeStatus::Ok);
            assert!(result.conflicts.is_empty());
        }
        other => panic!("expected a merge response, got {other:?}"),
    }

    match client
        .call(RequestBody::Get {
            key: "only-on-feature".into(),
        })
        .await
    {
        ResponseBody::Get { found, .. } => assert!(found, "merge did not land on main"),
        other => panic!("expected a get response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_destructive_proposal_comes_back_gated_with_its_diff() {
    let h = Harness::start_protected().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    // Real rows, because the server measures the impact itself. This test used
    // to send `rows_affected: 500_000` against an empty table and the server
    // believed it — which meant the property it claims to prove was being
    // proven against a number the client made up.
    for i in 0..200 {
        let row = theta_core::Value::Map(std::collections::BTreeMap::from([(
            "email".to_string(),
            theta_core::Value::Text(format!("u{i}@example.com")),
        )]));
        let put = client
            .call(RequestBody::Put {
                key: format!("users:{i}"),
                value_json: serde_json::to_string(&row).expect("encode"),
                ttl: 0,
            })
            .await;
        assert!(
            matches!(put, ResponseBody::Put { .. }),
            "seed write: {put:?}"
        );
    }

    let change = serde_json::json!({
        "change": "drop_column", "table": "users", "column": "email"
    })
    .to_string();

    let diff = match client
        .call(RequestBody::ProposeSchemaChange {
            change_json: change.clone(),
        })
        .await
    {
        ResponseBody::Propose(diff) => diff,
        other => panic!("expected a diff, got {other:?}"),
    };
    assert_eq!(
        diff.rows_affected, 200,
        "the server counted the rows itself: {}",
        diff.reason
    );
    assert!(diff.destructive && diff.requires_confirm);

    // Confirmation alone must not clear an irreversible, high-impact change,
    // and the refusal carries the diff so the caller need not ask twice.
    let err = expect_error(
        client
            .call(RequestBody::ApplySchemaChange {
                change_id: diff.change_id.clone(),
                confirm: true,
            })
            .await,
    );
    assert_eq!(err.code, StatusCode::ConfirmationRequired);
    assert_eq!(
        err.diff.expect("diff travels with the refusal").change_id,
        diff.change_id
    );
}

#[tokio::test]
async fn a_malformed_value_is_rejected_without_dropping_the_connection() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let err = expect_error(
        client
            .call(RequestBody::Put {
                key: "k".into(),
                value_json: "{not json".into(),
                ttl: 0,
            })
            .await,
    );
    assert_eq!(err.code, StatusCode::Rejected);

    // The connection survives, so one bad request does not cost a round trip
    // to re-establish.
    match client.call(RequestBody::Status).await {
        ResponseBody::Status(_) => {}
        other => panic!("connection did not survive a bad request: {other:?}"),
    }
}

#[tokio::test]
async fn a_sql_query_runs_over_the_wire_and_returns_arrow() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    for (i, name) in ["Alice", "Bob", "Carol"].iter().enumerate() {
        let row = serde_json::json!({
            "kind": "map",
            "value": { "name": { "kind": "text", "value": name } }
        });
        client
            .call(RequestBody::Put {
                key: format!("users:{i}"),
                value_json: row.to_string(),
                ttl: 0,
            })
            .await;
    }

    let plan = theta_proto::wire::QueryPlanWire {
        raw_query: "SELECT * FROM users".into(),
        ..Default::default()
    };
    match client.call(RequestBody::Query(plan)).await {
        ResponseBody::Query {
            result_set,
            row_count,
            plan_hash,
        } => {
            assert_eq!(row_count, 3);
            assert_ne!(plan_hash, 0, "a compiled plan has a cache key");
            assert!(!result_set.is_empty(), "results travel as Arrow IPC");
        }
        other => panic!("expected a query response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_query_parameter_is_bound_rather_than_interpolated() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    for (i, name) in ["Alice", "Bob"].iter().enumerate() {
        let row = serde_json::json!({
            "kind": "map",
            "value": { "name": { "kind": "text", "value": name } }
        });
        client
            .call(RequestBody::Put {
                key: format!("users:{i}"),
                value_json: row.to_string(),
                ttl: 0,
            })
            .await;
    }

    // A payload that would be catastrophic if it were ever spliced into the
    // query text. It arrives as a bound value and simply matches nothing.
    let hostile = serde_json::json!({
        "kind": "text",
        "value": "Alice'; DROP TABLE users; --"
    });
    let plan = theta_proto::wire::QueryPlanWire {
        raw_query: "SELECT * FROM users WHERE name = $who".into(),
        context_vars: vec![("who".into(), hostile.to_string())],
        ..Default::default()
    };
    match client.call(RequestBody::Query(plan)).await {
        ResponseBody::Query { row_count, .. } => assert_eq!(row_count, 0),
        other => panic!("expected a query response, got {other:?}"),
    }

    // The table is still there, and a real value still matches.
    let plan = theta_proto::wire::QueryPlanWire {
        raw_query: "SELECT * FROM users WHERE name = $who".into(),
        context_vars: vec![(
            "who".into(),
            serde_json::json!({"kind": "text", "value": "Alice"}).to_string(),
        )],
        ..Default::default()
    };
    match client.call(RequestBody::Query(plan)).await {
        ResponseBody::Query { row_count, .. } => assert_eq!(row_count, 1),
        other => panic!("expected a query response, got {other:?}"),
    }
}

#[tokio::test]
async fn explain_describes_a_query_without_running_it() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "users:1".into(),
            value_json: serde_json::json!({"kind": "int", "value": 1}).to_string(),
            ttl: 0,
        })
        .await;

    let plan = theta_proto::wire::QueryPlanWire {
        raw_query: "SELECT * FROM users WHERE a > 1".into(),
        ..Default::default()
    };
    match client.call(RequestBody::Explain(plan)).await {
        ResponseBody::Explain { explanation_json } => {
            assert!(
                explanation_json.contains("Filter"),
                "got: {explanation_json}"
            );
            assert!(
                explanation_json.contains("\"llmCalls\":0"),
                "EXPLAIN must state that no model call is involved: {explanation_json}"
            );
        }
        other => panic!("expected an explain response, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unparseable_query_is_refused_with_a_position() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let plan = theta_proto::wire::QueryPlanWire {
        raw_query: "DROP TABLE users".into(),
        ..Default::default()
    };
    let err = expect_error(client.call(RequestBody::Query(plan)).await);
    assert_eq!(err.code, StatusCode::Rejected);
    // A write statement is not merely unsupported — the read-only subset has no
    // form for it at all.
    assert!(err.message.contains("position"), "got: {}", err.message);
}

// ---- concurrency (closes the M1 gate clause) --------------------------------

#[tokio::test]
async fn concurrent_writers_all_succeed_and_none_are_lost() {
    let h = Harness::start().await;

    // Sixteen independent connections, each writing its own keys at once. The
    // engine serializes them; no writer should see a lost update or an error.
    let mut writers = Vec::new();
    for w in 0..16u64 {
        let addr = h.addr;
        let token = h.token();
        writers.push(tokio::spawn(async move {
            let mut client = Client::connect(addr, token).await.expect("connect");
            for i in 0..10u64 {
                let body = client
                    .call(RequestBody::Put {
                        key: format!("w{w}:k{i}"),
                        value_json: json((w * 100 + i) as i64),
                        ttl: 0,
                    })
                    .await;
                assert!(
                    matches!(body, ResponseBody::Put { .. }),
                    "write failed: {body:?}"
                );
            }
        }));
    }
    for writer in writers {
        writer.await.expect("writer task");
    }

    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");
    match client.call(RequestBody::Status).await {
        ResponseBody::Status(status) => {
            assert_eq!(status.commits_applied, 160, "a concurrent write was lost");
        }
        other => panic!("expected a status response, got {other:?}"),
    }

    // Every value is exactly what its writer wrote — no interleaving corrupted
    // another writer's key.
    for w in 0..16u64 {
        for i in 0..10u64 {
            match client
                .call(RequestBody::Get {
                    key: format!("w{w}:k{i}"),
                })
                .await
            {
                ResponseBody::Get {
                    found, value_json, ..
                } => {
                    assert!(found, "w{w}:k{i} is missing");
                    assert_eq!(value_json, json((w * 100 + i) as i64));
                }
                other => panic!("expected a get response, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn concurrent_writers_to_one_key_serialize_to_one_of_their_values() {
    let h = Harness::start().await;

    let mut writers = Vec::new();
    for w in 0..8i64 {
        let addr = h.addr;
        let token = h.token();
        writers.push(tokio::spawn(async move {
            let mut client = Client::connect(addr, token).await.expect("connect");
            client
                .call(RequestBody::Put {
                    key: "contended".into(),
                    value_json: json(w),
                    ttl: 0,
                })
                .await
        }));
    }
    for writer in writers {
        assert!(matches!(
            writer.await.expect("task"),
            ResponseBody::Put { .. }
        ));
    }

    // Within one branch there is a total order, so the survivor is one of the
    // writes — never a blend, never a missing key.
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");
    match client
        .call(RequestBody::Get {
            key: "contended".into(),
        })
        .await
    {
        ResponseBody::Get {
            found, value_json, ..
        } => {
            assert!(found);
            let written: Vec<String> = (0..8).map(json).collect();
            assert!(
                written.contains(&value_json),
                "got a value nobody wrote: {value_json}"
            );
        }
        other => panic!("expected a get response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_connection_over_the_limit_is_refused_rather_than_starved() {
    let h = Harness::start_with(|c| c.max_connections = 2).await;

    let _a = Client::connect(h.addr, h.token())
        .await
        .expect("first connects");
    let _b = Client::connect(h.addr, h.token())
        .await
        .expect("second connects");

    // The third is refused at accept. It must fail promptly — a hang here is
    // exactly the starvation the limit exists to prevent.
    let third = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(h.addr).await.expect("tcp connect");
        write(
            &mut stream,
            &Hello {
                protocol_version: PROTOCOL_VERSION,
                session_token: h.token(),
                client_name: "third".into(),
            }
            .encode(),
        )
        .await;
        read(&mut stream).await
    })
    .await
    .expect("refusal must be prompt, not a hang");

    assert!(
        third.is_none(),
        "a connection past the limit was served anyway"
    );
}

#[tokio::test]
async fn state_written_over_the_wire_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let keys = ProjectKeys::generate("test-project");

    // First process.
    {
        let mut config = Config::dev_default("test-project");
        config.data_dir = dir.path().to_path_buf();
        config.listen = "127.0.0.1:0".parse().expect("addr");
        let server_config = ServerConfig::from_config(&config);

        let authorizer = std::sync::Arc::new(tokio::sync::RwLock::new(Authorizer::new(
            keys.public_keyset(),
            Environment::Dev,
            60_000,
            now_ms(),
        )));

        let engine = Engine::open(config).expect("open");
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

        let addr = bound.await.expect("bound");
        let token = SessionToken::mint(&keys, &scope("test-project", "dev")).into_string();
        let mut client = Client::connect(addr, token).await.expect("connect");
        client
            .call(RequestBody::Put {
                key: "durable".into(),
                value_json: json(99),
                ttl: 0,
            })
            .await;
        let _ = shutdown.send(());
    }

    // Second process, same data directory.
    let mut config = Config::dev_default("test-project");
    config.data_dir = dir.path().to_path_buf();
    let engine = Engine::open(config).expect("reopen");
    assert_eq!(
        engine.get(theta_core::BranchId::MAIN, "durable"),
        Some(&theta_core::Value::Int(99)),
        "a write acknowledged over the wire did not survive the restart"
    );
}

// ---- the review surface, over a real socket --------------------------------
//
// `theta audit`, `theta branch`, `theta schema show/promote/reject` are all
// this wire surface. Tested here rather than only against the engine, because a
// reviewer's answer travels through encode/decode before it means anything
// (`07-agent-safety-layer.md` §5, §7).

/// Seed `count` two-column rows into `users`.
async fn seed_users(client: &mut Client, count: usize) {
    for i in 0..count {
        let row = theta_core::Value::Map(std::collections::BTreeMap::from([
            (
                "email".to_string(),
                theta_core::Value::Text(format!("u{i}@example.com")),
            ),
            (
                "name".to_string(),
                theta_core::Value::Text(format!("User {i}")),
            ),
        ]));
        let put = client
            .call(RequestBody::Put {
                key: format!("users:{i}"),
                value_json: serde_json::to_string(&row).expect("encode"),
                ttl: 0,
            })
            .await;
        assert!(matches!(put, ResponseBody::Put { .. }), "seed: {put:?}");
    }
}

fn drop_email_json() -> String {
    serde_json::json!({ "change": "drop_column", "table": "users", "column": "email" }).to_string()
}

#[tokio::test]
async fn a_proposal_at_the_shadow_gate_comes_back_already_validated() {
    // The flow §5 describes, and what §8's own example assumes: the redirect
    // and the validation are consequences of the gate, not two more commands a
    // reviewer has to know to run.
    let harness = Harness::start_protected().await;
    let mut client = harness.client().await;
    seed_users(&mut client, 200).await;

    let diff = match client
        .call(RequestBody::ProposeSchemaChange {
            change_json: drop_email_json(),
        })
        .await
    {
        ResponseBody::Propose(diff) => diff,
        other => panic!("expected a diff, got {other:?}"),
    };
    assert_eq!(diff.gate, theta_proto::wire::GateWire::ShadowValidate);
    assert!(
        diff.shadow_branch_id != 0,
        "the proposal should name the branch its change is on"
    );

    let state = match client
        .call(RequestBody::ShowChange {
            change_id: diff.change_id.clone(),
        })
        .await
    {
        ResponseBody::Change(state) => state,
        other => panic!("expected a change, got {other:?}"),
    };
    assert_eq!(state.validation_passed, Some(true), "{state:?}");
    assert!(
        state.checks.iter().all(|c| c.passed),
        "a passing validation with a failing check: {:?}",
        state.checks
    );

    // And the target is untouched until promotion.
    let before = client
        .call(RequestBody::Get {
            key: "users:0".into(),
        })
        .await;
    let ResponseBody::Get { value_json, .. } = before else {
        panic!("expected a row");
    };
    assert!(value_json.contains("email"));

    let promoted = client
        .call(RequestBody::PromoteChange {
            change_id: diff.change_id.clone(),
        })
        .await;
    assert!(
        matches!(
            promoted,
            ResponseBody::Merge(ref r) if r.status == theta_proto::wire::MergeStatus::Ok
        ),
        "{promoted:?}"
    );

    let after = client
        .call(RequestBody::Get {
            key: "users:0".into(),
        })
        .await;
    let ResponseBody::Get { value_json, .. } = after else {
        panic!("expected a row");
    };
    assert!(!value_json.contains("email"), "the drop did not land");
    assert!(value_json.contains("name"), "the whole row was dropped");
}

#[tokio::test]
async fn a_change_at_the_shadow_gate_is_refused_a_confirmation_over_the_wire() {
    let harness = Harness::start_protected().await;
    let mut client = harness.client().await;
    seed_users(&mut client, 200).await;

    let diff = match client
        .call(RequestBody::ProposeSchemaChange {
            change_json: drop_email_json(),
        })
        .await
    {
        ResponseBody::Propose(diff) => diff,
        other => panic!("expected a diff, got {other:?}"),
    };

    let err = expect_error(
        client
            .call(RequestBody::ApplySchemaChange {
                change_id: diff.change_id,
                confirm: true,
            })
            .await,
    );
    assert_eq!(err.code, StatusCode::ConfirmationRequired);
    assert!(err.message.contains("shadow"), "{}", err.message);
}

#[tokio::test]
async fn rejecting_a_change_reclaims_its_branch_and_refuses_it_afterwards() {
    let harness = Harness::start_protected().await;
    let mut client = harness.client().await;
    seed_users(&mut client, 200).await;

    let diff = match client
        .call(RequestBody::ProposeSchemaChange {
            change_json: drop_email_json(),
        })
        .await
    {
        ResponseBody::Propose(diff) => diff,
        other => panic!("expected a diff, got {other:?}"),
    };

    let rejected = client
        .call(RequestBody::RejectChange {
            change_id: diff.change_id.clone(),
            reason: "we still need that column".into(),
        })
        .await;
    assert!(matches!(rejected, ResponseBody::Ok), "{rejected:?}");

    // Nothing left to promote, and nothing left to show.
    expect_error(
        client
            .call(RequestBody::PromoteChange {
                change_id: diff.change_id.clone(),
            })
            .await,
    );

    let branches = match client.call(RequestBody::ListBranches).await {
        ResponseBody::Branches { branches } => branches,
        other => panic!("expected branches, got {other:?}"),
    };
    assert!(
        !branches.iter().any(|b| b.kind == 2),
        "a rejected change left its shadow branch behind"
    );
}

#[tokio::test]
async fn the_audit_trail_comes_back_worst_first_and_honours_the_risk_floor() {
    let harness = Harness::start_protected().await;
    let mut client = harness.client().await;
    seed_users(&mut client, 200).await;

    client
        .call(RequestBody::ProposeSchemaChange {
            change_json: drop_email_json(),
        })
        .await;

    let entries = match client
        .call(RequestBody::Audit {
            limit: 20,
            min_risk: 0,
        })
        .await
    {
        ResponseBody::Audit { entries } => entries,
        other => panic!("expected an audit trail, got {other:?}"),
    };
    assert!(!entries.is_empty());
    assert_eq!(entries[0].risk, 3, "the worst entry should come first");
    assert!(
        entries[0].summary.contains("drop column"),
        "{}",
        entries[0].summary
    );

    // A high floor drops everything below it rather than returning it anyway.
    let high = match client
        .call(RequestBody::Audit {
            limit: 20,
            min_risk: 3,
        })
        .await
    {
        ResponseBody::Audit { entries } => entries,
        other => panic!("expected an audit trail, got {other:?}"),
    };
    assert!(high.iter().all(|e| e.risk == 3));
}

#[tokio::test]
async fn branches_can_be_listed_and_discarded_but_never_main_or_a_shadow() {
    let harness = Harness::start_protected().await;
    let mut client = harness.client().await;
    seed_users(&mut client, 200).await;

    client
        .call(RequestBody::CreateBranch {
            name: "feature".into(),
            from: 0,
        })
        .await;
    client
        .call(RequestBody::ProposeSchemaChange {
            change_json: drop_email_json(),
        })
        .await;

    let branches = match client.call(RequestBody::ListBranches).await {
        ResponseBody::Branches { branches } => branches,
        other => panic!("expected branches, got {other:?}"),
    };
    assert!(branches.iter().any(|b| b.name == "main" && b.protected));
    assert!(branches.iter().any(|b| b.name == "feature"));
    let shadow = branches
        .iter()
        .find(|b| b.kind == 2)
        .expect("the proposal opened a shadow branch");

    // `main` is protected, so discarding it is refused.
    let err = expect_error(
        client
            .call(RequestBody::DiscardBranch {
                name: "main".into(),
            })
            .await,
    );
    assert!(err.message.contains("protected"), "{}", err.message);

    // A shadow branch belongs to the proposal it carries; discarding it would
    // leave a change nobody can answer.
    let err = expect_error(
        client
            .call(RequestBody::DiscardBranch {
                name: shadow.name.clone(),
            })
            .await,
    );
    assert!(err.message.contains("shadow"), "{}", err.message);

    // An ordinary branch discards fine.
    let ok = client
        .call(RequestBody::DiscardBranch {
            name: "feature".into(),
        })
        .await;
    assert!(matches!(ok, ResponseBody::Ok), "{ok:?}");
}
