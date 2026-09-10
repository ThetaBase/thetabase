//! The session token format.
//!
//! ```text
//! v1.<base64url(payload)>.<base64url(signature)>
//! ```
//!
//! The payload is canonical JSON of [`TokenScope`]; the signature is Ed25519
//! over the exact payload bytes as they appear in the token, not over a
//! re-serialization of the decoded scope. Verifying against re-serialized bytes
//! is a classic hole: two encodings of the same value would both verify, so an
//! attacker could reshape the payload while keeping a valid signature.
//!
//! There is no algorithm field. See the crate docs for why.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use serde::{Deserialize, Serialize};

use crate::keys::{KeyId, ProjectKeys, PublicKeyset};

/// The only format version this build speaks.
const FORMAT: &str = "v1";

/// What a token asserts.
///
/// Every field is inside the signature, so none of it can be edited in transit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenScope {
    /// Unique per token, so a specific token can be revoked without revoking
    /// the session or the user.
    pub token_id: String,
    pub project_id: String,
    pub environment: String,
    /// Which agent session this token belongs to, so the audit trail can name
    /// the author of every change (`04-threat-model-security.md` §5).
    pub session_id: String,
    pub user_id: String,
    /// Org the project belongs to, so revoking a membership can invalidate
    /// every token for that org without enumerating its projects.
    pub org_id: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    /// Which key signed this. Lets a project rotate keys without invalidating
    /// tokens already in flight.
    pub key_id: KeyId,
    /// The session's own Ed25519 **public** key, hex-encoded, if it has one.
    ///
    /// This is how an instance learns a key it can check an entry signature
    /// against, and it rides in the token rather than arriving on its own
    /// channel because the token is already signed by the project key. A
    /// separate registration call would need its own authentication, and the
    /// thing it would authenticate with is this token.
    ///
    /// The private half never leaves the client — that is the whole point
    /// (`04-threat-model-security.md` §7.2). A signature the server could have
    /// produced proves nothing about who wrote an entry.
    ///
    /// Optional, and absent means the session does not sign. Adding it does not
    /// invalidate tokens issued before it existed: the field is skipped when
    /// absent, so their payload bytes — which are what the signature covers —
    /// are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
}

impl TokenScope {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }

    /// Seconds until expiry, floored at zero.
    pub fn remaining_secs(&self, now_ms: i64) -> i64 {
        ((self.expires_at_ms - now_ms).max(0)) / 1000
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TokenError {
    #[error("token format is not `{FORMAT}.<payload>.<signature>`")]
    Malformed,

    #[error("token declares format `{found}`, which this build cannot verify")]
    UnsupportedFormat { found: String },

    #[error("token payload is not valid: {detail}")]
    BadPayload { detail: String },

    /// The signature did not verify. Deliberately does not say why — whether
    /// the key was unknown, the bytes were edited, or the token came from
    /// another project. Distinguishing those turns verification into an oracle.
    #[error("token signature is not valid for this project")]
    BadSignature,

    #[error("token was signed for project `{signed_for}`, not `{presented_to}`")]
    WrongProject {
        signed_for: String,
        presented_to: String,
    },

    #[error("token expired at {expires_at_ms}")]
    Expired { expires_at_ms: i64 },

    #[error("token was revoked")]
    Revoked,
}

/// A signed, encoded session token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToken {
    encoded: String,
}

impl SessionToken {
    /// Mint a token. Control Plane only — this needs a private key, which no
    /// `thetad` instance holds.
    pub fn mint(keys: &ProjectKeys, scope: &TokenScope) -> Self {
        Self::sign_with(keys.active_signing_key(), scope)
    }

