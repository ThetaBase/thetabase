//! Regional branches and follower reads (ROADMAP-V3 M23, items 1 and 2).
//!
//! # Regional branches: multi-master in the sense that matters
//!
//! Each region writes to its own branch and the branches merge explicitly. A
//! caller in Frankfurt writes locally with no cross-region round trip, which is
//! the property people want from multi-master.
//!
//! It costs nothing conceptually new, and that is the point: the conflicts are
//! branch-merge conflicts this design already resolves — deterministic for CRDT
//! fields, human-arbitrated otherwise (`specs/03` §3.2). Constant-time forks
//! made the shape free to create; what was missing was placement, routing, and
//! a merge cadence, which is what this is.
//!
//! **What it does to the guarantees, stated exactly.** `read-your-writes` holds
//! within a region, because a region is a branch and a branch has a total order.
//! Across regions you see your own writes immediately and another region's after
//! a merge — which is the branch model's existing behaviour, not a new
//! weakening. `totally-ordered-writes` is a total order *within a branch* and is
//! untouched.
//!
//! # Follower reads: a stale read becomes a redirect, not a wrong answer
//!
//! A read replica that lags breaks read-your-writes for a caller routed to it
//! after writing to the primary. There are two fixes and only one of them is
//! any good.
//!
//! Pinning a session to the primary after a write throws away the replica for
//! exactly the callers who most need it, and it is a session-level answer to a
//! per-key problem.
//!
//! So: the caller names the version it last saw, and the replica **refuses**
//! below it. `specs/03` §3.1 promises a client reads its own writes; a replica
//! that answered anyway would break that silently, and one that redirects
//! breaks nothing.
//!
//! The floor is **per key**. A global floor would redirect every read on a
//! replica that is behind on any key, which is a replica nobody can use.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use theta_core::branch::BranchId;

use crate::view::MaterializedView;

/// A region, as a name the deployment chooses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Region(pub String);

impl Region {
    pub fn new(name: impl Into<String>) -> Self {
        Region(name.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegionError {
    #[error(
        "no branch is placed in region `{0}`. A write routed to a region with no \
         branch would land somewhere the caller did not choose, so it is refused."
    )]
    Unplaced(String),

    #[error("region `{0}` already has a branch; placing a second would make routing ambiguous")]
    AlreadyPlaced(String),
}

/// Which branch serves which region.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    by_region: BTreeMap<Region, BranchId>,
    /// Where merges converge. Every regional branch merges into it and back out.
    home: Option<BranchId>,
}

impl Placement {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the branch every region eventually merges through.
    pub fn with_home(mut self, home: BranchId) -> Self {
        self.home = Some(home);
        self
    }

    pub fn home(&self) -> Option<BranchId> {
        self.home
    }

    /// Place a region's branch.
    ///
    /// Refuses a second placement for one region rather than replacing the
    /// first. Replacing would silently move every subsequent write in that
    /// region to a different branch, and the writes already on the old one would
    /// stop being visible to callers who are still reading from where they were
    /// told to.
    pub fn place(&mut self, region: Region, branch: BranchId) -> Result<(), RegionError> {
        if self.by_region.contains_key(&region) {
            return Err(RegionError::AlreadyPlaced(region.0));
        }
        self.by_region.insert(region, branch);
        Ok(())
    }

    /// Which branch a caller in `region` writes to.
    ///
    /// Refuses an unplaced region rather than falling back to the home branch.
    /// A fallback would send a Frankfurt write across the Atlantic silently,
    /// which is the exact latency the whole arrangement exists to avoid — and
    /// the caller would have no way to tell it happened.
    pub fn route(&self, region: &Region) -> Result<BranchId, RegionError> {
        self.by_region
            .get(region)
            .copied()
            .ok_or_else(|| RegionError::Unplaced(region.0.clone()))
    }

    pub fn regions(&self) -> impl Iterator<Item = (&Region, &BranchId)> {
        self.by_region.iter()
    }

    pub fn len(&self) -> usize {
        self.by_region.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_region.is_empty()
    }
}

