//! M10.5: conditional writes close the read-modify-write gap.
//!
//! The gap, stated exactly: writes to a branch are totally ordered and every
//! one of them succeeds, so two clients doing get-then-put both win and one
//! update is silently lost. `concurrent_writers_to_one_key_serialize_to_one_of_their_values`
//! in `server.rs` asserts the total order; this file asserts what the order
//! alone does not give you.
//!
//! The load-bearing test is `a_contended_counter_loses_no_increment`: N clients
//! read-modify-write the same key concurrently, and the final value must be
//! exactly N. It is written to fail if the precondition check is removed — the
//! unconditional version of the same loop loses updates, and
//! `the_same_contention_without_a_precondition_loses_updates` proves that,
//! which is what makes the first test evidence rather than decoration.

use std::collections::HashSet;
use std::sync::Arc;

use theta_proto::wire::{Precondition, RequestBody, ResponseBody};
use tokio::sync::Mutex;

mod harness;
use harness::{json, Client, Harness};

/// Read a key and return `(value, version)`.
async fn read(client: &mut Client, key: &str) -> (Option<i64>, u64) {
    match client.call(RequestBody::Get { key: key.into() }).await {
        ResponseBody::Get {
            found,
            value_json,
            version_id,
        } => {
            // Decoded the same way it was encoded: `json()` writes a
            // `theta_core::Value`, not a bare integer.
            let value = found.then(|| {
                match serde_json::from_str::<theta_core::Value>(&value_json).expect("a value") {
                    theta_core::Value::Int(n) => n,
                    other => panic!("expected an integer, got {other:?}"),
                }
            });
            (value, version_id)
        }
        other => panic!("expected a get, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rows_version_does_not_move_when_another_row_is_written() {
    // The bug this replaced: `version_id` was the branch's commit counter, so
    // it changed whenever anything was written. A conditional write built on
    // that would fail for reasons having nothing to do with the row in hand.
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "a".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;
    let (_, version_a) = read(&mut client, "a").await;

    client
        .call(RequestBody::Put {
            key: "b".into(),
            value_json: json(2),
            ttl: 0,
        })
        .await;
    let (_, version_a_again) = read(&mut client, "a").await;

    assert_eq!(
        version_a, version_a_again,
        "writing `b` moved `a`'s version, so it is a branch counter and not a \
         row version"
    );
}

#[tokio::test]
async fn a_conditional_write_lands_when_the_row_has_not_moved() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "k".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;
    let (_, version) = read(&mut client, "k").await;

    let response = client
        .call(RequestBody::PutIf {
            key: "k".into(),
            value_json: json(2),
            ttl: 0,
            expect: Precondition::Version(version),
        })
        .await;
    assert!(
        matches!(response, ResponseBody::Put { .. }),
        "expected the write to land, got {response:?}"
    );

    let (value, moved) = read(&mut client, "k").await;
    assert_eq!(value, Some(2));
    assert_ne!(moved, version, "the version did not advance after a write");
}

#[tokio::test]
async fn a_conditional_write_is_refused_when_the_row_moved_underneath_it() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    client
        .call(RequestBody::Put {
            key: "k".into(),
            value_json: json(1),
            ttl: 0,
        })
        .await;
    let (_, stale) = read(&mut client, "k").await;

    // Somebody else writes.
    client
        .call(RequestBody::Put {
            key: "k".into(),
            value_json: json(99),
            ttl: 0,
        })
        .await;

    let response = client
        .call(RequestBody::PutIf {
            key: "k".into(),
            value_json: json(2),
            ttl: 0,
            expect: Precondition::Version(stale),
        })
        .await;

    match response {
        ResponseBody::PreconditionFailed { key, found, actual } => {
            assert_eq!(key, "k");
            assert!(found, "the row exists, so `found` must say so");
            assert_ne!(actual, stale, "the refusal reported the stale version back");
            // The current version, so a retry needs no extra round trip.
            let (_, current) = read(&mut client, "k").await;
            assert_eq!(actual, current, "the reported version is not the real one");
        }
        other => panic!("expected a precondition failure, got {other:?}"),
    }

    let (value, _) = read(&mut client, "k").await;
    assert_eq!(value, Some(99), "the refused write landed anyway");
}

#[tokio::test]
async fn create_only_succeeds_once_and_refuses_after() {
    // `Absent` is a distinct precondition rather than "version zero", so this
    // is expressible at all. A sentinel would make "I forgot the version" mean
    // "this row must not exist", which is a far stronger claim than anyone
    // makes by accident.
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let first = client
        .call(RequestBody::PutIf {
            key: "once".into(),
            value_json: json(1),
            ttl: 0,
            expect: Precondition::Absent,
        })
        .await;
    assert!(matches!(first, ResponseBody::Put { .. }), "{first:?}");

    let second = client
        .call(RequestBody::PutIf {
            key: "once".into(),
            value_json: json(2),
            ttl: 0,
            expect: Precondition::Absent,
        })
        .await;
    match second {
        ResponseBody::PreconditionFailed { found, .. } => {
            assert!(found, "the row exists now, and the refusal should say so")
        }
        other => panic!("create-only wrote twice: {other:?}"),
    }

    let (value, _) = read(&mut client, "once").await;
    assert_eq!(value, Some(1), "the second create overwrote the first");
}

