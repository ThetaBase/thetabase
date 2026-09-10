//! Signed policy delivery (`07-agent-safety-layer.md` §7).
//!
//! A project's safety policy decides what the Safety Layer lets through, so who
//! may write it is a security question, not a configuration one. The answer here
//! is structural rather than a permission check: **the policy reaches an
//! instance only as bytes signed with the project's key**, and the only holder
//! of that private key is the Control Plane, which writes the policy on behalf
//! of a verified org member.
//!
//! An agent session token cannot produce that signature. So there is no request
//! an agent can make — well-formed, malformed, or ingenious — that raises its own
//! ceiling, in the same way that there is no code path in `thetad` that accepts
//! a query spanning two projects (`04-threat-model-security.md` §3). A
//! permission bit could be misread; a missing private key cannot.
//!
//! This is the same mechanism revocation lists use, deliberately: one signed
//! push channel, verified with the public keyset the instance already holds, and
//! no HTTP client on the instance side — which is also what keeps the no-LLM
//! hot-path dependency guard intact.

use serde::{Deserialize, Serialize};
use theta_identity::keys::{KeyId, ProjectKeys, PublicKeyset};

use crate::policy::SafetyPolicy;

/// A policy plus the version that orders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionedPolicy {
    /// Monotonic. An instance ignores a version it has already passed, so a
    /// replayed push cannot reinstate a policy an owner has since tightened.
    pub version: u64,
    pub project_id: String,
    pub policy: SafetyPolicy,
    pub issued_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignedPolicy {
    /// Canonical JSON of the policy, signed as-is. The signature covers these
    /// exact bytes rather than a re-serialization, for the same reason a token
    /// signature does.
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
    pub key_id: KeyId,
}

impl SignedPolicy {
    /// Sign a policy. Control Plane only — this needs a private key.
    pub fn sign(keys: &ProjectKeys, policy: &VersionedPolicy) -> Self {
        use ed25519_dalek::Signer;

        let payload = serde_json::to_vec(policy).expect("a policy is serializable");
        let signature = keys.active_signing_key().sign(&payload);

        Self {
            payload,
            signature: signature.to_bytes().to_vec(),
            key_id: keys.active.clone(),
        }
    }

