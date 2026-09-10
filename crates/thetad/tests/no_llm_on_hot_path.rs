//! CI guard: no LLM inference on the hot path.
//!
//! `05-prd.md` §3 and `09-sla-performance.md` §1 make this a product claim, and
//! `08-test-validation-plan.md` §4 requires it to be *asserted in CI, not just
//! measured once*. This is that assertion.
//!
//! It works structurally rather than by measurement: the crates that serve
//! `get`/`put`/`query` must not depend on an HTTP or model client at all, so
//! there is nothing to call. AI Query-Assist is a separate service reached by
//! the CLI/SDK, and is deliberately not in this dependency closure.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Crates that serve the typed read/write path.
const HOT_PATH_CRATES: &[&str] = &[
    "theta-core",
    "theta-storage",
    "theta-query",
    "theta-safety",
    "thetad",
];

/// Dependencies that would put a network or model call in reach. Matched against
/// dependency names in each crate's manifest.
const FORBIDDEN_DEPS: &[&str] = &[
    // AI Query-Assist (M9). The one crate in this workspace that may reach a
    // model, and therefore the one that must never appear here. Naming it
    // makes "Assist is a separate service" a build-graph fact rather than a
    // deployment convention: adding it to `thetad` fails this test.
    "theta-assist",
    "reqwest",
    "hyper-tls",
    "openai",
    "async-openai",
    "anthropic",
    "ureq",
    "curl",
    "langchain",
    "tiktoken",
    "candle-core",
    "llama-cpp",
    "ort",
    "tokenizers",
];

/// Call shapes that would indicate inference sneaking in through a transitive
/// path or a hand-rolled client.
const FORBIDDEN_CALLS: &[&str] = &[
    "completions.create",
    "chat.completions",
    "messages.create",
    "generate_text",
    "infer(",
];

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

/// The resolved dependency graph, as cargo sees it.
///
/// Read from `cargo metadata` rather than by grepping manifests. The grep this
/// replaces only ever saw each hot-path crate's *own* `Cargo.toml`, so a
/// forbidden crate one hop further away — a dependency that itself pulled
/// `reqwest` — passed cleanly while the doc comment above claimed a closure was
/// being checked. The claim is the useful one; this makes it true.
struct DepGraph {
    /// Package id to package name.
    names: BTreeMap<String, String>,
    /// Package id to the ids of its normal dependencies.
    ///
    /// Normal only. A dev-dependency is not built into anything that serves a
    /// request, and a build-dependency runs at compile time and is gone by the
    /// time there is a hot path to be on — so including either would fail the
    /// guard for code that cannot reach a request.
    normal_deps: BTreeMap<String, Vec<String>>,
}

impl DepGraph {
    fn load(root: &Path) -> Self {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--all-features"])
            .current_dir(root)
            .output()
            .expect("cargo metadata could not be run");

        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cargo metadata emitted invalid JSON");

        let mut names = BTreeMap::new();
        for package in metadata["packages"]
            .as_array()
            .expect("metadata has a package list")
        {
            names.insert(
                package["id"].as_str().expect("package id").to_string(),
                package["name"].as_str().expect("package name").to_string(),
            );
        }

        let mut normal_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in metadata["resolve"]["nodes"]
            .as_array()
            .expect("a resolved graph")
        {
            let id = node["id"].as_str().expect("node id").to_string();
            let mut deps = Vec::new();
            for dep in node["deps"].as_array().expect("node deps") {
                // `dep_kinds` carries one entry per edge; a null `kind` is a
                // normal dependency, "dev" and "build" are the others.
                let normal = dep["dep_kinds"]
                    .as_array()
                    .map(|kinds| kinds.iter().any(|k| k["kind"].is_null()))
                    .unwrap_or(false);
                if normal {
                    deps.push(dep["pkg"].as_str().expect("dep id").to_string());
                }
            }
            normal_deps.insert(id, deps);
        }

        Self { names, normal_deps }
    }

    /// Build a graph directly, for testing the walk itself.
    ///
    /// The workspace has no dependency that reaches a forbidden crate more than
    /// one hop away — which is lucky, and exactly why the walk needs a fixture
    /// that does. Otherwise the only evidence that this searches deeper than
    /// the old grep did would be that it happens to agree with it.
    fn from_edges(edges: &[(&str, &[&str])]) -> Self {
        let mut names = BTreeMap::new();
        let mut normal_deps = BTreeMap::new();
        for (from, tos) in edges {
            names.insert(from.to_string(), from.to_string());
            for to in *tos {
                names.insert(to.to_string(), to.to_string());
            }
            normal_deps.insert(
                from.to_string(),
                tos.iter().map(|t| t.to_string()).collect(),
            );
        }
        Self { names, normal_deps }
    }

    fn id_of(&self, name: &str) -> Option<&str> {
        self.names
            .iter()
            .find(|(_, n)| n.as_str() == name)
            .map(|(id, _)| id.as_str())
    }

    /// Breadth-first walk from `root_name`, returning the path to the first
    /// forbidden crate reached.
    ///
    /// The path is the point. "thetad depends on reqwest" is actionable;
    /// "something in thetad's closure depends on reqwest" sends someone
    /// spelunking through a lockfile.
    fn path_to_forbidden(&self, root_name: &str) -> Option<Vec<String>> {
        let root = self
            .id_of(root_name)
            .unwrap_or_else(|| panic!("`{root_name}` is not in the workspace"));

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue: std::collections::VecDeque<Vec<&str>> =
            std::collections::VecDeque::from([vec![root]]);

        while let Some(path) = queue.pop_front() {
            let id = *path.last().expect("a non-empty path");
            if !seen.insert(id) {
                continue;
            }

            let name = self.names.get(id).map(String::as_str).unwrap_or("?");
            if FORBIDDEN_DEPS.contains(&name) {
                return Some(
                    path.iter()
                        .map(|id| self.names.get(*id).cloned().unwrap_or_default())
                        .collect(),
                );
            }

            for dep in self.normal_deps.get(id).into_iter().flatten() {
                let mut next: Vec<&str> = path.clone();
                next.push(dep.as_str());
                queue.push_back(next);
            }
        }
        None
    }
}

