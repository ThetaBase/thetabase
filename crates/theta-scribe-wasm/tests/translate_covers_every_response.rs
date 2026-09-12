//! Every response variant has an arm in `translate.rs`.
//!
//! # The hole this closes
//!
//! `translate::from_wire` ends in a catch-all rendering anything it does not
//! recognise as `kind: "raw"` with a Rust debug string inside. That arm is not a
//! mistake: a host talking to a *newer* server has to degrade rather than
//! panic, and there is no other way to say that.
//!
//! What it also does is swallow our own additions. `ResponseBody::Transaction`
//! was added to the wire, the whole workspace compiled clean, every test passed,
//! and transactions would have reached all seven SDKs as an unreadable debug
//! string. Nothing failed, because a catch-all is exhaustive by construction —
//! the compiler cannot help here, and that is the point.
//!
//! # Why this reads source rather than building values
//!
//! Constructing one `ResponseBody` of each of twenty-one variants means
//! constructing every payload type they carry, and that fixture would need
//! updating whenever any of those changed — a maintenance cost paid to test
//! something that is really a question about the match. The question is "does
//! `translate.rs` name this variant", and the honest way to ask it is to look.
//!
//! The weakness is real and worth stating: naming a variant in a comment would
//! satisfy this. That is a poor way to cheat a test whose failure message tells
//! you to write the arm, and it beats the alternative, which is no check at all.

use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(relative: &str) -> String {
    let path = crate_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The variant names declared on `ResponseBody`, read from the wire crate.
fn response_variants() -> Vec<String> {
    let wire = read("../theta-proto/src/wire.rs");
    let start = wire
        .find("pub enum ResponseBody {")
        .expect("ResponseBody is still declared in wire.rs");
    let body = &wire[start..];
    let end = body.find("\n}").expect("the enum closes");

    body[..end]
        .lines()
        .skip(1)
        .filter_map(|line| {
            // A variant is a capitalised identifier at one level of indent.
            // Anything deeper is a field, and anything else is prose.
            let trimmed = line.strip_prefix("    ")?;
            if trimmed.starts_with(' ') || trimmed.starts_with("//") || trimmed.starts_with('#') {
                return None;
            }
            let name: String = trimmed
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            (!name.is_empty() && name.starts_with(|c: char| c.is_ascii_uppercase())).then_some(name)
        })
        .collect()
}

/// Responses no SDK will ever see, because no SDK sends the request.
///
/// `PushRevocations` and `PushPolicy` are Control Plane to instance: the
/// Control Plane signs a list, the instance verifies it with the project key it
/// already holds, and a customer's binding is not a party to either. Rendering
/// them for a host would be rendering something no host can receive.
///
/// Exempt by name rather than by a rule, so adding one is a deliberate act with
/// a reason attached. Everything else is host-facing until someone argues
/// otherwise here.
const CONTROL_PLANE_ONLY: &[&str] = &["Revocations", "Policy"];

#[test]
fn from_wire_names_every_response_variant() {
    let translate = read("src/translate.rs");
    let variants = response_variants();

    assert!(
        variants.len() > 10,
        "only {} variants were parsed out of wire.rs, so the parser has broken \
         and this test is checking almost nothing: {variants:?}",
        variants.len()
    );

    let missing: Vec<&String> = variants
        .iter()
        .filter(|v| !CONTROL_PLANE_ONLY.contains(&v.as_str()))
        .filter(|v| !translate.contains(&format!("ResponseBody::{v}")))
        .collect();

    assert!(
        missing.is_empty(),
        "these response variants have no arm in `translate.rs`, so they reach \
         every SDK as `kind: \"raw\"` with a Rust debug string in them: \
         {missing:?}\n\n\
         The catch-all in `from_wire` is why this compiled. Give each one an arm \
         naming the fields a host actually needs."
    );
}

#[test]
fn the_catch_all_is_still_there_and_still_last() {
    let translate = read("src/translate.rs");

    // If it were removed, a host talking to a newer server would stop degrading
    // — and the test above would keep passing while guarding nothing.
    let raw_at = translate.find("(\"raw\", serde_json::json!").expect(
        "the catch-all in `from_wire` is gone; a host can no longer \
                 degrade when it meets a response this build predates",
    );

    let last_named = translate
        .rfind("ResponseBody::")
        .expect("there are named arms");

    assert!(
        last_named < raw_at,
        "the catch-all now precedes a named arm, so it swallows it"
    );
}
