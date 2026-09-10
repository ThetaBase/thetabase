//! Who may change what the Safety Layer lets through
//! (`07-agent-safety-layer.md` §7).
//!
//! A project's policy sets every threshold the gates turn on, so an agent that
//! could write it could simply raise its own ceiling and walk a drop through.
//! The defence is not a permission bit on a session token — a bit can be
//! misread, and a token is exactly what a compromised agent holds. It is that a
//! policy reaches an instance only as bytes signed with the project's private
//! key, which lives in the Control Plane and nowhere an agent can reach.
//!
//! These tests are the assertion that no data-plane path exists.

use std::collections::BTreeMap;

use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, Value};
use theta_identity::keys::ProjectKeys;
use theta_safety::policy::SafetyPolicy;
use theta_safety::signed::{SignedPolicy, VersionedPolicy};
use thetad::engine::Engine;
use thetad::Config;

fn agent() -> Author {
    Author::agent("sess", "agent")
}

/// An engine whose keyset is installed, as a provisioned instance's would be.
fn engine_with_keys(project: &str) -> (Engine, ProjectKeys, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default(project);
    config.data_dir = dir.path().to_path_buf();
    config.safety = SafetyPolicy::protected();

    let keys = ProjectKeys::generate(project);
    let mut engine = Engine::open(config).expect("open");
    engine.install_keyset(keys.public_keyset());
    (engine, keys, dir)
}

fn seed(engine: &mut Engine, count: usize) {
    for i in 0..count {
        let row = Value::Map(BTreeMap::from([
            ("email".to_string(), Value::Text(format!("u{i}@x.com"))),
            ("name".to_string(), Value::Text(format!("n{i}"))),
        ]));
        engine
            .put(BranchId::MAIN, &format!("users:{i}"), row, agent(), 0)
            .expect("put");
    }
}

fn drop_email() -> SchemaChange {
    SchemaChange::DropColumn {
        table: "users".into(),
        column: "email".into(),
    }
}

fn versioned(project: &str, version: u64, policy: SafetyPolicy) -> VersionedPolicy {
    VersionedPolicy {
        version,
        project_id: project.to_string(),
        policy,
        issued_at_ms: 0,
    }
}

/// A policy that would wave anything through.
fn wide_open() -> SafetyPolicy {
    SafetyPolicy {
        row_impact_threshold: u64::MAX,
        irreversible_shadow_threshold: u64::MAX,
        breaker_row_ceiling: u64::MAX,
        ..SafetyPolicy::development()
    }
}

#[test]
fn a_policy_the_project_key_signed_takes_effect() {
    // A signed policy reaches the classifier. Demonstrated by *tightening*,
    // because `main` is protected and a protected branch floors its thresholds
    // at the protected preset's — so a loosening would be clamped and this test
    // would be asserting that delivery works while watching it not matter.
    //
    // It used to loosen: push a policy raising the irreversible-shadow threshold
    // to 10,000 and expect a 200-row drop on `main` to come back `Confirm`. That
    // stopped being possible when branch protection stopped being decorative
    // (external review R3-03), and `a_delivered_policy_cannot_loosen_a_protected_branch`
    // in `theta-control` now pins the refusal.
    let (mut engine, keys, _dir) = engine_with_keys("proj");
    seed(&mut engine, 200);

    // A lossless widening: non-destructive, so its gate turns purely on the
    // row-impact threshold, which a delivered policy may still tighten.
    let widen = theta_core::schema::SchemaChange::AlterColumnType {
        table: "users".into(),
        column: "email".into(),
        from: theta_core::ValueType::Text,
        to: theta_core::ValueType::Text,
    };

    let before = engine.propose_schema_change(BranchId::MAIN, widen.clone(), agent(), 0);
    assert_eq!(
        before.gate,
        theta_safety::classify::Gate::AutoApply,
        "200 rows under the protected 1,000-row threshold should need nobody"
    );

    let tightened = SafetyPolicy {
        row_impact_threshold: 10,
        ..SafetyPolicy::protected()
    };
    let signed = SignedPolicy::sign(&keys, &versioned("proj", 1, tightened));
    assert!(engine
        .apply_signed_policy(&signed, 1)
        .expect("a signed policy applies"));

    let after = engine.propose_schema_change(BranchId::MAIN, widen, agent(), 2);
    assert_eq!(
        after.gate,
        theta_safety::classify::Gate::Confirm,
        "the new policy did not take effect"
    );
}

#[test]
fn a_policy_signed_by_anything_but_the_project_key_is_refused() {
    // The whole security case in one test: authoring a policy an instance will
    // accept requires the project's private key. An agent holds a session
    // token, which is not that and cannot be turned into it.
    let (mut engine, _keys, _dir) = engine_with_keys("proj");
    seed(&mut engine, 200);

    let attacker = ProjectKeys::generate("proj");
    let forged = SignedPolicy::sign(&attacker, &versioned("proj", 99, wide_open()));

    assert!(
        engine.apply_signed_policy(&forged, 0).is_err(),
        "a forged policy was accepted"
    );
    assert_eq!(engine.policy_version(), 0);

    // And the gate is exactly where it was.
    let diff = engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 1);
    assert_eq!(diff.gate, theta_safety::classify::Gate::ShadowValidate);
}

