//! Writes the caller signed, and what that does and does not establish.
//!
//! `author` is otherwise something the *server* records, so the record is
//! exactly as good as the server: an operator with disk access can write an
//! entry attributed to anyone. A signature over the entry hash, by a key the
//! server never holds, makes the author a claim the log can check.
//!
//! What it establishes is narrower than "signed commits" usually implies. It
//! proves the holder of the session key produced the entry. It does not say
//! which human or model was behind that key, and **a stolen session key signs
//! exactly as well as an honest one**. What it removes is the operator.

use std::collections::BTreeMap;

use ed25519_dalek::{Signer, SigningKey};
use theta_core::{Author, BranchId, CommitId, LogEntry, OpType, Value};
use theta_proto::wire::{CrdtMutation, SignedOp};
use thetad::engine::{Engine, EngineError, SignedEnvelope};
use thetad::Config;

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("signed");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

/// A deterministic keypair, so a test can say which key signed.
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

fn author(session: &str) -> Author {
    Author::agent(session, "alice")
}

/// The row, in the encoding the wire uses.
///
/// This has to be the *same* value the signature was computed over, encoded the
/// way the server will decode it. Raw JSON — `{"n": 42}` — is a different thing
/// from `Value`'s serde representation, and a mismatch here produces a signature
/// failure whose message says nothing about encoding.
fn row(n: i64) -> String {
    serde_json::to_string(&Value::Map(BTreeMap::from([(
        "n".to_string(),
        Value::Int(n),
    )])))
    .expect("json")
}

/// Build the entry a caller would build, and sign it.
///
/// This is deliberately the *client's* job in the shape a client would do it:
/// the entry is assembled from what the caller knows — its own author, the
/// branch, the commit id the server published, and the op it wants — and the
/// signature is over that entry's content hash. Nothing here asks the server to
/// assemble anything.
fn sign(
    key: &SigningKey,
    branch: BranchId,
    commit_id: u64,
    timestamp_ms: i64,
    author: Author,
    op: OpType,
) -> Vec<u8> {
    let entry = LogEntry {
        // `prev_hash` is not covered by `content_hash`, so a caller does not
        // need the head to sign — only to know which commit it is signing for.
        prev_hash: theta_core::ContentHash::ZERO,
        commit_id: CommitId(commit_id),
        branch_id: branch,
        op,
        author,
        timestamp_ms,
    };
    key.sign(&entry.content_hash().0).to_bytes().to_vec()
}

fn put_op(key: &str, n: i64) -> OpType {
    OpType::Put {
        key: key.into(),
        value: Value::Map(BTreeMap::from([("n".to_string(), Value::Int(n))])),
    }
}

// ---- the write lands ------------------------------------------------------

#[test]
fn a_correctly_signed_write_lands() {
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    let commit = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        put_op("orders:1", 42),
    );

    engine
        .append_signed(
            BranchId::MAIN,
            SignedOp::Put {
                key: "orders:1".into(),
                value_json: row(42),
            },
            author("s1"),
            SignedEnvelope {
                commit_id: commit,
                timestamp_ms: 5_000,
                signature,
            },
            5_000,
        )
        .expect("a correctly signed write was refused");

    assert!(engine.get(BranchId::MAIN, "orders:1").is_some());
    assert_eq!(
        engine.verify_signatures(BranchId::MAIN).expect("verify"),
        1,
        "the signed entry was not counted as verified"
    );
}

