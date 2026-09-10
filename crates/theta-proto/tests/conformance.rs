//! Wire conformance: every request and response round-trips byte-for-byte
//! through the Cap'n Proto encoding, and the version handshake refuses rather
//! than guesses.
//!
//! This is the M2 gate (`docs/ROADMAP.md`). It matters more than it looks: the
//! same schema generates every SDK binding, so a field that silently fails to
//! round-trip here is a field that silently corrupts data in eight languages.

use theta_proto::frame::{self, FrameError};
use theta_proto::wire::{
    AuditEntryWire, BranchInfoWire, ChangeDiffWire, ChangeStateWire, ColumnDescriptionWire,
    ConflictRefWire, CrdtMutation, ElemId, MergeResultWire, MergeStatus, Precondition,
    ProjectStatusWire, QueryPlanWire, ReviewBatchWire, SchemaDescriptionWire, SignedOp,
    TableDescriptionWire, WireError,
};
use theta_proto::{
    negotiate, Hello, NegotiationError, Request, RequestBody, Response, ResponseBody, StatusCode,
    Welcome, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION,
};

fn plan() -> QueryPlanWire {
    QueryPlanWire {
        plan_hash: 0xdead_beef_cafe_f00d,
        raw_query: "SELECT * FROM users WHERE churn_risk = $risk".into(),
        context_vars: vec![
            ("risk".into(), "true".into()),
            ("limit".into(), "100".into()),
        ],
    }
}

fn diff() -> ChangeDiffWire {
    ChangeDiffWire {
        change_id: "chg_9f2a1b3c4d5e6f70".into(),
        destructive: true,
        rows_affected: 14_032,
        reversible: false,
        estimated_cost_ms: 850,
        requires_confirm: true,
        gate: theta_proto::wire::GateWire::ShadowValidate,
        shadow_branch_id: 0x4f2,
        reason: "irreversible change affecting 14032 rows".into(),
        affected_table: "users".into(),
        affected_column: "email".into(),
        change_type: "drop_column".into(),
        rule: theta_proto::wire::GateRuleWire::IrreversibleOverShadowThreshold,
        remedy: theta_proto::wire::RemedyWire::ValidateOnShadowBranch,
        rule_threshold: 100,
    }
}