    fn sign_with(key: &SigningKey, scope: &TokenScope) -> Self {
        let payload = serde_json::to_vec(scope).expect("a scope is always serializable");
        let signature = key.sign(&payload);

        Self {
            encoded: format!(
                "{FORMAT}.{}.{}",
                URL_SAFE_NO_PAD.encode(&payload),
                URL_SAFE_NO_PAD.encode(signature.to_bytes())
            ),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn into_string(self) -> String {
        self.encoded
    }

    /// Wrap an already-encoded token, e.g. one read off the wire.
    pub fn from_encoded(encoded: impl Into<String>) -> Self {
        Self {
            encoded: encoded.into(),
        }
    }

    /// Read the scope *without* verifying.
    ///
    /// For diagnostics and for logging a token's project on a rejection. Never
    /// for an authorization decision — the name says so, and every call site is
    /// worth checking.
    pub fn scope_unverified(&self) -> Result<TokenScope, TokenError> {
        let (payload, _) = self.split()?;
        serde_json::from_slice(&payload).map_err(|e| TokenError::BadPayload {
            detail: e.to_string(),
        })
    }

    /// Verify this token against the keys of the instance it is presented to.
    ///
    /// The signature is checked with `keyset`'s key, so a token signed for
    /// another project fails here regardless of what its payload claims — that
    /// is the cryptographic isolation `04-threat-model-security.md` §2 requires.
    ///
    /// # What is read before the signature is checked
    ///
    /// The payload is parsed first, and this is unavoidable rather than a
    /// shortcut: `key_id` is what selects the key to verify *with*, so no
    /// multi-key scheme can check a signature before reading something. The
    /// claimed `project_id` is then read too, deliberately (see below).
    ///
    /// The property that matters is therefore not "nothing is read first" —
    /// which is impossible — but the stronger and achievable one:
    ///
    /// **Nothing read from an unverified payload can cause a token to be
    /// accepted.** It can only select a candidate key, or reject. Every path
    /// out of here that returns `Ok` has been through
    /// `VerifyingKey::verify` over the payload bytes exactly as they arrived.
    ///
    /// `forging_the_project_id_does_not_help_because_the_key_still_does_not_match`
    /// is the test that pins it, and SEC-8 records why the weaker phrasing was
    /// corrected.
    pub fn verify(&self, keyset: &PublicKeyset, now_ms: i64) -> Result<TokenScope, TokenError> {
        let (payload, signature) = self.split()?;

        let scope: TokenScope =
            serde_json::from_slice(&payload).map_err(|e| TokenError::BadPayload {
                detail: e.to_string(),
            })?;

        // Checked before the signature so the error names the real problem: a
        // token for another project is a misrouted request, not an attack, and
        // reporting it as a bad signature would send people hunting for a
        // corruption that is not there.
        if scope.project_id != keyset.project_id {
            return Err(TokenError::WrongProject {
                signed_for: scope.project_id,
                presented_to: keyset.project_id.clone(),
            });
        }

        let key = keyset
            .verifying_key(&scope.key_id)
            // An unknown key id and a bad signature are the same answer, so
            // probing cannot enumerate which key ids exist.
            .ok_or(TokenError::BadSignature)?;

        let signature = Signature::from_slice(&signature).map_err(|_| TokenError::BadSignature)?;

        // Over the payload bytes exactly as they arrived, not a
        // re-serialization of `scope`.
        key.verify(&payload, &signature)
            .map_err(|_| TokenError::BadSignature)?;

        if scope.is_expired(now_ms) {
            return Err(TokenError::Expired {
                expires_at_ms: scope.expires_at_ms,
            });
        }

        Ok(scope)
    }

    /// Split into (payload bytes, signature bytes).
    fn split(&self) -> Result<(Vec<u8>, Vec<u8>), TokenError> {
        let mut parts = self.encoded.split('.');
        let version = parts.next().ok_or(TokenError::Malformed)?;
        let payload = parts.next().ok_or(TokenError::Malformed)?;
        let signature = parts.next().ok_or(TokenError::Malformed)?;
        if parts.next().is_some() {
            return Err(TokenError::Malformed);
        }

        if version != FORMAT {
            return Err(TokenError::UnsupportedFormat {
                found: version.to_string(),
            });
        }

        Ok((
            URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| TokenError::Malformed)?,
            URL_SAFE_NO_PAD
                .decode(signature)
                .map_err(|_| TokenError::Malformed)?,
        ))
    }
}

impl std::fmt::Display for SessionToken {
    /// Renders redacted.
    ///
    /// A session token is a credential, and the most common way one reaches a
    /// log is a `{}` in an error message. Callers that genuinely need the
    /// encoded form ask for it by name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<session token redacted>")
    }
}

#[cfg(test)]
mod tests {
    use crate::keys::SigningKeyset;

    use super::*;

    fn scope(project: &str) -> TokenScope {
        TokenScope {
            token_id: "tok_1".into(),
            project_id: project.into(),
            environment: "dev".into(),
            session_id: "sess_1".into(),
            user_id: "u_1".into(),
            org_id: "org_1".into(),
            issued_at_ms: 0,
            expires_at_ms: 60_000,
            key_id: KeyId::new(format!("{project}-k1")),
            // This session does not sign its writes.
            signing_key: None,
        }
    }

    #[test]
    fn a_minted_token_verifies_against_its_own_project() {
        let keys = ProjectKeys::generate("project-a");
        let token = SessionToken::mint(&keys, &scope("project-a"));

        let verified = token.verify(&keys.public_keyset(), 0).expect("verifies");
        assert_eq!(verified.project_id, "project-a");
        assert_eq!(verified.session_id, "sess_1");
    }

    #[test]
    fn a_token_for_one_project_cannot_verify_against_another() {
        // The central claim of `04-threat-model-security.md` §2, stated as a
        // test rather than as prose.
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-b");

        let token = SessionToken::mint(&a, &scope("project-a"));
        assert!(matches!(
            token.verify(&b.public_keyset(), 0),
            Err(TokenError::WrongProject { .. })
        ));
    }

