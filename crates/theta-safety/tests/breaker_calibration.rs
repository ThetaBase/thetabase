//! Breaker calibration under realistic load
//! (`07-agent-safety-layer.md` §6, `08-test-validation-plan.md` §3).
//!
//! The breaker's ceiling is the one number in the Safety Layer where both kinds
//! of error are expensive. Too high and a runaway agent burns a customer's money
//! before anything stops it — the case the breaker exists for. Too low and
//! ordinary work trips it, which costs the product the thing it sells: a
//! database an agent can write to at speed without a human in the loop. A
//! breaker that cries wolf gets its ceiling raised to infinity by the first
//! operator it annoys.
//!
//! So the default is not a guess. This file defines two workload corpora —
//! traffic that must never trip, and traffic that must always trip — sweeps
//! candidate ceilings across both, and asserts that the shipped defaults
//! separate them. Changing a default means facing this evidence.
//!
//! # Reading the sweep
//!
//! `cargo test -p theta-safety --test breaker_calibration -- --nocapture`
//! prints the table the defaults were chosen from.

use theta_safety::breaker::CircuitBreaker;
use theta_safety::policy::SafetyPolicy;

/// One write: when it happened, and how many rows it touched.
#[derive(Debug, Clone, Copy)]
struct Write {
    at_ms: u64,
    rows: u64,
}

/// A named traffic pattern, and why it is shaped the way it is.
struct Workload {
    name: &'static str,
    rationale: &'static str,
    writes: Vec<Write>,
}

/// How a ceiling handled one workload.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Outcome {
    tripped: bool,
    /// Milliseconds from the first write to the trip, if it tripped.
    time_to_trip_ms: Option<u64>,
    /// Rows written before the breaker stopped it.
    rows_before_trip: u64,
}

fn run(workload: &Workload, policy: &SafetyPolicy) -> Outcome {
    let mut breaker = CircuitBreaker::new(policy);
    let start = workload.writes.first().map(|w| w.at_ms).unwrap_or(0);
    let mut rows = 0;

    for write in &workload.writes {
        if breaker.record(write.rows, write.at_ms).is_allowed() {
            rows += write.rows;
            continue;
        }
        return Outcome {
            tripped: true,
            time_to_trip_ms: Some(write.at_ms - start),
            rows_before_trip: rows,
        };
    }

    Outcome {
        tripped: false,
        time_to_trip_ms: None,
        rows_before_trip: rows,
    }
}

// ---- legitimate workloads: these must never trip ---------------------------
//
// Drawn from what the product is *for*. ThetaBase's pitch is that an agent can
// write to it at speed; every one of these is an agent or an app doing its job.

fn steady_api_traffic() -> Workload {
    // A busy application: 50 writes a second, one row each, for five minutes.
    // Unremarkable, and the single most common shape there is.
    Workload {
        name: "steady api traffic",
        rationale: "50 single-row writes/sec for 5 min — 3,000 rows/min",
        writes: (0..15_000)
            .map(|i| Write {
                at_ms: i * 20,
                rows: 1,
            })
            .collect(),
    }
}

fn csv_import() -> Workload {
    // A user uploads a spreadsheet. One batch, all at once — the shape most
    // likely to look like an attack while being entirely routine.
    Workload {
        name: "csv import",
        rationale: "a user uploads 25,000 rows as one batch",
        writes: vec![Write {
            at_ms: 1_000,
            rows: 25_000,
        }],
    }
}

fn admin_bulk_edit() -> Workload {
    // "Set every user in this org to the new plan." Large, deliberate, human.
    Workload {
        name: "admin bulk edit",
        rationale: "an admin updates 40,000 rows in one operation",
        writes: vec![Write {
            at_ms: 500,
            rows: 40_000,
        }],
    }
}

fn nightly_aggregation() -> Workload {
    // A scheduled job: ten batches of 5,000 over five minutes.
    Workload {
        name: "nightly aggregation",
        rationale: "10 batches of 5,000 rows spread over 5 minutes",
        writes: (0..10)
            .map(|i| Write {
                at_ms: i * 30_000,
                rows: 5_000,
            })
            .collect(),
    }
}

fn backfill_after_migration() -> Workload {
    // The heaviest legitimate thing an agent does: populating a new column
    // across a large table, in sensible batches.
    Workload {
        name: "backfill after migration",
        rationale: "80,000 rows in 2,000-row batches over 10 minutes",
        writes: (0..40)
            .map(|i| Write {
                at_ms: i * 15_000,
                rows: 2_000,
            })
            .collect(),
    }
}

fn burst_then_idle() -> Workload {
    // A deploy-time seed: heavy for twenty seconds, then nothing. Tests that a
    // short legitimate burst is not treated as the start of a runaway.
    Workload {
        name: "burst then idle",
        rationale: "30,000 rows in 20s at deploy time, then quiet",
        writes: (0..30)
            .map(|i| Write {
                at_ms: i * 666,
                rows: 1_000,
            })
            .collect(),
    }
}

