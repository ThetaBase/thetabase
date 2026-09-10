//! Per-project, per-environment safety thresholds.
//!
//! Defaults are strict on protected branches and looser elsewhere
//! (`07-agent-safety-layer.md` §3). Every threshold is configurable, but
//! configuration can only be supplied by a human/project owner — never by the
//! agent whose changes are being gated.

use serde::{Deserialize, Serialize};

/// The highest a project may set its irreversible-change shadow threshold.
///
/// `07-agent-safety-layer.md` §4 says an irreversible change above "a low
/// threshold" gets the strongest gate *regardless of who or what is asking*. A
/// threshold a project could raise without limit would make that sentence
/// false: setting it to `u64::MAX` turns every drop back into something a
/// single confirmation clears.
///
/// The ceiling is the loosest preset this product ships
/// ([`SafetyPolicy::development`]), so no project can be more permissive here
/// than a dev branch already is. A policy may still make it stricter — that
/// direction is always allowed, because it only ever adds review.
///
/// **And that permissiveness stops at a protected branch.** This ceiling bounds
/// what a project may ask for anywhere; on a protected target the thresholds are
/// additionally floored at [`SafetyPolicy::protected`]'s, so a delivered policy
/// can tighten `main` and cannot loosen it. Before that, an owner could raise
/// this to 10,000 and a 9,999-row irreversible drop on the production branch was
/// one confirmation away — which an external review (R3-03) reported as the
/// impact of branch protection being decorative.
///
/// It is a real reduction in what a policy can do, and it applies only to
/// branches marked protected. Everywhere else the dial is unchanged.
pub const MAX_IRREVERSIBLE_SHADOW_THRESHOLD: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyPolicy {
    /// Rows a single operation may touch before it needs review regardless of
    /// whether it is destructive by type.
    pub row_impact_threshold: u64,

    /// An irreversible change touching more rows than this may not be approved
    /// by confirmation alone — shadow-branch validation is the only path
    /// (`07-agent-safety-layer.md` §4).
    /// This is what a project *asked for*. What applies is
    /// [`SafetyPolicy::effective_irreversible_shadow_threshold`], which caps it
    /// at [`MAX_IRREVERSIBLE_SHADOW_THRESHOLD`] — the classifier reads that and
    /// never this field directly.
    pub irreversible_shadow_threshold: u64,

    /// Cumulative rows across the breaker's rolling window before it trips.
    pub breaker_row_ceiling: u64,

    /// Length of that rolling window.
    pub breaker_window_ms: u64,

    /// Narrowly-scoped auto-approvals, e.g. "additive index changes under 10k
    /// rows". Empty by default: nothing is auto-approved that the rules would
    /// otherwise gate.
    pub auto_approve: Vec<AutoApproveRule>,

    /// Reclassify ambiguous changes (rename, backfill) as non-destructive.
    /// Off by default — ambiguous means destructive until a human says otherwise.
    pub treat_ambiguous_as_safe: bool,

    /// How long a shadow branch lives before garbage collection reclaims it.
    ///
    /// Shadow branches are ephemeral by definition (`01-system-architecture.md`
    /// §2.2). Nothing forces a proposer to come back and promote or reject one,
    /// so without a deadline an agent that proposes a thousand drops leaves a
    /// thousand branches behind.
    ///
    /// Reclaiming early is safe in the direction that matters: it can only
    /// force a change to be validated again, never let one land unvalidated.
    pub shadow_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoApproveRule {
    /// Change tag this rule applies to, e.g. `"add_index"`.
    pub change: String,
    pub max_rows: u64,
}

impl SafetyPolicy {
    /// The irreversible-change shadow threshold that actually applies.
    ///
    /// Capped, so raising the configured value past the ceiling has no effect.
    /// Every read in the classifier goes through here; reading the field
    /// directly would reintroduce the bypass this exists to close.
    pub fn effective_irreversible_shadow_threshold(&self) -> u64 {
        self.irreversible_shadow_threshold
            .min(MAX_IRREVERSIBLE_SHADOW_THRESHOLD)
    }

    /// This policy, tightened to at least what a protected branch demands.
    ///
    /// Branch protection was decorative: `classify` took a `protected` flag,
    /// passed it to the rationale, and never let it reach the gate. Protection
    /// was meant to be carried by *which policy* was in force, but an instance
    /// holds one policy chosen by its environment at startup — so a project's
    /// `main` running on a Dev or Preview instance was gated with dev-loose
    /// thresholds. An irreversible drop of up to 10,000 rows on `main` was a
    /// single `Confirm` away, where a protected policy demands shadow
    /// validation at 100. An external review (R3-03) found it.
    ///
    /// Takes the stricter of each threshold rather than replacing the policy, so
    /// a project that configured something stricter than `protected()` keeps it.
    /// A protected branch can only ever be gated harder.
    pub fn tightened_for_protected(&self) -> Self {
        let floor = Self::protected();
        Self {
            row_impact_threshold: self.row_impact_threshold.min(floor.row_impact_threshold),
            irreversible_shadow_threshold: self
                .irreversible_shadow_threshold
                .min(floor.irreversible_shadow_threshold),
            // The breaker and the shadow TTL are not gate inputs; leaving them
            // alone keeps this a statement about *gating* rather than a second,
            // quieter policy switch.
            breaker_row_ceiling: self.breaker_row_ceiling,
            breaker_window_ms: self.breaker_window_ms,
            // An auto-approve rule cannot apply on a protected branch. It exists
            // to wave through routine work on a branch where a mistake costs a
            // discarded branch; on `main` it is a standing exemption.
            auto_approve: Vec::new(),
            // Ambiguity resolves against the change, never for it.
            treat_ambiguous_as_safe: false,
            shadow_ttl_ms: self.shadow_ttl_ms,
        }
    }

    /// Defaults for a protected branch (`main`, `prod`).
    ///
    /// `breaker_row_ceiling` is calibrated rather than chosen: the corpus in
    /// `tests/breaker_calibration.rs` measures the heaviest legitimate minute at
    /// 40,000 rows (an admin bulk edit) and the lightest runaway at 300,000 (a
    /// sustained retry loop). 100,000 sits between them with 2.5x headroom over
    /// real work, and tests pin it from both directions so it cannot drift.
    pub fn protected() -> Self {
        Self {
            row_impact_threshold: 1_000,
            irreversible_shadow_threshold: 100,
            breaker_row_ceiling: 100_000,
            breaker_window_ms: 60_000,
            auto_approve: Vec::new(),
            treat_ambiguous_as_safe: false,
            shadow_ttl_ms: DEFAULT_SHADOW_TTL_MS,
        }
    }

    /// Defaults for a dev/preview branch. Looser, because the cost of a mistake
    /// is a discarded branch — but never unbounded, because a runaway agent loop
    /// costs money on any branch.
    ///
    /// The breaker ceiling was 1,000,000, which did not honour that last clause:
    /// a loop sustaining 300,000 rows a minute ran indefinitely without tripping.
    /// 250,000 sits inside the band the calibration corpus measures — above
    /// every legitimate workload by more than six times, below the lightest
    /// runaway — and `tests/breaker_calibration.rs` holds it there from both
    /// sides.
    pub fn development() -> Self {
        Self {
            row_impact_threshold: 50_000,
            irreversible_shadow_threshold: 10_000,
            breaker_row_ceiling: 250_000,
            breaker_window_ms: 60_000,
            auto_approve: Vec::new(),
            treat_ambiguous_as_safe: false,
            shadow_ttl_ms: DEFAULT_SHADOW_TTL_MS,
        }
    }
}

/// A day: long enough that a human review spanning a weekend night is not cut
/// short, short enough that abandoned branches do not accumulate for a week.
pub const DEFAULT_SHADOW_TTL_MS: u64 = 24 * 60 * 60 * 1_000;

impl Default for SafetyPolicy {
    /// Fail safe: an unconfigured project gets protected-branch strictness.
    fn default() -> Self {
        Self::protected()
    }
}
