//! Session tokens as `thetad` sees them.
//!
//! An instance holds only its own project's **public** keys, so it can verify
//! tokens and cannot mint them. A compromised storage node therefore cannot
//! issue credentials — for itself or for anyone else — and a token minted for
//! another project fails verification here rather than merely failing a scope
//! check (`04-threat-model-security.md` §2).
//!
//! The format, signing and revocation logic live in `theta-identity`, shared
//! with the Control Plane so the two cannot drift.

use theta_identity::revocation::{RevocationDecision, RevocationSync};
use theta_identity::{PublicKeyset, SessionToken, TokenError, TokenScope};
use thiserror::Error;

use crate::config::Environment;

#[derive(Debug, Error, PartialEq)]
pub enum AuthError {
    #[error("token is not valid: {0}")]
    Token(#[from] TokenError),

    #[error(
        "token is scoped to the {token_env} environment, this instance serves {instance_env:?}"
    )]
    WrongEnvironment {
        token_env: String,
        instance_env: Environment,
    },

    #[error("token was revoked")]
    Revoked,

    /// Distinct from `Revoked`: the instance cannot currently tell whether the
    /// token was revoked. The operator response differs — this is an
    /// infrastructure problem, not a security event.
    #[error(
        "revocation state is {age_ms}ms old, past the {limit_ms}ms limit; refusing to authorize"
    )]
    RevocationStale { age_ms: i64, limit_ms: i64 },
}

/// Everything an instance needs to authorize a request.
#[derive(Debug)]
pub struct Authorizer {
    keys: PublicKeyset,
    environment: Environment,
    revocations: RevocationSync,
}

impl Authorizer {
    pub fn new(
        keys: PublicKeyset,
        environment: Environment,
        staleness_limit_ms: i64,
        now_ms: i64,
    ) -> Self {
        Self {
            keys,
            environment,
            revocations: RevocationSync::new(staleness_limit_ms, now_ms),
        }
    }

    /// An authorizer that refuses everything until its first real sync.
    ///
    /// **The production constructor.** [`Authorizer::new`] marks an *empty*
    /// revocation list as fresh as of startup, which is right for a test or for
    /// an instance handed a list, and wrong for a process that has just come up:
    /// for the whole staleness window it would answer `Allowed` for every token,
    /// including one revoked while it was down. `RevocationSync::new`'s own doc
    /// already said production goes through `awaiting_first_sync`; nothing did.
    ///
    /// Found by an external review (R1-02), which noted that
    /// `an_instance_that_has_never_synced_authorizes_nothing` encoded the intent
    /// and was never wired to the path that ships.
    pub fn awaiting_first_sync(
        keys: PublicKeyset,
        environment: Environment,
        staleness_limit_ms: i64,
    ) -> Self {
        Self {
            keys,
            environment,
            revocations: RevocationSync::awaiting_first_sync(staleness_limit_ms),
        }
    }

    pub fn project_id(&self) -> &str {
        &self.keys.project_id
    }

    pub fn revocations_mut(&mut self) -> &mut RevocationSync {
        &mut self.revocations
    }

    pub fn revocations(&self) -> &RevocationSync {
        &self.revocations
    }

    /// Verify a pushed revocation list against this project's keys.
    ///
    /// The same keys that verify tokens, so a list signed for another project —
    /// or edited in transit to drop an entry — is refused.
    pub fn verify_revocations(
        &self,
        signed: &theta_identity::SignedRevocationList,
    ) -> Result<theta_identity::RevocationList, theta_identity::RevocationError> {
        signed.verify(&self.keys)
    }

