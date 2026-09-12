//! Every SDK has to claim the version the workspace is releasing.
//!
//! Six manifests carry a version string of their own — Java's `pom.xml`,
//! Ruby's `VERSION`, Python's `pyproject.toml`, TypeScript's `package.json`
//! and its lock file — and nothing checked any of them against
//! `[workspace.package] version`. Bumping the workspace to `0.0.2` left all
//! six saying `0.0.1`, and neither `cargo test` nor any SDK's own build had
//! anything to say about it.
//!
//! That matters more than a cosmetic mismatch. A published package whose
//! version disagrees with the release it belongs to cannot be matched to the
//! engine it was generated from, and these clients are *generated from one
//! protocol definition* — being able to say which schema a client was built
//! against is the property that makes that worth doing.
//!
//! Swift and Go are absent on purpose: SwiftPM and Go modules take their
//! version from the git tag, so there is no string in the tree to drift.
//!
//! A source-reading test, guarded the way that kind has to be: it asserts it
//! found each version before comparing it, so a manifest that moved cannot
//! make the check quietly vacuous.

use std::path::{Path, PathBuf};

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read(relative: &str) -> String {
    let path = repo(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{relative} should exist: {e}"))
}

/// The version the workspace is releasing.
fn workspace_version() -> String {
    let manifest = read("Cargo.toml");
    let value = manifest
        .split("[workspace.package]")
        .nth(1)
        .and_then(|tail| tail.split("version = \"").nth(1))
        .and_then(|tail| tail.split('"').next())
        .expect("Cargo.toml declares [workspace.package] version");

    assert!(
        value.split('.').count() == 3,
        "`{value}` is not a three-part version, so this test is reading the \
         wrong field"
    );
    value.to_string()
}

/// One version string out of a manifest, by the text that precedes it.
fn declared(relative: &str, prefix: &str) -> String {
    let text = read(relative);
    let found = text
        .split(prefix)
        .nth(1)
        .and_then(|tail| tail.split(['"', '<']).next().map(str::trim))
        .unwrap_or_else(|| {
            panic!("{relative} has no `{prefix}…` — the manifest's shape has changed")
        });

    assert!(
        !found.is_empty() && found.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "`{found}` read out of {relative} is not a version, so the comparison \
         below would be meaningless"
    );
    found.to_string()
}

#[test]
fn every_sdk_manifest_agrees_with_the_workspace_version() {
    let expected = workspace_version();

    // `(file, the text immediately before the version)`.
    let manifests = [
        ("sdk/java/pom.xml", "<version>"),
        ("sdk/ruby/lib/thetabase.rb", "VERSION = \""),
        ("sdk/python/pyproject.toml", "version = \""),
        ("sdk/typescript/package.json", "\"version\": \""),
        ("sdk/typescript/package-lock.json", "\"version\": \""),
    ];

    for (relative, prefix) in manifests {
        let found = declared(relative, prefix);
        assert_eq!(
            found, expected,
            "{relative} claims version `{found}` and the workspace is releasing \
             `{expected}`. A published client whose version disagrees with its \
             release cannot be matched to the protocol definition it was \
             generated from."
        );
    }
}

/// The lock file carries the package's version twice, and both have to move.
///
/// `npm ci` fails when `package.json` and `package-lock.json` disagree, and the
/// SDK job in CI runs `npm ci` — so this would surface there. It is asserted
/// here as well because the failure text `npm` produces names neither version,
/// and a release blocked on a lock file should say so in one line.
#[test]
fn the_typescript_lock_file_has_no_stale_version_left_in_it() {
    let expected = workspace_version();
    let lock = read("sdk/typescript/package-lock.json");

    let stale: Vec<&str> = lock
        .split("\"version\": \"")
        .skip(1)
        .filter_map(|tail| tail.split('"').next())
        // Only this package's own version, not its dependencies'.
        .filter(|v| v.starts_with("0.0.") && *v != expected)
        .collect();

    assert!(
        stale.is_empty(),
        "package-lock.json still carries {stale:?} where the workspace is \
         releasing `{expected}`, so `npm ci` will refuse"
    );
}

/// The install script must not pin a version the release does not produce.
///
/// It defaults to `latest`, which is right. A hardcoded version here would be a
/// seventh place to forget.
#[test]
fn the_install_script_does_not_pin_a_version() {
    let install = read("install.sh");
    let line = install
        .lines()
        .find(|l| l.contains("VERSION="))
        .expect("install.sh sets VERSION");

    assert!(
        line.contains("latest"),
        "install.sh pins a version: `{line}`. A pin here is a seventh place to \
         forget on a release, and it is the one a customer runs."
    );
}
