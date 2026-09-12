//! Every target CI invokes has to exist in the Makefile.
//!
//! This is here because I destroyed the Makefile and shipped it.
//!
//! A test of the `if_member` guard ran under WSL with a `cd` that failed
//! silently, so `cat frag.mk > Makefile` executed in the repository root and
//! replaced 370 lines with the 10-line fragment it had just extracted. `git
//! add -A` committed it, the export copied it, and it was pushed and tagged.
//! Six CI jobs then failed with `No rule to make target 'identity'`, `'wasm'`,
//! `'query'` — one broken file wearing six different faces, none of which named
//! the Makefile.
//!
//! Nothing caught it in between. The full test suite passed 2,007 tests,
//! because no test reads the Makefile; `cargo build` does not either. The one
//! thing that would have noticed was running a gate, and gates are what CI
//! runs *after* the push.
//!
//! So: the workflow is the source of truth for which targets must exist, and
//! this checks the Makefile against it. It catches a Makefile that lost its
//! targets, a target renamed out from under CI, and a workflow that invokes one
//! that was never written.

use std::collections::BTreeSet;
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

/// Targets declared in the Makefile.
///
/// A target is a line starting at column zero with a name and a colon.
/// Deliberately not parsing variables or `.PHONY`: the question is only whether
/// `make <name>` has a rule to run.
fn makefile_targets() -> BTreeSet<String> {
    let makefile = read("Makefile");
    let found: BTreeSet<String> = makefile
        .lines()
        .filter(|line| !line.starts_with([' ', '\t', '#', '.']))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_string())
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
        })
        .collect();

    // The guard every source-reading test needs. A Makefile reduced to a
    // fragment parses fine and yields almost nothing, and without this the
    // assertions below would be comparing against an empty set.
    assert!(
        found.len() >= 20,
        "only {} targets found in the Makefile. It has been truncated or its \
         shape has changed -- the gates alone account for more than this. \
         Found: {found:?}",
        found.len()
    );
    found
}

/// Every `make <target>` the CI workflow runs.
fn targets_ci_invokes() -> BTreeSet<String> {
    let workflow = read(".github/workflows/ci.yml");
    let found: BTreeSet<String> = workflow
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- run: make "))
        .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        .filter(|name| !name.is_empty() && !name.starts_with('-'))
        .collect();

    assert!(
        !found.is_empty(),
        "no `make` invocations were found in ci.yml, so this test is checking \
         nothing"
    );
    found
}

#[test]
fn every_make_target_ci_invokes_exists() {
    let declared = makefile_targets();
    let invoked = targets_ci_invokes();

    let missing: Vec<&String> = invoked.difference(&declared).collect();
    assert!(
        missing.is_empty(),
        "CI runs `make` on targets the Makefile does not declare: {missing:?}.\n\
         Either the workflow names a target that was renamed, or the Makefile \
         has been damaged. It declares: {declared:?}"
    );
}

/// The aggregate target, and the gates it chains.
///
/// `gates` is the one target a human runs before a release, and it is a list of
/// prerequisites rather than a recipe -- so a gate dropped from that list stops
/// running with nothing to show for it.
#[test]
fn the_gates_target_still_chains_its_gates() {
    let makefile = read("Makefile");
    let declared = makefile_targets();

    let line = makefile
        .lines()
        .find(|l| l.starts_with("gates:"))
        .expect("the Makefile declares a `gates` target");

    let prerequisites: Vec<&str> = line
        .trim_start_matches("gates:")
        .split("##")
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();

    assert!(
        prerequisites.len() >= 10,
        "`gates` chains only {} prerequisites, which is fewer than the gates \
         this project has: {prerequisites:?}",
        prerequisites.len()
    );

    for gate in prerequisites {
        assert!(
            declared.contains(gate),
            "`gates` depends on `{gate}`, which is not a target. Running \
             `make gates` would stop there."
        );
    }
}

/// The release workflow's build line, for the same reason.
///
/// The Makefile is not the only build file a stray redirect could truncate, and
/// the release workflow is the one whose breakage is discovered by a customer
/// downloading a binary that was never produced.
#[test]
fn the_release_workflow_still_builds_the_published_binaries() {
    let release = read(".github/workflows/release.yml");

    assert!(
        release.contains("tags:"),
        "the release workflow no longer triggers on a tag"
    );

    let build = release
        .lines()
        .find(|l| l.contains("cargo build") && l.contains("--locked"))
        .expect("the release workflow builds with --locked");

    for crate_name in ["theta-cli", "theta-mcp"] {
        assert!(
            build.contains(crate_name),
            "the release no longer builds `{crate_name}`, so the archive would \
             be missing a binary the installer expects:\n  {build}"
        );
    }
}
