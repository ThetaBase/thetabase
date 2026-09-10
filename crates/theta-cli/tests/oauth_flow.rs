//! The OAuth flow, end to end against a mock provider.
//!
//! Google and GitHub cannot be reached from a test, so the flow runs against a
//! provider stood up locally. What that genuinely exercises: the authorization
//! URL, the loopback callback, `state` validation, the PKCE binding, the code
//! exchange, and identity mapping. What it does not: the real providers'
//! quirks — which is why `ProviderConfig` keeps its endpoints as fields rather
//! than constants, so the same code path is used either way.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use theta_cli::login;
use theta_identity::oauth::{OAuthError, OrgSource, ProviderConfig, ProviderKind};

/// What the mock provider recorded, so a test can assert on what the client
/// actually sent rather than only on what came back.
#[derive(Debug, Default)]
struct Recorded {
    /// The `code_challenge` from the authorization request.
    challenge: Option<String>,
    /// The `code_verifier` from the token exchange.
    verifier: Option<String>,
    redirect_uri: Option<String>,
    /// Set when the client's verifier did not match the challenge.
    pkce_mismatch: bool,
}

type Shared = Arc<Mutex<Recorded>>;

/// A provider that implements just enough of OAuth to drive the client.
async fn start_mock(kind: ProviderKind) -> (String, Shared) {
    let recorded: Shared = Arc::new(Mutex::new(Recorded::default()));

    let app = Router::new()
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/userinfo", get(move || userinfo(kind)))
        .route("/orgs", get(orgs))
        .with_state(Arc::clone(&recorded));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), recorded)
}

async fn authorize(
    State(recorded): State<Shared>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> StatusCode {
    let mut guard = recorded.lock().expect("lock");
    guard.challenge = params.get("code_challenge").cloned();
    guard.redirect_uri = params.get("redirect_uri").cloned();
    StatusCode::OK
}

async fn token(
    State(recorded): State<Shared>,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut guard = recorded.lock().expect("lock");
    guard.verifier = form.get("code_verifier").cloned();

    // The provider's half of PKCE: recompute the challenge from the verifier
    // and compare. A client that skipped PKCE fails here, which is what makes
    // the binding testable rather than assumed.
    let expected = guard.challenge.clone();
    let matches = match (&guard.verifier, &expected) {
        (Some(verifier), Some(challenge)) => &challenge_for(verifier) == challenge,
        _ => false,
    };

    if !matches {
        guard.pkce_mismatch = true;
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "code_verifier does not match code_challenge",
            })),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "access_token": "mock-access-token",
            "token_type": "Bearer",
            "expires_in": 3600,
        })),
    )
}

async fn userinfo(kind: ProviderKind) -> Json<serde_json::Value> {
    Json(match kind {
        ProviderKind::Google => serde_json::json!({
            "sub": "google-user-1",
            "email": "alice@bigcorp.com",
            "email_verified": true,
            "name": "Alice",
            "hd": "bigcorp.com",
        }),
        ProviderKind::Github => serde_json::json!({
            "id": 4242,
            "login": "alice",
            "name": "Alice",
            "email": "alice@example.com",
        }),
    })
}

async fn orgs() -> Json<serde_json::Value> {
    Json(serde_json::json!([
        { "id": 1, "login": "acme" },
        { "id": 2, "login": "widgets" },
    ]))
}

/// S256 challenge, as the provider would compute it.
fn challenge_for(verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2_lite::sha256;
    URL_SAFE_NO_PAD.encode(sha256(verifier.as_bytes()))
}

/// A local SHA-256, so the mock computes the challenge independently of the
/// implementation under test — otherwise a bug in one would hide a bug in the
/// other.
mod sha2_lite {
    pub fn sha256(message: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut padded = message.to_vec();
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&((message.len() as u64) * 8).to_be_bytes());

        for chunk in padded.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes(word.try_into().expect("4 bytes"));
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(value);
            }
        }
        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Drive one full login against the mock, as a browser and the user would.
