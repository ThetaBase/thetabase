//! What ThetaBase's own cold start costs, and what that means for hibernation.
//!
//! # Why this exists
//!
//! Hibernation stops the process behind an idle project and starts it on the
//! next connection. The cost model turns on it — about $0.15 a month for a
//! stopped machine against $2 for a running one — and the *product* turns on the
//! wake being fast enough that nobody notices.
//!
//! A wake has two halves, and only one is the platform's:
//!
//!   * resuming the machine — 200–500ms for a stopped Firecracker VM; an ECS
//!     `RunTask` is 10–90s, because a stopped task does not exist and has to be
//!     re-provisioned and re-pulled;
//!   * **`Engine::open`** — ours, and it follows us to any platform.
//!
//! `09-sla-performance.md` §2 budgets 500ms p50 / 1.5s p99 for a cold start.
//! That was written before either half had been measured.
//!
//! # What was measured, and the hypothesis it killed
//!
//! Recovery is **linear in the number of rows**, at roughly 1.2µs each:
//!
//! ```text
//!    1,000 rows      2.0 ms
//!   10,000 rows     12.2 ms
//!   50,000 rows     59.8 ms
//!  100,000 rows    118.2 ms
//! ```
//!
//! The first guess was that recovery replays the log. It does not — the
//! checkpoint interval is 1,000 entries and `open` resumes from the view
//! snapshot, which is what it is for.
//!
//! The second guess was that the snapshot's `serde_json` encoding was the cost.
//! **Measured against MessagePack on the real 7.1MB snapshot: 1.1× faster and
//! 1.3× smaller.** Changing the format would have been a refactor across the
//! storage layer for five per cent, and the measurement is the only reason it
//! did not happen.
//!
//! The cost is **building the map**. A view is an in-memory `OrdMap` of every
//! row, and there is no way to have the map without paying to construct it. So
//! this is not a defect to fix; it is a property to design around, and the tests
//! below pin the two things that follow from it.
//!
//! # What follows
//!
//! Small projects wake fast, and small projects are the ones that hibernate —
//! development instances on the free tier. A large project is a production
//! instance, which never hibernates, so its recovery time is paid once at
//! deploy rather than on somebody's first query of the day.
//!
//! The number that matters is therefore **the per-row rate**, not the total:
//! it is what lets anybody predict a given project's wake, and it is what
//! `09-sla-performance.md` §2 now qualifies its budget by.
//!
//! Run with `--release`; a debug build measures the allocator.

use std::time::Instant;

use theta_core::{Author, BranchId, Value};
use thetad::engine::Engine;
use thetad::Config;

/// Optimised builds only. A debug measurement of recovery measures `serde`
/// without inlining, and quoting it would flatter or damn the design for
/// reasons unrelated to it.
fn release_only() -> bool {
    cfg!(debug_assertions)
}

fn seed(dir: &std::path::Path, rows: u64) {
    let mut config = Config::dev_default("cold-start");
    config.data_dir = dir.to_path_buf();
    let mut engine = Engine::open(config).expect("open");

    for i in 0..rows {
        engine
            .put(
                BranchId::MAIN,
                &format!("row:{i:07}"),
                Value::Text(format!("value-{i}")),
                Author::agent("sess_cold", "agent"),
                i as i64,
            )
            .expect("put");
    }
}

/// Reopen, and time only that. Best of three — a single reading on a laptop
/// measures whatever else the machine was doing, and this number is quoted in a
/// spec.
fn open_ms(dir: &std::path::Path) -> f64 {
    (0..3)
        .map(|_| {
            let mut config = Config::dev_default("cold-start");
            config.data_dir = dir.to_path_buf();

            let start = Instant::now();
            let engine = Engine::open(config).expect("reopen");
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;

            // Touched so the open cannot be optimised away, and so a recovery
            // that "succeeded" without producing a usable engine fails here
            // rather than reporting a very good time.
            assert_eq!(
                engine.rows_written(),
                0,
                "a reopened engine counts writes from zero"
            );
            elapsed
        })
        .fold(f64::INFINITY, f64::min)
}

/// The rate a project's wake can be predicted from.
///
/// Loose enough to survive a busy laptop, tight enough that a tenfold
/// regression in recovery fails rather than being absorbed. Measured at ~1.2µs;
/// this allows four.
const MAX_MICROS_PER_ROW: f64 = 4.0;

