//! ThetaBase in your process (ROADMAP-V3 M22).
//!
//! # One engine, not a subset
//!
//! The tempting shape for an embedded build is a smaller engine: drop the
//! Safety Layer, drop shadow branches, keep the key-value store. That is the
//! shape a subset takes, and a subset diverges — which is the same argument the
//! Scribe core makes about the protocol, applied to the engine.
//!
//! So this is a facade over the identical [`thetad::engine::Engine`] the server
//! runs. Same log, same fold, same classifier, same gates. What is absent is the
//! *server*: no socket, no token, no Control Plane. Nothing here reimplements a
//! decision the engine already makes.
//!
//! # The gate comes along, and somebody still has to answer it
//!
//! **An embedded ThetaBase with no gate is a different product**, so the gate is
//! not optional here. A destructive change is classified exactly as it would be
//! on a server and refused exactly as hard.
//!
//! What changes is who answers. In a hosted deployment a human reviews;
//! in-process there may be no human and no review queue, so the *caller* is the
//! reviewer. [`Theta::confirm`] is how they say so, and there is deliberately no
//! `auto_confirm` flag, no `force: bool`, and no configuration that turns the
//! gate off. A caller who wants a drop applied writes the confirmation, which is
//! a line of code somebody can find in review.
//!
//! Shadow validation works unchanged: an ephemeral branch is a data structure,
//! not a deployment.
//!
//! # What is genuinely not here, stated rather than implied
//!
//! [`Limitations`] is a value, not a paragraph in a README, because the whole
//! point of "embeddable, honestly" is that the missing parts are checkable:
//!
//! - **No cross-project isolation.** The server's isolation is one project per
//!   process (`specs/04` §3). Embedded, the process is yours: if you open two
//!   projects in it, they share an address space and there is no boundary
//!   between them. That is not a weaker version of the hosted guarantee, it is
//!   the absence of it.
//! - **No token scoping or revocation.** There is no credential, so there is
//!   nothing to scope and nothing to revoke. Access control is whatever your
//!   process already has.
//! - **No archive, no anchor, no drills** unless you run them. The scheduler is
//!   a hosted concern; the crates are available and nothing calls them for you.
//! - **No audit trail you cannot edit.** The hosted trail is append-only because
//!   the process holding it is not yours. Here it is.
//!
//! Every one of those is a real capability of the hosted product that an
//! embedded caller does not get by embedding. Listing them in code means a
//! caller can assert on them, and means removing one is a diff rather than an
//! edit to prose nobody re-reads.

use serde::{Deserialize, Serialize};
use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, Value};
use theta_safety::diff::{ChangeDiff, ChangeId, Gate};
use theta_safety::SafetyPolicy;
use thetad::engine::{Engine, EngineError};
use thetad::Config;

pub use theta_safety::rationale::{GateRationale, Remedy};

/// What an embedded deployment does not have.
///
/// A value rather than documentation, so a caller can assert on it and so
/// removing one is a diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limitations {
    /// Two projects opened in one process share an address space.
    pub cross_project_isolation: bool,
    /// There is no credential, so nothing to scope or revoke.
    pub token_scoping: bool,
    /// Nothing sweeps segments to an archive unless the caller does.
    pub managed_archive: bool,
    /// The audit trail is in a process the caller controls.
    pub tamper_proof_audit_trail: bool,
}

impl Limitations {
    /// What embedding actually gives you.
    ///
    /// Every field is `false`. That is the honest state and it is written out
    /// rather than derived, so a future capability has to be turned on
    /// deliberately here rather than arriving by accident.
    pub fn embedded() -> Self {
        Self {
            cross_project_isolation: false,
            token_scoping: false,
            managed_archive: false,
            tamper_proof_audit_trail: false,
        }
    }