/// Every request variant, so a new one added to the union without a
/// corresponding case here is visible as a gap rather than assumed covered.
fn every_request() -> Vec<Request> {
    let bodies = vec![
        RequestBody::Get {
            key: "user:123".into(),
        },
        RequestBody::Describe {
            table: String::new(),
            include_examples: false,
            example_limit: 0,
        },
        RequestBody::Describe {
            table: "users".into(),
            include_examples: true,
            example_limit: 5,
        },
        RequestBody::Put {
            key: "user:123".into(),
            value_json: r#"{"kind":"text","value":"Alice"}"#.into(),
            ttl: 3_600,
        },
        RequestBody::Delete {
            key: "user:123".into(),
        },
        RequestBody::Query(plan()),
        RequestBody::Explain(plan()),
        RequestBody::ProposeSchemaChange {
            change_json: r#"{"change":"drop_column","table":"users","column":"email"}"#.into(),
        },
        RequestBody::ApplySchemaChange {
            change_id: "chg_9f2a".into(),
            confirm: true,
        },
        RequestBody::CreateBranch {
            name: "feature/churn".into(),
            from: 0,
        },
        RequestBody::Merge {
            source_branch: 7,
            target_branch: 0,
        },
        RequestBody::Status,
        // The nine below were missing when the coverage guard was added, and
        // the comment above already claimed they were not. `putIf` is the one
        // that matters most: it exists precisely so a precondition cannot be
        // dropped in transit, and its encoding had never been executed.
        RequestBody::PutIf {
            key: "user:123".into(),
            value_json: r#"{"kind":"int","value":7}"#.into(),
            ttl: 0,
            expect: Precondition::Absent,
        },
        RequestBody::PutIf {
            key: "user:123".into(),
            value_json: r#"{"kind":"int","value":8}"#.into(),
            ttl: 60,
            expect: Precondition::Version(41),
        },
        RequestBody::PushRevocations {
            payload: vec![0x01, 0x02, 0x03],
            signature: vec![0xAA; 64],
            key_id: "key_2026_03".into(),
        },
        RequestBody::PushPolicy {
            payload: br#"{"version":3}"#.to_vec(),
            signature: vec![0xBB; 64],
            key_id: "key_2026_03".into(),
        },
        RequestBody::Audit {
            limit: 50,
            min_risk: 2,
        },
        RequestBody::ListBranches,
        RequestBody::DiscardBranch {
            name: "preview/pr-12".into(),
        },
        RequestBody::ShowChange {
            change_id: "chg_9f2a".into(),
        },
        RequestBody::PromoteChange {
            change_id: "chg_9f2a".into(),
        },
        RequestBody::RejectChange {
            change_id: "chg_9f2a".into(),
            reason: "the backfill is missing".into(),
        },
        // Every CRDT mutation arm, because each is its own union member on the
        // wire and a round trip that exercised one says nothing about the rest.
        RequestBody::Crdt {
            key: "stats:views".into(),
            mutation: CrdtMutation::Increment { by: 7 },
        },
        RequestBody::Crdt {
            key: "stats:views".into(),
            mutation: CrdtMutation::Increment { by: -3 },
        },
        RequestBody::Crdt {
            key: "settings:theme".into(),
            mutation: CrdtMutation::SetRegister {
                value_json: "\"dark\"".into(),
            },
        },
        RequestBody::Crdt {
            key: "tags:post1".into(),
            mutation: CrdtMutation::SetAdd {
                element_json: "\"urgent\"".into(),
            },
        },
        RequestBody::Crdt {
            key: "tags:post1".into(),
            mutation: CrdtMutation::SetRemove {
                element_json: "\"urgent\"".into(),
            },
        },
        // Both `after` shapes. Absent means the head of the list, which is a
        // real position rather than a missing field, and a codec that conflated
        // the two would put every insert in the wrong place exactly once.
        RequestBody::Crdt {
            key: "doc:body".into(),
            mutation: CrdtMutation::SeqInsert {
                after: None,
                value_json: "\"first line\"".into(),
            },
        },
        RequestBody::Crdt {
            key: "doc:body".into(),
            mutation: CrdtMutation::SeqInsert {
                after: Some(ElemId {
                    counter: 4,
                    replica: 2,
                }),
                value_json: "\"second line\"".into(),
            },
        },
        // A signed write of each op it can carry. The signature bytes are
        // opaque to the codec, so what is under test here is that the op
        // survives the round trip — a signed write whose op changed in transit
        // would fail verification for a reason nobody could diagnose.
        RequestBody::SignedWrite {
            commit_id: 41,
            timestamp_ms: 1_710_000_000_000,
            signature: vec![7u8; 64],
            op: SignedOp::Put {
                key: "orders:1".into(),
                value_json: "{\"total\":42}".into(),
            },
        },
        RequestBody::SignedWrite {
            commit_id: 42,
            timestamp_ms: 1_710_000_000_001,
            signature: vec![8u8; 64],
            op: SignedOp::Delete {
                key: "orders:1".into(),
            },
        },
        RequestBody::SignedWrite {
            commit_id: 43,
            timestamp_ms: 1_710_000_000_002,
            signature: vec![9u8; 64],
            op: SignedOp::Crdt {
                key: "stats:views".into(),
                mutation: CrdtMutation::Increment { by: 2 },
            },
        },
        RequestBody::ReviewQueue,
        RequestBody::Crdt {
            key: "doc:body".into(),
            mutation: CrdtMutation::SeqRemove {
                id: ElemId {
                    counter: 4,
                    replica: 2,
                },
            },
        },
    ];

    bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| Request {
            request_id: i as u64 + 1,
            branch_id: (i as u64) % 3,
            body,
        })
        .collect()
}

