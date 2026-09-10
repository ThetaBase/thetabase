//! Blast-radius circuit breaker (`07-agent-safety-layer.md` §6).
//!
//! Independent of destructive/non-destructive classification: this is what
//! catches a runaway agent loop, where every individual write is perfectly safe
//! and the aggregate is not.
//!
//! The decision must be immediate — the SLA budget is <50ms
//! (`09-sla-performance.md` §3) — so this is a pure in-memory computation with
//! no I/O and no allocation on the accept path beyond the window ring.
//!
//! Time is injected rather than read from the clock, so the breaker is
//! deterministically testable under simulated load.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::policy::SafetyPolicy;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum BreakerDecision {
    Allow,
    /// Tripped. Carries the numbers a human needs to understand why, because a
    /// silent 503 is explicitly not acceptable here.
    Trip {
        window_rows: u64,
        ceiling: u64,
        window_ms: u64,
        reason: String,
    },
}

impl BreakerDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, BreakerDecision::Allow)
    }
}

#[derive(Debug, Clone)]
struct Sample {
    at_ms: u64,
    rows: u64,
}

/// Rolling-window accumulator over write impact.
#[derive(Debug)]
pub struct CircuitBreaker {
    window: VecDeque<Sample>,
    window_rows: u64,
    ceiling: u64,
    window_ms: u64,
    tripped: bool,
}

impl CircuitBreaker {
    pub fn new(policy: &SafetyPolicy) -> Self {
        Self {
            window: VecDeque::new(),
            window_rows: 0,
            ceiling: policy.breaker_row_ceiling,
            window_ms: policy.breaker_window_ms,
            tripped: false,
        }
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Rows accumulated in the current window. Surfaced via the `status` RPC.
    pub fn window_rows(&self) -> u64 {
        self.window_rows
    }

    /// Record a write of `rows` at `now_ms` and decide whether it may proceed.
    ///
    /// Once tripped the breaker stays tripped until [`CircuitBreaker::reset`] —
    /// a runaway loop must not be able to wait out its own window and resume.
    pub fn record(&mut self, rows: u64, now_ms: u64) -> BreakerDecision {
        self.evict_expired(now_ms);

        if self.tripped {
            return self
                .trip_decision("breaker already tripped; reset required before writes resume");
        }

        let projected = self.window_rows.saturating_add(rows);
        if projected > self.ceiling {
            self.tripped = true;
            self.window_rows = projected;
            return self.trip_decision(
                "cumulative write impact exceeded the project ceiling within the rolling window",
            );
        }

        self.window.push_back(Sample {
            at_ms: now_ms,
            rows,
        });
        self.window_rows = projected;
        BreakerDecision::Allow
    }

    /// Clear the breaker. Deliberately explicit: an operator or a policy action,
    /// never an automatic timeout.
    pub fn reset(&mut self) {
        self.window.clear();
        self.window_rows = 0;
        self.tripped = false;
    }

    fn evict_expired(&mut self, now_ms: u64) {
        // Compared as `at + window <= now` rather than against a subtracted
        // cutoff: near t=0 a saturating subtraction floors the cutoff at zero and
        // evicts samples that are still inside the window.
        while let Some(front) = self.window.front() {
            if front.at_ms.saturating_add(self.window_ms) <= now_ms {
                self.window_rows = self.window_rows.saturating_sub(front.rows);
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    fn trip_decision(&self, reason: &str) -> BreakerDecision {
        BreakerDecision::Trip {
            window_rows: self.window_rows,
            ceiling: self.ceiling,
            window_ms: self.window_ms,
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ceiling: u64) -> SafetyPolicy {
        SafetyPolicy {
            breaker_row_ceiling: ceiling,
            breaker_window_ms: 60_000,
            ..SafetyPolicy::protected()
        }
    }

    #[test]
    fn writes_under_the_ceiling_are_allowed() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        assert!(b.record(400, 0).is_allowed());
        assert!(b.record(400, 100).is_allowed());
        assert!(!b.is_tripped());
    }

    #[test]
    fn a_runaway_loop_trips_the_breaker() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        let mut tripped_at = None;
        for i in 0..100u64 {
            if !b.record(100, i * 10).is_allowed() {
                tripped_at = Some(i);
                break;
            }
        }
        assert_eq!(
            tripped_at,
            Some(10),
            "should trip once cumulative rows pass 1000"
        );
        assert!(b.is_tripped());
    }

    #[test]
    fn old_samples_leave_the_window() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        assert!(b.record(900, 0).is_allowed());
        // Same volume a full window later is fine; the first sample has expired.
        assert!(b.record(900, 61_000).is_allowed());
        assert_eq!(b.window_rows(), 900);
    }

    #[test]
    fn a_tripped_breaker_does_not_clear_itself_by_waiting() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        b.record(2_000, 0);
        assert!(b.is_tripped());
        assert!(
            !b.record(1, 10_000_000).is_allowed(),
            "waiting must not reset the breaker"
        );
        b.reset();
        assert!(b.record(1, 10_000_001).is_allowed());
    }

    #[test]
    fn a_sample_exactly_one_window_old_expires_and_a_younger_one_does_not() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        b.record(500, 0);
        // Regression: a saturating cutoff used to evict this sample immediately,
        // silently under-counting the window for the first 60s of a process.
        assert!(b.record(400, 1).is_allowed());
        assert_eq!(b.window_rows(), 900);
        b.record(0, 60_000);
        assert_eq!(b.window_rows(), 400, "only the sample at t=0 has aged out");
    }

    #[test]
    fn a_single_oversized_write_trips_rather_than_slipping_through() {
        let mut b = CircuitBreaker::new(&policy(1_000));
        assert!(!b.record(50_000, 0).is_allowed());
    }
}