#[test]
fn recovery_costs_a_predictable_amount_per_row() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    const SIZES: [u64; 3] = [10_000, 50_000, 100_000];

    let measured: Vec<(u64, f64)> = SIZES
        .iter()
        .map(|&rows| {
            let dir = tempfile::tempdir().expect("tempdir");
            seed(dir.path(), rows);
            (rows, open_ms(dir.path()))
        })
        .collect();

    let report: String = measured
        .iter()
        .map(|(rows, ms)| {
            format!(
                "\x20 {rows:>7} rows   {ms:>7.1} ms   {:.2} µs/row\n",
                ms * 1000.0 / *rows as f64
            )
        })
        .collect();

    eprintln!(
        "\nThetaBase's own cold start — `Engine::open`, best of three\n\n{report}\n\
         \x20 Platform wake sits on top:\n\
         \x20   Fly, resume a stopped machine    200-500 ms\n\
         \x20   ECS RunTask, re-provision        10-90 s\n"
    );

    for (rows, ms) in &measured {
        let per_row = ms * 1000.0 / *rows as f64;
        assert!(
            per_row < MAX_MICROS_PER_ROW,
            "recovery costs {per_row:.2}µs/row at {rows} rows, over the \
             {MAX_MICROS_PER_ROW}µs this is allowed to drift to. Wake time is \
             what hibernation sells, and it is quoted per row in \
             `09-sla-performance.md` §2."
        );
    }
}

#[test]
fn a_hibernating_project_wakes_inside_the_sla_budget() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    // The case the cost model depends on: a development instance on the free
    // tier, asleep, woken by somebody's first query of the day. This is the one
    // that must be both cheap *and* fast, and the one a bad answer ruins.
    const ROWS: u64 = 10_000;

    // Our share of the 500ms p50. The platform needs 200-500ms of it, so ours
    // has to be small rather than merely under the total.
    const OUR_SHARE_MS: f64 = 150.0;

    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path(), ROWS);
    let measured = open_ms(dir.path());

    eprintln!("\ncold start, {ROWS}-row project: {measured:.1} ms (our share of the budget: {OUR_SHARE_MS} ms)\n");

    assert!(
        measured < OUR_SHARE_MS,
        "recovery of a {ROWS}-row project took {measured:.1}ms. With a 200-500ms \
         platform wake on top, the 500ms p50 in `09-sla-performance.md` §2 is no \
         longer reachable for the projects that actually hibernate."
    );
}

#[test]
fn recovery_scales_with_a_projects_size_not_its_history() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    // The property that decides whether hibernation stays viable, and the one a
    // file-size comparison only gestures at.
    //
    // Recovery resumes from the view snapshot. If it instead replayed the log
    // from genesis, cold start would grow with everything a project had *ever*
    // done rather than with what it currently holds — so a year-old project
    // would wake more slowly every month while a fresh one stayed fast, and the
    // benchmark that tested a fresh one would never notice.
    //
    // Same number of rows, five times the write history.
    const ROWS: u64 = 10_000;
    const REWRITES: u64 = 5;

    let fresh_dir = tempfile::tempdir().expect("tempdir");
    seed(fresh_dir.path(), ROWS);
    let fresh = open_ms(fresh_dir.path());

    let churned_dir = tempfile::tempdir().expect("tempdir");
    {
        let mut config = Config::dev_default("cold-start");
        config.data_dir = churned_dir.path().to_path_buf();
        let mut engine = Engine::open(config).expect("open");
        for round in 0..REWRITES {
            for i in 0..ROWS {
                engine
                    .put(
                        BranchId::MAIN,
                        &format!("row:{i:07}"),
                        Value::Text(format!("value-{round}-{i}")),
                        Author::agent("sess_cold", "agent"),
                        (round * ROWS + i) as i64,
                    )
                    .expect("put");
            }
        }
    }
    let churned = open_ms(churned_dir.path());

    eprintln!(
        "
{ROWS} rows, {REWRITES}x the history
           written once      {fresh:>7.1} ms
           rewritten {REWRITES}x     {churned:>7.1} ms
           ratio             {:.2}x
",
        churned / fresh.max(0.001)
    );

    // Generous: the churned project genuinely has more log to walk since its
    // last checkpoint, and a laptop is noisy. What this rules out is the shape
    // that matters — five times the history costing anything like five times
    // the wake.
    assert!(
        churned < fresh * 2.5,
        "recovering a project with {REWRITES}x the history took {churned:.1}ms against {fresh:.1}ms for the same rows written once. Cold start is tracking history rather than size, so every project gets slower to wake for ever."
    );
}