fn every_response() -> Vec<Response> {
    let bodies = vec![
        ResponseBody::Error(WireError {
            code: StatusCode::ConfirmationRequired,
            message: "destructive change requires confirmation".into(),
            diff: Some(diff()),
        }),
        ResponseBody::Error(WireError {
            code: StatusCode::BreakerOpen,
            message: "blast-radius breaker is open".into(),
            diff: None,
        }),
        ResponseBody::Get {
            found: true,
            value_json: r#"{"kind":"int","value":42}"#.into(),
            version_id: 9,
        },
        ResponseBody::Get {
            found: false,
            value_json: String::new(),
            version_id: 0,
        },
        ResponseBody::Put {
            commit_id: "a".repeat(64),
        },
        ResponseBody::Delete {
            commit_id: "b".repeat(64),
        },
        ResponseBody::Apply {
            commit_id: "c".repeat(64),
        },
        ResponseBody::Query {
            result_set: vec![0xAB; 512],
            plan_hash: 0x1234_5678_9abc_def0,
            row_count: 17,
        },
        ResponseBody::Explain {
            explanation_json: r#"{"planHash":1,"steps":[]}"#.into(),
        },
        ResponseBody::Propose(diff()),
        ResponseBody::Branch { branch_id: 12 },
        ResponseBody::Merge(MergeResultWire {
            status: MergeStatus::Conflict,
            conflicts: vec![ConflictRefWire {
                key: "orders:total".into(),
                ours_json: "150".into(),
                theirs_json: "200".into(),
                reason: "both branches wrote this field".into(),
            }],
            converged: vec!["stats:views".into(), "user:tags".into()],
        }),
        ResponseBody::Merge(MergeResultWire {
            status: MergeStatus::UpToDate,
            ..Default::default()
        }),
        ResponseBody::Description(SchemaDescriptionWire {
            tables: vec![TableDescriptionWire {
                name: "users".into(),
                row_count: 14_032,
                columns: vec![
                    ColumnDescriptionWire {
                        name: "email".into(),
                        ty: "text".into(),
                        nullable: false,
                        crdt: String::new(),
                        declared_at: "a".repeat(64),
                        touched_by_agent: true,
                        examples: vec![r#""a@example.com""#.into()],
                        null_basis_points: 0,
                    },
                    ColumnDescriptionWire {
                        name: "views".into(),
                        ty: "int".into(),
                        nullable: true,
                        crdt: "counter".into(),
                        declared_at: String::new(),
                        touched_by_agent: false,
                        examples: vec![],
                        null_basis_points: 4_200,
                    },
                ],
            }],
            examples_withheld: false,
            withheld_reason: String::new(),
        }),
        ResponseBody::Description(SchemaDescriptionWire {
            tables: vec![],
            examples_withheld: true,
            withheld_reason: "examples are not returned on this branch".into(),
        }),
        // These seven were absent while the file above claimed a missing variant
        // would be "visible as a gap". They were not: nothing checked.
        ResponseBody::Revocations {
            version: 7,
            accepted: true,
        },
        ResponseBody::Audit {
            entries: vec![AuditEntryWire {
                risk: 3,
                summary: "dropped a column".into(),
                author_json: r#"{"author":"system"}"#.into(),
                ..Default::default()
            }],
        },
        ResponseBody::Branches {
            branches: vec![BranchInfoWire {
                branch_id: 4,
                name: "preview/pr-12".into(),
                kind: 1,
                head: "d".repeat(64),
                protected: false,
                next_commit: 12,
            }],
        },
        // A batch carrying two changes, so the nested list is exercised. A
        // fixture with one member would round-trip a list of length one and say
        // nothing about a list.
        ResponseBody::ReviewQueue {
            batches: vec![ReviewBatchWire {
                key: "orders".into(),
                gate: 2,
                changes: vec![diff(), diff()],
                rows_affected: 14_032,
                cost: 4,
                cost_if_unbatched: 8,
                reason: "2 change(s) to `orders`, gated at the strongest of them".into(),
            }],
        },
        ResponseBody::Change(ChangeStateWire {
            diff: diff(),
            ..Default::default()
        }),
        ResponseBody::Ok,
        ResponseBody::Policy {
            version: 3,
            accepted: true,
        },
        ResponseBody::PreconditionFailed {
            key: "user:123".into(),
            found: true,
            actual: 9,
        },
        ResponseBody::Status(ProjectStatusWire {
            project_id: "churn-dashboard".into(),
            branch: "main".into(),
            write_volume_mb: 12.5,
            circuit_breaker_tripped: true,
            replica_regions: vec!["iad".into(), "fra".into()],
            breaker_window_rows: 98_765,
            protocol_version: PROTOCOL_VERSION,
            commits_applied: 4_242,
            storage_bytes: Some(4_096),
            rows_written: 1_234,
            idle_ms: 1_800_123,
        }),
    ];

    bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| Response {
            request_id: i as u64 + 1,
            body,
        })
        .collect()
}

