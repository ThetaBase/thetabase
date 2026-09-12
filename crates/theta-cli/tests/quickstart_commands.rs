//! Every command the quickstart tells somebody to run has to exist.
//!
//! The README's quickstart is the first thing a prospect does and the script
//! for the launch demo recording. A command renamed in `main.rs` breaks it
//! silently — the tests all pass, the docs still read fine, and the failure
//! surfaces as a new user typing something that does not work in their first
//! ninety seconds.
//!
//! This is a source-reading test, which is a weak kind. It is guarded against
//! the way that kind usually fails: it asserts it found something first.

use std::collections::BTreeSet;

/// The CLI's own definition of what it accepts.
const CLI: &str = include_str!("../src/main.rs");
const README: &str = include_str!("../../../README.md");
const INSTALL: &str = include_str!("../../../install.sh");

/// Subcommand names, as clap derives them: a bare variant in the `Command`
/// enum becomes its kebab-case name.
fn declared_subcommands() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let body = CLI
        .split("enum Command {")
        .nth(1)
        .expect("main.rs declares a Command enum");

    for line in body.lines() {
        let line = line.trim();
        // `Demo,` / `Contexts,` / `Use {` / `Branch(BranchCommand),`
        let name = line
            .trim_end_matches(',')
            .split(['{', '('])
            .next()
            .unwrap_or("")
            .trim();
        if name.len() > 1
            && name.starts_with(|c: char| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric())
        {
            found.insert(kebab(name));
        }
        if line == "}" {
            break;
        }
    }
    found
}

fn kebab(variant: &str) -> String {
    let mut out = String::new();
    for (i, c) in variant.char_indices() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[test]
fn the_subcommand_extractor_actually_found_the_commands() {
    // Guard the extractor before trusting it. A source-reading test that
    // silently matches nothing passes forever and proves nothing.
    let declared = declared_subcommands();
    assert!(
        declared.len() >= 8,
        "only {} subcommands were extracted from main.rs; the extractor has \
         stopped matching and the checks below are no longer checking anything: \
         {declared:?}",
        declared.len()
    );
    for expected in ["login", "use", "demo", "schema"] {
        assert!(
            declared.contains(expected),
            "`{expected}` was not extracted, so the extractor is wrong rather \
             than the CLI: {declared:?}"
        );
    }
}

#[test]
fn every_command_the_readme_quickstart_names_exists() {
    let declared = declared_subcommands();

    // Taken from the quickstart block, in the order a new user types them.
    for command in ["login", "use", "demo", "schema"] {
        assert!(
            README.contains(&format!("theta {command}")),
            "the README quickstart no longer mentions `theta {command}`; if the \
             flow changed, this list changed with it"
        );
        assert!(
            declared.contains(command),
            "the README tells a new user to run `theta {command}` and the CLI \
             has no such subcommand"
        );
    }
}

#[test]
fn the_install_script_points_at_the_quickstart_that_follows_it() {
    // Somebody who has just installed is at the highest-intent moment they will
    // ever be. The script ending without telling them what to type next wastes
    // it, and the commands it prints have to be real ones.
    let declared = declared_subcommands();

    for command in ["login", "use", "demo"] {
        assert!(
            INSTALL.contains(&format!("theta {command}")),
            "the install script stopped naming `theta {command}` as a next step"
        );
        assert!(declared.contains(command));
    }
}

#[test]
fn the_demo_writes_the_file_the_readme_tells_people_to_propose() {
    // The one join between two files that nothing else would catch: `demo`
    // writes a change file, and the README's next line proposes it *by name*.
    let commands = include_str!("../src/commands.rs");

    let declared_name = commands
        .split("const DEMO_CHANGE_FILE: &str = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("commands.rs declares the demo's change file name");

    assert!(
        README.contains(&format!("theta schema propose {declared_name}")),
        "`theta demo` writes {declared_name}, and the README tells people to \
         propose something else"
    );
}

#[test]
fn the_readme_does_not_promise_the_gate_is_a_wall() {
    // `docs/legal/TERMS.md` §2.1 is explicit that a change you confirm is a
    // change you asked for, and the quickstart is where most people form their
    // idea of what the Safety Layer does. A quickstart that implied the gate
    // prevents loss would be the single most expensive inconsistency available
    // to this product.
    let quickstart = README
        .split("## Getting started")
        .nth(1)
        .expect("the README has a getting started section")
        .split("### Building from source")
        .next()
        .expect("terminated by the build section");

    assert!(
        quickstart.contains("still happens") || quickstart.contains("still drops"),
        "the quickstart no longer says that a confirmed change is applied; a \
         reader who takes the gate for a guarantee has been misled by us"
    );
}

/// Every command in the quickstart has to *parse*, not only exist.
///
/// `quickstart` above checks that the README's subcommand names are declared.
/// It cannot catch a documented invocation the CLI rejects -- and the README
/// said `theta login github` for months, which is a positional argument on a
/// command that takes `--provider`. Typing it answers "unexpected argument
/// 'github' found", in the first ninety seconds, from the page that is also the
/// launch demo script.
#[test]
fn no_quickstart_command_passes_an_argument_the_cli_does_not_accept() {
    // Which subcommands take a positional argument, read from the CLI's own
    // declaration. A variant with no fields, or only `#[arg(long)]` fields,
    // accepts none.
    let body = CLI
        .split("enum Command {")
        .nth(1)
        .and_then(|rest| rest.split("
}").next())
        .expect("main.rs declares a Command enum");

    for line in README.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("theta ") else {
            continue;
        };
        // Only the shell blocks, not prose mentioning a command.
        let mut words = rest.split_whitespace();
        let Some(subcommand) = words.next() else {
            continue;
        };
        if !subcommand.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            continue;
        }

        // The next word, if it is not a flag or a comment, is a positional.
        let Some(next) = words.next() else { continue };
        if next.starts_with('-') || next.starts_with('#') {
            continue;
        }

        // Find the variant and see whether it declares a bare field.
        let variant: String = subcommand
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                    None => String::new(),
                }
            })
            .collect();

        let Some(declaration) = body
            .split(&format!("
    {variant} {{"))
            .nth(1)
            .and_then(|rest| rest.split("
    },").next())
        else {
            // Either a subcommand group (`Plan`, `Branch`, `Schema`) or one
            // with no block. Those take positionals through their own enums and
            // are out of scope here.
            continue;
        };

        // A positional is a field with no `#[arg(long` above it. Approximated
        // by asking whether the variant declares any field at all that is not
        // behind a long flag.
        let has_positional = declaration
            .lines()
            .filter(|l| l.trim_end().ends_with(',') && l.contains(": "))
            .any(|field| {
                let name = field.split(':').next().unwrap_or("").trim();
                !name.starts_with('#')
                    && !declaration
                        .split(field)
                        .next()
                        .unwrap_or("")
                        .rsplit("
")
                        .take(3)
                        .any(|prev| prev.contains("#[arg(long"))
            });

        assert!(
            has_positional,
            "the README runs `theta {subcommand} {next}`, and `{variant}`              declares no positional argument -- so the CLI answers \"unexpected              argument\" to a command on its own quickstart"
        );
    }
}