#[test]
fn no_model_or_http_client_exists_anywhere_in_the_hot_paths_dependency_closure() {
    // The structural half of the no-LLM claim. `get`, `put` and typed `query`
    // cannot reach a model because there is no client anywhere beneath them to
    // reach — not in the crate, and not in anything it pulls in.
    let graph = DepGraph::load(&workspace_root());
    let mut findings = Vec::new();

    for crate_name in HOT_PATH_CRATES {
        if let Some(path) = graph.path_to_forbidden(crate_name) {
            findings.push(path.join(" → "));
        }
    }

    assert!(
        findings.is_empty(),
        "an LLM/HTTP client reached the hot path:\n  {}\n\nThe hot path must be \
         structurally incapable of a model call, not merely not making one \
         today (05-prd.md §3).",
        findings.join("\n  ")
    );
}

#[test]
fn the_walk_finds_a_client_several_hops_away() {
    // The bug this replaces: the old check read each hot-path crate's own
    // manifest and nothing further, so a forbidden crate reached through a
    // dependency passed cleanly while the doc comment claimed a closure had
    // been walked.
    let graph = DepGraph::from_edges(&[
        ("thetad", &["theta-storage", "serde"]),
        ("theta-storage", &["innocent-looking"]),
        ("innocent-looking", &["helpful-utils"]),
        ("helpful-utils", &["reqwest"]),
        ("serde", &[]),
        ("reqwest", &[]),
    ]);

    assert_eq!(
        graph.path_to_forbidden("thetad"),
        Some(vec![
            "thetad".to_string(),
            "theta-storage".to_string(),
            "innocent-looking".to_string(),
            "helpful-utils".to_string(),
            "reqwest".to_string(),
        ]),
        "the walk did not follow the chain to the client at its end"
    );
}

#[test]
fn the_walk_terminates_on_a_dependency_cycle() {
    // Cargo forbids cycles between packages, but the walk must not rely on
    // that to halt: a guard that hangs is a guard someone deletes.
    let graph = DepGraph::from_edges(&[("a", &["b"]), ("b", &["a"])]);
    assert_eq!(graph.path_to_forbidden("a"), None);
}

#[test]
fn the_walk_reports_the_shortest_route_it_found() {
    // Breadth-first, so the reported path is the shortest one. Someone reading
    // a failure wants the most direct edge to remove, not whichever route the
    // search wandered down first.
    let graph = DepGraph::from_edges(&[
        ("root", &["long-way", "reqwest"]),
        ("long-way", &["middle"]),
        ("middle", &["reqwest"]),
        ("reqwest", &[]),
    ]);

    assert_eq!(
        graph.path_to_forbidden("root"),
        Some(vec!["root".to_string(), "reqwest".to_string()])
    );
}

#[test]
fn the_guard_would_notice_a_client_one_hop_away() {
    // The bug this test's neighbour used to have: the check read each hot-path
    // crate's own manifest, so a forbidden crate reached through a dependency
    // passed cleanly while claiming a closure had been walked.
    //
    // `theta-control` legitimately depends on `reqwest` — it calls OAuth
    // providers — and nothing on the hot path depends on `theta-control`. That
    // makes it the honest fixture for "can this walk see past one hop": if the
    // guard cannot find reqwest from there, it could not find it from anywhere.
    let graph = DepGraph::load(&workspace_root());

    let path = graph
        .path_to_forbidden("theta-control")
        .expect("the walk found nothing from a crate that really does depend on reqwest");

    assert!(path.len() >= 2, "expected a path, got {path:?}");
    assert_eq!(path.first().map(String::as_str), Some("theta-control"));
    assert!(
        FORBIDDEN_DEPS.contains(&path.last().expect("an end").as_str()),
        "{path:?}"
    );
}

#[test]
fn hot_path_sources_contain_no_inference_calls() {
    let root = workspace_root();
    let mut findings = Vec::new();

    for crate_name in HOT_PATH_CRATES {
        let mut sources = Vec::new();
        rust_sources(
            &root.join("crates").join(crate_name).join("src"),
            &mut sources,
        );

        for path in sources {
            let text = fs::read_to_string(&path).expect("source file is readable");
            for (lineno, line) in text.lines().enumerate() {
                // Doc comments discuss the constraint; they do not violate it.
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for needle in FORBIDDEN_CALLS {
                    if code.contains(needle) {
                        findings.push(format!(
                            "{}:{}: `{needle}`",
                            path.strip_prefix(&root).unwrap_or(&path).display(),
                            lineno + 1
                        ));
                    }
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "inference call found on the hot path: {findings:#?}"
    );
}

/// The guard is only meaningful if it would actually fire. This checks the
/// detector itself, so a refactor that quietly breaks the scan is caught.
#[test]
fn the_guard_detects_a_planted_violation() {
    let sample = "let response = client.chat.completions(request);";
    assert!(
        FORBIDDEN_CALLS.iter().any(|needle| sample.contains(needle)),
        "the forbidden-call list no longer matches a real inference call"
    );
}
