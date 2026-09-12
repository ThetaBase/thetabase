//! The first two commands a new customer runs have to work.
//!
//! Both of these were broken, and both were found the same way: by installing
//! nothing, typing what the documentation says, and reading the error.
//!
//! **`theta login` defaulted to a provider the deployment does not offer.**
//! `--provider` defaulted to `google`; the hosted Control Plane offers only
//! `github`. So the first command in the README failed with "this deployment
//! does not offer google login (see its provider settings)" — pointing a new
//! customer at settings they do not administer, on a deployment that was
//! working correctly.
//!
//! **The CLI pointed at localhost.** `--control-url` defaulted to
//! `http://127.0.0.1:8080`, so the same first command answered "Is it running?
//! Start one locally with `theta-control --dev-seed`" — telling somebody who
//! just installed a hosted product to run the server.
//!
//! Neither is the kind of bug a unit test finds, because both were *defaults
//! chosen for the person developing this repository*. Every test in the suite
//! passed with them in place, because every test supplies its own values. So
//! these assert the defaults themselves.

use std::collections::BTreeSet;

/// The CLI's own definition of what it accepts.
const CLI: &str = include_str!("../src/main.rs");

#[test]
fn the_control_plane_defaults_to_the_hosted_service() {
    // Read out of the source, which is a weak kind of test — guarded the way
    // that kind has to be: it asserts it found the declaration before asserting
    // anything about it.
    let declaration = CLI
        .split("control_url: String")
        .next()
        .expect("main.rs has a control_url argument");
    let default = declaration
        .rsplit("default_value = \"")
        .next()
        .and_then(|tail| tail.split('"').next())
        .expect("control_url has a default_value");

    assert!(
        default.starts_with("https://"),
        "the CLI's default control plane is `{default}`. Somebody who has just \
         run the install script and typed `theta login` will be told to start a \
         server."
    );
    assert!(
        !default.contains("127.0.0.1") && !default.contains("localhost"),
        "the CLI defaults to a control plane on the user's own machine: `{default}`"
    );

    // And the escape hatch a contributor needs is still there, or working on
    // this repository means passing a flag to every command.
    //
    // Matched on the attribute, not on the name appearing somewhere nearby. I
    // planted the removal of `env = ...` and this passed: the doc comment above
    // the argument mentions the variable, so the assertion was reading my own
    // prose rather than the declaration.
    assert!(
        declaration.contains("env = \"THETA_CONTROL_URL\""),
        "there is no environment variable to point the CLI at a local control \
         plane, so developing against one means a flag on every invocation"
    );
}

#[test]
fn login_does_not_default_to_a_provider_of_its_own_choosing() {
    // The fix is that the default *is the deployment's answer*: ask what is
    // configured and, if there is exactly one, use it. A compiled-in default
    // cannot be right, because which providers exist is a property of the
    // deployment and not of this binary.
    let login = CLI
        .split("    Login {")
        .nth(1)
        .and_then(|body| body.split("},").next())
        .expect("main.rs has a Login command");

    assert!(
        !login.contains("default_value"),
        "`theta login` carries a compiled-in provider default. Which providers \
         exist is a property of the deployment, so any value here is wrong for \
         some deployment -- and was wrong for the hosted one: {login}"
    );
    assert!(
        login.contains("Option<String>"),
        "`--provider` is not optional, so there is no way to ask the deployment \
         what it offers"
    );
}

#[test]
fn a_deployment_with_one_provider_needs_no_flag_and_several_needs_one() {
    // The resolution logic, read from the source for the same reason as above:
    // exercising it needs a Control Plane, and the bug was in the default
    // rather than in the flow.
    let body = CLI
        .split("async fn login(")
        .nth(1)
        .and_then(|rest| rest.split("\nasync fn ").next())
        .expect("main.rs has a login function");

    assert!(
        body.contains("configured.len() == 1"),
        "nothing uses a single configured provider as the default, which is the \
         case that used to fail"
    );
    assert!(
        body.contains("theta login --provider"),
        "a deployment offering several providers does not tell the caller how to \
         choose one"
    );
    assert!(
        body.contains("is_empty()"),
        "a deployment with no provider configured is not distinguished from one \
         whose provider was not matched, so the error would blame the caller"
    );
}

#[test]
fn every_provider_the_cli_can_parse_is_named_in_an_error() {
    // A provider name the CLI accepts but never mentions is a name nobody can
    // discover. This is the weakest of the four and it is here because the old
    // error hard-coded "try `google` or `github`" — a list that was correct
    // when written and is a second copy of the enum.
    let kinds: BTreeSet<&str> = CLI
        .split("ProviderKind::parse(")
        .skip(1)
        .map(|_| "parse")
        .collect();
    assert!(
        !kinds.is_empty(),
        "the login flow no longer parses a provider name at all"
    );

    let body = CLI
        .split("async fn login(")
        .nth(1)
        .and_then(|rest| rest.split("\nasync fn ").next())
        .expect("login function");
    assert!(
        !body.contains("try `google` or `github`"),
        "the unknown-provider error still hard-codes a provider list, which is a \
         second copy of the enum and was already out of step with the \
         deployment"
    );
    assert!(
        body.contains("this deployment offers"),
        "the unknown-provider error does not say what the deployment actually \
         offers, which is the only list that is ever right"
    );
}