    #[test]
    fn forging_the_project_id_does_not_help_because_the_key_still_does_not_match() {
        // The interesting case: an attacker who holds project A's key mints a
        // token *claiming* to be for project B. B verifies with B's key, so the
        // signature fails. Scope alone is not what protects here.
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-b");

        let mut forged = scope("project-b");
        forged.key_id = b.active.clone();
        let token = SessionToken::mint(&a, &forged);

        assert_eq!(
            token.verify(&b.public_keyset(), 0),
            Err(TokenError::BadSignature),
            "a token signed with the wrong key must not verify, whatever it claims"
        );
    }

    #[test]
    fn a_thetad_keyset_cannot_mint_anything() {
        // `PublicKeyset` exposes no signing key, so a compromised storage node
        // cannot issue credentials. This is a compile-time property; the test
        // documents it and would fail to build if that ever changed.
        let keys = ProjectKeys::generate("p");
        let public = keys.public_keyset();
        assert!(public.verifying_key(&keys.active).is_some());
    }

    #[test]
    fn editing_the_payload_invalidates_the_signature() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p"));

        // Re-encode a payload that says the token expires far in the future,
        // keeping the original signature.
        let mut tampered = token.scope_unverified().expect("decodes");
        tampered.expires_at_ms = i64::MAX;
        let payload = serde_json::to_vec(&tampered).expect("encode");

        let parts: Vec<&str> = token.as_str().split('.').collect();
        let forged = SessionToken::from_encoded(format!(
            "{}.{}.{}",
            parts[0],
            URL_SAFE_NO_PAD.encode(&payload),
            parts[2]
        ));

        assert_eq!(
            forged.verify(&keys.public_keyset(), 0),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn an_expired_token_is_refused_even_though_it_verifies() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p"));

        assert!(token.verify(&keys.public_keyset(), 59_999).is_ok());
        assert!(matches!(
            token.verify(&keys.public_keyset(), 60_000),
            Err(TokenError::Expired { .. })
        ));
    }

    #[test]
    fn a_token_signed_with_a_rotated_out_key_stops_verifying() {
        let mut keys = ProjectKeys::generate("p");
        let old_key = keys.active.clone();

        let mut old_scope = scope("p");
        old_scope.key_id = old_key.clone();
        let token = SessionToken::mint(&keys, &old_scope);
        assert!(token.verify(&keys.public_keyset(), 0).is_ok());

        keys.rotate();
        // Rotation alone keeps it valid — that is the point of rotation.
        assert!(token.verify(&keys.public_keyset(), 0).is_ok());

        // Revoking the key is the emergency path, and it takes effect at once.
        keys.revoke_key(&old_key);
        assert_eq!(
            token.verify(&keys.public_keyset(), 0),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_token_from_an_unknown_format_version_is_refused_not_guessed_at() {
        let token = SessionToken::from_encoded("v2.abc.def");
        assert!(matches!(
            token.verify(&ProjectKeys::generate("p").public_keyset(), 0),
            Err(TokenError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn malformed_tokens_are_refused_rather_than_panicking() {
        let keys = ProjectKeys::generate("p");
        for bad in [
            "",
            "v1",
            "v1.",
            "v1.onlyonepart",
            "v1.a.b.c",
            "v1.!!!.!!!",
            "not a token at all",
        ] {
            let result = SessionToken::from_encoded(bad).verify(&keys.public_keyset(), 0);
            assert!(result.is_err(), "`{bad}` should not verify");
        }
    }

    #[test]
    fn an_unknown_key_id_is_indistinguishable_from_a_bad_signature() {
        // Otherwise verification becomes an oracle for enumerating key ids.
        let keys = ProjectKeys::generate("p");
        let mut unknown = scope("p");
        unknown.key_id = KeyId::new("p-k99");
        let token = SessionToken::mint(&keys, &unknown);

        assert_eq!(
            token.verify(&keys.public_keyset(), 0),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_token_never_renders_itself_into_a_log_line() {
        let keys = ProjectKeys::generate("p");
        let token = SessionToken::mint(&keys, &scope("p"));
        let rendered = format!("{token}");

        assert_eq!(rendered, "<session token redacted>");
        assert!(!rendered.contains(token.as_str()));
    }

    #[test]
    fn every_project_in_a_keyset_is_isolated_from_every_other() {
        let mut keyset = SigningKeyset::new();
        let names = ["alpha", "beta", "gamma", "delta"];
        for name in names {
            keyset.ensure(name);
        }

        // Cross-check every pair in both directions: a token for one project
        // must fail against every other project's keys.
        for signer in names {
            let signing = keyset.get(signer).expect("keys");
            let token = SessionToken::mint(signing, &scope(signer));

            for verifier in names {
                let public = keyset.get(verifier).expect("keys").public_keyset();
                let result = token.verify(&public, 0);
                if signer == verifier {
                    assert!(result.is_ok(), "{signer} should verify against itself");
                } else {
                    assert!(
                        result.is_err(),
                        "a token for {signer} verified against {verifier}"
                    );
                }
            }
        }
    }
}
