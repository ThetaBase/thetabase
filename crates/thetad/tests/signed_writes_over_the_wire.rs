//! Signed writes end to end, over a real socket.
//!
//! `signed_writes.rs` drives the engine directly. What only this can reach is
//! the part where the server *learns* the key: it comes off the session token,
//! and nothing else in the suite exercises that path. A signing scheme whose key
//! never arrives is one that refuses every honest write.

mod harness;

use ed25519_dalek::{Signer, SigningKey};
use harness::{expect_error, Harness};
use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, Value};
use theta_proto::wire::SignedOp;
use theta_proto::{RequestBody, ResponseBody, StatusCode};

/// The session id the harness mints tokens for.
const SESSION: &str = "sess_test";

fn keypair(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn hex_public(key: &SigningKey) -> String {
    key.verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Ask the server which commit the next entry on `main` will carry.
///
/// The way a real client learns it. Guessing — counting the entries you have
/// seen, or assuming a fresh branch starts at one — is how a caller signs for a
/// position it does not hold, and the first draft of these tests did exactly
/// that and blamed the server.
async fn next_commit(client: &mut harness::Client) -> u64 {
    match client.call(RequestBody::ListBranches).await {
        ResponseBody::Branches { branches } => {
            branches
                .iter()
                .find(|b| b.branch_id == BranchId::MAIN.0)
                .expect("main is always listed")
                .next_commit
        }
        other => panic!("listing branches failed: {other:?}"),
    }
}

/// Sign the entry the server will build, from what a client knows.
fn sign(key: &SigningKey, commit_id: u64, timestamp_ms: i64, value: i64) -> Vec<u8> {
    let entry = LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(commit_id),
        branch_id: BranchId::MAIN,
        op: OpType::Put {
            key: "orders:1".into(),
            value: Value::Int(value),
        },
        author: Author::agent(SESSION, "u_test"),
        timestamp_ms,
    };
    key.sign(&entry.content_hash().0).to_bytes().to_vec()
}

#[tokio::test]
async fn a_signed_write_lands_with_the_key_the_token_carried() {
    // The whole path: the Control Plane binds a public key to a session inside a
    // token the project key signs, the instance learns it on the request, and a
    // signature made by the matching private key is accepted.
    let harness = Harness::start().await;
    let key = keypair(3);
    let mut client =
        harness::Client::connect(harness.addr, harness.signing_token(&hex_public(&key)))
            .await
            .expect("handshake");

    // The commit id comes from the server: a signed entry commits to its own
    // position, and guessing it is how a caller silently signs for a position
    // somebody else holds.
    let commit = next_commit(&mut client).await;
    let timestamp = harness::now_ms();

    let response = client
        .call(RequestBody::SignedWrite {
            commit_id: commit,
            timestamp_ms: timestamp,
            signature: sign(&key, commit, timestamp, 42),
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: harness::json(42),
            },
        })
        .await;

    assert!(
        matches!(response, ResponseBody::Put { .. }),
        "a correctly signed write was refused: {response:?}"
    );
}

#[tokio::test]
async fn a_forged_signature_is_rejected_over_the_wire() {
    // The same forgery the engine test covers, but arriving the way an attacker
    // would send it — and answered with a status a client can act on rather than
    // an internal error.
    let harness = Harness::start().await;
    let honest = keypair(3);
    let attacker = keypair(9);
    let mut client =
        harness::Client::connect(harness.addr, harness.signing_token(&hex_public(&honest)))
            .await
            .expect("handshake");

    let commit = next_commit(&mut client).await;
    let timestamp = harness::now_ms();

    let response = client
        .call(RequestBody::SignedWrite {
            commit_id: commit,
            timestamp_ms: timestamp,
            signature: sign(&attacker, commit, timestamp, 42),
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: harness::json(42),
            },
        })
        .await;

    let error = expect_error(response);
    assert_eq!(
        error.code,
        StatusCode::Rejected,
        "a forged signature came back as something other than a rejection: {error:?}"
    );
    assert!(
        !error.message.is_empty(),
        "the rejection carried no explanation"
    );
}

