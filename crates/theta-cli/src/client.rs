//! Talking to the Control Plane.

use serde::{Deserialize, Serialize};
use theta_identity::oauth::ProviderConfig;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the control plane at {url}: {detail}")]
    Unreachable { url: String, detail: String },

    #[error("{message}")]
    Refused { status: u16, message: String },

    #[error("unexpected response from the control plane: {0}")]
    Malformed(String),
}

/// What `resolveContext` came back with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ResolveOutcome {
    Ready {
        project_id: String,
        org_id: String,
        environment: String,
        address: String,
        token: String,
        expires_at_ms: i64,
    },
    NeedsClarification {
        question: String,
        candidates: Vec<String>,
    },
    ConfirmCreate {
        question: String,
        org_id: String,
        project_name: String,
    },
}

#[derive(Debug, Serialize)]
struct ResolveBody<'a> {
    org_hint: &'a str,
    project_hint: &'a str,
    env_hint: Option<&'a str>,
    session_id: &'a str,
    confirm_create: bool,
}

/// What a successful `/v1/login` returns.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Orgs the provider vouched for, shown so the user can see what one login
    /// just gave them access to.
    #[serde(default)]
    pub orgs: Vec<String>,
    pub expires_at_ms: i64,
}

pub struct ControlPlaneClient {
    base_url: String,
    http: reqwest::Client,
}