fn legitimate() -> Vec<Workload> {
    vec![
        steady_api_traffic(),
        csv_import(),
        admin_bulk_edit(),
        nightly_aggregation(),
        backfill_after_migration(),
        burst_then_idle(),
    ]
}

// ---- runaway workloads: these must always trip -----------------------------
//
// Every one of these is made of writes that are individually unremarkable. That
// is the point: classification by type cannot catch any of them, which is why
// the breaker is independent of it (§6).

fn infinite_retry_loop() -> Workload {
    // An agent that retries a failing write forever. The classic.
    Workload {
        name: "infinite retry loop",
        rationale: "1,000 rows re-written every 10ms, forever",
        writes: (0..6_000)
            .map(|i| Write {
                at_ms: i * 10,
                rows: 1_000,
            })
            .collect(),
    }
}

fn n_plus_one_amplification() -> Workload {
    // A confused agent writing per-row inside a per-row loop.
    Workload {
        name: "n+1 amplification",
        rationale: "500 writes/sec of 200 rows each",
        writes: (0..30_000)
            .map(|i| Write {
                at_ms: i * 2,
                rows: 200,
            })
            .collect(),
    }
}

fn runaway_pagination() -> Workload {
    // A loop whose batch size grows because its termination condition is wrong.
    // Slow to start, which is what makes it dangerous: it looks fine for a while.
    Workload {
        name: "runaway pagination",
        rationale: "batch size doubles each second, unbounded",
        writes: (0..30)
            .map(|i| Write {
                at_ms: i * 1_000,
                rows: 100u64 << i.min(20),
            })
            .collect(),
    }
}

fn slow_burn() -> Workload {
    // The hardest case: just fast enough to be ruinous, slow enough to look
    // like heavy legitimate traffic. This is where a ceiling earns its keep.
    Workload {
        name: "slow burn",
        rationale: "5,000 rows/sec sustained — 300,000 rows/min, indefinitely",
        writes: (0..600)
            .map(|i| Write {
                at_ms: i * 200,
                rows: 1_000,
            })
            .collect(),
    }
}

fn runaway() -> Vec<Workload> {
    vec![
        infinite_retry_loop(),
        n_plus_one_amplification(),
        runaway_pagination(),
        slow_burn(),
    ]
}

fn with_ceiling(ceiling: u64) -> SafetyPolicy {
    SafetyPolicy {
        breaker_row_ceiling: ceiling,
        breaker_window_ms: 60_000,
        ..SafetyPolicy::protected()
    }
}

// ---- the sweep -------------------------------------------------------------

#[test]
fn the_sweep_that_the_shipped_ceilings_were_chosen_from() {
    let candidates = [
        25_000u64, 50_000, 100_000, 150_000, 200_000, 500_000, 1_000_000,
    ];

    println!("\n  ceiling false positives   runaways caught   worst time-to-trip");
    println!("  ---------------------------------------------------------------------");

    for ceiling in candidates {
        let policy = with_ceiling(ceiling);

        let false_positives: Vec<&str> = legitimate()
            .iter()
            .filter(|w| run(w, &policy).tripped)
            .map(|w| w.name)
            .collect();

        let caught: Vec<Outcome> = runaway()
            .iter()
            .map(|w| run(w, &policy))
            .filter(|o| o.tripped)
            .collect();

        let worst = caught.iter().filter_map(|o| o.time_to_trip_ms).max();

        println!(
            "  {:>9}    {:>15}   {:>15}   {}",
            ceiling,
            match false_positives.is_empty() {
                true => "none".to_string(),
                false => format!("{} ({})", false_positives.len(), false_positives.join(", ")),
            },
            format!("{}/{}", caught.len(), runaway().len()),
            match worst {
                Some(ms) => format!("{:.1}s", ms as f64 / 1000.0),
                None => "never".to_string(),
            },
        );
    }
    println!();
}

#[test]
fn the_workload_profile_the_ceilings_are_placed_against() {
    println!("\n  workload busiest 60s window");
    println!("  ------------------------------------------------");
    for w in legitimate() {
        println!("  legit    {:<22} {:>10}", w.name, busiest_minute(&w));
    }
    for w in runaway() {
        println!("  runaway  {:<22} {:>10}", w.name, busiest_minute(&w));
    }
    println!();
}

#[test]
fn no_legitimate_workload_trips_the_protected_default() {
    // The expensive failure. A breaker that stops ordinary work gets its ceiling
    // raised to infinity by the first operator it annoys, and then it protects
    // nobody from anything.
    let policy = SafetyPolicy::protected();

    for workload in legitimate() {
        let outcome = run(&workload, &policy);
        assert!(
            !outcome.tripped,
            "`{}` ({}) tripped the protected default of {} rows/{}s — \
             this is ordinary work being refused",
            workload.name,
            workload.rationale,
            policy.breaker_row_ceiling,
            policy.breaker_window_ms / 1_000,
        );
    }
}

