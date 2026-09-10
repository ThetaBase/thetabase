//! Plan cache and cost estimation.
//!
//! STATUS: the cache and the cost model's interface are real; index selection
//! and join ordering land in ROADMAP M3.

use std::collections::HashMap;

use theta_core::schema::Schema;

use crate::explain::Explain;
use crate::plan::{Plan, PlanHash};

/// Compiled-plan cache, keyed by [`PlanHash`]. A hit skips parse and planning
/// entirely, which is the difference between the cached (15ms p50) and uncached
/// (40ms p50) query budgets in `09-sla-performance.md` §2.
#[derive(Debug, Default)]
pub struct PlanCache {
    entries: HashMap<PlanHash, Plan>,
    hits: u64,
    misses: u64,
}

impl PlanCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, hash: PlanHash) -> Option<&Plan> {
        match self.entries.contains_key(&hash) {
            true => {
                self.hits += 1;
                self.entries.get(&hash)
            }
            false => {
                self.misses += 1;
                None
            }
        }
    }

    pub fn insert(&mut self, plan: Plan) -> PlanHash {
        let hash = plan.hash();
        self.entries.insert(hash, plan);
        hash
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

#[derive(Debug, Default)]
pub struct Planner {
    cache: PlanCache,
    stats: crate::stats::Statistics,
}

impl Planner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Optimize a plan against the current statistics.
    ///
    /// Every pass is meaning-preserving; see [`crate::optimize`]. A plan the
    /// optimizer cannot improve comes back unchanged, which is the documented
    /// degraded mode (`01-system-architecture.md` §7): correct but
    /// unoptimized, never a fallback to unchecked raw execution.
    pub fn optimize(&mut self, plan: Plan, _schema: &Schema) -> Plan {
        crate::optimize::optimize(plan, &self.stats)
    }

    /// Replace the statistics the planner reasons about.
    pub fn set_statistics(&mut self, stats: crate::stats::Statistics) {
        self.stats = stats;
    }

    pub fn statistics(&self) -> &crate::stats::Statistics {
        &self.stats
    }

    pub fn statistics_mut(&mut self) -> &mut crate::stats::Statistics {
        &mut self.stats
    }

    /// Produce the EXPLAIN output that the Safety Layer and human reviewers read
    /// before anything executes (`02-api-wire-protocol.md` §4).
    pub fn explain(&self, plan: &Plan, schema: &Schema) -> Explain {
        Explain::with_stats(plan, schema, &self.stats)
    }

    pub fn cache_mut(&mut self) -> &mut PlanCache {
        &mut self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_reports_hits_and_misses() {
        let mut cache = PlanCache::new();
        let hash = cache.insert(Plan::Scan {
            table: "users".into(),
        });
        assert!(cache.get(hash).is_some());
        assert!(cache.get(PlanHash(999)).is_none());
        assert_eq!(cache.stats(), (1, 1));
    }
}
