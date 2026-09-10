//! Generates the ThetaBase SDK surfaces from `theta.capnp`.
//!
//! `02-api-wire-protocol.md` §3: bindings are generated from the single Cap'n
//! Proto schema, "kept in lockstep since they compile from one source of truth,
//! avoiding drift between an agent's generated code and the live schema".
//!
//! The drift is not hypothetical. Before this existed, the hand-written SDK
//! surfaces claimed a `schema.apply(changeId, change, confirm)` that the server
//! had stopped accepting, and had no `reject` at all — because a person has to
//! remember to update two files in two languages every time the schema moves,
//! and eventually does not.
//!
//! Run with `make sdk`. `make sdk-check` regenerates into a temporary directory
//! and fails if the committed output differs, so a schema change that was not
//! regenerated fails the build rather than shipping.

use theta_codegen::{csharp, golang, java, python, ruby, schema, swift, typescript};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One file the generator owns, start to finish.
pub struct Generated {
    pub path: PathBuf,
    pub contents: String,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let check_only = args.iter().any(|a| a == "--check");
    let root = workspace_root()?;

    let schema_dir = root.join("crates/theta-proto/schema");
    let schema = schema::load(&schema_dir.join("theta.capnp"), &schema_dir)
        .context("could not read the wire schema")?;

    let outputs = vec![
        Generated {
            path: root.join("sdk/typescript/src/generated.ts"),
            contents: typescript::render(&schema),
        },
        Generated {
            path: root.join("sdk/python/src/thetabase/generated.py"),
            contents: python::render(&schema),
        },
        Generated {
            path: root.join("sdk/go/generated.go"),
            contents: golang::render(&schema),
        },
        Generated {
            path: root.join("sdk/java/src/main/java/io/thetabase/Generated.java"),
            contents: java::render(&schema),
        },
        Generated {
            path: root.join("sdk/csharp/src/Generated.cs"),
            contents: csharp::render(&schema),
        },
        Generated {
            path: root.join("sdk/ruby/lib/thetabase/generated.rb"),
            contents: ruby::render(&schema),
        },
        Generated {
            path: root.join("sdk/swift/Sources/ThetaBase/Generated.swift"),
            contents: swift::render(&schema),
        },
    ];

    let mut drifted = Vec::new();
    for output in &outputs {
        let current = std::fs::read_to_string(&output.path).unwrap_or_default();
        if current == output.contents {
            continue;
        }
        match check_only {
            true => drifted.push(output.path.clone()),
            false => {
                if let Some(parent) = output.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&output.path, &output.contents)
                    .with_context(|| format!("writing {}", output.path.display()))?;
                println!("wrote {}", relative(&root, &output.path));
            }
        }
    }

    if !drifted.is_empty() {
        eprintln!("The SDK bindings are out of date with the wire schema:\n");
        for path in &drifted {
            eprintln!("  {}", relative(&root, path));
        }
        eprintln!(
            "\nThe schema moved and the bindings did not. Run `make sdk` and commit \
             the result.\n\nThis check exists because the alternative is an SDK that \
             describes a protocol the server no longer speaks — which is worse than \
             no SDK, because it looks like it works."
        );
        std::process::exit(1);
    }

    if check_only {
        println!("SDK bindings match the wire schema.");
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn workspace_root() -> Result<PathBuf> {
    // The crate directory is `<root>/crates/theta-codegen`.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("could not locate the workspace root")
}
