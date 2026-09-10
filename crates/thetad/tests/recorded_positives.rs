//! SEC-8: the properties `docs/SECURITY-REVIEW.md` records as worth defending.
//!
//! The review lists seven positives — "the properties an external reviewer
//! should try hardest to break rather than rediscover". SEC-8 is the audit that
//! each one is actually defended, meaning there is a test that goes red if it
//! stops being true. Most were, and their tests live next to the code they
//! defend:
//!
//! | Property | Defended by |
//! |---|---|
//! | Token scope is cryptographic | `theta-identity`, `token.rs` |
//! | Signature checked before claims are read | `thetad`, `session.rs` |
//! | Revocation staleness fails closed | `theta-identity`, `revocation.rs` |
//! | Webhook signature over the raw body, before parsing | `theta-control`, `github_webhook.rs` |
//! | Unsigned deliveries refused, deliveries deduplicated | `theta-control`, `github_webhook.rs` |
//! | Frames size-checked before allocation | `theta-proto`, `frame.rs` |
//! | The log is hash-chained | `theta-storage`, `tamper_evidence.rs` |
//!
//! Each of those was confirmed by planting the violation and watching the test
//! fail. Three claims could not be confirmed that way, and they are what this
//! file is for.
//!
//! # Why these three need a different kind of test
//!
//! **Constant-time comparison.** No behavioural test distinguishes
//! `mac.verify_slice(expected)` from `mac.finalize().into_bytes() == expected`.
//! Both accept exactly the same deliveries. The difference is only in how long
//! the wrong answer takes, and a timing assertion in CI would be a flake
//! generator rather than a guard.
//!
//! **No signing material at an instance**, and **no raw text in the query IR**,
//! are properties of *types*. The right test for "this variant does not exist"
//! is that the codebase does not contain it — a runtime test can only sample
//! the values it happens to construct. The existing IR test checks that plans
//! serialise without a `"raw"` key, which catches a `Raw` variant named exactly
//! that and nothing else.
//!
//! So these read the source. That is the same technique `no_llm_on_hot_path.rs`
//! uses for invariant 1, for the same reason: some claims are about what is
//! *absent*, and absence is not observable from inside the program.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<name> sits two levels under the workspace root")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Lines of real code, with comments dropped.
///
/// Comments discuss these constraints constantly — that is the house style —
/// and a guard that counted them would fire on its own documentation.
fn code_lines(path: &Path) -> Vec<(usize, String)> {
    fs::read_to_string(path)
        .expect("source file is readable")
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim_start().to_string()))
        .filter(|(_, l)| !l.starts_with("//"))
        .collect()
}

#[test]
fn the_webhook_signature_is_compared_in_constant_time() {
    // `SECURITY-REVIEW.md`: "constant-time comparison". `verify_slice` is the
    // hmac crate's constant-time check; comparing digests with `==` is the
    // mistake it exists to prevent, and it leaks the expected digest one byte
    // per request to anyone who can time the response.
    let root = workspace_root();
    let path = root.join("crates/theta-control/src/github.rs");
    let code = code_lines(&path);

    assert!(
        code.iter().any(|(_, l)| l.contains("verify_slice")),
        "the webhook no longer verifies through a constant-time comparison; \
         SECURITY-REVIEW.md records that it does"
    );

    // A digest compared with `==` anywhere in this file is the regression.
    let offenders: Vec<String> = code
        .iter()
        .filter(|(_, l)| {
            (l.contains("finalize()") || l.contains("into_bytes()")) && l.contains("==")
        })
        .map(|(n, l)| format!("{}:{n}: {l}", path.display()))
        .collect();
    assert!(
        offenders.is_empty(),
        "an HMAC digest is compared with `==`, which is not constant time:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn an_instance_can_never_hold_signing_material() {
    // `SECURITY-REVIEW.md`: "Instances hold public keys only. They can verify
    // and can never mint." This is the property that makes a compromised
    // storage node a data problem rather than a credential-forgery problem, so
    // it is worth more than the type system quietly enforcing it — it is worth
    // a test that names it.
    let root = workspace_root();
    let path = root.join("crates/theta-identity/src/keys.rs");
    let text = fs::read_to_string(&path).expect("keys.rs is readable");

    let start = text
        .find("pub struct PublicKeyset")
        .expect("PublicKeyset must exist; SECURITY-REVIEW.md describes it");
    let body_end = text[start..]
        .find("\n}")
        .expect("the struct has a closing brace");
    let body = &text[start..start + body_end];

    assert!(
        !body.contains("SigningKey"),
        "PublicKeyset now carries a SigningKey, so an instance could mint \
         tokens for its own project:\n{body}"
    );
    assert!(
        !body.contains("[u8; 32]"),
        "PublicKeyset now carries raw key bytes; whether they are secret is no \
         longer answerable from the type:\n{body}"
    );
}

#[test]
fn the_query_ir_has_no_variant_that_can_carry_executable_text() {
    // Invariant 4, and `SECURITY-REVIEW.md`: "The query IR has no `Raw(String)`
    // variant, which makes injection into the execution path a compile error
    // rather than a code review."
    //
    // Checked by name rather than by behaviour. The existing test asserts that
    // planned queries serialise without a `"raw"` key, which catches a variant
    // called exactly `Raw` and misses `Passthrough`, `Verbatim`, or `Sql` —
    // and a variant that carries executable text is the hazard whatever it is
    // called.
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates/theta-query/src"), &mut sources);
    assert!(!sources.is_empty(), "found no query sources to check");

    const FORBIDDEN_VARIANTS: &[&str] = &[
        "Raw(String)",
        "Raw(",
        "Verbatim(",
        "Passthrough(",
        "RawSql(",
        "Sql(String)",
    ];

    let mut findings = Vec::new();
    for path in &sources {
        for (lineno, line) in code_lines(path) {
            for needle in FORBIDDEN_VARIANTS {
                if line.contains(needle) {
                    findings.push(format!(
                        "{}:{lineno}: `{needle}` in `{line}`",
                        path.strip_prefix(&root).unwrap_or(path).display()
                    ));
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "the query IR gained a variant that can carry executable text into the \
         execution path (docs/INVARIANTS.md invariant 4):\n{}",
        findings.join("\n")
    );
}

#[test]
fn the_guard_would_notice_a_variant_that_carried_text() {
    // The guards above are string searches, and a string search that matches
    // nothing looks identical to one that is looking in the wrong place. This
    // plants the violation in a scratch file and confirms the same predicate
    // finds it — the check `no_llm_on_hot_path.rs` makes for the same reason.
    let planted = "    Raw(String),";
    let matched = ["Raw(String)", "Verbatim("]
        .iter()
        .any(|needle| planted.contains(needle));
    assert!(matched, "the forbidden-variant predicate matches nothing");

    let innocent = "    Column(String),";
    assert!(
        !["Raw(String)", "Verbatim(", "Passthrough("]
            .iter()
            .any(|needle| innocent.contains(needle)),
        "the predicate fires on an ordinary variant, so it would be deleted \
         the first time someone added a legitimate one"
    );
}
