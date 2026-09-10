//! Review finding R1-02 — the startup fail-open revocation window.
//!
//! The review's own test asserted that `RevocationSync::new` returns `Stale`.
//! That is not quite the defect: `new` means "a list already known fresh", which
//! is right for a test and for an instance handed a list at startup, and its doc
//! says so. The defect was that **production used it** — `main.rs` built its
//! `Authorizer` with `new` over an *empty* list, marking nothing-known as
//! fresh-as-of-boot.
//!
//! So the test moved to the seam where the bug lived. It asserts on the
//! constructor `main.rs` actually calls, which is the thing that can regress.

use theta_identity::keys::KeyId;
use theta_identity::{ProjectKeys, RevocationList, SignedRevocationList, TokenScope};
use thetad::config::Environment;
use thetad::session::{AuthError, Authorizer};

const STALENESS_LIMIT_MS: i64 = 15_000; // thetad/src/main.rs

fn scope(token_id: &str) -> TokenScope {
    TokenScope {
        token_id: token_id.into(),
        project_id: "test-project".into(),
        environment: "dev".into(),
        session_id: "sess".into(),
        user_id: "u".into(),
        org_id: "org".into(),
        issued_at_ms: 0,
        expires_at_ms: i64::MAX,
        key_id: KeyId::new("test-project-k1"),
        signing_key: None,
    }
}

#[test]
fn a_freshly_started_instance_refuses_until_its_first_sync() {
    // A token revoked while the instance was down. The empty list cannot know
    // that, and the honest answer is "I cannot tell", not "allowed".
    let keys = ProjectKeys::generate("test-project");
    let token = theta_identity::SessionToken::mint(&keys, &scope("tok_revoked_while_down"));

    let authorizer =
        Authorizer::awaiting_first_sync(keys.public_keyset(), Environment::Dev, STALENESS_LIMIT_MS);

    let decision = authorizer.authorize(&token.clone().into_string(), 1_000_000);
    assert!(
        matches!(decision, Err(AuthError::RevocationStale { .. })),
        "a freshly started instance authorized a token before its first sync \
         (R1-02): {decision:?}"
    );
}

#[test]
fn it_starts_serving_once_a_real_list_arrives() {
    // The refusal has to end, or the fix is an outage. One signed push is enough.
    let keys = ProjectKeys::generate("test-project");
    let token = theta_identity::SessionToken::mint(&keys, &scope("tok_fine"));

    let mut authorizer =
        Authorizer::awaiting_first_sync(keys.public_keyset(), Environment::Dev, STALENESS_LIMIT_MS);

    let signed = SignedRevocationList::sign(&keys, &RevocationList::new());
    let list = authorizer
        .verify_revocations(&signed)
        .expect("a signed list from the project key is accepted");
    authorizer.revocations_mut().apply(list, 1_000_000);

    authorizer
        .authorize(&token.into_string(), 1_000_001)
        .expect("a token should be served once the instance has a real list");
}

#[test]
fn a_revocation_that_arrived_before_startup_is_honoured_after_the_first_sync() {
    // The case the window was hiding. Revoked while down, then the first push
    // carries the revocation — it must bite, not be treated as new information
    // about a token already waved through.
    let keys = ProjectKeys::generate("test-project");
    let token = theta_identity::SessionToken::mint(&keys, &scope("tok_revoked_while_down"));

    let mut authorizer =
        Authorizer::awaiting_first_sync(keys.public_keyset(), Environment::Dev, STALENESS_LIMIT_MS);

    let mut list = RevocationList::new();
    list.revoke_token("tok_revoked_while_down");
    let signed = SignedRevocationList::sign(&keys, &list);
    let verified = authorizer.verify_revocations(&signed).expect("signed list");
    authorizer.revocations_mut().apply(verified, 1_000_000);

    let decision = authorizer.authorize(&token.into_string(), 1_000_001);
    assert!(
        matches!(decision, Err(AuthError::Revoked)),
        "a token revoked before startup was served after the first sync: {decision:?}"
    );
}

/// The production path uses the refusing constructor.
///
/// Every test above proves `Authorizer::awaiting_first_sync` behaves correctly.
/// None of them proves that `main.rs` *calls* it — and that was the entire
/// defect: the correct behaviour already existed and the binary used the other
/// constructor. Reverting the fix is a one-word edit in a file no test covers,
/// because a binary entry point has none.
///
/// So this reads the source. Crude, and the alternative is a guarantee that
/// rests on nobody changing one line. It reads `main.rs`, not this file, so the
/// assertion cannot satisfy itself — a trap this repo has fallen into before.
#[test]
fn the_daemon_builds_its_authorizer_with_the_refusing_constructor() {
    const MAIN: &str = include_str!("../src/main.rs");

    assert!(
        MAIN.contains("Authorizer::awaiting_first_sync("),
        "main.rs no longer builds its authorizer with awaiting_first_sync (R1-02)"
    );
    assert!(
        !MAIN.contains("Authorizer::new("),
        "main.rs constructs an Authorizer with `new`, which treats an empty \
         revocation list as fresh as of startup and authorizes everything until \
         the first push (R1-02)"
    );
}