/// Percent-encode one path segment.
///
/// Written here rather than pulled in as a dependency: it is one call site, and
/// the alternative was adding a crate to the CLI's closure for four lines.
///
/// It matters because org ids are not tame. They arrive from OAuth as
/// `workspace:bigcorp.com` and `user:google:10101`, so a segment interpolated
/// raw can carry `/` and turn one org's billing request into a path pointing
/// somewhere else entirely.
fn escape_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            // RFC 3986 unreserved. Everything else is encoded, including the
            // sub-delims a laxer encoder would pass through — this is a path
            // segment, not a query string, and nothing here needs them.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl ControlPlaneClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// The OAuth providers this deployment offers.
    ///
    /// Fetched rather than configured: a CLI that needed its own client id
    /// would mean a setup step, and the product claim is that there isn't one.
    pub async fn providers(&self) -> Result<Vec<ProviderConfig>, ClientError> {
        let url = format!("{}/v1/providers", self.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?;

        response
            .json()
            .await
            .map_err(|e| ClientError::Malformed(format!("unrecognised provider list: {e}")))
    }

    /// Exchange a provider access token for a ThetaBase identity token.
    ///
    /// The provider token is spent here and never stored. The Control Plane,
    /// not this process, is what asks the provider whose token it is — so a
    /// tampered CLI cannot assert an identity.
    pub async fn login(
        &self,
        provider: &str,
        access_token: &str,
    ) -> Result<LoginResponse, ClientError> {
        let url = format!("{}/v1/login", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "provider": provider,
                "access_token": access_token,
            }))
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        let body: serde_json::Value = response.json().await.map_err(|e| {
            ClientError::Malformed(format!("could not read the response body: {e}"))
        })?;

        if !status.is_success() {
            return Err(ClientError::Refused {
                status: status.as_u16(),
                message: body
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("the control plane refused the login")
                    .to_string(),
            });
        }

        serde_json::from_value(body)
            .map_err(|e| ClientError::Malformed(format!("unrecognised login response: {e}")))
    }

    /// Read an org's plan, usage and what it is accruing.
    pub async fn billing_summary(
        &self,
        identity_token: &str,
        org_id: &str,
    ) -> Result<serde_json::Value, ClientError> {
        self.billing_call(identity_token, "GET", org_id, "summary", None)
            .await
    }

    /// Change the plan an org is on.
    pub async fn change_plan(
        &self,
        identity_token: &str,
        org_id: &str,
        plan_id: &str,
    ) -> Result<serde_json::Value, ClientError> {
        self.billing_call(
            identity_token,
            "POST",
            org_id,
            "subscription",
            Some(serde_json::json!({ "plan_id": plan_id })),
        )
        .await
    }

    /// Move between monthly and annual billing.
    pub async fn change_interval(
        &self,
        identity_token: &str,
        org_id: &str,
        interval: &str,
    ) -> Result<serde_json::Value, ClientError> {
        self.billing_call(
            identity_token,
            "POST",
            org_id,
            "interval",
            Some(serde_json::json!({ "interval": interval })),
        )
        .await
    }

    /// One shape for every billing call, so the identity header, the error
    /// mapping and the org-id escaping cannot be got right in one place and
    /// wrong in the next.
    async fn billing_call(
        &self,
        identity_token: &str,
        method: &str,
        org_id: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, ClientError> {
        let url = format!(
            "{}/v1/billing/{}/{path}",
            self.base_url,
            escape_segment(org_id)
        );
        let mut request = match method {
            "POST" => self.http.post(&url),
            _ => self.http.get(&url),
        }
        .bearer_auth(identity_token);

        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request.send().await.map_err(|e| ClientError::Unreachable {
            url: url.clone(),
            detail: e.to_string(),
        })?;

        let status = response.status();
        let parsed: serde_json::Value = response.json().await.map_err(|e| {
            ClientError::Malformed(format!("could not read the response body: {e}"))
        })?;

        if !status.is_success() {
            return Err(ClientError::Refused {
                status: status.as_u16(),
                message: parsed
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("the control plane refused the billing request")
                    .to_string(),
            });
        }
        Ok(parsed)
    }

    /// Resolve an org/project hint to a provisioned project and a scoped token.
    ///
    /// `identity_token` is required: the Control Plane will not resolve for an
    /// unauthenticated caller, and there is deliberately no field here for
    /// naming a user id.
    pub async fn resolve(
        &self,
        identity_token: &str,
        org_hint: &str,
        project_hint: &str,
        env_hint: Option<&str>,
        session_id: &str,
        confirm_create: bool,
    ) -> Result<ResolveOutcome, ClientError> {
        let url = format!("{}/v1/resolve", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(identity_token)
            .json(&ResolveBody {
                org_hint,
                project_hint,
                env_hint,
                session_id,
                confirm_create,
            })
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        let body: serde_json::Value = response.json().await.map_err(|e| {
            ClientError::Malformed(format!("could not read the response body: {e}"))
        })?;

        if !status.is_success() {
            return Err(ClientError::Refused {
                status: status.as_u16(),
                message: body
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("the control plane refused the request")
                    .to_string(),
            });
        }

        serde_json::from_value(body)
            .map_err(|e| ClientError::Malformed(format!("unrecognised outcome: {e}")))
    }

    /// Revoke a credential.
    pub async fn revoke(&self, field: &str, id: &str) -> Result<u64, ClientError> {
        let url = format!("{}/v1/revoke", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ field: id }))
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?;

        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ClientError::Malformed(e.to_string()))?;

        if !status.is_success() {
            return Err(ClientError::Refused {
                status: status.as_u16(),
                message: body
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("revocation refused")
                    .to_string(),
            });
        }

        body.get("version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ClientError::Malformed("no version in the response".into()))
    }
}

#[cfg(test)]
mod segment_tests {
    use super::escape_segment;

    #[test]
    fn an_org_id_cannot_carry_a_path_separator_into_a_url() {
        // The reason this function exists. Org ids arrive from OAuth and are
        // not tame: a `/` interpolated raw turns one org's billing request into
        // a path pointing somewhere else.
        assert_eq!(escape_segment("org_a/../org_b"), "org_a%2F..%2Forg_b");
        assert_eq!(
            escape_segment("workspace:bigcorp.com"),
            "workspace%3Abigcorp.com"
        );
        assert_eq!(escape_segment("user:google:10101"), "user%3Agoogle%3A10101");
    }

    #[test]
    fn ordinary_ids_pass_through_unchanged() {
        // Encoding everything would work and would make every URL in a log
        // unreadable, which is its own cost.
        assert_eq!(escape_segment("org_a"), "org_a");
        assert_eq!(escape_segment("acme-checkout.v2~1"), "acme-checkout.v2~1");
    }

    #[test]
    fn a_query_string_cannot_be_smuggled_through_a_segment() {
        assert_eq!(escape_segment("org?a=b#c"), "org%3Fa%3Db%23c");
        assert_eq!(escape_segment("org a"), "org%20a");
    }
}