    /// Verify a token presented on the wire.
    ///
    /// Order matters: signature first, then environment, then revocation. A
    /// token that does not verify has told us nothing, so nothing in its
    /// payload should be acted on — including which environment it claims.
    pub fn authorize(&self, encoded: &str, now_ms: i64) -> Result<TokenScope, AuthError> {
        let token = SessionToken::from_encoded(encoded);
        let scope = token.verify(&self.keys, now_ms)?;

        if scope.environment != self.environment.as_str() {
            return Err(AuthError::WrongEnvironment {
                token_env: scope.environment,
                instance_env: self.environment,
            });
        }

        match self.revocations.check(&scope, now_ms) {
            RevocationDecision::Allowed => Ok(scope),
            RevocationDecision::Revoked => Err(AuthError::Revoked),
            RevocationDecision::Stale { age_ms, limit_ms } => {
                Err(AuthError::RevocationStale { age_ms, limit_ms })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use theta_identity::keys::KeyId;
    use theta_identity::{ProjectKeys, RevocationList};

    use super::*;

    fn scope(project: &str, environment: &str) -> TokenScope {
        TokenScope {
            token_id: "tok_1".into(),
            project_id: project.into(),
            environment: environment.into(),
            session_id: "sess_1".into(),
            user_id: "u_1".into(),
            org_id: "org_1".into(),
            issued_at_ms: 0,
            expires_at_ms: 3_600_000,
            key_id: KeyId::new(format!("{project}-k1")),
            // This session does not sign its writes.
            signing_key: None,
        }
    }

    fn authorizer(keys: &ProjectKeys) -> Authorizer {
        Authorizer::new(keys.public_keyset(), Environment::Dev, 15_000, 0)
    }

    #[test]
    fn a_valid_token_authorizes() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p", "dev"));
        let verified = authorizer(&keys)
            .authorize(token.as_str(), 0)
            .expect("authorizes");
        assert_eq!(verified.session_id, "sess_1");
    }

    #[test]
    fn a_token_for_another_project_is_refused() {
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-b");
        let token = SessionToken::mint(&a, &scope("project-a", "dev"));

        assert!(authorizer(&b).authorize(token.as_str(), 0).is_err());
    }

    #[test]
    fn a_dev_token_cannot_reach_prod() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p", "dev"));
        let prod = Authorizer::new(keys.public_keyset(), Environment::Prod, 15_000, 0);

        assert!(matches!(
            prod.authorize(token.as_str(), 0),
            Err(AuthError::WrongEnvironment { .. })
        ));
    }

    #[test]
    fn a_revoked_token_is_refused_even_though_it_verifies() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p", "dev"));

        let mut auth = authorizer(&keys);
        assert!(auth.authorize(token.as_str(), 0).is_ok());

        let mut list = RevocationList::new();
        list.revoke_token("tok_1");
        auth.revocations_mut().apply(list, 0);

        assert_eq!(auth.authorize(token.as_str(), 0), Err(AuthError::Revoked));
    }

    #[test]
    fn a_stale_revocation_list_refuses_rather_than_serving_on() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p", "dev"));
        let auth = authorizer(&keys);

        // Past the staleness limit with no heartbeat: the instance can no
        // longer tell whether this token was revoked, so it stops.
        assert!(matches!(
            auth.authorize(token.as_str(), 20_000),
            Err(AuthError::RevocationStale { .. })
        ));
    }

    #[test]
    fn a_garbage_token_is_refused_without_panicking() {
        let keys = ProjectKeys::generate("p");
        let auth = authorizer(&keys);
        for bad in ["", "garbage", "v1.a.b", "v9.x.y"] {
            assert!(
                auth.authorize(bad, 0).is_err(),
                "`{bad}` should not authorize"
            );
        }
    }

    #[test]
    fn an_unverifiable_token_is_rejected_before_its_claims_are_read() {
        // A token that does not verify has told us nothing, so its claimed
        // environment must not be acted on — not even to produce a more
        // specific error.
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-b");
        let token = SessionToken::mint(&a, &scope("project-a", "prod"));

        let dev_instance = Authorizer::new(b.public_keyset(), Environment::Dev, 15_000, 0);
        assert!(
            matches!(
                dev_instance.authorize(token.as_str(), 0),
                Err(AuthError::Token(_))
            ),
            "the signature failure must come first"
        );
    }
}
