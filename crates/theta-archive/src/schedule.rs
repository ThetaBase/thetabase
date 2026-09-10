//! The timer that drives sweeps (ROADMAP M10).
//!
//! Deliberately thin. Everything that decides anything lives in
//! [`Custodian::tick`], which is synchronous and takes its clock as an
//! argument — so the behaviour is tested without sleeping, and this module is
//! left with nothing to get wrong but the interval.
//!
//! That split is the point. A scheduler that also decided what to archive would
//! be a scheduler whose logic could only be tested by waiting.

use std::time::Duration;

use crate::custodian::{Custodian, SegmentSource, TickReport};
use crate::ArchiveBackend;

/// How often to sweep.
///
/// Five minutes. The cost of a tick with nothing to do is one directory
/// listing, and the cost of sweeping too rarely is local disk holding segments
/// that are already safely archived — so the bias is toward frequent and cheap.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// What a caller wants to know about each tick as it happens.
///
/// A callback rather than a return value, because the loop does not end. An
/// operator needs to see a failed proof when it happens, not when the process
/// finally stops.
pub trait TickObserver: Send {
    fn observed(&mut self, report: &TickReport);
}

/// Logs each tick, loudly when something needs a human.
#[derive(Debug, Default)]
pub struct LogObserver;

impl TickObserver for LogObserver {
    fn observed(&mut self, report: &TickReport) {
        if report.needs_attention() {
            // `error`, not `warn`. A failed proof means the archive returned
            // something other than what went in, and a gap means the archive
            // cannot be restored from — neither is a thing to notice later.
            tracing::error!(
                summary = report.summary(),
                failed = ?report.failed,
                gaps = ?report.gaps,
                "archive sweep needs attention"
            );
            return;
        }
        // Quiet when there was nothing to do, so the log is readable and a tick
        // that *did* something stands out in it.
        if report.archived.is_empty() && report.released.is_empty() {
            tracing::debug!(summary = report.summary(), "archive sweep: nothing to do");
        } else {
            tracing::info!(summary = report.summary(), "archive sweep");
        }
    }
}

/// Run sweeps until cancelled.
///
/// Never returns on its own. A tick that fails does not stop the loop: the next
/// one may succeed, and an archiver that exits on the first unreachable archive
/// is an archiver that stops the first time object storage blinks and stays
/// stopped.
pub async fn run<B, S, O>(
    mut custodian: Custodian<B>,
    mut source: S,
    mut observer: O,
    interval: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) where
    B: ArchiveBackend,
    S: SegmentSource,
    O: TickObserver,
{
    let mut ticker = tokio::time::interval(interval);
    // Skip missed ticks rather than firing them back to back. If the process
    // was paused — a laptop lid, a stalled disk — catching up buys nothing:
    // one sweep archives everything the missed ones would have.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let report = custodian.tick(&mut source, now_ms());
                observer.observed(&report);
            }
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    tracing::info!("archive sweeper stopping");
                    return;
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{ArchiveError, Manifest, StoredRef, Sweep};

    /// A source with nothing in it. What is under test here is the loop, not
    /// the sweep — `custodian_release.rs` owns that.
    struct Empty;
    impl SegmentSource for Empty {
        fn archivable(&self) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }
        fn release(&mut self, _sequence: u64) -> Result<u64, String> {
            unreachable!("nothing to release")
        }
    }

    struct Nowhere;
    impl ArchiveBackend for Nowhere {
        fn store(
            &mut self,
            _key: &str,
            _source: &std::path::Path,
        ) -> Result<StoredRef, ArchiveError> {
            unreachable!("nothing to store")
        }
        fn fetch(&self, _stored: &StoredRef, _dest: &std::path::Path) -> Result<(), ArchiveError> {
            unreachable!()
        }
        fn check_integrity(&self, _stored: &StoredRef) -> Result<bool, ArchiveError> {
            Ok(true)
        }
    }

    #[derive(Clone, Default)]
    struct Counter(Arc<Mutex<usize>>);
    impl TickObserver for Counter {
        fn observed(&mut self, _report: &TickReport) {
            *self.0.lock().expect("counter") += 1;
        }
    }

    fn custodian() -> Custodian<Nowhere> {
        Custodian::new(Sweep::new(
            Nowhere,
            Manifest::new("org_a/checkout"),
            std::env::temp_dir(),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn it_keeps_sweeping() {
        // Paused clock: the loop's behaviour over an hour is asserted in
        // microseconds, and a scheduler tested with real sleeps is one nobody
        // will ever assert more than one tick of.
        let counter = Counter::default();
        let (tx, rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(run(
            custodian(),
            Empty,
            counter.clone(),
            Duration::from_secs(60),
            rx,
        ));

        tokio::time::sleep(Duration::from_secs(305)).await;
        tx.send(true).expect("cancel");
        handle.await.expect("the loop must stop");

        let ticks = *counter.0.lock().expect("counter");
        assert!(
            (5..=6).contains(&ticks),
            "expected about five minutes of one-minute ticks, got {ticks}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_stops_it() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run(
            custodian(),
            Empty,
            Counter::default(),
            Duration::from_secs(60),
            rx,
        ));

        tx.send(true).expect("cancel");
        // Must return without waiting for the next interval: a shutdown that
        // takes a full sweep interval is a deploy that takes one.
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("the loop must stop promptly")
            .expect("no panic");
    }
}
