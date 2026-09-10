//! The interactive login flow.
//!
//! Opens the provider's authorization page in a browser, listens on a loopback
//! port for the callback, and exchanges the code for an access token. The
//! security-relevant decisions live in `theta_identity::oauth`; this module is
//! the plumbing around them.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use theta_identity::oauth::{
    generate_state, parse_callback, LoginAttempt, OAuthError, OAuthProvider, PkceVerifier,
    ProviderConfig, TokenResponse, UserIdentity,
};

/// How long to wait for the user to finish in the browser.
///
/// Generous: a first login may involve picking an account, a password manager,
/// and a second factor. Short enough that a forgotten terminal does not hold a
/// port forever.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Start a login attempt: bind a loopback port and build the authorization URL.
///
/// The listener is returned still bound, because the port has to be reserved
/// *before* the URL naming it is handed to a browser.
pub fn begin(config: &ProviderConfig) -> std::io::Result<(LoginAttempt, TcpListener)> {
    // Port 0: the OS picks a free one. Loopback only — a redirect reachable
    // from off the machine would let anything on the network claim the code.
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let verifier = PkceVerifier::generate();
    let state = generate_state();
    let authorize_url = config.authorize_url(&verifier.challenge(), &state, &redirect_uri);

    Ok((
        LoginAttempt {
            provider: config.kind,
            verifier,
            state,
            redirect_uri,
            authorize_url,
        },
        listener,
    ))
}

/// Wait for the provider to redirect the browser back, and return the code.
///
/// Serves a small page either way, so the user sees an outcome in the browser
/// rather than a connection error and has to guess.
pub fn await_callback(
    listener: &TcpListener,
    attempt: &LoginAttempt,
    timeout: Duration,
) -> Result<String, OAuthError> {
    listener
        .set_nonblocking(true)
        .map_err(|e| OAuthError::Exchange(e.to_string()))?;

    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() >= deadline {
            return Err(OAuthError::TimedOut {
                seconds: timeout.as_secs(),
            });
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let Some(query) = read_request_query(stream.try_clone().ok().as_ref()) else {
                    // A stray connection — a port scanner, a browser preconnect.
                    // Ignored rather than failing the login.
                    continue;
                };

                let result = parse_callback(&query, &attempt.state);
                respond(stream, &result);
                return result;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(OAuthError::Exchange(e.to_string())),
        }
    }
}

/// Read the request line and pull out the query string.
fn read_request_query(stream: Option<&TcpStream>) -> Option<String> {
    let mut stream = stream?.try_clone().ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;

    // The request line is all that matters, and it is first. Bounded so a
    // client that connects and streams forever cannot exhaust memory.
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer).ok()?;
    let request = String::from_utf8_lossy(&buffer[..read]);

    let line = request.lines().next()?;
    let path = line.split_whitespace().nth(1)?;
    Some(
        path.split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .to_string(),
    )
}

fn respond(mut stream: TcpStream, result: &Result<String, OAuthError>) {
    let (status, body) = match result {
        Ok(_) => (
            "200 OK",
            "<h1>Signed in</h1><p>You can close this tab and return to your terminal.</p>",
        ),
        Err(_) => (
            "400 Bad Request",
            "<h1>Sign-in failed</h1><p>Return to your terminal for details.</p>",
        ),
    };

    // The error detail deliberately stays in the terminal. The browser page is
    // reachable by anything that can hit the loopback port, so it says as
    // little as possible.
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Exchange the authorization code for an access token, then read the identity.
pub async fn complete(
    http: &reqwest::Client,
    config: &ProviderConfig,
    attempt: &LoginAttempt,
    code: &str,
) -> Result<(UserIdentity, TokenResponse), OAuthError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", attempt.redirect_uri.as_str()),
        ("client_id", config.client_id.as_str()),
        // Proves this process started the flow. Without it, an intercepted
        // code would be redeemable by whoever holds it.
        ("code_verifier", attempt.verifier.expose_for_exchange()),
    ];

    let response = http
        .post(&config.token_url)
        // GitHub returns form-encoded unless asked otherwise.
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| OAuthError::Exchange(e.to_string()))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| OAuthError::Exchange(format!("unreadable token response: {e}")))?;

    if !status.is_success() || body.get("error").is_some() {
        return Err(OAuthError::Provider {
            error: body
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("token exchange failed")
                .to_string(),
            description: body
                .get("error_description")
                .and_then(|e| e.as_str())
                .map(str::to_string),
        });
    }

    let tokens: TokenResponse = serde_json::from_value(body)
        .map_err(|e| OAuthError::Exchange(format!("unrecognised token response: {e}")))?;

    let identity = read_identity(http, config, &tokens.access_token).await?;
    Ok((identity, tokens))
}

/// Read who signed in, and which orgs they verifiably belong to.
async fn read_identity(
    http: &reqwest::Client,
    config: &ProviderConfig,
    access_token: &str,
) -> Result<UserIdentity, OAuthError> {
    let userinfo: serde_json::Value = http
        .get(&config.userinfo_url)
        .bearer_auth(access_token)
        // GitHub rejects requests without one.
        .header("user-agent", "thetabase-cli")
        .send()
        .await
        .map_err(|e| OAuthError::Identity(e.to_string()))?
        .json()
        .await
        .map_err(|e| OAuthError::Identity(format!("unreadable userinfo: {e}")))?;

    let orgs = match &config.orgs_url {
        Some(url) => http
            .get(url)
            .bearer_auth(access_token)
            .header("user-agent", "thetabase-cli")
            .send()
            .await
            .map_err(|e| OAuthError::Identity(e.to_string()))?
            .json()
            .await
            .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };

    config.identity_from_userinfo(&userinfo, &orgs)
}

/// Try to open `url` in the user's browser.
///
/// Best effort. When it fails the caller prints the URL, because a login that
/// cannot proceed on a headless machine is a login that does not work over SSH.
pub fn open_browser(url: &str) -> bool {
    let candidates: &[(&str, &[&str])] = &[
        ("xdg-open", &[]),
        ("open", &[]),
        ("cmd", &["/C", "start", ""]),
    ];

    candidates.iter().any(|(program, prefix)| {
        std::process::Command::new(program)
            .args(*prefix)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beginning_a_login_binds_a_loopback_port_and_names_it_in_the_url() {
        let config = ProviderConfig::google("client-id");
        let (attempt, listener) = begin(&config).expect("binds");

        let port = listener.local_addr().expect("addr").port();
        assert!(attempt.redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(attempt.redirect_uri.contains(&port.to_string()));
        assert!(attempt.authorize_url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn the_listener_is_bound_to_loopback_only() {
        // A redirect reachable from off the machine would let anything on the
        // network claim the authorization code.
        let (_, listener) = begin(&ProviderConfig::github("c")).expect("binds");
        assert!(listener.local_addr().expect("addr").ip().is_loopback());
    }

    #[test]
    fn two_attempts_never_share_a_state_or_a_verifier() {
        let config = ProviderConfig::google("c");
        let (first, _a) = begin(&config).expect("binds");
        let (second, _b) = begin(&config).expect("binds");

        assert_ne!(first.state, second.state);
        assert_ne!(first.verifier, second.verifier);
    }

    #[test]
    fn waiting_for_a_callback_gives_up_rather_than_hanging_forever() {
        let (attempt, listener) = begin(&ProviderConfig::google("c")).expect("binds");
        let result = await_callback(&listener, &attempt, Duration::from_millis(150));
        assert!(matches!(result, Err(OAuthError::TimedOut { .. })));
    }
}
