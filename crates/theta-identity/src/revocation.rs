//! Revocation.
//!
//! `04-threat-model-security.md` §2 requires that a leaked token be revocable
//! *instantly*, propagating to `thetad` within one heartbeat (target: under
//! five seconds). A short token lifetime alone does not meet that: a token with
//! an hour to live is useful to an attacker for an hour.
//!
//! So revocation is a list, pushed rather than polled for by each request, and
//! `thetad` refuses anything on it regardless of how valid the signature is.
//!
//! # Fail closed
//!
//! If a `thetad` instance stops hearing from the Control Plane, its revocation
//! list goes stale — it would keep honouring a token that has since been
//! revoked. [`RevocationSync::is_stale`] makes that visible, and the instance
//! treats a sufficiently stale list as a reason to refuse rather than a reason
//! to carry on. An instance that cannot learn about revocations is an instance
//! that cannot safely authorize.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Tokens, sessions, and users whose access has been withdrawn.
///
/// Three granularities because the reasons differ: one leaked token, one
/// compromised agent session, or a user who left the company.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationList {
    /// Monotonic; lets an instance tell whether an update is newer than what it
    /// already has, so an out-of-order delivery cannot roll the list back.
    pub version: u64,
    pub tokens: BTreeSet<String>,
    pub sessions: BTreeSet<String>,
    pub users: BTreeSet<String>,
    /// Orgs whose membership was revoked externally. Revoking an org withdraws
    /// every token for every one of its projects without having to enumerate
    /// them (`06-provisioning-identity-flow.md` §6).
    pub orgs: BTreeSet<String>,
}

impl RevocationList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revoke_token(&mut self, token_id: impl Into<String>) {
        self.tokens.insert(token_id.into());
        self.version += 1;
    }

    pub fn revoke_session(&mut self, session_id: impl Into<String>) {
        self.sessions.insert(session_id.into());
        self.version += 1;
    }

    pub fn revoke_user(&mut self, user_id: impl Into<String>) {
        self.users.insert(user_id.into());
        self.version += 1;
    }

    pub fn revoke_org(&mut self, org_id: impl Into<String>) {
        self.orgs.insert(org_id.into());
        self.version += 1;
    }

    /// Whether this scope has been withdrawn by any of the four routes.
    pub fn covers(&self, scope: &crate::TokenScope) -> bool {
        self.tokens.contains(&scope.token_id)
            || self.sessions.contains(&scope.session_id)
            || self.users.contains(&scope.user_id)
            || self.orgs.contains(&scope.org_id)
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
            && self.sessions.is_empty()
            && self.users.is_empty()
            && self.orgs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tokens.len() + self.sessions.len() + self.users.len() + self.orgs.len()
    }

    /// Drop entries for tokens that have expired anyway.
    ///
    /// Without this the list grows forever. An entry is only safe to drop once
    /// no token carrying it could still be valid, so the caller passes the
    /// cutoff — the oldest issue time any live token could have.
    pub fn compact(&mut self, still_relevant: impl Fn(&str) -> bool) {
        self.tokens.retain(|id| still_relevant(id));
    }
}

/// A revocation list signed by the project it applies to.
///
/// The Control Plane *pushes* revocations to instances rather than instances
/// polling for them, and this is what makes that safe without inventing a
/// second authentication mechanism: the list is signed with the same project
/// key that signs tokens, and verified with the same public key the instance
/// already holds.
///
/// Two consequences worth stating. An instance needs no HTTP client, which
/// keeps the no-LLM dependency guard on the hot-path crates intact. And an
/// instance cannot forge a revocation list either — it holds no signing key —
/// so a compromised storage node cannot un-revoke a credential by fabricating
/// an older list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedRevocationList {
    /// Canonical JSON of the list, signed as-is. The signature covers these
    /// exact bytes rather than a re-serialization, for the same reason a token
    /// signature does.
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
    pub key_id: crate::keys::KeyId,
}

impl SignedRevocationList {
    /// Sign a list. Control Plane only — this needs a private key.
    pub fn sign(keys: &crate::keys::ProjectKeys, list: &RevocationList) -> Self {
        use ed25519_dalek::Signer;

        let payload = serde_json::to_vec(list).expect("a revocation list is serializable");
        let signature = keys.active_signing_key().sign(&payload);

        Self {
            payload,
            signature: signature.to_bytes().to_vec(),
            key_id: keys.active.clone(),
        }
    }

