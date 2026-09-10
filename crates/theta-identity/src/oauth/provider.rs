//! Google and GitHub, behind one abstraction.
//!
//! The abstraction earns its place by isolating the two things that genuinely
//! differ: the endpoint URLs, and how each provider reports a user's verified
//! organizations. Everything else — PKCE, state, the loopback redirect, the
//! code exchange — is identical, and identical code is what keeps the security
//! properties from diverging between providers.
//!
//! # Verified memberships only
//!
//! `06-provisioning-identity-flow.md` §2 says orgs are discovered for
//! memberships the user is "already a verified member of". That word is
//! load-bearing, and each provider needs care:
//!
//! * **GitHub** — only orgs the API reports for the authenticated user, and the
//!   `read:org` scope is requested so private memberships are visible. An org
//!   the user merely *names* is never accepted.
//! * **Google** — the `hd` (hosted domain) claim, which Google sets only for a
//!   Workspace account and which the user cannot influence. A `gmail.com`
//!   address gets a personal org, not a shared one, because anyone can make a
//!   Gmail address at any domain-looking name.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Google,
    Github,
}

impl ProviderKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "google" => Some(ProviderKind::Google),
            "github" => Some(ProviderKind::Github),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Google => "google",
            ProviderKind::Github => "github",
        }
    }
}

/// Endpoints and scopes for one provider.
///
/// Base URLs are fields rather than constants so a test can point the whole
/// flow at a mock provider. That is what makes the exchange logic testable
/// without reaching Google.
/// Everything here is public by construction: an OAuth client id and the
/// provider's own endpoints appear in the authorization URL a browser sees.
/// There is no client *secret* field, because a public client (RFC 8252) must
/// not have one — PKCE is what takes its place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    /// Where verified org memberships come from. `None` for providers whose
    /// orgs are derived from the identity itself, as Google's are.
    pub orgs_url: Option<String>,
    pub scopes: Vec<String>,
}

impl ProviderConfig {
    /// Google, the primary provider (`06-provisioning-identity-flow.md` §2).
    pub fn google(client_id: impl Into<String>) -> Self {
        Self {
            kind: ProviderKind::Google,
            client_id: client_id.into(),
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo".into(),
            orgs_url: None,
            // The minimum that answers "who is this, and what Workspace domain
            // are they in". Asking for more would be scope the user has to
            // grant for no purpose.
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }

    /// GitHub, the secondary provider.
    pub fn github(client_id: impl Into<String>) -> Self {
        Self {
            kind: ProviderKind::Github,
            client_id: client_id.into(),
            authorize_url: "https://github.com/login/oauth/authorize".into(),
            token_url: "https://github.com/login/oauth/access_token".into(),
            userinfo_url: "https://api.github.com/user".into(),
            orgs_url: Some("https://api.github.com/user/orgs".into()),
            // `read:org` so private memberships are visible; without it a user
            // in a private org would silently see none of it.
            scopes: vec!["read:user".into(), "user:email".into(), "read:org".into()],
        }
    }

    /// Point every endpoint at `base`, for testing against a mock provider.
    pub fn with_base_url(mut self, base: &str) -> Self {
        let base = base.trim_end_matches('/');
        self.authorize_url = format!("{base}/authorize");
        self.token_url = format!("{base}/token");
        self.userinfo_url = format!("{base}/userinfo");
        self.orgs_url = self.orgs_url.map(|_| format!("{base}/orgs"));
        self
    }

    /// Build the authorization URL for one login attempt.
    pub fn authorize_url(
        &self,
        challenge: &crate::oauth::PkceChallenge,
        state: &str,
        redirect_uri: &str,
    ) -> String {
        use crate::oauth::percent_encode;

        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}\
             &code_challenge={}&code_challenge_method={}",
            self.authorize_url,
            percent_encode(&self.client_id),
            percent_encode(redirect_uri),
            percent_encode(state),
            percent_encode(challenge.as_str()),
            crate::oauth::PkceChallenge::METHOD,
        );

        if !self.scopes.is_empty() {
            url.push_str(&format!(
                "&scope={}",
                percent_encode(&self.scopes.join(" "))
            ));
        }
        url
    }
}

/// Who signed in, and which orgs they are verifiably a member of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserIdentity {
    /// Stable id, namespaced by provider so a GitHub user `123` and a Google
    /// user `123` are never the same person.
    pub user_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub provider: ProviderKind,
    pub orgs: Vec<VerifiedOrg>,
}

/// An org membership the provider vouches for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedOrg {
    pub id: String,
    pub name: String,
    /// How the membership was established — recorded because "verified" is only
    /// meaningful if you can say by what.
    pub source: OrgSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrgSource {
    GoogleWorkspace,
    GithubOrg,
    /// The user's own space. Always present, so a personal account is never
    /// left with nowhere to put a project.
    Personal,
}