#[test]
fn a_signed_delete_and_a_signed_crdt_mutation_both_land() {
    // Every data operation a caller can perform, because a signing scheme that
    // covers puts and not deletes leaves the operation that destroys data as the
    // one nobody can attribute.
    let (mut engine, _dir) = engine();
    let key = keypair(2);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    engine
        .put(
            BranchId::MAIN,
            "orders:1",
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(1))])),
            author("setup"),
            1_000,
        )
        .expect("seed");

    let commit = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        OpType::Delete {
            key: "orders:1".into(),
        },
    );
    engine
        .append_signed(
            BranchId::MAIN,
            SignedOp::Delete {
                key: "orders:1".into(),
            },
            author("s1"),
            SignedEnvelope {
                commit_id: commit,
                timestamp_ms: 5_000,
                signature,
            },
            5_000,
        )
        .expect("a signed delete was refused");
    assert!(engine.get(BranchId::MAIN, "orders:1").is_none());

    let commit = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_001,
        author("s1"),
        OpType::Crdt {
            key: "stats:views".into(),
            mutation: theta_core::CrdtOp::Increment { by: 3 },
        },
    );
    engine
        .append_signed(
            BranchId::MAIN,
            SignedOp::Crdt {
                key: "stats:views".into(),
                mutation: CrdtMutation::Increment { by: 3 },
            },
            author("s1"),
            SignedEnvelope {
                commit_id: commit,
                timestamp_ms: 5_001,
                signature,
            },
            5_001,
        )
        .expect("a signed CRDT mutation was refused");
    assert_eq!(
        engine.get_crdt(BranchId::MAIN, "stats:views"),
        Some(Value::Int(3))
    );
}

// ---- forgery --------------------------------------------------------------

#[test]
fn a_signature_from_the_wrong_key_is_refused() {
    // The property the whole mechanism exists for. Somebody who can reach this
    // instance but does not hold the session's private key cannot write as that
    // session.
    let (mut engine, _dir) = engine();
    let real = keypair(1);
    let attacker = keypair(9);
    engine
        .register_session_key("s1", &hex_public(&real))
        .expect("register");

    let commit = engine.next_commit(BranchId::MAIN);
    let forged = sign(
        &attacker,
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        put_op("orders:1", 42),
    );

    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(42),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: 5_000,
            signature: forged,
        },
        5_000,
    );

    assert!(
        matches!(result, Err(EngineError::Signature(_))),
        "a write signed by the wrong key was accepted: {result:?}"
    );
    assert!(
        engine.get(BranchId::MAIN, "orders:1").is_none(),
        "the forged write landed anyway"
    );
}

#[test]
fn a_signature_over_a_different_value_does_not_cover_the_one_sent() {
    // The signature is over the entry, so changing the payload after signing
    // must break it. Otherwise a signature would authenticate the *shape* of a
    // write rather than the write.
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    let commit = engine.next_commit(BranchId::MAIN);
    // Signed for 42...
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        put_op("orders:1", 42),
    );

    // ...sent as 999.
    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(999),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: 5_000,
            signature,
        },
        5_000,
    );
    assert!(
        matches!(result, Err(EngineError::Signature(_))),
        "the value was changed after signing and the write was accepted: {result:?}"
    );
}

#[test]
fn signing_another_sessions_entry_fails_because_the_key_is_chosen_by_the_server() {
    // Named for what it actually establishes. The first version was called
    // `one_session_cannot_sign_for_another` and a plant deleting the key-id
    // guard in `SignatureBook::verify` passed it — because this path never
    // reaches that guard.
    //
    // `append_signed` derives the key id from the *author*, which comes from the
    // token, so a caller cannot nominate which key should check their signature.
    // Mallory's signature is therefore checked against the victim's key and
    // fails there. The property holds by construction on this path; the guard
    // covers the paths where a signature arrives with a key id attached, and is
    // tested in `theta-storage` where that is reachable.
    let (mut engine, _dir) = engine();
    let mallory = keypair(7);
    engine
        .register_session_key("mallory", &hex_public(&mallory))
        .expect("register");
    engine
        .register_session_key("victim", &hex_public(&keypair(1)))
        .expect("register");

    let commit = engine.next_commit(BranchId::MAIN);
    // Mallory signs an entry that claims to be the victim's.
    let signature = sign(
        &mallory,
        BranchId::MAIN,
        commit,
        5_000,
        author("victim"),
        put_op("orders:1", 42),
    );

    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(42),
        },
        author("victim"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: 5_000,
            signature,
        },
        5_000,
    );
    assert!(
        matches!(result, Err(EngineError::Signature(_))),
        "one session signed an entry attributed to another: {result:?}"
    );
}