    /// Verify and decode, using the keys the instance already holds.
    pub fn verify(
        &self,
        keys: &crate::keys::PublicKeyset,
    ) -> Result<RevocationList, RevocationError> {
        use ed25519_dalek::Verifier;

        let key = keys
            .verifying_key(&self.key_id)
            .ok_or(RevocationError::BadSignature)?;
        let signature = ed25519_dalek::Signature::from_slice(&self.signature)
            .map_err(|_| RevocationError::BadSignature)?;

        key.verify(&self.payload, &signature)
            .map_err(|_| RevocationError::BadSignature)?;

        serde_json::from_slice(&self.payload).map_err(|e| RevocationError::BadPayload {
            detail: e.to_string(),
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RevocationError {
    #[error("revocation list signature is not valid for this project")]
    BadSignature,

    #[error("revocation list payload is not valid: {detail}")]
    BadPayload { detail: String },
}

/// A `thetad` instance's view of revocation state.
///
/// Holds the list plus when it was last confirmed fresh, because those two
/// facts are only useful together: a list is trustworthy exactly as long as the
/// heartbeat that maintains it is alive.
#[derive(Debug, Clone)]
pub struct RevocationSync {
    list: RevocationList,
    /// `None` until the first successful sync. A freshly started instance has
    /// an *empty* revocation list, which is not the same as an accurate one:
    /// serving on it would honour every token revoked before startup. So an
    /// instance that has never synced authorizes nothing.
    last_synced_ms: Option<i64>,
    /// How long a list may go unrefreshed before it is treated as unreliable.
    /// Several heartbeats, so one dropped packet is not an outage, but far
    /// short of a token's lifetime.
    staleness_limit_ms: i64,
}

impl RevocationSync {
    /// A sync that has not yet heard from the Control Plane, and therefore
    /// authorizes nothing.
    pub fn awaiting_first_sync(staleness_limit_ms: i64) -> Self {
        Self {
            list: RevocationList::new(),
            last_synced_ms: None,
            staleness_limit_ms,
        }
    }

    /// A sync already known fresh as of `now_ms`.
    ///
    /// For tests and for an instance handed a list at startup. Production goes
    /// through [`RevocationSync::awaiting_first_sync`] and earns freshness from
    /// a real heartbeat.
    pub fn new(staleness_limit_ms: i64, now_ms: i64) -> Self {
        Self {
            list: RevocationList::new(),
            last_synced_ms: Some(now_ms),
            staleness_limit_ms,
        }
    }

    /// Whether this instance has ever heard from the Control Plane.
    pub fn has_synced(&self) -> bool {
        self.last_synced_ms.is_some()
    }

    /// Accept an update from the Control Plane.
    ///
    /// An older version is ignored rather than applied: revocations only ever
    /// accumulate, and applying a stale list would *un*-revoke a token.
    pub fn apply(&mut self, update: RevocationList, now_ms: i64) -> bool {
        if update.version < self.list.version {
            tracing_stale(update.version, self.list.version);
            // Still counts as a heartbeat — the Control Plane is reachable,
            // which is the thing staleness is really tracking.
            self.last_synced_ms = Some(now_ms);
            return false;
        }
        self.list = update;
        self.last_synced_ms = Some(now_ms);
        true
    }

    /// Record that the Control Plane answered, even if nothing changed.
    pub fn heartbeat(&mut self, now_ms: i64) {
        self.last_synced_ms = Some(now_ms);
    }

    pub fn list(&self) -> &RevocationList {
        &self.list
    }

    pub fn version(&self) -> u64 {
        self.list.version
    }

    /// How long since the last sync. `i64::MAX` when there has never been one,
    /// because "never" is older than any limit.
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        match self.last_synced_ms {
            Some(at) => (now_ms - at).max(0),
            None => i64::MAX,
        }
    }

    /// Whether the list is too old — or too absent — to rely on.
    pub fn is_stale(&self, now_ms: i64) -> bool {
        self.age_ms(now_ms) > self.staleness_limit_ms
    }

    /// Whether this scope may proceed.
    ///
    /// Fails closed when the list is stale *or* has never been synced: an
    /// instance that cannot learn about revocations cannot safely authorize.
    /// Continuing to serve is how a revoked token stays useful for as long as a
    /// partition lasts — and how one revoked before startup stays useful
    /// forever.
    pub fn check(&self, scope: &crate::TokenScope, now_ms: i64) -> RevocationDecision {
        if self.list.covers(scope) {
            return RevocationDecision::Revoked;
        }
        if self.is_stale(now_ms) {
            return RevocationDecision::Stale {
                age_ms: self.age_ms(now_ms),
                limit_ms: self.staleness_limit_ms,
            };
        }
        RevocationDecision::Allowed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationDecision {
    Allowed,
    Revoked,
    /// The list is too old to rely on. Distinct from `Revoked` because the
    /// operator response differs: this is an infrastructure problem, not a
    /// security event.
    Stale {
        age_ms: i64,
        limit_ms: i64,
    },
}

impl RevocationDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, RevocationDecision::Allowed)
    }
}

fn tracing_stale(received: u64, held: u64) {
    // Kept as a plain eprintln rather than a tracing dependency: this crate is
    // in `thetad`'s hot-path closure, and the no-LLM guard is easier to reason
    // about the fewer dependencies live there.
    if cfg!(debug_assertions) {
        eprintln!("ignoring revocation list v{received}, already holding v{held}");
    }
}

#[cfg(test)]
mod tests {
    use crate::keys::KeyId;
    use crate::TokenScope;

    use super::*;

    fn scope() -> TokenScope {
        TokenScope {
            token_id: "tok_1".into(),
            project_id: "p".into(),
            environment: "dev".into(),
            session_id: "sess_1".into(),
            user_id: "u_1".into(),
            org_id: "org_1".into(),
            issued_at_ms: 0,
            expires_at_ms: 3_600_000,
            key_id: KeyId::new("p-k1"),
            // This session does not sign its writes.
            signing_key: None,
        }
    }

    #[test]
    fn revoking_any_of_the_four_granularities_covers_the_scope() {
        for (label, mut list) in [
            ("token", {
                let mut l = RevocationList::new();
                l.revoke_token("tok_1");
                l
            }),
            ("session", {
                let mut l = RevocationList::new();
                l.revoke_session("sess_1");
                l
            }),
            ("user", {
                let mut l = RevocationList::new();
                l.revoke_user("u_1");
                l
            }),
            ("org", {
                let mut l = RevocationList::new();
                l.revoke_org("org_1");
                l
            }),
        ] {
            assert!(
                list.covers(&scope()),
                "revoking by {label} did not cover the token"
            );
            list.version = 0;
        }
    }

    #[test]
    fn an_unrelated_revocation_does_not_cover_the_scope() {
        let mut list = RevocationList::new();
        list.revoke_token("tok_someone_else");
        list.revoke_user("u_someone_else");
        assert!(!list.covers(&scope()));
    }

    #[test]
    fn revoking_an_org_withdraws_its_tokens_without_enumerating_projects() {
        // The membership-revoked path: a user leaves a company, and every token
        // for every project of that org has to stop working.
        let mut list = RevocationList::new();
        list.revoke_org("org_1");

        let mut other_project = scope();
        other_project.project_id = "a-different-project".into();
        other_project.token_id = "tok_2".into();

        assert!(list.covers(&other_project));
    }

    #[test]
    fn a_revoked_token_is_refused_however_valid_its_signature() {
        let mut sync = RevocationSync::new(15_000, 0);
        let mut list = RevocationList::new();
        list.revoke_token("tok_1");
        sync.apply(list, 0);

        assert_eq!(sync.check(&scope(), 0), RevocationDecision::Revoked);
    }

    #[test]
    fn a_fresh_list_allows_an_unrevoked_token() {
        let sync = RevocationSync::new(15_000, 0);
        assert_eq!(sync.check(&scope(), 5_000), RevocationDecision::Allowed);
    }

    #[test]
    fn a_stale_list_fails_closed_rather_than_carrying_on() {
        // An instance that cannot learn about revocations cannot safely
        // authorize; continuing to serve is how a revoked token stays useful
        // for as long as a partition lasts.
        let sync = RevocationSync::new(15_000, 0);
        assert!(matches!(
            sync.check(&scope(), 15_001),
            RevocationDecision::Stale { .. }
        ));
    }

    #[test]
    fn a_signed_list_verifies_against_the_project_it_applies_to() {
        let keys = crate::keys::ProjectKeys::generate("p");
        let mut list = RevocationList::new();
        list.revoke_token("tok_1");

        let signed = SignedRevocationList::sign(&keys, &list);
        assert_eq!(signed.verify(&keys.public_keyset()), Ok(list));
    }

    #[test]
    fn a_list_signed_for_another_project_is_refused() {
        // Otherwise one project's Control Plane key could push revocations —
        // or an empty list — to every other project's instances.
        let a = crate::keys::ProjectKeys::generate("project-a");
        let b = crate::keys::ProjectKeys::generate("project-b");

        let signed = SignedRevocationList::sign(&a, &RevocationList::new());
        assert_eq!(
            signed.verify(&b.public_keyset()),
            Err(RevocationError::BadSignature)
        );
    }

    #[test]
    fn editing_a_signed_list_invalidates_it() {
        let keys = crate::keys::ProjectKeys::generate("p");
        let mut list = RevocationList::new();
        list.revoke_token("tok_1");

        let mut signed = SignedRevocationList::sign(&keys, &list);
        // An attacker in the middle removing a revocation is exactly the attack
        // the signature exists to stop.
        signed.payload = serde_json::to_vec(&RevocationList::new()).expect("encode");

        assert_eq!(
            signed.verify(&keys.public_keyset()),
            Err(RevocationError::BadSignature)
        );
    }

    #[test]
    fn an_instance_that_has_never_synced_authorizes_nothing() {
        // An empty revocation list is not the same as an accurate one. Serving
        // on it would honour every token revoked before this process started.
        let sync = RevocationSync::awaiting_first_sync(15_000);
        assert!(!sync.has_synced());
        assert!(matches!(
            sync.check(&scope(), 0),
            RevocationDecision::Stale { .. }
        ));
    }

    #[test]
    fn the_first_sync_is_what_lets_an_instance_start_serving() {
        let mut sync = RevocationSync::awaiting_first_sync(15_000);
        assert!(!sync.check(&scope(), 0).is_allowed());

        sync.apply(RevocationList::new(), 1_000);
        assert!(sync.has_synced());
        assert!(sync.check(&scope(), 1_000).is_allowed());
    }

    #[test]
    fn a_heartbeat_refreshes_the_list_even_when_nothing_changed() {
        let mut sync = RevocationSync::new(15_000, 0);
        assert!(sync.is_stale(20_000));
        sync.heartbeat(20_000);
        assert!(!sync.is_stale(20_001));
    }

    #[test]
    fn an_older_list_is_ignored_so_a_revocation_cannot_be_undone() {
        let mut sync = RevocationSync::new(15_000, 0);

        let mut current = RevocationList::new();
        current.revoke_token("tok_1");
        current.version = 5;
        assert!(sync.apply(current, 0));

        // An out-of-order delivery of an older list would otherwise
        // *un*-revoke the token.
        let mut stale = RevocationList::new();
        stale.version = 3;
        assert!(!sync.apply(stale, 1_000));

        assert_eq!(sync.check(&scope(), 1_000), RevocationDecision::Revoked);
        assert_eq!(sync.version(), 5);
    }

    #[test]
    fn an_out_of_order_delivery_still_counts_as_a_heartbeat() {
        // The Control Plane answered, which is what staleness actually tracks.
        let mut sync = RevocationSync::new(15_000, 0);
        let mut current = RevocationList::new();
        current.version = 5;
        sync.apply(current, 0);

        let mut stale = RevocationList::new();
        stale.version = 1;
        sync.apply(stale, 14_000);

        assert!(!sync.is_stale(14_001));
    }

    #[test]
    fn the_version_advances_on_every_revocation() {
        let mut list = RevocationList::new();
        assert_eq!(list.version, 0);
        list.revoke_token("a");
        list.revoke_session("b");
        assert_eq!(
            list.version, 2,
            "an instance tells updates apart by version"
        );
    }

    #[test]
    fn compaction_drops_only_entries_that_can_no_longer_matter() {
        let mut list = RevocationList::new();
        list.revoke_token("live");
        list.revoke_token("expired");
        list.revoke_user("u_1");

        list.compact(|id| id != "expired");

        assert!(list.tokens.contains("live"));
        assert!(!list.tokens.contains("expired"));
        // User and org revocations outlive any single token, so compaction must
        // not touch them.
        assert!(list.users.contains("u_1"));
    }
}
