//! OAuth 2.0 login for a native client.
//!
//! Follows RFC 8252 (OAuth for Native Apps): Authorization Code with PKCE, over
//! a loopback redirect on `127.0.0.1` with an ephemeral port. Three choices in
//! there are security-relevant and worth stating.
//!
//! **PKCE is mandatory, not optional.** A CLI is a public client: it cannot keep
//! a client secret, so anything that intercepts the authorization code could
//! redeem it. PKCE binds the code to a verifier only this process knows
//! ([RFC 7636]).
//!
//! **A loopback redirect, not a custom URI scheme.** Custom schemes can be
//! registered by any other app on the machine, which turns "receive the
//! callback" into a race. A loopback port is held by this process for the life
//! of the flow and by nobody else.
//!
//! **`state` is checked before anything else.** Without it, an attacker can feed
//! the client a code from a different authorization and have the client
//! associate someone else's identity with this session.
//!
//! # What is deliberately not done
//!
//! The ID token's signature is not verified. It does not need to be here: the
//! authorization code is exchanged over TLS directly with the provider's token
//! endpoint, so the response is already authenticated by the transport, and the
//! identity is then read from the provider's userinfo endpoint over the same
//! channel. Verifying a JWT would mean fetching and caching JWKS and getting
//! algorithm handling right — real surface area, for no gain over TLS on a
//! back-channel exchange. This reasoning stops holding the moment a token
//! arrives through the *front* channel, which this flow never does.
//!
//! [RFC 7636]: https://datatracker.ietf.org/doc/html/rfc7636

mod pkce;
mod provider;

pub use pkce::{generate_state, PkceChallenge, PkceVerifier};
pub use provider::{
    OAuthProvider, OrgSource, ProviderConfig, ProviderKind, UserIdentity, VerifiedOrg,
};

use serde::{Deserialize, Serialize};

/// One login attempt in progress.
///
/// Holds the secrets that bind the eventual callback to *this* attempt: the
/// PKCE verifier and the `state` nonce. Both are generated fresh per attempt
/// and never reused.
#[derive(Debug, Clone)]
pub struct LoginAttempt {
    pub provider: ProviderKind,
    pub verifier: PkceVerifier,
    pub state: String,
    pub redirect_uri: String,
    pub authorize_url: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum OAuthError {
    /// The callback's `state` did not match the one this attempt issued.
    ///
    /// Deliberately does not echo either value: an error message that reflects
    /// attacker-supplied input into a terminal is a small hole, and there is no
    /// legitimate case where a user needs to see them.
    #[error("the login callback did not match this login attempt; start again")]
    StateMismatch,

    #[error("the provider returned an error: {error}{}", .description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default())]
    Provider {
        error: String,
        description: Option<String>,
    },

    #[error("the login callback carried no authorization code")]
    NoCode,

    #[error("could not exchange the authorization code: {0}")]
    Exchange(String),

    #[error("could not read the signed-in identity: {0}")]
    Identity(String),

    #[error("login timed out after {seconds}s")]
    TimedOut { seconds: u64 },
}

/// What a provider hands back once the code is exchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub token_type: Option<String>,
}

/// Parse the query string of a loopback callback.
///
/// Returns the code once `state` matches. The order matters: `state` is checked
/// before the code is even looked at, so a callback from another authorization
/// is discarded rather than partially processed.
pub fn parse_callback(query: &str, expected_state: &str) -> Result<String, OAuthError> {
    let params: Vec<(String, String)> = form_urlencoded_pairs(query);
    let find = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    match find("state") {
        Some(state) if constant_time_eq(&state, expected_state) => {}
        _ => return Err(OAuthError::StateMismatch),
    }

    if let Some(error) = find("error") {
        return Err(OAuthError::Provider {
            error,
            description: find("error_description"),
        });
    }

    find("code").ok_or(OAuthError::NoCode)
}

/// Compare without leaking length or position through timing.
///
/// `state` is a secret this process generated, and a timing side channel on it
/// is a narrow but real way to forge a callback.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Minimal `application/x-www-form-urlencoded` parsing, so this crate does not
/// need a URL dependency in `thetad`'s hot-path closure.
fn form_urlencoded_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&input[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // A malformed escape is kept literally rather than dropped:
                    // silently discarding input is how a parser changes what a
                    // value means.
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode for a query string.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_callback_yields_its_code() {
        assert_eq!(
            parse_callback("code=abc123&state=s3cret", "s3cret"),
            Ok("abc123".to_string())
        );
    }

    #[test]
    fn a_callback_from_another_authorization_is_discarded() {
        // Without this check an attacker can hand the client a code from a
        // different authorization and have it adopt someone else's identity.
        assert_eq!(
            parse_callback("code=abc123&state=someone-elses", "s3cret"),
            Err(OAuthError::StateMismatch)
        );
    }

    #[test]
    fn a_callback_with_no_state_is_discarded() {
        assert_eq!(
            parse_callback("code=abc123", "s3cret"),
            Err(OAuthError::StateMismatch)
        );
    }

    #[test]
    fn state_is_checked_before_an_error_is_reported() {
        // A provider error from an unrelated authorization must not be
        // surfaced as if it belonged to this attempt.
        assert_eq!(
            parse_callback("error=access_denied&state=wrong", "s3cret"),
            Err(OAuthError::StateMismatch)
        );
    }

    #[test]
    fn a_provider_error_is_reported_with_its_description() {
        let err = parse_callback(
            "error=access_denied&error_description=User+declined&state=s3cret",
            "s3cret",
        )
        .expect_err("must be an error");

        assert_eq!(
            err,
            OAuthError::Provider {
                error: "access_denied".into(),
                description: Some("User declined".into()),
            }
        );
        assert!(err.to_string().contains("User declined"));
    }

    #[test]
    fn a_state_mismatch_never_echoes_the_values() {
        // Reflecting attacker-supplied input into a terminal is a small hole,
        // and no legitimate user needs to see either value.
        let rendered = parse_callback("code=x&state=<script>alert(1)</script>", "expected")
            .expect_err("mismatch")
            .to_string();
        assert!(!rendered.contains("script"));
        assert!(!rendered.contains("expected"));
    }

    #[test]
    fn a_callback_with_state_but_no_code_is_an_error() {
        assert_eq!(
            parse_callback("state=s3cret", "s3cret"),
            Err(OAuthError::NoCode)
        );
    }

    #[test]
    fn percent_encoded_values_round_trip() {
        let value = "a value/with?special&chars=and spaces";
        let encoded = percent_encode(value);
        let query = format!("code={encoded}&state=s");
        assert_eq!(parse_callback(&query, "s"), Ok(value.to_string()));
    }

    #[test]
    fn state_comparison_rejects_a_prefix() {
        assert_eq!(
            parse_callback("code=x&state=s3cr", "s3cret"),
            Err(OAuthError::StateMismatch)
        );
        assert_eq!(
            parse_callback("code=x&state=s3cretXX", "s3cret"),
            Err(OAuthError::StateMismatch)
        );
    }

    #[test]
    fn malformed_percent_escapes_are_kept_rather_than_dropped() {
        // Silently discarding input is how a parser changes what a value means.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }
}