    /// A line for a caller who wants to print what they are running.
    pub fn describe(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.cross_project_isolation {
            missing.push(
                "no cross-project isolation: two projects in one process share an address space",
            );
        }
        if !self.token_scoping {
            missing.push("no token scoping or revocation: there is no credential");
        }
        if !self.managed_archive {
            missing.push("no managed archive: nothing sweeps segments unless you do");
        }
        if !self.tamper_proof_audit_trail {
            missing.push("the audit trail is in a process you control");
        }
        missing
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(transparent)]
    Engine(#[from] EngineError),

    #[error(
        "this change is gated at {gate:?} and has not been confirmed. Embedded, \
         you are the reviewer: call `confirm` (or `promote`, for a change that \
         needs shadow validation) rather than looking for a flag that turns the \
         gate off, because there is not one."
    )]
    NeedsReview { change_id: String, gate: Gate },
}

/// A ThetaBase in this process.
pub struct Theta {
    engine: Engine,
    limitations: Limitations,
}

impl Theta {
    /// Open a project at `path`.
    ///
    /// Takes a [`SafetyPolicy`] explicitly rather than defaulting quietly. The
    /// hosted product gets its policy signed by the Control Plane; embedded
    /// there is nobody to sign one, so the caller chooses — and being made to
    /// choose is the point. A default here would be a gate somebody did not
    /// know they had.
    pub fn open(
        project_id: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
        policy: SafetyPolicy,
    ) -> Result<Self, EmbedError> {
        let mut config = Config::dev_default(project_id);
        config.data_dir = path.into();
        config.safety = policy;
        Ok(Self {
            engine: Engine::open(config)?,
            limitations: Limitations::embedded(),
        })
    }

    /// What this deployment does not have.
    pub fn limitations(&self) -> &Limitations {
        &self.limitations
    }