#[tokio::test]
async fn requiring_a_version_on_a_row_that_does_not_exist_is_refused() {
    let h = Harness::start().await;
    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");

    let response = client
        .call(RequestBody::PutIf {
            key: "missing".into(),
            value_json: json(1),
            ttl: 0,
            expect: Precondition::Version(7),
        })
        .await;
    match response {
        ResponseBody::PreconditionFailed { found, .. } => assert!(
            !found,
            "the row does not exist, and the refusal must distinguish that from \
             a version mismatch — they call for different retries"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_deleted_row_is_absent_again() {
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

    let response = client
        .call(RequestBody::PutIf {
            key: "k".into(),
            value_json: json(2),
            ttl: 0,
            expect: Precondition::Absent,
        })
        .await;
    assert!(
        matches!(response, ResponseBody::Put { .. }),
        "a deleted row did not read as absent: {response:?}"
    );
}

// ---- the one that matters ---------------------------------------------------

/// How many clients contend, and how many increments must survive.
const CONTENDERS: i64 = 24;

#[tokio::test]
async fn a_contended_counter_loses_no_increment() {
    // The whole point of the milestone. Each client reads, increments, and
    // writes back conditionally, retrying on refusal. With a correct
    // precondition the final value is exactly `CONTENDERS`; with a broken one
    // it is less, and the test below shows how much less.
    let h = Harness::start().await;

    let mut seed = Client::connect(h.addr, h.token()).await.expect("connect");
    seed.call(RequestBody::Put {
        key: "counter".into(),
        value_json: json(0),
        ttl: 0,
    })
    .await;
    drop(seed);

    let mut workers = Vec::new();
    for _ in 0..CONTENDERS {
        let addr = h.addr;
        let token = h.token();
        workers.push(tokio::spawn(async move {
            let mut client = Client::connect(addr, token).await.expect("connect");
            // Bounded, so a livelock fails the test rather than hanging it.
            for _ in 0..200 {
                let (value, version) = read(&mut client, "counter").await;
                let next = value.expect("the counter exists") + 1;
                match client
                    .call(RequestBody::PutIf {
                        key: "counter".into(),
                        value_json: json(next),
                        ttl: 0,
                        expect: Precondition::Version(version),
                    })
                    .await
                {
                    ResponseBody::Put { .. } => return true,
                    ResponseBody::PreconditionFailed { .. } => continue,
                    other => panic!("unexpected response: {other:?}"),
                }
            }
            false
        }));
    }

    for worker in workers {
        assert!(
            worker.await.expect("task"),
            "a writer exhausted its retries, which means the precondition is \
             refusing writes it should accept"
        );
    }

    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");
    let (value, _) = read(&mut client, "counter").await;
    assert_eq!(
        value,
        Some(CONTENDERS),
        "increments were lost: {CONTENDERS} clients each committed one write, \
         so every one of them must be in the total"
    );
}

#[tokio::test]
async fn the_same_contention_without_a_precondition_loses_updates() {
    // What makes the test above evidence rather than decoration. The identical
    // loop with an unconditional put loses increments — so if the precondition
    // check were removed, the first test would fail rather than quietly keep
    // passing.
    //
    // Asserted as "at least one was lost" rather than an exact count, because
    // how many depends on scheduling. The point is that the number is not
    // `CONTENDERS`, and it never can be without a precondition.
    let h = Harness::start().await;

    let mut seed = Client::connect(h.addr, h.token()).await.expect("connect");
    seed.call(RequestBody::Put {
        key: "counter".into(),
        value_json: json(0),
        ttl: 0,
    })
    .await;
    drop(seed);

    // A barrier, so every reader reads before any writer writes. Without it the
    // clients might genuinely serialize and lose nothing, which would make this
    // test flaky in the direction that matters.
    let gate = Arc::new(Mutex::new(()));
    let held = gate.clone().lock_owned().await;

    let mut workers = Vec::new();
    for _ in 0..CONTENDERS {
        let addr = h.addr;
        let token = h.token();
        let gate = gate.clone();
        workers.push(tokio::spawn(async move {
            let mut client = Client::connect(addr, token).await.expect("connect");
            let (value, _) = read(&mut client, "counter").await;
            let next = value.expect("the counter exists") + 1;
            // Wait until every reader has read.
            let _ = gate.lock().await;
            client
                .call(RequestBody::Put {
                    key: "counter".into(),
                    value_json: json(next),
                    ttl: 0,
                })
                .await;
            next
        }));
    }

    // Give every task time to reach the gate, then release them together.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(held);

    let mut wrote = HashSet::new();
    for worker in workers {
        wrote.insert(worker.await.expect("task"));
    }

    let mut client = Client::connect(h.addr, h.token()).await.expect("connect");
    let (value, _) = read(&mut client, "counter").await;
    let final_value = value.expect("the counter exists");

    assert!(
        final_value < CONTENDERS,
        "an unconditional read-modify-write under contention reached \
         {final_value}, which would mean the conditional test above proves \
         nothing — every client read the same value before any of them wrote"
    );
}
