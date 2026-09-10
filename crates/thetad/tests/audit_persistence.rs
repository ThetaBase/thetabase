//! The forensic trail outlives the process.
//!
//! "What happened, and who or what did it" is a question usually asked after
//! something went wrong — which is exactly when a process is most likely to have
//! restarted. A trail held only in memory answers it right up until the moment
//! anyone needs it (`04-threat-model-security.md` §5).

use std::collections::BTreeMap;

use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, Value};
use theta_safety::audit::RiskLevel;
use thetad::engine::Engine;
use thetad::Config;

fn agent() -> Author {
    Author::agent("sess_7", "agent")
}

fn config(dir: &std::path::Path, project: &str) -> Config {
    let mut config = Config::dev_default(project);
    config.data_dir = dir.to_path_buf();
    config
}

fn seed(engine: &mut Engine, count: usize) {
    for i in 0..count {
        let row = Value::Map(BTreeMap::from([
            (
                "email".to_string(),
                Value::Text(format!("u{i}@example.com")),
            ),
            ("name".to_string(), Value::Text(format!("User {i}"))),
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

#[test]
fn a_gated_change_is_still_on_the_record_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let mut engine = Engine::open(config(dir.path(), "proj")).expect("open");
        seed(&mut engine, 200);
        engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 1_000);
        assert!(!engine.audit_log().is_empty());
        engine.checkpoint().expect("checkpoint");
    }

    let engine = Engine::open(config(dir.path(), "proj")).expect("reopen");
    let trail = engine.audit_log();
    let entry = trail
        .iter()
        .find(|e| e.summary.contains("drop column"))
        .expect("the gated proposal survived the restart");
    assert!(entry.summary.contains("agent session sess_7"));
    assert!(entry.summary.contains("200 rows"));
}

#[test]
fn the_whole_lifecycle_of_a_change_is_recoverable_from_the_trail() {
    // Propose, validate, promote — three events, and after a restart all three
    // are still there in order, which is what "forensic reconstruction" means.
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let mut engine = Engine::open(config(dir.path(), "proj")).expect("open");
        seed(&mut engine, 200);
        let diff = engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 1);
        engine
            .open_shadow_branch(BranchId::MAIN, &diff.change_id, drop_email(), agent(), 2)
            .expect("shadow");
        engine
            .validate_shadow(&diff.change_id, agent(), 3)
            .expect("validate");
        engine
            .promote_shadow(
                &diff.change_id,
                Author::Human {
                    user_id: "reviewer".into(),
                },
                4,
            )
            .expect("promote");
    }

    let engine = Engine::open(config(dir.path(), "proj")).expect("reopen");
    let trail = engine.audit_log();
    let summaries: Vec<&str> = trail.iter().map(|e| e.summary.as_str()).collect();

    assert!(summaries
        .iter()
        .any(|s| s.contains("attempted drop column")));
    assert!(summaries.iter().any(|s| s.contains("validated")));
    assert!(
        summaries.iter().any(|s| s.contains("promoted")),
        "the moment the change actually landed is missing: {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("user reviewer")),
        "the trail must name who approved it"
    );
}

#[test]
fn an_engine_will_not_open_another_projects_audit_trail() {
    // Cross-project isolation is structural rather than an access check
    // (`04-threat-model-security.md` §3). A data directory pointed at the wrong
    // project has to fail loudly, not merge two projects' forensic records.
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let mut engine = Engine::open(config(dir.path(), "project-a")).expect("open");
        seed(&mut engine, 200);
        engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 1);
    }

    let wrong = Engine::open(config(dir.path(), "project-b"));
    assert!(
        wrong.is_err(),
        "an engine serving project-b opened project-a's audit trail"
    );
}

#[test]
fn a_weekly_review_surfaces_the_worst_thing_first() {
    // The shape §8 asks for: a human has five minutes and wants the high-risk
    // events, not a chronological dump.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut engine = Engine::open(config(dir.path(), "proj")).expect("open");
    seed(&mut engine, 200);

    // A low-risk additive change, then an irreversible drop.
    engine.propose_schema_change(
        BranchId::MAIN,
        SchemaChange::AddIndex {
            table: "users".into(),
            index: theta_core::schema::IndexDef {
                name: "by_name".into(),
                columns: vec!["name".into()],
                unique: false,
            },
        },
        agent(),
        1,
    );
    engine.propose_schema_change(BranchId::MAIN, drop_email(), agent(), 2);

    let review = engine.audit().review(RiskLevel::Medium, 10);
    assert!(!review.is_empty(), "nothing surfaced for review");
    assert_eq!(review[0].risk, RiskLevel::High);
    assert!(review[0].summary.contains("drop column"));
}

#[test]
fn the_trail_records_a_tripped_breaker_across_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let mut config = config(dir.path(), "proj");
        config.safety.breaker_row_ceiling = 10;
        config.safety.breaker_window_ms = 60_000;
        let mut engine = Engine::open(config).expect("open");

        for i in 0..50 {
            let _ = engine.put(
                BranchId::MAIN,
                &format!("k:{i}"),
                Value::Int(i),
                agent(),
                1_000,
            );
        }
    }

    let engine = Engine::open(config(dir.path(), "proj")).expect("reopen");
    let trail = engine.audit_log();
    assert!(
        trail.iter().any(|e| e.summary.contains("breaker tripped")),
        "a tripped breaker did not survive the restart"
    );
}
