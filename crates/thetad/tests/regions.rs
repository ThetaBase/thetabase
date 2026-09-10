//! Regional branches, through the engine.
//!
//! A region is a branch: each region writes locally, and the branches merge
//! explicitly. That half of M23 works in a single process and is wired. The
//! other half — follower reads — is not, and the note at the bottom of this file
//! says why rather than leaving the absence to be read as an oversight.

use std::collections::BTreeMap;

use theta_core::{Author, BranchId, Value};
use theta_storage::region::Region;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::dev_default("regional");
    config.data_dir = dir.path().to_path_buf();
    (Engine::open(config).expect("open"), dir)
}

fn branch(engine: &mut Engine, name: &str) -> BranchId {
    engine
        .create_branch(name, BranchId::MAIN, Author::System, 1_000)
        .expect("branch")
}

#[test]
fn a_write_routes_to_the_branch_its_region_is_placed_on() {
    let (mut engine, _dir) = engine();
    let eu = branch(&mut engine, "eu");
    let us = branch(&mut engine, "us");

    engine
        .place_region(Region::new("eu-central"), eu)
        .expect("place eu");
    engine
        .place_region(Region::new("us-east"), us)
        .expect("place us");

    assert_eq!(
        engine
            .route_write(&Region::new("eu-central"))
            .expect("route"),
        eu
    );
    assert_eq!(
        engine.route_write(&Region::new("us-east")).expect("route"),
        us
    );
}

#[test]
fn an_unplaced_region_is_refused_rather_than_sent_home() {
    // A fallback to the home branch would send a Frankfurt write across the
    // Atlantic silently — the exact cost the arrangement exists to avoid — and
    // the caller would have no way to tell it happened.
    let (mut engine, _dir) = engine();
    let eu = branch(&mut engine, "eu");
    engine
        .place_region(Region::new("eu-central"), eu)
        .expect("place");

    let result = engine.route_write(&Region::new("ap-southeast"));
    assert!(
        matches!(result, Err(EngineError::Region(_))),
        "an unplaced region was routed somewhere anyway: {result:?}"
    );
}

#[test]
fn placing_a_region_twice_is_refused_rather_than_replacing_the_first() {
    // Replacing would move every subsequent write in that region, and the writes
    // already on the old branch would stop being visible to callers who are
    // still reading where they were told to.
    let (mut engine, _dir) = engine();
    let first = branch(&mut engine, "eu-1");
    let second = branch(&mut engine, "eu-2");

    engine
        .place_region(Region::new("eu-central"), first)
        .expect("first placement");
    let result = engine.place_region(Region::new("eu-central"), second);

    assert!(
        matches!(result, Err(EngineError::Region(_))),
        "a second placement silently replaced the first"
    );
    assert_eq!(
        engine
            .route_write(&Region::new("eu-central"))
            .expect("route"),
        first,
        "the region moved despite the refusal"
    );
}

#[test]
fn a_region_cannot_be_placed_on_a_branch_that_does_not_exist() {
    // The routing table would resolve cleanly and send every write in that
    // region to nowhere — a failure that surfaces at write time, in another
    // region's timezone.
    let (mut engine, _dir) = engine();

    let result = engine.place_region(Region::new("eu-central"), BranchId(9_999));
    assert!(
        result.is_err(),
        "a region was placed on a branch that does not exist"
    );
    assert!(
        engine.route_write(&Region::new("eu-central")).is_err(),
        "the refused placement was recorded anyway"
    );
}

#[test]
fn regions_write_to_their_own_branches_without_seeing_each_others_writes_yet() {
    // The guarantee, stated exactly. Read-your-writes holds within a region
    // because a region is a branch and a branch has a total order. Across
    // regions you see another region's writes after a merge — the branch model's
    // existing behaviour, not a new weakening.
    let (mut engine, _dir) = engine();
    let eu = branch(&mut engine, "eu");
    let us = branch(&mut engine, "us");
    engine.place_region(Region::new("eu"), eu).expect("place");
    engine.place_region(Region::new("us"), us).expect("place");

    let target = engine.route_write(&Region::new("eu")).expect("route");
    engine
        .put(
            target,
            "orders:1",
            Value::Map(BTreeMap::from([("n".to_string(), Value::Int(1))])),
            Author::agent("frankfurt", "alice"),
            2_000,
        )
        .expect("put");

    assert!(
        engine.get(eu, "orders:1").is_some(),
        "a region cannot read its own write"
    );
    assert!(
        engine.get(us, "orders:1").is_none(),
        "a write in one region was visible in another without a merge"
    );
}

#[test]
fn the_status_surface_names_the_regions_this_instance_serves() {
    // It reported an empty list unconditionally before regions were wired, which
    // is correct for a deployment with none and a lie for one with them.
    let (mut engine, _dir) = engine();
    assert!(
        engine.regions().is_empty(),
        "an unconfigured instance claims regions"
    );

    let eu = branch(&mut engine, "eu");
    engine
        .place_region(Region::new("eu-central"), eu)
        .expect("place");

    assert_eq!(engine.regions(), vec!["eu-central".to_string()]);
}

// NOTE — follower reads.
//
// M23's other half is a replica that refuses to answer below a version the
// caller has already seen, redirecting instead of serving a stale row. It is
// implemented in `theta_storage::region::may_serve` and tested there, and it is
// deliberately **not** wired here.
//
// This instance is a primary, and a primary is never behind. `may_serve` against
// its own view can only ever return `Serve` — the `Redirect` arm is unreachable
// by construction. Wiring it would add a call that always says yes and a
// guarantee that reports as enforced without ever having had occasion to enforce
// anything, which is worse than the gap: it would look covered.
//
// It lands with read replicas. There are none.