    /// The engine, for callers that need something this facade does not expose.
    ///
    /// Public because a facade that hides the engine forces the next feature to
    /// be reimplemented here as a subset — the exact divergence this crate
    /// exists to avoid. Reaching through it is fine; what is not available *at
    /// all* is a way around the gate, and there is none to reach for.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    pub fn get(&self, branch: BranchId, key: &str) -> Option<Value> {
        self.engine
            .get(branch, key)
            .cloned()
            .or_else(|| self.engine.get_crdt(branch, key))
    }

    pub fn put(
        &mut self,
        branch: BranchId,
        key: &str,
        value: Value,
        author: Author,
        now_ms: i64,
    ) -> Result<(), EmbedError> {
        self.engine.put(branch, key, value, author, now_ms)?;
        Ok(())
    }

    /// Propose a schema change and get the diff.
    ///
    /// Returns without applying, exactly as the server does. Every proposal is
    /// held by id including the ones the gate would auto-apply, because a caller
    /// does not know a change's gate before proposing it — a proposal that
    /// applied itself when the gate turned out to be `AutoApply` would mean the
    /// caller learns what happened only after it has happened.
    pub fn propose(
        &mut self,
        branch: BranchId,
        change: SchemaChange,
        author: Author,
        now_ms: i64,
    ) -> ChangeDiff {
        self.engine
            .propose_schema_change(branch, change, author, now_ms)
    }

    /// Confirm a proposal, as the reviewer.
    ///
    /// Refuses a change at the shadow gate, like the server: confirmation is not
    /// sufficient there (`specs/07` §4), and offering an embedded caller a way
    /// past it would make the embedded build the weaker product this crate
    /// exists to not be.
    pub fn confirm(
        &mut self,
        change_id: &ChangeId,
        author: Author,
        now_ms: i64,
    ) -> Result<(), EmbedError> {
        self.engine
            .apply_schema_change(change_id, true, author, now_ms)?;
        Ok(())
    }

    /// Reject a proposal, with a reason.
    ///
    /// Present so that "no" is an action a caller takes rather than something
    /// they express by not calling `confirm`: a proposal nobody answers is
    /// indistinguishable from one nobody saw, and it leaves its shadow branch
    /// behind.
    ///
    /// The reason is required rather than optional for the same reason it is on
    /// the server — a record of *what* without *why* tells whoever reads it
    /// later nothing they can act on.
    pub fn reject(
        &mut self,
        change_id: &ChangeId,
        reason: &str,
        author: Author,
        now_ms: i64,
    ) -> Result<(), EmbedError> {
        self.engine
            .reject_shadow(change_id, reason, author, now_ms)?;
        Ok(())
    }

    /// Promote a change that has been validated on a shadow branch.
    ///
    /// The path `confirm` refuses. Distinct from confirmation because they are
    /// different acts: one says "yes, do it" and the other says "the validation
    /// passed and I have read it", and collapsing them would put the strongest
    /// gate behind the same call as the weakest.
    pub fn promote(
        &mut self,
        change_id: &ChangeId,
        author: Author,
        now_ms: i64,
    ) -> Result<(), EmbedError> {
        self.engine.promote_shadow(change_id, author, now_ms)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use theta_core::schema::{FieldDef, TableDef};
    use theta_core::ValueType;

    fn open() -> (Theta, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let theta = Theta::open("embedded", dir.path(), SafetyPolicy::protected()).expect("open");
        (theta, dir)
    }

    fn human() -> Author {
        Author::Human {
            user_id: "alice".into(),
        }
    }

    fn table() -> SchemaChange {
        SchemaChange::AddTable {
            table: TableDef {
                name: "orders".into(),
                fields: BTreeMap::from([(
                    "total".to_string(),
                    FieldDef {
                        name: "total".into(),
                        ty: ValueType::Int,
                        nullable: true,
                        crdt: None,
                        declared_at: None,
                    },
                )]),
                indexes: vec![],
            },
        }
    }

    #[test]
    fn an_embedded_theta_reads_and_writes_without_a_server() {
        let (mut theta, _dir) = open();
        let diff = theta.propose(BranchId::MAIN, table(), human(), 1_000);
        theta.confirm(&diff.change_id, human(), 1_001).unwrap();

        theta
            .put(
                BranchId::MAIN,
                "orders:1",
                Value::Map(BTreeMap::from([("total".into(), Value::Int(7))])),
                human(),
                2_000,
            )
            .unwrap();

        assert!(theta.get(BranchId::MAIN, "orders:1").is_some());
    }

    #[test]
    fn the_gate_is_the_same_gate_and_there_is_no_flag_that_turns_it_off() {
        // An embedded ThetaBase with no gate is a different product. A destructive
        // change is classified here exactly as it would be on a server.
        let (mut theta, _dir) = open();
        let diff = theta.propose(BranchId::MAIN, table(), human(), 1_000);
        theta.confirm(&diff.change_id, human(), 1_001).unwrap();

        for i in 0..200 {
            theta
                .put(
                    BranchId::MAIN,
                    &format!("orders:{i}"),
                    Value::Map(BTreeMap::from([("total".into(), Value::Int(i))])),
                    human(),
                    2_000 + i,
                )
                .unwrap();
        }

        let drop = theta.propose(
            BranchId::MAIN,
            SchemaChange::DropTable {
                table: "orders".into(),
            },
            human(),
            3_000,
        );

        assert!(drop.destructive);
        assert_ne!(
            drop.gate,
            Gate::AutoApply,
            "a drop of 200 rows must be gated in an embedded build too"
        );
    }

    #[test]
    fn confirmation_is_not_sufficient_at_the_shadow_gate_here_either() {
        // Offering an embedded caller a way past the strongest gate would make
        // the embedded build the weaker product this crate exists to not be.
        let (mut theta, _dir) = open();
        let diff = theta.propose(BranchId::MAIN, table(), human(), 1_000);
        theta.confirm(&diff.change_id, human(), 1_001).unwrap();

        for i in 0..500 {
            theta
                .put(
                    BranchId::MAIN,
                    &format!("orders:{i}"),
                    Value::Map(BTreeMap::from([("total".into(), Value::Int(i))])),
                    human(),
                    2_000 + i,
                )
                .unwrap();
        }

        let drop = theta.propose(
            BranchId::MAIN,
            SchemaChange::DropTable {
                table: "orders".into(),
            },
            human(),
            3_000,
        );
        assert_eq!(drop.gate, Gate::ShadowValidate);

        let refused = theta.confirm(&drop.change_id, human(), 3_001);
        assert!(
            refused.is_err(),
            "a confirmation cleared the gate that says confirmation is not enough"
        );
    }

    #[test]
    fn the_rationale_travels_into_the_embedded_api() {
        // An embedded caller is the reviewer, so it needs the same thing a
        // reviewer needs: which rule fired and what would unblock it. Getting
        // only a boolean would leave it guessing between "split the change" and
        // "validate it on a shadow branch".
        let (mut theta, _dir) = open();
        let diff = theta.propose(BranchId::MAIN, table(), human(), 1_000);
        assert!(
            diff.rationale.remedy.is_some() || diff.gate == Gate::AutoApply,
            "a gated change must say what would unblock it"
        );
    }

    #[test]
    fn what_embedding_does_not_give_you_is_a_value_rather_than_a_readme() {
        // The whole point of "embeddable, honestly": the missing parts are
        // checkable, and removing one is a diff rather than an edit to prose
        // nobody re-reads.
        let (theta, _dir) = open();
        let limits = theta.limitations();

        assert!(!limits.cross_project_isolation);
        assert!(!limits.token_scoping);
        assert!(!limits.managed_archive);
        assert!(!limits.tamper_proof_audit_trail);
        assert_eq!(limits.describe().len(), 4);
    }

    #[test]
    fn opening_requires_a_policy_rather_than_defaulting_to_one() {
        // A default here would be a gate somebody did not know they had. The
        // hosted product's policy is signed by the Control Plane; embedded there
        // is nobody to sign one, so the caller chooses.
        //
        // Structural — `open` takes a `SafetyPolicy` and there is no overload
        // that does not — and asserted through its consequence: two databases
        // opened with different policies gate the same change differently.
        //
        // The two policies differ in the *tightening* direction, and that is not
        // cosmetic. `main` is a protected branch, and a protected branch floors
        // its thresholds at `SafetyPolicy::protected()`'s — so comparing
        // `protected()` against `development()` here would compare two policies
        // that both land on the floor and produce the same gate. That is what
        // this test used to do, and it started failing when branch protection
        // stopped being decorative (external review R3-03).
        //
        // A caller's policy still decides. It decides by being stricter, which is
        // the only direction a protected branch allows.
        let strict_dir = tempfile::tempdir().unwrap();
        let loose_dir = tempfile::tempdir().unwrap();
        let mut strict = Theta::open(
            "p",
            strict_dir.path(),
            SafetyPolicy {
                irreversible_shadow_threshold: 10,
                ..SafetyPolicy::protected()
            },
        )
        .unwrap();
        let mut loose = Theta::open("p", loose_dir.path(), SafetyPolicy::development()).unwrap();

        for theta in [&mut strict, &mut loose] {
            let diff = theta.propose(BranchId::MAIN, table(), human(), 1_000);
            theta.confirm(&diff.change_id, human(), 1_001).unwrap();
            // Fifty rows: over the strict policy's threshold of 10, under the
            // 100 a protected branch floors the loose one to. The gap between
            // the two policies is where the answers differ.
            for i in 0..50 {
                theta
                    .put(
                        BranchId::MAIN,
                        &format!("orders:{i}"),
                        Value::Map(BTreeMap::from([("total".into(), Value::Int(i))])),
                        human(),
                        2_000 + i,
                    )
                    .unwrap();
            }
        }

        let strict_drop = strict.propose(
            BranchId::MAIN,
            SchemaChange::DropTable {
                table: "orders".into(),
            },
            human(),
            3_000,
        );
        let loose_drop = loose.propose(
            BranchId::MAIN,
            SchemaChange::DropTable {
                table: "orders".into(),
            },
            human(),
            3_000,
        );

        assert_ne!(
            strict_drop.gate, loose_drop.gate,
            "the policy the caller chose has to actually decide something"
        );
    }
}
