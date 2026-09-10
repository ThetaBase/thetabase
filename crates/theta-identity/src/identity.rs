//! The long-lived user identity token.
//!
//! Minted once at login and scoped to the *user*, never to a project
//! (`04-threat-model-security.md` §2). It is what a caller presents to
//! `resolveContext` to get a short-lived, project-scoped session token.
//!
//! # Why the Control Plane does not trust the client
//!
//! The CLI completes an OAuth flow and ends up holding a provider access token.
//! It could simply tell the Control Plane "I am alice@bigcorp.com" — and a
//! Control Plane that believed it would have no authentication at all, since
//! anyone can send that string.
//!
//! So the CLI sends the *provider's* access token, and the Control Plane calls
//! the provider itself to find out whose it is. The identity is then something
//! the provider asserted to the Control Plane directly, not something the
//! client claimed.
//!
//! # Why this is the highest-value credential
//!
//! It is long-lived and covers every org the user belongs to, which makes it a
//! far richer target than any single session token
//! (`04-threat-model-security.md` §4). It is signed with a Control Plane key
//! distinct from any project key, so a leaked project key cannot mint one.

use serde::{Deserialize, Serialize};

use crate::keys::{KeyId, ProjectKeys, PublicKeyset};
use crate::oauth::{ProviderKind, VerifiedOrg};
use crate::token::TokenError;

/// Default identity token lifetime.
///
/// Thirty days: long enough that "one login, ever" is true in practice, short
/// enough that a device lost and forgotten stops working within a month.
pub const DEFAULT_IDENTITY_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What an identity token asserts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityScope {
    pub token_id: String,
    /// Namespaced by provider, so two providers' user `123` are different
    /// people.
    pub user_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub provider: ProviderKind,
    /// Orgs the provider vouched for at login. Carried in the token so
    /// resolution does not have to call the provider on every request — and
    /// re-established at each login, so leaving a company takes effect on the
    /// user's next login or immediately via revocation.
    pub orgs: Vec<VerifiedOrg>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub key_id: KeyId,
}

impl IdentityScope {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }

    /// Whether this user is a verified member of `org_id`.
    ///
    /// The only place an org membership is decided. A caller naming an org it
    /// is not in gets nothing, because the answer comes from the token rather
    /// than from the request.
    pub fn is_member_of(&self, org_id: &str) -> bool {
        self.orgs.iter().any(|org| org.id == org_id)
    }
}

/// A signed identity token.
///
/// Reuses the session token's format and signing, so there is one signature
/// implementation to get right rather than two. The distinction is the key it
/// is signed with and the scope it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityToken {
    encoded: String,
}

/// The Control Plane's own signing identity.
///
/// A distinct key from any project's, so a leaked project key cannot mint an
/// identity token — and an identity token cannot be presented to a `thetad`
/// instance as if it were a session token, because no instance holds this key.
pub const CONTROL_PLANE_KEY_OWNER: &str = "control-plane";

impl IdentityToken {
    pub fn mint(keys: &ProjectKeys, scope: &IdentityScope) -> Self {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use ed25519_dalek::Signer;

        let payload = serde_json::to_vec(scope).expect("an identity scope is serializable");
        let signature = keys.active_signing_key().sign(&payload);

        Self {
            encoded: format!(
                "vi1.{}.{}",
                URL_SAFE_NO_PAD.encode(&payload),
                URL_SAFE_NO_PAD.encode(signature.to_bytes())
            ),
        }
    }

    pub fn from_encoded(encoded: impl Into<String>) -> Self {
        Self {
            encoded: encoded.into(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn into_string(self) -> String {
        self.encoded
    }

    /// Verify against the Control Plane's public keys.
    pub fn verify(&self, keys: &PublicKeyset, now_ms: i64) -> Result<IdentityScope, TokenError> {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use ed25519_dalek::Verifier;

        let mut parts = self.encoded.split('.');
        let version = parts.next().ok_or(TokenError::Malformed)?;
        let payload = parts.next().ok_or(TokenError::Malformed)?;
        let signature = parts.next().ok_or(TokenError::Malformed)?;
        if parts.next().is_some() {
            return Err(TokenError::Malformed);
        }

        // A distinct prefix from a session token's `v1`. Presenting one where
        // the other is expected is refused by format rather than by luck.
        if version != "vi1" {
            return Err(TokenError::UnsupportedFormat {
                found: version.to_string(),
            });
        }

        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| TokenError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| TokenError::Malformed)?;

        let scope: IdentityScope =
            serde_json::from_slice(&payload).map_err(|e| TokenError::BadPayload {
                detail: e.to_string(),
            })?;

        let key = keys
            .verifying_key(&scope.key_id)
            .ok_or(TokenError::BadSignature)?;
        let signature = ed25519_dalek::Signature::from_slice(&signature)
            .map_err(|_| TokenError::BadSignature)?;

        key.verify(&payload, &signature)
            .map_err(|_| TokenError::BadSignature)?;

        if scope.is_expired(now_ms) {
            return Err(TokenError::Expired {
                expires_at_ms: scope.expires_at_ms,
            });
        }

        Ok(scope)
    }
}

impl std::fmt::Display for IdentityToken {
    /// Redacted. This is the highest-value credential in the system, and the
    /// most common way one reaches a log is a `{}` in an error message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<identity token redacted>")
    }
}

#[cfg(test)]
mod tests {
    use crate::oauth::OrgSource;
    use crate::{SessionToken, TokenScope};