#[test]
fn a_session_with_no_registered_key_cannot_sign() {
    let (mut engine, _dir) = engine();
    let key = keypair(1);

    let commit = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_000,
        author("unknown"),
        put_op("orders:1", 42),
    );

    assert!(
        engine
            .append_signed(
                BranchId::MAIN,
                SignedOp::Put {
                    key: "orders:1".into(),
                    value_json: row(42),
                },
                author("unknown"),
                SignedEnvelope {
                    commit_id: commit,
                    timestamp_ms: 5_000,
                    signature,
                },
                5_000,
            )
            .is_err(),
        "a session the instance holds no key for signed successfully"
    );
}

// ---- the fields the caller chose are the fields the server checks ----------

#[test]
fn a_write_signed_for_a_commit_somebody_else_took_is_refused_and_retryable() {
    // The caller signed a specific position in the log. If somebody wrote in
    // between, the entry that would land is not the entry that was signed.
    // Refused rather than re-signed on the caller's behalf, because the server
    // cannot sign — and refused as a *conflict*, because retrying is the answer.
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    let stale = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        stale,
        5_000,
        author("s1"),
        put_op("orders:1", 42),
    );

    // Somebody else takes the position first.
    engine
        .put(
            BranchId::MAIN,
            "orders:other",
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(0))])),
            author("someone-else"),
            5_001,
        )
        .expect("interloping write");

    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(42),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: stale,
            timestamp_ms: 5_000,
            signature: signature.clone(),
        },
        5_002,
    );
    assert!(
        matches!(result, Err(EngineError::SignedWriteRaced { .. })),
        "a write signed for a taken commit was accepted: {result:?}"
    );

    // And the retry works: re-read the head, sign again, land.
    let fresh = engine.next_commit(BranchId::MAIN);
    assert_ne!(fresh, stale, "the fixture did not actually move the head");
    let signature = sign(
        &key,
        BranchId::MAIN,
        fresh,
        5_003,
        author("s1"),
        put_op("orders:1", 42),
    );
    engine
        .append_signed(
            BranchId::MAIN,
            SignedOp::Put {
                key: "orders:1".into(),
                value_json: row(42),
            },
            author("s1"),
            SignedEnvelope {
                commit_id: fresh,
                timestamp_ms: 5_003,
                signature,
            },
            5_003,
        )
        .expect("the retry was refused too");
}

#[test]
fn a_timestamp_far_from_the_instances_clock_is_refused() {
    // The timestamp is inside the signature, so without a bound a caller could
    // sign an entry dated years from now, hold it, and have the record say it
    // was written then.
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    let commit = engine.next_commit(BranchId::MAIN);
    let far_future = 5_000 + 365 * 24 * 60 * 60 * 1_000;
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        far_future,
        author("s1"),
        put_op("orders:1", 42),
    );

    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(42),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: far_future,
            signature,
        },
        5_000,
    );
    assert!(
        matches!(result, Err(EngineError::BadRequest { .. })),
        "an entry dated a year ahead was accepted: {result:?}"
    );
}

// ---- a signature is evidence, never an exemption ---------------------------