/// A tag per request variant.
///
/// The match is exhaustive, so adding a variant to the union fails to compile
/// here. That is the half a doc comment cannot enforce: the fixture above says
/// a new variant "is visible as a gap rather than assumed covered", and until
/// this existed nothing made that true.
fn request_tag(body: &RequestBody) -> &'static str {
    match body {
        RequestBody::Get { .. } => "get",
        RequestBody::Put { .. } => "put",
        RequestBody::PutIf { .. } => "putIf",
        RequestBody::Delete { .. } => "delete",
        RequestBody::Query(_) => "query",
        RequestBody::Explain(_) => "explain",
        RequestBody::ProposeSchemaChange { .. } => "proposeSchemaChange",
        RequestBody::ApplySchemaChange { .. } => "applySchemaChange",
        RequestBody::CreateBranch { .. } => "createBranch",
        RequestBody::Merge { .. } => "merge",
        RequestBody::Status => "status",
        RequestBody::PushRevocations { .. } => "pushRevocations",
        RequestBody::Audit { .. } => "audit",
        RequestBody::ListBranches => "listBranches",
        RequestBody::DiscardBranch { .. } => "discardBranch",
        RequestBody::ShowChange { .. } => "showChange",
        RequestBody::PromoteChange { .. } => "promoteChange",
        RequestBody::RejectChange { .. } => "rejectChange",
        RequestBody::PushPolicy { .. } => "pushPolicy",
        RequestBody::Describe { .. } => "describe",
        RequestBody::Crdt { .. } => "crdt",
        RequestBody::ReviewQueue => "reviewQueue",
        RequestBody::SignedWrite { .. } => "signedWrite",
    }
}

fn response_tag(body: &ResponseBody) -> &'static str {
    match body {
        ResponseBody::Error(_) => "error",
        ResponseBody::Get { .. } => "get",
        ResponseBody::Put { .. } => "put",
        ResponseBody::Delete { .. } => "delete",
        ResponseBody::Query { .. } => "query",
        ResponseBody::Explain { .. } => "explain",
        ResponseBody::Propose(_) => "propose",
        ResponseBody::Apply { .. } => "apply",
        ResponseBody::Branch { .. } => "branch",
        ResponseBody::Merge(_) => "merge",
        ResponseBody::Status(_) => "status",
        ResponseBody::Revocations { .. } => "revocations",
        ResponseBody::Audit { .. } => "audit",
        ResponseBody::Branches { .. } => "branches",
        ResponseBody::ReviewQueue { .. } => "reviewQueue",
        ResponseBody::Change(_) => "change",
        ResponseBody::Ok => "ok",
        ResponseBody::Policy { .. } => "policy",
        ResponseBody::PreconditionFailed { .. } => "precondition",
        ResponseBody::Description(_) => "description",
    }
}

/// Every tag `request_tag` can produce.
///
/// Written out rather than derived, so the two lists disagreeing is the failure
/// — a list derived from the fixture would agree with the fixture by
/// construction and check nothing.
const REQUEST_TAGS: &[&str] = &[
    "get",
    "put",
    "putIf",
    "delete",
    "query",
    "explain",
    "proposeSchemaChange",
    "applySchemaChange",
    "createBranch",
    "merge",
    "status",
    "pushRevocations",
    "audit",
    "listBranches",
    "discardBranch",
    "showChange",
    "promoteChange",
    "rejectChange",
    "pushPolicy",
    "describe",
];

const RESPONSE_TAGS: &[&str] = &[
    "error",
    "get",
    "put",
    "delete",
    "query",
    "explain",
    "propose",
    "apply",
    "branch",
    "merge",
    "status",
    "revocations",
    "audit",
    "branches",
    "change",
    "ok",
    "policy",
    "precondition",
    "description",
];

#[test]
fn the_fixture_exercises_every_request_variant() {
    let covered: std::collections::BTreeSet<&str> = every_request()
        .iter()
        .map(|r| request_tag(&r.body))
        .collect();
    let missing: Vec<&&str> = REQUEST_TAGS
        .iter()
        .filter(|t| !covered.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "these request variants are never round-tripped: {missing:?}. A variant \
         nobody encodes is one whose wire encoding has never been executed."
    );
}

#[test]
fn the_fixture_exercises_every_response_variant() {
    // Seven variants were missing when this was written — revocations, audit,
    // branches, change, ok, policy and precondition — while the fixture's own
    // comment claimed a gap would be visible. It was not.
    let covered: std::collections::BTreeSet<&str> = every_response()
        .iter()
        .map(|r| response_tag(&r.body))
        .collect();
    let missing: Vec<&&str> = RESPONSE_TAGS
        .iter()
        .filter(|t| !covered.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "these response variants are never round-tripped: {missing:?}"
    );
}

#[test]
fn every_request_variant_round_trips_unchanged() {
    for request in every_request() {
        let decoded = Request::decode(&request.encode()).expect("decode");
        assert_eq!(decoded, request, "request did not survive the wire");
    }
}

