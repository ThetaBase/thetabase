//! No code path accepts two project identifiers (ROADMAP-V3 M26, item 2).
//!
//! # What this discharges, and what it does not
//!
//! `specs/04` §3 claims cross-project queries are *architecturally impossible*
//! rather than access-controlled: one project is one process, and no code path
//! accepts two project identifiers. External review 2 exists precisely because
//! that is the strongest claim in the product with the least in-repo evidence —
//! it has an architecture diagram and an assertion, where every other claim has
//! a test.
//!
//! **This is not the WASM sandbox M26 asks for.** That would make the boundary a
//! compiler-enforced property of a sandbox, and it is not built. What this does
//! is turn the *code half* of the claim into something checkable: today the
//! sentence "no code path accepts two project identifiers" is true because
//! somebody read the code, and after this it is true because a test fails if it
//! stops being.
//!
//! The deployment half — that two projects really are two processes — is still a
//! property of how ThetaBase is run, still has no in-repo evidence, and is still
//! what review 2 is for. Saying so here rather than letting this test be read as
//! closing the whole item.
//!
//! # Why it reads the source
//!
//! The property is about the shape of the code, so there is nowhere else to
//! stand. `no_llm_on_hot_path.rs` reads the dependency graph for the same reason
//! and `route_coverage.rs` reads a route table for the same reason; this is the
//! third instance of the same argument, which is roughly when a pattern stops
//! being a hack and starts being how this repository checks structural claims.

/// Crates that serve a single project's data.
///
/// The Control Plane is deliberately absent: it is *supposed* to know about
/// every project, that is its job, and including it would make this test fail
/// for the one component whose whole purpose is the thing being forbidden
/// elsewhere.
const SINGLE_PROJECT_CRATES: &[&str] = &[
    "theta-core",
    "theta-storage",
    "theta-query",
    "thetad",
    // Added when Assist stopped being one shared process serving every caller.
    // That shape is what made a cache key missing the tenant into a
    // cross-project leak (external review R2-02): a shared cache is only a
    // cross-project surface because the process is shared. Assist now takes one
    // project at startup and refuses a question about any other, so it belongs
    // under the same rule as everything else that holds one project's data.
    "theta-assist",
];

/// Files read for this check, with their crate.
fn sources() -> Vec<(&'static str, &'static str, String)> {
    let crates_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate lives under crates/");

    let mut out = Vec::new();
    for crate_name in SINGLE_PROJECT_CRATES {
        collect(
            &crates_dir.join(crate_name).join("src"),
            crate_name,
            &mut out,
        );
    }
    out
}

fn collect(
    dir: &std::path::Path,
    crate_name: &'static str,
    out: &mut Vec<(&'static str, &'static str, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, crate_name, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name: &'static str = Box::leak(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string()
                .into_boxed_str(),
        );
        out.push((crate_name, name, text));
    }
}

/// Strip comments and test modules.
///
/// A doc comment naming two projects is prose, and a test fixture that builds
/// two of them is exercising the boundary rather than crossing it. Including
/// either would make this test fail on the writing *about* the property, which
/// is the fastest way to get a structural check deleted.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tests = false;
    let mut depth = 0i32;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            in_tests = true;
            depth = 0;
            continue;
        }
        if in_tests {
            depth += line.matches('{').count() as i32;
            depth -= line.matches('}').count() as i32;
            if depth <= 0 && line.contains('}') {
                in_tests = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn no_single_project_crate_takes_two_project_identifiers() {
    // The shape a cross-project path would have: a function signature naming a
    // project twice, or naming a source and a target project. There is no
    // legitimate reason for one in a crate that serves a single project's data,
    // and it is the first thing that would appear if somebody added one.
    let forbidden: &[&str] = &[
        "source_project",
        "target_project",
        "from_project",
        "to_project",
        "other_project",
        "project_a",
        "project_b",
        "projects:",
        "project_ids",
    ];

    let mut found: Vec<String> = Vec::new();
    let mut scanned = 0;

    for (crate_name, file, text) in sources() {
        scanned += 1;
        let code = code_only(&text);
        for needle in forbidden {
            if code.contains(needle) {
                found.push(format!("{crate_name}/{file}: `{needle}`"));
            }
        }
    }

    // Guard the reader before trusting it. A source-reading test that silently
    // reads nothing passes forever, which is the first failure mode this kind of
    // test has to rule out about itself.
    assert!(
        scanned >= 20,
        "only {scanned} source files were read; the reader has stopped finding them \
         and this test is no longer checking anything"
    );

    assert!(
        found.is_empty(),
        "these look like paths that could carry two projects at once:\n  {}\n\n\
         `specs/04` §3 claims cross-project queries are architecturally \
         impossible rather than access-controlled. If one of these is a false \
         positive, rename it; if it is real, the claim needs narrowing before \
         the code lands.",
        found.join("\n  ")
    );
}

#[test]
fn the_engine_is_constructed_with_exactly_one_project() {
    // The positive half. A test that only forbids things passes on an empty
    // codebase, so this pins that the single-project shape is actually the one
    // in use: `Config` carries one project id, and nothing offers a second.
    let config = thetad::Config::dev_default("only-one");
    assert_eq!(config.project_id, "only-one");

    let serialised = serde_json::to_string(&config).expect("config is serialisable");
    let occurrences =
        serialised.matches("projectId").count() + serialised.matches("project_id").count();
    assert_eq!(
        occurrences, 1,
        "a configuration carrying more than one project identifier is the shape \
         a cross-project path would need: {serialised}"
    );
}

// The limitation above — that this checks the code half and not the deployment
// half — deliberately has no test here.
//
// The obvious one reads this file with `include_str!` and asserts the caveat is
// still in it. That cannot fail: the assertion's own string literal is in the
// file being read, so the test satisfies itself. It was written, it passed, and
// removing the caveat left it passing — which is the third time in this
// milestone sequence that a test turned out to be checking nothing.
//
// Prose cannot guard itself. The mechanism that can is `docs/claims.toml`, which
// pins every published statement to a verbatim quote in a spec and fails when the
// spec stops saying it. So the limitation is filed there as the non-claim
// `in-process-isolation-is-checked-in-code-not-in-deployment`, and the registry
// enforces it.