/// Provider-shaped responses, mapped to [`UserIdentity`].
pub trait OAuthProvider {
    fn identity_from_userinfo(
        &self,
        userinfo: &serde_json::Value,
        orgs: &serde_json::Value,
    ) -> Result<UserIdentity, super::OAuthError>;
}

impl OAuthProvider for ProviderConfig {
    fn identity_from_userinfo(
        &self,
        userinfo: &serde_json::Value,
        orgs: &serde_json::Value,
    ) -> Result<UserIdentity, super::OAuthError> {
        match self.kind {
            ProviderKind::Google => google_identity(userinfo),
            ProviderKind::Github => github_identity(userinfo, orgs),
        }
    }
}

fn google_identity(userinfo: &serde_json::Value) -> Result<UserIdentity, super::OAuthError> {
    let subject = userinfo
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| super::OAuthError::Identity("no subject in the userinfo response".into()))?;

    let email = userinfo.get("email").and_then(|v| v.as_str());
    // Google reports whether it verified the address. An unverified one proves
    // nothing about who holds it.
    let email_verified = userinfo
        .get("email_verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut orgs = vec![VerifiedOrg {
        id: format!("user:google:{subject}"),
        name: "Personal".into(),
        source: OrgSource::Personal,
    }];

    // `hd` is set by Google only for a Workspace account, and the user cannot
    // influence it. Deriving a domain from the email address instead would let
    // anyone with a `@bigcorp.com`-looking Gmail alias claim that org.
    if let Some(domain) = userinfo.get("hd").and_then(|v| v.as_str()) {
        if email_verified {
            orgs.push(VerifiedOrg {
                id: format!("workspace:{domain}"),
                name: domain.to_string(),
                source: OrgSource::GoogleWorkspace,
            });
        }
    }

    Ok(UserIdentity {
        user_id: format!("google:{subject}"),
        email: email.filter(|_| email_verified).map(str::to_string),
        display_name: userinfo
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        provider: ProviderKind::Google,
        orgs,
    })
}

fn github_identity(
    userinfo: &serde_json::Value,
    orgs: &serde_json::Value,
) -> Result<UserIdentity, super::OAuthError> {
    let id = userinfo
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| super::OAuthError::Identity("no id in the user response".into()))?;

    let login = userinfo
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let mut verified = vec![VerifiedOrg {
        id: format!("user:github:{id}"),
        name: format!("{login} (personal)"),
        source: OrgSource::Personal,
    }];

    // Only what the API reports for the authenticated user. An org the caller
    // merely names is never accepted.
    if let Some(list) = orgs.as_array() {
        for org in list {
            let (Some(org_id), Some(org_login)) = (
                org.get("id").and_then(|v| v.as_u64()),
                org.get("login").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            verified.push(VerifiedOrg {
                id: format!("github:{org_id}"),
                name: org_login.to_string(),
                source: OrgSource::GithubOrg,
            });
        }
    }

    Ok(UserIdentity {
        user_id: format!("github:{id}"),
        email: userinfo
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        display_name: userinfo
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        provider: ProviderKind::Github,
        orgs: verified,
    })
}

#[cfg(test)]
mod tests {
    use crate::oauth::PkceVerifier;

    use super::*;