/// How often regional branches merge, and how far behind they are allowed to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeCadence {
    /// Target interval between merges.
    pub interval_ms: i64,
    /// The longest a region may go unmerged before it is a finding.
    ///
    /// Separate from the interval, because a missed merge is normal and a region
    /// that has not merged all day is not. Without the second number the first
    /// is a hope.
    pub max_lag_ms: i64,
}

impl Default for MergeCadence {
    fn default() -> Self {
        Self {
            interval_ms: 60 * 1_000,
            max_lag_ms: 15 * 60 * 1_000,
        }
    }
}

/// Regions that have fallen behind.
///
/// Returned rather than logged: a region whose writes are not reaching the
/// others is a correctness problem for anyone reading across regions, and it
/// should reach a caller who can act rather than a log nobody watches.
pub fn lagging(
    last_merged_ms: &BTreeMap<Region, i64>,
    placement: &Placement,
    cadence: &MergeCadence,
    now_ms: i64,
) -> Vec<(Region, i64)> {
    placement
        .regions()
        .filter_map(|(region, _)| {
            // A region that has never merged is lagging by however long it has
            // existed, and reporting it as zero would make a brand-new region
            // that is silently failing to merge look healthy.
            let last = last_merged_ms.get(region).copied().unwrap_or(i64::MIN);
            let lag = if last == i64::MIN {
                i64::MAX
            } else {
                now_ms.saturating_sub(last)
            };
            (lag > cadence.max_lag_ms).then(|| (region.clone(), lag))
        })
        .collect()
}

/// What a caller must have seen before a replica may answer.
///
/// Per key, not a single number for the whole replica. A global floor redirects
/// every read on a replica behind on any key, which is a replica nobody can use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionFloor {
    floors: BTreeMap<String, u64>,
}

impl VersionFloor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the version a caller saw for a key.
    ///
    /// Monotonic: a caller cannot lower its own floor. Allowing that would let a
    /// client that had seen version 9 ask to be served version 4, which is
    /// read-your-writes broken by the client's own request rather than by the
    /// replica.
    /// Record what a read actually returned.
    ///
    /// Takes the decision rather than a number, so there is no call that can
    /// record a floor for a key the replica did not have. That is the shape the
    /// `Option` above exists to enforce.
    pub fn record(&mut self, key: &str, decision: &ReadDecision) {
        if let ReadDecision::Serve {
            version: Some(version),
        } = decision
        {
            self.observed(key, *version);
        }
    }

    pub fn observed(&mut self, key: impl Into<String>, version: u64) {
        let key = key.into();
        let entry = self.floors.entry(key).or_insert(version);
        *entry = (*entry).max(version);
    }

    pub fn required(&self, key: &str) -> Option<u64> {
        self.floors.get(key).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.floors.is_empty()
    }
}

/// What a replica decided about one read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ReadDecision {
    /// The replica is current enough for this key.
    ///
    /// `version` is `None` when the replica genuinely does not hold the key.
    /// **Absent is not version zero**, and this field is an `Option` because the
    /// first version of it was a `u64` defaulting to `0` — which meant a caller
    /// reading a key that does not exist recorded a floor of 0 for it, and a
    /// later read of the same key would then be served by a replica that still
    /// did not have it. The redirect branch below already refused to make that
    /// conflation; this one was making it two lines away.
    Serve { version: Option<u64> },
    /// The replica is behind. **Not an error and not a stale answer** — the
    /// caller is told where to go instead.
    Redirect {
        key: String,
        replica_version: Option<u64>,
        required_version: u64,
        reason: String,
    },
}

impl ReadDecision {
    pub fn served(&self) -> bool {
        matches!(self, ReadDecision::Serve { .. })
    }
}