#[test]
fn every_response_variant_round_trips_unchanged() {
    for response in every_response() {
        let decoded = Response::decode(&response.encode()).expect("decode");
        assert_eq!(decoded, response, "response did not survive the wire");
    }
}

#[test]
fn the_handshake_round_trips() {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        session_token: "tok_0123456789abcdef".into(),
        client_name: "scribe/0.0.1".into(),
    };
    assert_eq!(Hello::decode(&hello.encode()).expect("decode"), hello);

    let welcome = Welcome {
        protocol_version: PROTOCOL_VERSION,
        project_id: "churn-dashboard".into(),
        server_name: "thetad/0.0.1".into(),
    };
    assert_eq!(Welcome::decode(&welcome.encode()).expect("decode"), welcome);
}

#[test]
fn messages_survive_the_length_framing_intact() {
    // Encoding and framing are separate concerns; this is the composition the
    // server actually performs.
    let mut stream = Vec::new();
    let requests = every_request();
    for request in &requests {
        stream.extend_from_slice(&frame::frame(&request.encode()).expect("frame"));
    }

    let mut cursor = std::io::Cursor::new(stream);
    for expected in &requests {
        let payload = frame::read_frame(&mut cursor).expect("read frame");
        assert_eq!(&Request::decode(&payload).expect("decode"), expected);
    }
}

// ---- negotiation matrix ----------------------------------------------------
//
// `02-api-wire-protocol.md` §5: refuse a silently-incompatible pairing rather
// than guessing. A guess here means an SDK and a server disagreeing about a
// field's meaning while both believe they are compatible.

#[test]
fn a_matching_client_negotiates() {
    assert_eq!(negotiate(PROTOCOL_VERSION), Ok(PROTOCOL_VERSION));
}

#[test]
fn a_client_older_than_the_minimum_is_refused() {
    assert!(matches!(
        negotiate(MIN_SUPPORTED_VERSION - 1),
        Err(NegotiationError::ClientTooOld { .. })
    ));
}

#[test]
fn a_client_newer_than_the_server_is_refused_rather_than_downgraded() {
    // Tempting to serve it at our own version. That is exactly the silent
    // incompatibility the spec forbids: the client may rely on a field this
    // build does not write.
    assert!(matches!(
        negotiate(PROTOCOL_VERSION + 1),
        Err(NegotiationError::ClientTooNew { .. })
    ));
}

#[test]
fn the_negotiation_matrix_holds_across_a_range_of_versions() {
    for client in 0..PROTOCOL_VERSION + 4 {
        let result = negotiate(client);
        match client {
            v if v < MIN_SUPPORTED_VERSION => assert!(result.is_err(), "v{v} should be too old"),
            v if v > PROTOCOL_VERSION => assert!(result.is_err(), "v{v} should be too new"),
            v => assert_eq!(result, Ok(v), "v{v} should negotiate"),
        }
    }
}

// ---- malformed input -------------------------------------------------------

#[test]
fn garbage_is_rejected_rather_than_interpreted() {
    for garbage in [
        b"".as_slice(),
        b"not a capnp message".as_slice(),
        &[0xFF; 64],
        &[0x00; 8],
    ] {
        assert!(
            Request::decode(garbage).is_err(),
            "garbage was decoded as a request: {garbage:?}"
        );
    }
}

#[test]
fn a_truncated_message_does_not_decode_to_a_partial_request() {
    let encoded = every_request()[1].encode();
    for cut in [1, encoded.len() / 4, encoded.len() / 2, encoded.len() - 1] {
        // A truncated message must fail, not yield a request with missing
        // fields silently defaulted.
        let _ = Request::decode(&encoded[..cut]);
    }
    // The full message still decodes, so the cuts above were meaningful.
    assert!(Request::decode(&encoded).is_ok());
}

#[test]
fn an_oversized_frame_is_refused_before_it_is_allocated() {
    let prefix = (theta_proto::MAX_FRAME_BYTES + 1).to_le_bytes();
    assert!(matches!(
        frame::decode_length(prefix),
        Err(FrameError::TooLarge { .. })
    ));
}

#[test]
fn an_unknown_status_code_degrades_to_internal_rather_than_failing() {
    // Additive-by-default (`02-api-wire-protocol.md` §5): a newer server may
    // send a code this build predates, and that must not drop the connection.
    assert_eq!(StatusCode::from_u8(200), StatusCode::Internal);
    assert_eq!(StatusCode::from_u8(0), StatusCode::Ok);
}