#[test]
fn a_signed_write_is_still_type_checked() {
    // A signature says who wrote something. It says nothing about whether what
    // they wrote is allowed, and a path that skipped the checks would make
    // signing a way around them.
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    // Declare `orders.n` as an Int, then sign a write putting text in it.
    let diff = engine.propose_schema_change(
        BranchId::MAIN,
        theta_core::schema::SchemaChange::AddTable {
            table: theta_core::schema::TableDef {
                name: "orders".into(),
                fields: BTreeMap::from([(
                    "n".to_string(),
                    theta_core::schema::FieldDef {
                        name: "n".into(),
                        ty: theta_core::ValueType::Int,
                        nullable: false,
                        crdt: None,
                        declared_at: None,
                    },
                )]),
                indexes: vec![],
            },
        },
        author("setup"),
        1_000,
    );
    engine
        .apply_schema_change(&diff.change_id, true, author("setup"), 1_001)
        .expect("table");

    let commit = engine.next_commit(BranchId::MAIN);
    let bad = OpType::Put {
        key: "orders:1".into(),
        value: Value::Map(BTreeMap::from([(
            "n".to_string(),
            Value::Text("not an int".into()),
        )])),
    };
    let signature = sign(&key, BranchId::MAIN, commit, 5_000, author("s1"), bad);

    let result = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: serde_json::to_string(&Value::Map(BTreeMap::from([(
                "n".to_string(),
                Value::Text("not an int".into()),
            )])))
            .expect("encode"),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: 5_000,
            signature,
        },
        5_000,
    );
    assert!(
        matches!(result, Err(EngineError::TypeMismatch { .. })),
        "a signed write skipped the type check: {result:?}"
    );
}

#[test]
fn a_forged_write_is_refused_before_it_can_spend_breaker_budget() {
    // Order matters. A forged request that reached the breaker on its way to
    // being refused would let anyone who can reach the port exhaust a project's
    // write ceiling without ever writing anything.
    let (mut engine, _dir) = engine();
    engine
        .register_session_key("s1", &hex_public(&keypair(1)))
        .expect("register");

    let before = engine.breaker().window_rows();
    let commit = engine.next_commit(BranchId::MAIN);
    let forged = sign(
        &keypair(9),
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        put_op("orders:1", 42),
    );

    let _ = engine.append_signed(
        BranchId::MAIN,
        SignedOp::Put {
            key: "orders:1".into(),
            value_json: row(42),
        },
        author("s1"),
        SignedEnvelope {
            commit_id: commit,
            timestamp_ms: 5_000,
            signature: forged,
        },
        5_000,
    );

    assert_eq!(
        engine.breaker().window_rows(),
        before,
        "a forged write spent breaker budget on its way to being refused"
    );
}

// ---- what verification is driven by ---------------------------------------

#[test]
fn verification_is_driven_by_the_entries_not_by_the_signatures_present() {
    // A signature cannot live inside the thing it signs, so signatures sit
    // beside entries — and a missing one does not break the chain. Iterating the
    // signatures would verify only what is present, which is exactly what an
    // attacker who stripped one would want.
    let (mut engine, _dir) = engine();
    let key = keypair(1);
    engine
        .register_session_key("s1", &hex_public(&key))
        .expect("register");

    // One signed entry.
    let commit = engine.next_commit(BranchId::MAIN);
    let signature = sign(
        &key,
        BranchId::MAIN,
        commit,
        5_000,
        author("s1"),
        put_op("orders:1", 1),
    );
    engine
        .append_signed(
            BranchId::MAIN,
            SignedOp::Put {
                key: "orders:1".into(),
                value_json: row(1),
            },
            author("s1"),
            SignedEnvelope {
                commit_id: commit,
                timestamp_ms: 5_000,
                signature,
            },
            5_000,
        )
        .expect("signed write");

    // And one the same session wrote *without* signing — the shape of a stripped
    // signature, and the case that must fail.
    engine
        .put(
            BranchId::MAIN,
            "orders:2",
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(2))])),
            author("s1"),
            5_001,
        )
        .expect("unsigned write");

    assert!(
        engine.verify_signatures(BranchId::MAIN).is_err(),
        "an entry from a signing session with no signature verified anyway"
    );
}

#[test]
fn a_session_that_never_signs_is_not_treated_as_tampering() {
    // Otherwise this is unusable on any project that has not adopted signing
    // everywhere, and a check nobody can turn on protects nothing.
    let (mut engine, _dir) = engine();
    engine
        .put(
            BranchId::MAIN,
            "orders:1",
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(1))])),
            author("never-signs"),
            5_000,
        )
        .expect("write");

    assert_eq!(
        engine.verify_signatures(BranchId::MAIN).expect("verify"),
        0,
        "a project with no signing sessions reported signature failures"
    );
}