    use super::*;

    fn scope(user: &str, orgs: &[&str]) -> IdentityScope {
        IdentityScope {
            token_id: "id_1".into(),
            user_id: user.into(),
            email: Some("alice@bigcorp.com".into()),
            display_name: Some("Alice".into()),
            provider: ProviderKind::Google,
            orgs: orgs
                .iter()
                .map(|id| VerifiedOrg {
                    id: (*id).into(),
                    name: (*id).into(),
                    source: OrgSource::GoogleWorkspace,
                })
                .collect(),
            issued_at_ms: 0,
            expires_at_ms: 60_000,
            key_id: KeyId::new("control-plane-k1"),
        }
    }

    fn keys() -> ProjectKeys {
        ProjectKeys::generate("control-plane")
    }

    #[test]
    fn an_identity_token_verifies_and_carries_its_orgs() {
        let keys = keys();
        let token = IdentityToken::mint(&keys, &scope("google:1", &["workspace:bigcorp.com"]));

        let verified = token.verify(&keys.public_keyset(), 0).expect("verifies");
        assert_eq!(verified.user_id, "google:1");
        assert!(verified.is_member_of("workspace:bigcorp.com"));
    }

    #[test]
    fn membership_is_decided_by_the_token_not_by_the_request() {
        // A caller naming an org it is not in gets nothing, because the answer
        // comes from the signed token.
        let keys = keys();
        let token = IdentityToken::mint(&keys, &scope("google:1", &["workspace:mine.com"]));
        let verified = token.verify(&keys.public_keyset(), 0).expect("verifies");

        assert!(!verified.is_member_of("workspace:someone-elses.com"));
    }

    #[test]
    fn editing_the_orgs_invalidates_the_token() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let keys = keys();
        let token = IdentityToken::mint(&keys, &scope("google:1", &["workspace:mine.com"]));

        // Adding an org to the payload is the attack this signature prevents.
        let mut forged = scope("google:1", &["workspace:mine.com", "workspace:bigcorp.com"]);
        forged.token_id = "id_1".into();
        let payload = serde_json::to_vec(&forged).expect("encode");

        let parts: Vec<&str> = token.as_str().split('.').collect();
        let tampered = IdentityToken::from_encoded(format!(
            "{}.{}.{}",
            parts[0],
            URL_SAFE_NO_PAD.encode(&payload),
            parts[2]
        ));

        assert_eq!(
            tampered.verify(&keys.public_keyset(), 0),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_session_token_cannot_be_presented_as_an_identity_token() {
        // Distinct format prefixes, so the confusion is refused by format
        // rather than caught by luck.
        let keys = keys();
        let session = SessionToken::mint(
            &keys,
            &TokenScope {
                token_id: "tok_1".into(),
                project_id: "control-plane".into(),
                environment: "dev".into(),
                session_id: "s".into(),
                user_id: "u".into(),
                org_id: "o".into(),
                issued_at_ms: 0,
                expires_at_ms: 60_000,
                key_id: keys.active.clone(),
                // This session does not sign its writes.
                signing_key: None,
            },
        );

        let as_identity = IdentityToken::from_encoded(session.as_str());
        assert!(matches!(
            as_identity.verify(&keys.public_keyset(), 0),
            Err(TokenError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn an_identity_token_cannot_be_presented_as_a_session_token() {
        let keys = keys();
        let identity = IdentityToken::mint(&keys, &scope("google:1", &[]));

        let as_session = SessionToken::from_encoded(identity.as_str());
        assert!(matches!(
            as_session.verify(&keys.public_keyset(), 0),
            Err(TokenError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn a_token_signed_by_a_project_key_is_not_an_identity_token() {
        // A leaked project key must not be able to mint one.
        let control_plane = keys();
        let project = ProjectKeys::generate("some-project");

        let token = IdentityToken::mint(&project, &scope("google:1", &[]));
        assert_eq!(
            token.verify(&control_plane.public_keyset(), 0),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn an_expired_identity_token_is_refused() {
        let keys = keys();
        let token = IdentityToken::mint(&keys, &scope("google:1", &[]));
        assert!(token.verify(&keys.public_keyset(), 59_999).is_ok());
        assert!(matches!(
            token.verify(&keys.public_keyset(), 60_000),
            Err(TokenError::Expired { .. })
        ));
    }

    #[test]
    fn an_identity_token_never_renders_itself_into_a_log_line() {
        let token = IdentityToken::mint(&keys(), &scope("google:1", &[]));
        assert_eq!(format!("{token}"), "<identity token redacted>");
    }

    #[test]
    fn malformed_identity_tokens_are_refused_without_panicking() {
        let keys = keys();
        for bad in ["", "vi1", "vi1.", "vi1.a.b.c", "vi1.!!!.!!!", "nonsense"] {
            assert!(IdentityToken::from_encoded(bad)
                .verify(&keys.public_keyset(), 0)
                .is_err());
        }
    }
}