    #[test]
    fn the_authorize_url_carries_pkce_and_state() {
        let config = ProviderConfig::google("client-123");
        let verifier = PkceVerifier::generate();
        let url = config.authorize_url(&verifier.challenge(), "st4te", "http://127.0.0.1:9999/cb");

        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains(verifier.challenge().as_str()));
        // The verifier itself must never appear in a URL the browser sees.
        assert!(!url.contains(verifier.expose_for_exchange()));
    }

    #[test]
    fn the_redirect_uri_is_percent_encoded() {
        let url = ProviderConfig::github("c").authorize_url(
            &PkceVerifier::generate().challenge(),
            "s",
            "http://127.0.0.1:9999/callback",
        );
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9999%2Fcallback"));
    }

    #[test]
    fn github_requests_read_org_so_private_memberships_are_visible() {
        // Without it a user in a private org silently sees none of it.
        assert!(ProviderConfig::github("c")
            .scopes
            .contains(&"read:org".to_string()));
    }

    #[test]
    fn google_asks_for_no_more_scope_than_it_needs() {
        let scopes = ProviderConfig::google("c").scopes;
        assert_eq!(scopes, vec!["openid", "email", "profile"]);
    }

    #[test]
    fn a_workspace_account_gets_its_domain_as_a_verified_org() {
        let userinfo = serde_json::json!({
            "sub": "1234567890",
            "email": "alice@bigcorp.com",
            "email_verified": true,
            "name": "Alice",
            "hd": "bigcorp.com",
        });
        let identity = ProviderConfig::google("c")
            .identity_from_userinfo(&userinfo, &serde_json::Value::Null)
            .expect("identity");

        assert_eq!(identity.user_id, "google:1234567890");
        assert!(identity
            .orgs
            .iter()
            .any(|o| o.source == OrgSource::GoogleWorkspace && o.name == "bigcorp.com"));
    }

    #[test]
    fn a_personal_google_account_gets_no_shared_org() {
        // Anyone can create a Gmail address; deriving an org from the email
        // domain would let them claim one.
        let userinfo = serde_json::json!({
            "sub": "42",
            "email": "someone@gmail.com",
            "email_verified": true,
        });
        let identity = ProviderConfig::google("c")
            .identity_from_userinfo(&userinfo, &serde_json::Value::Null)
            .expect("identity");

        assert_eq!(identity.orgs.len(), 1);
        assert_eq!(identity.orgs[0].source, OrgSource::Personal);
    }

    #[test]
    fn an_unverified_email_yields_no_workspace_org_and_no_email() {
        // An unverified address proves nothing about who holds it.
        let userinfo = serde_json::json!({
            "sub": "42",
            "email": "alice@bigcorp.com",
            "email_verified": false,
            "hd": "bigcorp.com",
        });
        let identity = ProviderConfig::google("c")
            .identity_from_userinfo(&userinfo, &serde_json::Value::Null)
            .expect("identity");

        assert_eq!(identity.email, None);
        assert!(identity
            .orgs
            .iter()
            .all(|o| o.source != OrgSource::GoogleWorkspace));
    }

    #[test]
    fn github_orgs_come_from_the_api_not_from_anything_the_caller_says() {
        let userinfo = serde_json::json!({ "id": 99, "login": "alice", "name": "Alice" });
        let orgs = serde_json::json!([
            { "id": 1, "login": "acme" },
            { "id": 2, "login": "widgets" },
        ]);

        let identity = ProviderConfig::github("c")
            .identity_from_userinfo(&userinfo, &orgs)
            .expect("identity");

        let names: Vec<&str> = identity
            .orgs
            .iter()
            .filter(|o| o.source == OrgSource::GithubOrg)
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(names, vec!["acme", "widgets"]);
    }

    #[test]
    fn a_malformed_org_entry_is_skipped_rather_than_guessed_at() {
        let userinfo = serde_json::json!({ "id": 99, "login": "alice" });
        let orgs = serde_json::json!([
            { "id": 1, "login": "acme" },
            { "login": "no-id" },
            { "id": 3 },
        ]);

        let identity = ProviderConfig::github("c")
            .identity_from_userinfo(&userinfo, &orgs)
            .expect("identity");
        assert_eq!(
            identity
                .orgs
                .iter()
                .filter(|o| o.source == OrgSource::GithubOrg)
                .count(),
            1
        );
    }

    #[test]
    fn a_user_always_has_somewhere_to_put_a_project() {
        // Even with no orgs at all, a personal space exists.
        let identity = ProviderConfig::github("c")
            .identity_from_userinfo(
                &serde_json::json!({ "id": 7, "login": "solo" }),
                &serde_json::json!([]),
            )
            .expect("identity");
        assert_eq!(identity.orgs.len(), 1);
        assert_eq!(identity.orgs[0].source, OrgSource::Personal);
    }

    #[test]
    fn the_two_providers_never_collide_on_a_user_id() {
        let google = ProviderConfig::google("c")
            .identity_from_userinfo(
                &serde_json::json!({ "sub": "123", "email_verified": false }),
                &serde_json::Value::Null,
            )
            .expect("identity");
        let github = ProviderConfig::github("c")
            .identity_from_userinfo(
                &serde_json::json!({ "id": 123, "login": "x" }),
                &serde_json::json!([]),
            )
            .expect("identity");

        assert_ne!(google.user_id, github.user_id);
    }

    #[test]
    fn a_userinfo_response_with_no_subject_is_refused() {
        assert!(ProviderConfig::google("c")
            .identity_from_userinfo(&serde_json::json!({}), &serde_json::Value::Null)
            .is_err());
        assert!(ProviderConfig::github("c")
            .identity_from_userinfo(&serde_json::json!({}), &serde_json::json!([]))
            .is_err());
    }

    #[test]
    fn pointing_a_provider_at_a_mock_rewrites_every_endpoint() {
        let config = ProviderConfig::github("c").with_base_url("http://127.0.0.1:9999");
        assert_eq!(config.token_url, "http://127.0.0.1:9999/token");
        assert_eq!(
            config.orgs_url.as_deref(),
            Some("http://127.0.0.1:9999/orgs")
        );
        assert!(!config.authorize_url.contains("github.com"));
    }
}
