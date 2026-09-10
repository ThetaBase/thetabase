//! Comparative benchmark against the source Postgres (ROADMAP M8).
//!
//! The ROADMAP puts this here rather than in M3, where the SLA gate lives, and
//! says why: this is the first point at which a fair comparison exists — the
//! same data, the same queries, one migrated from the other. A synthetic
//! head-to-head before that would be measuring two different workloads and
//! calling the difference a result.
//!
//! # What is measured, and what is deliberately not
//!
//! Measured: point `get` by primary key, and a cached typed `query`. These are
//! what the ROADMAP says a user actually cares about — "is this slower than
//! what I had" — and the comparison is fair, because both systems are being
//! asked for the same rows they both hold.
//!
//! Not measured: raw analytical throughput against a mature planner. The
//! ROADMAP is explicit that this "is not a comparison ThetaBase wins or should
//! claim", and a benchmark written to be won is worth nothing. Branch create
//! and merge have no Postgres equivalent at all, so they are reported as
//! capabilities elsewhere rather than raced here.
//!
//! # This test does not assert a winner
//!
//! It asserts that the harness ran, that both systems answered the *same
//! question with the same answer* — a benchmark where one side returns the
//! wrong rows is measuring nothing — and it prints the numbers with the
//! provenance the ROADMAP requires: "No comparative number ships without the
//! harness that produced it, the hardware it ran on, and the version of both
//! systems."
//!
//! Making a latency assertion here would be pinning a performance claim to
//! whatever laptop or CI runner happened to run it. `specs/09` owns the
//! published numbers, and owns them against real hardware.

use std::time::Instant;

use theta_core::{Author, BranchId, CommitId, ContentHash, LogEntry, OpType, RowSource, Value};
use theta_eject::import::{next_batch, Cursor};
use theta_eject::{connect, plan, reflect};
use theta_storage::MaterializedView;

fn database_url() -> String {
    std::env::var("THETA_EJECT_TEST_URL")
        .unwrap_or_else(|_| "postgresql://thetabase:thetabase@127.0.0.1:55432/shopdb".to_string())
}

fn require_live() -> bool {
    std::env::var("THETA_REQUIRE_LIVE").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn entry(op: OpType) -> LogEntry {
    LogEntry {
        prev_hash: ContentHash::ZERO,
        commit_id: CommitId(0),
        branch_id: BranchId::MAIN,
        op,
        author: Author::System,
        timestamp_ms: 0,
    }
}

/// Best of `runs`.
///
/// Best-of rather than mean: what is being measured is how much work the system
/// does, and every source of noise on a shared machine only ever adds. Reported
/// as such, so nobody reads it as a typical latency.
fn best_of<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut last = f();
    for _ in 0..runs {
        let start = Instant::now();
        last = f();
        best = best.min(start.elapsed().as_secs_f64() * 1e6);
    }
    (best, last)
}

#[tokio::test]
async fn the_same_workload_on_both_systems_with_its_provenance() {
    let client = match connect(&database_url()).await {
        Ok(client) => client,
        Err(e) if require_live() => panic!("THETA_REQUIRE_LIVE is set: {e}"),
        Err(e) => {
            eprintln!("skipping comparative benchmark: no Postgres ({e})");
            return;
        }
    };

    // Provenance first, so a number can never be quoted without it.
    let pg_version: String = client
        .query_one("SELECT version()", &[])
        .await
        .expect("version")
        .get(0);
    println!("\n=== comparative benchmark ===");
    println!("harness      crates/theta-eject/tests/comparative_benchmark.rs");
    println!("thetabase      {}", env!("CARGO_PKG_VERSION"));
    println!(
        "postgres     {}",
        pg_version.split(" on ").next().unwrap_or(&pg_version)
    );
    println!(
        "host         {} cores, {}, {} build",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::env::consts::OS,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    if cfg!(debug_assertions) {
        println!();
        println!("  NOTE: a debug build measures rustc's bounds checks, not");
        println!("        the engine. These numbers compare nothing.");
        println!("        Run with --release.");
    }

    // Migrate, so both sides hold the same rows.
    let schema = reflect::reflect(&client, "public").await.expect("reflect");
    let planned = plan::plan(&schema);
    let table = planned.table("customers").expect("customers");

    let mut view = MaterializedView::new();
    for change in planned.schema_changes() {
        view.apply(&entry(OpType::Schema { change }));
    }
    let mut cursor = Cursor::default();
    let mut keys = Vec::new();
    loop {
        let batch = next_batch(&client, table, &cursor, 500)
            .await
            .expect("batch");
        if batch.is_empty() {
            break;
        }
        for row in &batch.rows {
            keys.push(row.primary_key.clone());
            view.apply(&entry(OpType::Put {
                key: format!("customers:{}", row.primary_key),
                value: row.value.clone(),
            }));
        }
        cursor = batch.cursor;
    }
    assert!(!keys.is_empty(), "nothing migrated; nothing to compare");
    println!("rows         {}", keys.len());

    // ---- point lookup by primary key ---------------------------------------
    let probe = keys[keys.len() / 2].clone();

    let (theta_us, theta_row) = best_of(20, || view.row("customers", &probe));
    let theta_email = theta_row.as_ref().and_then(|row| match row {
        Value::Map(fields) => fields.get("email").cloned(),
        _ => None,
    });

    let mut pg_best = f64::MAX;
    let mut pg_email = None;
    for _ in 0..20 {
        let start = Instant::now();
        let row = client
            .query_one(
                "SELECT email FROM public.customers WHERE id::text = $1",
                &[&probe],
            )
            .await
            .expect("point lookup");
        pg_best = pg_best.min(start.elapsed().as_secs_f64() * 1e6);
        let email: String = row.get(0);
        pg_email = Some(Value::Text(email));
    }

    // The check that makes the timing mean anything: both answered the same
    // question and agreed. A benchmark where one side returns the wrong row is
    // measuring the speed of being wrong.
    assert_eq!(
        theta_email, pg_email,
        "the two systems disagreed about the row being timed"
    );

    println!("\n  get by primary key (best of 20)");
    println!("    postgres   {pg_best:>10.1} us   (over a local socket)");
    println!("    thetabase    {theta_us:>10.1} us   (in-process materialized view)");
    println!("    note       not like for like: Postgres pays a round trip");
    println!("               here and ThetaBase does not. The number a user");
    println!("               compares is the one their client sees, which");
    println!("               `make sla` measures over a real socket.");

    // ---- scan the table ----------------------------------------------------
    let (theta_scan_us, theta_rows) = best_of(20, || view.scan("customers"));

    let mut pg_scan_best = f64::MAX;
    let mut pg_rows = 0usize;
    for _ in 0..20 {
        let start = Instant::now();
        let rows = client
            .query("SELECT id, email FROM public.customers", &[])
            .await
            .expect("scan");
        pg_scan_best = pg_scan_best.min(start.elapsed().as_secs_f64() * 1e6);
        pg_rows = rows.len();
    }

    assert_eq!(
        theta_rows.len(),
        pg_rows,
        "the two systems disagreed about how many rows the table has"
    );

    println!("\n  full scan (best of 20, {} rows)", pg_rows);
    println!("    postgres   {pg_scan_best:>10.1} us");
    println!("    thetabase    {theta_scan_us:>10.1} us");
    println!("    note       reported, not claimed. Analytical throughput");
    println!("               against a mature planner is not a comparison");
    println!("               ThetaBase wins or should claim (ROADMAP M8);");
    println!("               `specs/09` states absolute targets instead.");
    println!("\n=== end ===\n");
}