#[tokio::test]
async fn a_session_whose_token_carries_no_key_cannot_sign() {
    // A caller cannot opt itself into being checkable. The key is bound to the
    // session by the Control Plane, inside a payload the project key signed, so
    // sending a signature without one leaves the server with nothing to check
    // against — and it must refuse rather than accept on faith.
    let harness = Harness::start().await;
    let key = keypair(3);
    let mut client = harness::Client::connect(harness.addr, harness.token())
        .await
        .expect("handshake");

    let commit = next_commit(&mut client).await;
    let timestamp = harness::now_ms();

    let response = client
        .call(RequestBody::SignedWrite {
            commit_id: commit,
            timestamp_ms: timestamp,
            signature: sign(&key, commit, timestamp, 42),
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: harness::json(42),
            },
        })
        .await;

    assert!(
        matches!(response, ResponseBody::Error(_)),
        "a signature was accepted for a session the server holds no key for: {response:?}"
    );
}

#[tokio::test]
async fn a_write_signed_for_a_position_that_moved_comes_back_as_a_conflict() {
    // The caller's retry signal. `Conflict` is the code a client already retries
    // on, and reporting a lost race any other way would either look like a
    // permanent failure or like a server fault.
    let harness = Harness::start().await;
    let key = keypair(3);
    let mut client =
        harness::Client::connect(harness.addr, harness.signing_token(&hex_public(&key)))
            .await
            .expect("handshake");

    let stale = next_commit(&mut client).await;

    // An ordinary write takes the position that was just read.
    let taken = client
        .call(RequestBody::Put {
            key: "orders:0".into(),
            value_json: harness::json(1),
            ttl: 0,
        })
        .await;
    assert!(matches!(taken, ResponseBody::Put { .. }), "{taken:?}");

    // Now sign for the position that has just gone.
    let timestamp = harness::now_ms();
    let response = client
        .call(RequestBody::SignedWrite {
            commit_id: stale,
            timestamp_ms: timestamp,
            signature: sign(&key, stale, timestamp, 42),
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: harness::json(42),
            },
        })
        .await;

    let error = expect_error(response);
    assert_eq!(
        error.code,
        StatusCode::Conflict,
        "a lost race was not reported as a conflict: {error:?}"
    );
}

#[tokio::test]
async fn a_signed_put_and_an_ordinary_put_read_the_value_field_the_same_way() {
    // Found by these tests failing for the wrong reason. The signed path decoded
    // `value` as raw JSON while `put` decoded it as `Value`'s own serde
    // representation, so the same bytes became two different values.
    //
    // That is worse than an ordinary inconsistency, because the signature covers
    // the *decoded* value: a client encoding it the way `put` documents would
    // have got a signature failure, with nothing in the message pointing at the
    // encoding. Pinned by writing identical bytes through both paths and
    // requiring the stored row to match.
    let harness = Harness::start().await;
    let key = keypair(3);
    let mut client =
        harness::Client::connect(harness.addr, harness.signing_token(&hex_public(&key)))
            .await
            .expect("handshake");

    let encoded = harness::json(4242);

    let plain = client
        .call(RequestBody::Put {
            key: "orders:plain".into(),
            value_json: encoded.clone(),
            ttl: 0,
        })
        .await;
    assert!(matches!(plain, ResponseBody::Put { .. }), "{plain:?}");

    let commit = next_commit(&mut client).await;
    let timestamp = harness::now_ms();
    let signed = client
        .call(RequestBody::SignedWrite {
            commit_id: commit,
            timestamp_ms: timestamp,
            signature: sign(&key, commit, timestamp, 4242),
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: encoded,
            },
        })
        .await;
    assert!(
        matches!(signed, ResponseBody::Put { .. }),
        "the signed path rejected bytes the plain path accepted: {signed:?}"
    );

    // And both rows read back as the same value.
    let a = client
        .call(RequestBody::Get {
            key: "orders:plain".into(),
        })
        .await;
    let b = client
        .call(RequestBody::Get {
            key: "orders:1".into(),
        })
        .await;
    match (a, b) {
        (
            ResponseBody::Get {
                value_json: plain, ..
            },
            ResponseBody::Get {
                value_json: signed, ..
            },
        ) => assert_eq!(
            plain, signed,
            "the same bytes written through the two paths stored different values"
        ),
        other => panic!("unexpected read: {other:?}"),
    }
}