#[test]
fn no_legitimate_workload_trips_the_development_default() {
    let policy = SafetyPolicy::development();

    for workload in legitimate() {
        assert!(
            !run(&workload, &policy).tripped,
            "`{}` tripped the development default",
            workload.name
        );
    }
}

#[test]
fn every_runaway_workload_trips_the_protected_default() {
    let policy = SafetyPolicy::protected();

    for workload in runaway() {
        let outcome = run(&workload, &policy);
        assert!(
            outcome.tripped,
            "`{}` ({}) ran to completion under the protected default — \
             this is the case the breaker exists for",
            workload.name, workload.rationale,
        );
    }
}

#[test]
fn every_runaway_workload_trips_the_development_default() {
    // Looser, because the cost of a false positive on a dev branch is higher and
    // the cost of a mistake is lower — but never unbounded, because a runaway
    // loop costs money on any branch.
    let policy = SafetyPolicy::development();

    for workload in runaway() {
        assert!(
            run(&workload, &policy).tripped,
            "`{}` ran unbounded on a development policy",
            workload.name
        );
    }
}

#[test]
fn a_runaway_is_stopped_within_one_window_rather_than_eventually() {
    // Time-to-trip is the number that decides what a runaway costs. A breaker
    // that catches an infinite loop after an hour has not caught it.
    let policy = SafetyPolicy::protected();
    let window_ms = policy.breaker_window_ms;

    for workload in runaway() {
        let outcome = run(&workload, &policy);
        let elapsed = outcome
            .time_to_trip_ms
            .expect("every runaway workload trips");
        assert!(
            elapsed <= window_ms,
            "`{}` took {:.1}s to trip, longer than the {}s window it accumulates over",
            workload.name,
            elapsed as f64 / 1000.0,
            window_ms / 1_000,
        );
    }
}

/// The two bounds every shipped ceiling has to sit between.
///
/// Between them the ceiling is pinned from above and below, so it cannot drift
/// in either direction without a test saying so.
fn assert_well_placed(policy: &SafetyPolicy, name: &str, min_headroom: u64) {
    let heaviest_legitimate = legitimate()
        .iter()
        .map(busiest_minute)
        .max()
        .expect("there are legitimate workloads");
    let lightest_runaway = runaway()
        .iter()
        .map(busiest_minute)
        .min()
        .expect("there are runaway workloads");

    // Margin rather than a bare `>`: a ceiling one row above the worst observed
    // minute would pass a test and page someone at 3am.
    assert!(
        policy.breaker_row_ceiling >= heaviest_legitimate * min_headroom,
        "the {name} ceiling ({}) leaves less than {min_headroom}x headroom over the \
         heaviest legitimate minute ({heaviest_legitimate} rows)",
        policy.breaker_row_ceiling,
    );

    assert!(
        policy.breaker_row_ceiling < lightest_runaway,
        "the {name} ceiling ({}) is at or above the lightest runaway minute \
         ({lightest_runaway} rows), so that runaway would never be caught",
        policy.breaker_row_ceiling,
    );
}

#[test]
fn the_protected_ceiling_sits_between_real_work_and_a_runaway() {
    assert_well_placed(&SafetyPolicy::protected(), "protected", 2);
}

#[test]
fn the_development_ceiling_sits_between_real_work_and_a_runaway() {
    // More headroom than protected — a dev branch is where experiments happen,
    // and a false positive there is more annoying and less costly. The upper
    // bound is the same, because a runaway loop costs money on any branch.
    //
    // This is the assertion that caught the shipped default: at 1,000,000 the
    // development ceiling sat above the lightest runaway, so a loop sustaining
    // 300,000 rows a minute ran indefinitely on a dev branch without tripping.
    assert_well_placed(&SafetyPolicy::development(), "development", 6);
}

#[test]
fn the_development_ceiling_is_looser_than_the_protected_one() {
    assert!(
        SafetyPolicy::development().breaker_row_ceiling
            > SafetyPolicy::protected().breaker_row_ceiling,
        "a dev branch should tolerate more than production, not less"
    );
}

/// The most rows any 60-second window of this workload contains.
///
/// Computed over the *whole* workload, ignoring the breaker, so it describes the
/// traffic rather than the response to it.
fn busiest_minute(workload: &Workload) -> u64 {
    let window_ms = 60_000;
    let mut worst = 0;

    for (i, start) in workload.writes.iter().enumerate() {
        let rows: u64 = workload.writes[i..]
            .iter()
            .take_while(|w| w.at_ms < start.at_ms + window_ms)
            .map(|w| w.rows)
            .sum();
        worst = worst.max(rows);
    }
    worst
}