    /// Verify and decode, using the keys the instance already holds.
    pub fn verify(&self, keys: &PublicKeyset) -> Result<VersionedPolicy, PolicyError> {
        use ed25519_dalek::Verifier;

        let key = keys
            .verifying_key(&self.key_id)
            .ok_or(PolicyError::BadSignature)?;
        let signature = ed25519_dalek::Signature::from_slice(&self.signature)
            .map_err(|_| PolicyError::BadSignature)?;

        key.verify(&self.payload, &signature)
            .map_err(|_| PolicyError::BadSignature)?;

        serde_json::from_slice(&self.payload).map_err(|e| PolicyError::BadPayload {
            detail: e.to_string(),
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PolicyError {
    #[error("policy signature is not valid for this project")]
    BadSignature,

    #[error("policy payload is not valid: {detail}")]
    BadPayload { detail: String },

    #[error("policy is for project `{found}`, but this instance serves `{expected}`")]
    WrongProject { found: String, expected: String },
}

/// An instance's view of its policy and where that came from.
#[derive(Debug, Clone)]
pub struct PolicySync {
    policy: SafetyPolicy,
    version: u64,
}

impl PolicySync {
    /// Start from the policy this instance was configured with.
    ///
    /// Version zero: any signed push supersedes it. A locally configured policy
    /// is a starting point, not an authority.
    pub fn new(policy: SafetyPolicy) -> Self {
        Self { policy, version: 0 }
    }

    pub fn policy(&self) -> &SafetyPolicy {
        &self.policy
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Accept a verified policy if it is newer than what is held.
    ///
    /// Returns whether it was applied. An older or equal version is ignored
    /// rather than rejected: a Control Plane re-pushing what an instance already
    /// has is normal, and treating it as an error would make a harmless retry
    /// look like an attack.
    pub fn apply(
        &mut self,
        incoming: VersionedPolicy,
        expected_project: &str,
    ) -> Result<bool, PolicyError> {
        // Cross-project isolation, checked rather than assumed: a policy signed
        // for another project must not land here even if it somehow verifies.
        if incoming.project_id != expected_project {
            return Err(PolicyError::WrongProject {
                found: incoming.project_id,
                expected: expected_project.to_string(),
            });
        }

        if incoming.version <= self.version {
            return Ok(false);
        }
        self.version = incoming.version;
        self.policy = incoming.policy;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> ProjectKeys {
        ProjectKeys::generate("project-a")
    }

    fn versioned(version: u64, threshold: u64) -> VersionedPolicy {
        VersionedPolicy {
            version,
            project_id: "project-a".into(),
            policy: SafetyPolicy {
                row_impact_threshold: threshold,
                ..SafetyPolicy::protected()
            },
            issued_at_ms: 0,
        }
    }

    #[test]
    fn a_policy_signed_by_the_project_key_verifies() {
        let keys = keys();
        let signed = SignedPolicy::sign(&keys, &versioned(1, 500));
        let decoded = signed.verify(&keys.public_keyset()).expect("verifies");
        assert_eq!(decoded.policy.row_impact_threshold, 500);
    }

    #[test]
    fn a_policy_signed_by_another_projects_key_does_not_verify() {
        // The whole security case: without the project's private key there is
        // no way to author a policy this instance will accept.
        let other = ProjectKeys::generate("project-b");
        let signed = SignedPolicy::sign(&other, &versioned(1, 1_000_000));

        assert_eq!(
            signed.verify(&keys().public_keyset()),
            Err(PolicyError::BadSignature)
        );
    }

    #[test]
    fn a_tampered_policy_does_not_verify() {
        let keys = keys();
        let mut signed = SignedPolicy::sign(&keys, &versioned(1, 100));

        // Raise the ceiling in the payload without re-signing — what an
        // attacker who intercepted the push would try.
        let mut policy: VersionedPolicy = serde_json::from_slice(&signed.payload).expect("decode");
        policy.policy.row_impact_threshold = 10_000_000;
        signed.payload = serde_json::to_vec(&policy).expect("encode");

        assert_eq!(
            signed.verify(&keys.public_keyset()),
            Err(PolicyError::BadSignature)
        );
    }

    #[test]
    fn a_newer_policy_supersedes_the_one_in_force() {
        let mut sync = PolicySync::new(SafetyPolicy::protected());
        assert!(sync.apply(versioned(1, 5_000), "project-a").expect("apply"));
        assert_eq!(sync.policy().row_impact_threshold, 5_000);
        assert_eq!(sync.version(), 1);
    }

    #[test]
    fn a_replayed_older_policy_cannot_reinstate_a_loosened_ceiling() {
        // The attack this version counter exists for: capture a push from when
        // the policy was loose, replay it after the owner tightened things.
        let mut sync = PolicySync::new(SafetyPolicy::protected());
        sync.apply(versioned(1, 1_000_000), "project-a")
            .expect("v1");
        sync.apply(versioned(2, 100), "project-a").expect("v2");

        assert!(!sync
            .apply(versioned(1, 1_000_000), "project-a")
            .expect("replay is ignored, not an error"));
        assert_eq!(
            sync.policy().row_impact_threshold,
            100,
            "a replayed push reinstated the loose ceiling"
        );
    }

    #[test]
    fn a_policy_for_another_project_is_refused_even_when_it_verifies() {
        let mut sync = PolicySync::new(SafetyPolicy::protected());
        let mut foreign = versioned(1, 999_999);
        foreign.project_id = "project-b".into();

        assert!(matches!(
            sync.apply(foreign, "project-a"),
            Err(PolicyError::WrongProject { .. })
        ));
        assert_eq!(sync.version(), 0);
    }

    #[test]
    fn an_unconfigured_instance_starts_at_version_zero_so_any_push_supersedes_it() {
        let sync = PolicySync::new(SafetyPolicy::development());
        assert_eq!(sync.version(), 0);
    }
}