/// May this replica answer for `key`?
pub fn may_serve(replica: &MaterializedView, floor: &VersionFloor, key: &str) -> ReadDecision {
    let required = match floor.required(key) {
        // The caller has never seen this key, so any version is consistent with
        // what they know. A replica that refused here would refuse every first
        // read, which is most reads.
        None => {
            return ReadDecision::Serve {
                version: replica.version_of(key),
            }
        }
        Some(required) => required,
    };

    let have = replica.version_of(key);
    match have {
        Some(version) if version >= required => ReadDecision::Serve {
            version: Some(version),
        },
        // Absent is *behind*, not equal to zero. A key the caller has seen and
        // the replica has not is the clearest case of a lagging replica, and
        // treating a missing key as version 0 would serve "not found" to a
        // caller who wrote it a moment ago.
        _ => ReadDecision::Redirect {
            key: key.to_string(),
            replica_version: have,
            required_version: required,
            reason: format!(
                "this replica holds {} for `{key}` and you have already seen version \
                 {required}. Answering would break read-your-writes, so you are being \
                 sent to a replica that is current instead.",
                have.map(|v| format!("version {v}"))
                    .unwrap_or_else(|| "nothing".into())
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::{Author, CommitId, OpType};
    use theta_core::{ContentHash, LogEntry, Value};

    const HOME: BranchId = BranchId::MAIN;
    const FRA: BranchId = BranchId(1);
    const IAD: BranchId = BranchId(2);

    fn view_at(rows: &[(&str, i64)]) -> MaterializedView {
        let mut view = MaterializedView::new();
        for (i, (key, value)) in rows.iter().enumerate() {
            view.apply(&LogEntry {
                prev_hash: ContentHash([i as u8; 32]),
                commit_id: CommitId(i as u64 + 1),
                branch_id: HOME,
                op: OpType::Put {
                    key: (*key).into(),
                    value: Value::Int(*value),
                },
                author: Author::System,
                timestamp_ms: i as i64,
            });
        }
        view
    }

    fn placement() -> Placement {
        let mut placement = Placement::new().with_home(HOME);
        placement.place(Region::new("fra"), FRA).unwrap();
        placement.place(Region::new("iad"), IAD).unwrap();
        placement
    }

    #[test]
    fn a_caller_is_routed_to_their_regions_branch() {
        let placement = placement();
        assert_eq!(placement.route(&Region::new("fra")).unwrap(), FRA);
        assert_eq!(placement.route(&Region::new("iad")).unwrap(), IAD);
    }

    #[test]
    fn an_unplaced_region_is_refused_rather_than_sent_home() {
        // A fallback would send a Frankfurt write across the Atlantic silently,
        // which is the exact latency the arrangement exists to avoid — and the
        // caller would have no way to tell it happened.
        let placement = placement();
        let err = placement.route(&Region::new("syd")).unwrap_err();
        assert!(matches!(err, RegionError::Unplaced(_)), "{err:?}");
    }

    #[test]
    fn a_region_cannot_be_silently_replaced() {
        // Replacing would move every subsequent write to a different branch, and
        // the writes already on the old one would stop being visible to callers
        // still reading where they were told to.
        let mut placement = placement();
        let err = placement
            .place(Region::new("fra"), BranchId(9))
            .unwrap_err();
        assert!(matches!(err, RegionError::AlreadyPlaced(_)));
        assert_eq!(placement.route(&Region::new("fra")).unwrap(), FRA);
    }

    #[test]
    fn a_region_that_has_never_merged_is_lagging_rather_than_current() {
        // Reporting it as zero would make a brand-new region that is silently
        // failing to merge look like the healthiest one in the fleet.
        let placement = placement();
        let cadence = MergeCadence::default();
        let behind = lagging(&BTreeMap::new(), &placement, &cadence, 1_000);
        assert_eq!(behind.len(), 2, "neither region has ever merged");
    }

    #[test]
    fn a_region_within_the_cadence_is_not_reported() {
        let placement = placement();
        let cadence = MergeCadence::default();
        let merged = BTreeMap::from([
            (Region::new("fra"), 1_000i64),
            (Region::new("iad"), 1_000i64),
        ]);
        assert!(lagging(&merged, &placement, &cadence, 2_000).is_empty());

        let behind = lagging(
            &merged,
            &placement,
            &cadence,
            1_000 + cadence.max_lag_ms + 1,
        );
        assert_eq!(behind.len(), 2, "both have now fallen behind");
    }

    #[test]
    fn a_replica_that_is_current_enough_serves_the_read() {
        let replica = view_at(&[("orders:1", 1)]);
        let mut floor = VersionFloor::new();
        floor.observed("orders:1", replica.version_of("orders:1").unwrap());

        assert!(may_serve(&replica, &floor, "orders:1").served());
    }

    #[test]
    fn a_key_the_replica_does_not_have_is_served_as_absent_not_as_version_zero() {
        // Found by planting. `Serve` used to carry a bare `u64` defaulting to 0,
        // so a caller reading a key that does not exist recorded a floor of 0
        // for it — and a later read was then served by a replica that still did
        // not have it. The redirect branch already refused that conflation; this
        // one was making it two lines away.
        let replica = view_at(&[("orders:1", 1)]);
        let mut floor = VersionFloor::new();

        let decision = may_serve(&replica, &floor, "orders:absent");
        assert_eq!(decision, ReadDecision::Serve { version: None });

        floor.record("orders:absent", &decision);
        assert_eq!(
            floor.required("orders:absent"),
            None,
            "an absent read must not set a floor"
        );
    }

    #[test]
    fn a_stale_replica_redirects_rather_than_answering() {
        // `specs/03` §3.1 promises a client reads its own writes. A replica that
        // answered anyway would break that silently; one that redirects breaks
        // nothing.
        let replica = view_at(&[("orders:1", 1)]);
        let mut floor = VersionFloor::new();
        floor.observed("orders:1", 999);

        match may_serve(&replica, &floor, "orders:1") {
            ReadDecision::Redirect {
                required_version,
                reason,
                ..
            } => {
                assert_eq!(required_version, 999);
                assert!(reason.contains("read-your-writes"));
            }
            other => panic!("a stale replica answered: {other:?}"),
        }
    }

    #[test]
    fn a_key_the_replica_has_never_seen_is_behind_rather_than_at_version_zero() {
        // Treating a missing key as version 0 would serve "not found" to a
        // caller who wrote it a moment ago, which is the worst available answer:
        // it looks like data loss.
        let replica = view_at(&[("orders:1", 1)]);
        let mut floor = VersionFloor::new();
        floor.observed("orders:99", 1);

        match may_serve(&replica, &floor, "orders:99") {
            ReadDecision::Redirect {
                replica_version, ..
            } => assert_eq!(replica_version, None),
            other => panic!("expected a redirect, got {other:?}"),
        }
    }

    #[test]
    fn a_first_read_of_a_key_is_never_redirected() {
        // The caller has seen nothing, so any version is consistent with what
        // they know. A replica that refused here would refuse most reads, which
        // is a replica nobody can use.
        let replica = view_at(&[("orders:1", 1)]);
        let floor = VersionFloor::new();
        assert!(may_serve(&replica, &floor, "orders:1").served());
        assert!(may_serve(&replica, &floor, "orders:absent").served());
    }

    #[test]
    fn the_floor_is_per_key_so_one_lagging_key_does_not_redirect_everything() {
        // A global floor redirects every read on a replica behind on any key.
        let replica = view_at(&[("orders:1", 1), ("orders:2", 2)]);
        let mut floor = VersionFloor::new();
        floor.observed("orders:1", 999);
        floor.observed("orders:2", replica.version_of("orders:2").unwrap());

        assert!(!may_serve(&replica, &floor, "orders:1").served());
        assert!(
            may_serve(&replica, &floor, "orders:2").served(),
            "a key the replica is current on must still be servable"
        );
    }

    #[test]
    fn a_caller_cannot_lower_its_own_floor() {
        // Otherwise a client that had seen version 9 could ask to be served
        // version 4 — read-your-writes broken by the client's own request rather
        // than by the replica.
        let mut floor = VersionFloor::new();
        floor.observed("orders:1", 9);
        floor.observed("orders:1", 4);
        assert_eq!(floor.required("orders:1"), Some(9));
    }
}
