//! What sealing a record actually costs, measured rather than assumed.
//!
//! Run with `--release --nocapture`. This exists because the end-to-end SLA
//! suite cannot answer the question: a `put` is dominated by an fsync, and the
//! run-to-run variance of a loaded host is far larger than anything a cipher
//! contributes. Measuring the cipher alone is the only way to get a number that
//! means something.

use std::time::Instant;

use theta_storage::DataKey;

/// Skipped in a debug build, where this would measure rustc's bounds checks
/// rather than the cipher — and report a number that is wrong by an order of
/// magnitude in the reassuring direction.
fn release_only() -> bool {
    cfg!(debug_assertions)
}

#[test]
fn sealing_a_record_is_cheap_next_to_the_fsync_it_rides_along_with() {
    if release_only() {
        eprintln!("skipped: debug build");
        return;
    }

    let key = DataKey::from_hex(&"3f".repeat(32)).expect("key");
    // A representative record: a JSON-encoded `Put` with a realistic row.
    let payload = br#"{"prev_hash":"0000000000000000000000000000000000000000000000000000000000000000","commit_id":8421,"branch_id":0,"op":{"Put":{"key":"customers:4417","value":{"Map":{"email":{"Text":"someone@example.com"},"name":{"Text":"A Customer"},"plan":{"Text":"team"},"seats":{"Int":12}}}}},"author":"System","timestamp_ms":1750000000000}"#;

    const ROUNDS: u32 = 100_000;
    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..ROUNDS {
        total += key.seal(payload).expect("seal").len();
    }
    let per_record = start.elapsed().as_secs_f64() * 1e6 / ROUNDS as f64;
    assert!(total > 0);

    eprintln!(
        "seal: {per_record:.3}us per {}-byte record ({:.0} MB/s)",
        payload.len(),
        payload.len() as f64 / per_record
    );

    // A generous ceiling, not a target. The point is to catch a cipher swap or
    // a build misconfiguration that makes this cost milliseconds — at which
    // point encryption would start showing up in the SLA rather than hiding
    // under the fsync.
    assert!(
        per_record < 50.0,
        "sealing a record took {per_record:.3}us, which is no longer negligible \
         against a ~3.6ms put"
    );
}