async fn run_login(
    kind: ProviderKind,
    tamper: impl FnOnce(&str) -> String + Send + 'static,
) -> (
    Result<
        (
            theta_identity::oauth::UserIdentity,
            theta_identity::oauth::TokenResponse,
        ),
        OAuthError,
    >,
    Shared,
) {
    let (base, recorded) = start_mock(kind).await;
    let config = match kind {
        ProviderKind::Google => ProviderConfig::google("mock-client"),
        ProviderKind::Github => ProviderConfig::github("mock-client"),
    }
    .with_base_url(&base);

    let (attempt, listener) = login::begin(&config).expect("begin");
    let http = reqwest::Client::new();

    // Stand in for the browser: fetch the authorization URL so the provider
    // records the challenge, then hit the loopback callback.
    http.get(&attempt.authorize_url)
        .send()
        .await
        .expect("authorize");

    let callback = format!(
        "{}?code=mock-code&state={}",
        attempt.redirect_uri, attempt.state
    );
    let callback = tamper(&callback);

    let waiting = std::thread::spawn({
        let state = attempt.state.clone();
        let provider = attempt.provider;
        let redirect_uri = attempt.redirect_uri.clone();
        let verifier = attempt.verifier.clone();
        let authorize_url = attempt.authorize_url.clone();
        move || {
            let attempt = theta_identity::oauth::LoginAttempt {
                provider,
                verifier,
                state,
                redirect_uri,
                authorize_url,
            };
            login::await_callback(&listener, &attempt, Duration::from_secs(5))
        }
    });

    // Give the listener a moment to start polling before the callback arrives.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = http.get(&callback).send().await;

    let code = match waiting.join().expect("callback thread") {
        Ok(code) => code,
        Err(e) => return (Err(e), recorded),
    };

    (
        login::complete(&http, &config, &attempt, &code).await,
        recorded,
    )
}

#[tokio::test]
async fn a_google_login_completes_and_yields_a_workspace_org() {
    let (result, _) = run_login(ProviderKind::Google, |url| url.to_string()).await;
    let (identity, tokens) = result.expect("login completes");

    assert_eq!(identity.user_id, "google:google-user-1");
    assert_eq!(identity.email.as_deref(), Some("alice@bigcorp.com"));
    assert_eq!(tokens.access_token, "mock-access-token");

    assert!(
        identity
            .orgs
            .iter()
            .any(|o| o.source == OrgSource::GoogleWorkspace && o.name == "bigcorp.com"),
        "a Workspace domain must become a verified org"
    );
}

#[tokio::test]
async fn a_github_login_completes_and_yields_its_org_memberships() {
    let (result, _) = run_login(ProviderKind::Github, |url| url.to_string()).await;
    let (identity, _) = result.expect("login completes");

    assert_eq!(identity.user_id, "github:4242");
    let orgs: Vec<&str> = identity
        .orgs
        .iter()
        .filter(|o| o.source == OrgSource::GithubOrg)
        .map(|o| o.name.as_str())
        .collect();
    assert_eq!(orgs, vec!["acme", "widgets"]);
}

#[tokio::test]
async fn the_client_proves_pkce_to_the_provider() {
    // The mock recomputes the challenge from the verifier the client sent. If
    // the client skipped PKCE or sent the wrong verifier, the exchange fails.
    let (result, recorded) = run_login(ProviderKind::Google, |url| url.to_string()).await;
    assert!(result.is_ok());

    let guard = recorded.lock().expect("lock");
    assert!(
        guard.challenge.is_some(),
        "no challenge reached the provider"
    );
    assert!(
        guard.verifier.is_some(),
        "no verifier reached the token endpoint"
    );
    assert!(
        !guard.pkce_mismatch,
        "the provider rejected the PKCE binding"
    );

    // And they are genuinely different values — a `plain` downgrade would make
    // them equal.
    assert_ne!(guard.challenge, guard.verifier);
}

#[tokio::test]
async fn a_callback_with_the_wrong_state_is_refused() {
    let (result, _) = run_login(ProviderKind::Google, |url| {
        let (base, _) = url.split_once("&state=").expect("state in url");
        format!("{base}&state=an-attackers-state")
    })
    .await;

    assert!(
        matches!(result, Err(OAuthError::StateMismatch)),
        "a callback from another authorization was accepted: {result:?}"
    );
}

#[tokio::test]
async fn a_provider_error_in_the_callback_surfaces_with_its_reason() {
    let (result, _) = run_login(ProviderKind::Google, |url| {
        let (base, state) = url.split_once("&state=").expect("state in url");
        let base = base.split_once("?code=").expect("code in url").0;
        format!("{base}?error=access_denied&error_description=User+declined&state={state}")
    })
    .await;

    match result {
        Err(OAuthError::Provider { error, description }) => {
            assert_eq!(error, "access_denied");
            assert_eq!(description.as_deref(), Some("User declined"));
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn the_redirect_uri_the_provider_saw_is_the_loopback_one() {
    // A redirect the provider could send anywhere else would let another
    // process claim the code.
    let (_, recorded) = run_login(ProviderKind::Github, |url| url.to_string()).await;
    let redirect = recorded
        .lock()
        .expect("lock")
        .redirect_uri
        .clone()
        .expect("a redirect_uri reached the provider");

    assert!(redirect.starts_with("http://127.0.0.1:"), "got {redirect}");
    assert!(redirect.ends_with("/callback"));
}