#[test]
fn a_policy_edited_in_transit_is_refused() {
    let (mut engine, keys, _dir) = engine_with_keys("proj");
    let signed = SignedPolicy::sign(&keys, &versioned("proj", 1, SafetyPolicy::protected()));

    // Raise the ceiling in the payload without re-signing.
    let mut tampered = signed.clone();
    let mut policy: VersionedPolicy = serde_json::from_slice(&tampered.payload).expect("decode");
    policy.policy = wide_open();
    tampered.payload = serde_json::to_vec(&policy).expect("encode");

    assert!(engine.apply_signed_policy(&tampered, 0).is_err());
    assert_eq!(engine.policy_version(), 0);
}

#[test]
fn an_instance_with_no_keyset_adopts_no_policy_at_all() {
    // Fail closed. An instance that cannot verify a policy has no way to tell a
    // real one from an invented one, and adopting either is the bypass this
    // mechanism exists to prevent.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("proj");
    config.data_dir = dir.path().to_path_buf();
    let mut engine = Engine::open(config).expect("open");

    let keys = ProjectKeys::generate("proj");
    let signed = SignedPolicy::sign(&keys, &versioned("proj", 1, wide_open()));

    assert!(
        engine.apply_signed_policy(&signed, 0).is_err(),
        "an instance with no keyset adopted a policy"
    );
}

#[test]
fn a_policy_signed_for_another_project_does_not_land_here() {
    let (mut engine, keys, _dir) = engine_with_keys("proj");

    // Correctly signed by this project's key, but naming another project. The
    // signature is not the only check: cross-project isolation is asserted
    // rather than inferred (`04-threat-model-security.md` §3).
    let signed = SignedPolicy::sign(&keys, &versioned("someone-else", 1, wide_open()));

    assert!(engine.apply_signed_policy(&signed, 0).is_err());
    assert_eq!(engine.policy_version(), 0);
}

#[test]
fn a_replayed_policy_cannot_reinstate_a_ceiling_the_owner_tightened() {
    let (mut engine, keys, _dir) = engine_with_keys("proj");
    seed(&mut engine, 200);

    let loose = SignedPolicy::sign(&keys, &versioned("proj", 1, wide_open()));
    engine.apply_signed_policy(&loose, 0).expect("v1");

    let tight = SignedPolicy::sign(&keys, &versioned("proj", 2, SafetyPolicy::protected()));
    engine.apply_signed_policy(&tight, 1).expect("v2");

    // Replaying the captured v1 push — a genuine signature over genuine bytes.
    assert!(
        !engine
            .apply_signed_policy(&loose, 2)
            .expect("a replay is ignored, not an error"),
        "a replayed policy was applied"
    );

    let diff = engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 3);
    assert_eq!(
        diff.gate,
        theta_safety::classify::Gate::ShadowValidate,
        "a replayed push reinstated the loose ceiling"
    );
}

#[test]
fn a_policy_change_leaves_an_audit_entry_naming_the_new_limits() {
    // Somebody widening the ceiling is exactly what a weekly review should
    // surface (`07-agent-safety-layer.md` §8).
    let (mut engine, keys, _dir) = engine_with_keys("proj");
    let signed = SignedPolicy::sign(&keys, &versioned("proj", 1, wide_open()));
    engine.apply_signed_policy(&signed, 5).expect("apply");

    let trail = engine.audit_log();
    let entry = trail
        .iter()
        .find(|e| e.summary.contains("safety policy"))
        .expect("the policy change is on the record");
    assert!(entry.summary.contains("v1"));
    assert!(
        entry.risk >= theta_safety::audit::RiskLevel::Medium,
        "a policy change should not be filed as routine"
    );
}

#[test]
fn a_new_policy_moves_the_breaker_and_not_only_the_classifier() {
    // The breaker reads its ceiling from the policy at construction, so a policy
    // that changed only the classifier would leave the old limits quietly in
    // force while the audit trail said otherwise.
    let (mut engine, keys, _dir) = engine_with_keys("proj");

    let tight = SafetyPolicy {
        breaker_row_ceiling: 10,
        breaker_window_ms: 60_000,
        ..SafetyPolicy::protected()
    };
    let signed = SignedPolicy::sign(&keys, &versioned("proj", 1, tight));
    engine.apply_signed_policy(&signed, 0).expect("apply");

    let mut refused = false;
    for i in 0..50 {
        if engine
            .put(BranchId::MAIN, &format!("k:{i}"), Value::Int(i), agent(), 1)
            .is_err()
        {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "the breaker kept its old ceiling after a policy change"
    );
}
