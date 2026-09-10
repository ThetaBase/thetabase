//! Which environment a checkout is in, worked out rather than configured.
//!
//! ROADMAP M7: "environment auto-detection from git branch context, so a PR
//! checkout resolves to `preview` without configuration".
//!
//! # Why this is worth getting right
//!
//! The alternative is an environment variable someone sets in CI. That works
//! until the day it is missing or stale, and the failure is silent and in the
//! worst direction: a preview job that quietly resolves to `prod` writes test
//! data into production, and nothing about the run looks unusual.
//!
//! So detection is deliberately conservative. `prod` is only ever chosen from an
//! explicit signal — the default branch, or an operator saying so. Everything
//! else, including "no idea", lands on a non-production environment. A preview
//! run that should have been production is an inconvenience; production written
//! by a PR is an incident.
//!
//! # Order of precedence
//!
//! 1. An explicit `--env` flag. A person overriding this is doing it on purpose.
//! 2. `THETA_ENV`, for a deploy that knows what it is.
//! 3. CI pull-request context — the strongest evidence of a preview there is.
//! 4. The git branch.
//! 5. `dev`, when nothing else is known.

use std::path::Path;
use std::process::Command;

/// Where a checkout should read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Dev,
    Preview,
    Prod,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Preview => "preview",
            Environment::Prod => "prod",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" => Some(Environment::Dev),
            "preview" | "staging" => Some(Environment::Preview),
            "prod" | "production" => Some(Environment::Prod),
            _ => None,
        }
    }
}

/// How an environment was arrived at.
///
/// Reported alongside the answer, and printed by `theta contexts`, because a
/// resolution a user cannot explain is one they cannot correct. "preview,
/// because GITHUB_EVENT_NAME=pull_request" is debuggable; "preview" is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub environment: Environment,
    pub because: String,
}

impl Resolved {
    fn new(environment: Environment, because: impl Into<String>) -> Self {
        Self {
            environment,
            because: because.into(),
        }
    }
}

/// The signals detection reads. Injected so the logic is testable without a
/// git repository, a CI runner, or a particular machine's environment.
pub trait Context {
    fn var(&self, key: &str) -> Option<String>;
    /// The current branch, or `None` outside a repository or on a detached head.
    fn git_branch(&self) -> Option<String>;
    /// The repository's default branch, if it can be determined.
    fn default_branch(&self) -> Option<String>;
}

/// Reads the real environment and the real git repository.
pub struct RealContext {
    root: std::path::PathBuf,
}

impl RealContext {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

impl Context for RealContext {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    fn git_branch(&self) -> Option<String> {
        // `--symbolic-full-name HEAD` returns "HEAD" on a detached head rather
        // than failing, which would otherwise read as a branch named HEAD.
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|b| b != "HEAD")
    }

    fn default_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "refs/remotes/origin/HEAD"])
            .and_then(|r| r.rsplit('/').next().map(str::to_string))
    }
}

/// Branch names that mean production when nothing else is known.
///
/// Only consulted when the repository cannot say what its default branch is.
/// A repository whose production branch is called something else is normal, and
/// asking git beats guessing from a list.
const CONVENTIONAL_DEFAULTS: &[&str] = &["main", "master"];

/// Work out the environment.
pub fn detect(context: &impl Context, explicit: Option<&str>) -> Resolved {
    // 1. An explicit flag. Someone typing `--env prod` means it.
    if let Some(text) = explicit {
        return match Environment::parse(text) {
            Some(environment) => Resolved::new(environment, format!("--env {text}")),
            // An unrecognised value is not silently downgraded to a default:
            // `--env prodd` must not quietly write to dev.
            None => Resolved::new(
                Environment::Dev,
                format!(
                    "`{text}` is not an environment — using dev. Expected dev, preview or prod"
                ),
            ),
        };
    }

    // 2. A deploy that knows what it is.
    if let Some(text) = context.var("THETA_ENV") {
        if let Some(environment) = Environment::parse(&text) {
            return Resolved::new(environment, format!("THETA_ENV={text}"));
        }
    }

    // 3. CI pull-request context. The strongest evidence of a preview: a PR
    //    build is a preview build whatever branch it happens to be on.
    if let Some(reason) = pull_request_signal(context) {
        return Resolved::new(Environment::Preview, reason);
    }

    // 4. The branch.
    if let Some(branch) = branch_of(context) {
        let default = context.default_branch();
        let is_default = match &default {
            Some(name) => &branch == name,
            // Only when git cannot say. Convention is a fallback, not the rule.
            None => CONVENTIONAL_DEFAULTS.contains(&branch.as_str()),
        };

        return match is_default {
            true => Resolved::new(
                Environment::Prod,
                format!("on the default branch `{branch}`"),
            ),
            false => Resolved::new(Environment::Preview, format!("on branch `{branch}`")),
        };
    }

    // 5. Nothing known. Dev, which is the environment where being wrong is
    //    cheapest.
    Resolved::new(
        Environment::Dev,
        "no branch or CI context — defaulting to dev",
    )
}

/// Whether CI says this is a pull request, and which signal said so.
fn pull_request_signal(context: &impl Context) -> Option<String> {
    // GitHub Actions.
    if context.var("GITHUB_EVENT_NAME").as_deref() == Some("pull_request")
        || context.var("GITHUB_EVENT_NAME").as_deref() == Some("pull_request_target")
    {
        return Some("GITHUB_EVENT_NAME is a pull request".into());
    }
    if let Some(head) = context.var("GITHUB_HEAD_REF") {
        // Only set for pull-request events; empty on a push build.
        return Some(format!("GITHUB_HEAD_REF={head}"));
    }

    // The other CI systems people actually use, so a ThetaBase project is not
    // GitHub-only by accident.
    for key in [
        "CHANGE_ID",                        // Jenkins multibranch
        "CI_MERGE_REQUEST_IID",             // GitLab
        "BITBUCKET_PR_ID",                  // Bitbucket
        "SYSTEM_PULLREQUEST_PULLREQUESTID", // Azure Pipelines
        "CIRCLE_PULL_REQUEST",              // CircleCI
    ] {
        if let Some(value) = context.var(key) {
            return Some(format!("{key}={value}"));
        }
    }
    None
}

/// The branch, preferring CI's idea of it.
///
/// In a PR build the checkout is often a detached merge commit, so git reports
/// no branch while CI knows exactly which one it is.
fn branch_of(context: &impl Context) -> Option<String> {
    if let Some(head) = context.var("GITHUB_HEAD_REF") {
        return Some(head);
    }
    if let Some(git_ref) = context.var("GITHUB_REF_NAME") {
        return Some(git_ref);
    }
    if let Some(branch) = context.var("CI_COMMIT_REF_NAME") {
        return Some(branch);
    }
    context.git_branch()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        vars: BTreeMap<String, String>,
        branch: Option<String>,
        default_branch: Option<String>,
    }

    impl Fake {
        fn var(mut self, key: &str, value: &str) -> Self {
            self.vars.insert(key.into(), value.into());
            self
        }
        fn branch(mut self, name: &str) -> Self {
            self.branch = Some(name.into());
            self
        }
        fn default_branch(mut self, name: &str) -> Self {
            self.default_branch = Some(name.into());
            self
        }
    }

    impl Context for Fake {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn git_branch(&self) -> Option<String> {
            self.branch.clone()
        }
        fn default_branch(&self) -> Option<String> {
            self.default_branch.clone()
        }
    }

    #[test]
    fn a_pull_request_checkout_resolves_to_preview_with_nothing_configured() {
        // The whole point of M7's detection clause.
        let context = Fake::default()
            .var("GITHUB_EVENT_NAME", "pull_request")
            .var("GITHUB_HEAD_REF", "feature/add-column")
            .branch("feature/add-column")
            .default_branch("main");

        assert_eq!(detect(&context, None).environment, Environment::Preview);
    }

    #[test]
    fn the_default_branch_resolves_to_prod() {
        let context = Fake::default().branch("main").default_branch("main");
        let resolved = detect(&context, None);

        assert_eq!(resolved.environment, Environment::Prod);
        assert!(
            resolved.because.contains("default branch"),
            "{}",
            resolved.because
        );
    }

    #[test]
    fn a_repository_whose_default_branch_is_not_main_is_still_understood() {
        // Asking git beats guessing from a list of conventional names.
        let context = Fake::default().branch("trunk").default_branch("trunk");
        assert_eq!(detect(&context, None).environment, Environment::Prod);

        // And `main` in that repository is an ordinary branch.
        let context = Fake::default().branch("main").default_branch("trunk");
        assert_eq!(detect(&context, None).environment, Environment::Preview);
    }

    #[test]
    fn convention_is_used_only_when_git_cannot_say() {
        let context = Fake::default().branch("main");
        assert_eq!(detect(&context, None).environment, Environment::Prod);

        let context = Fake::default().branch("some-feature");
        assert_eq!(detect(&context, None).environment, Environment::Preview);
    }

    #[test]
    fn any_other_branch_resolves_to_preview() {
        let context = Fake::default()
            .branch("dylan/fix-the-thing")
            .default_branch("main");
        assert_eq!(detect(&context, None).environment, Environment::Preview);
    }

    #[test]
    fn a_pull_request_from_the_default_branch_name_is_still_a_preview() {
        // A fork's `main` opened as a PR against our `main`. The PR signal wins,
        // because a PR build is a preview build whatever the branch is called.
        let context = Fake::default()
            .var("GITHUB_EVENT_NAME", "pull_request")
            .branch("main")
            .default_branch("main");

        assert_eq!(
            detect(&context, None).environment,
            Environment::Preview,
            "a pull request resolved to production"
        );
    }

    #[test]
    fn knowing_nothing_resolves_to_dev_and_never_to_prod() {
        // The direction of the default is the point: a preview run that should
        // have been production is an inconvenience, and production written by a
        // job that could not tell where it was is an incident.
        let resolved = detect(&Fake::default(), None);

        assert_eq!(resolved.environment, Environment::Dev);
        assert!(resolved.because.contains("defaulting to dev"));
    }

    #[test]
    fn an_explicit_flag_beats_everything_else() {
        let context = Fake::default()
            .var("GITHUB_EVENT_NAME", "pull_request")
            .branch("feature")
            .default_branch("main");

        assert_eq!(
            detect(&context, Some("prod")).environment,
            Environment::Prod,
            "someone typing --env prod means it"
        );
    }

    #[test]
    fn a_misspelled_environment_does_not_silently_become_a_default() {
        // `--env prodd` writing to dev without comment is how someone spends an
        // afternoon wondering why production is not changing.
        let resolved = detect(&Fake::default().branch("main"), Some("prodd"));

        assert_eq!(resolved.environment, Environment::Dev);
        assert!(
            resolved.because.contains("is not an environment"),
            "{}",
            resolved.because
        );
    }

    #[test]
    fn theta_env_is_read_when_no_flag_is_given() {
        let context = Fake::default().var("THETA_ENV", "prod").branch("feature");
        assert_eq!(detect(&context, None).environment, Environment::Prod);
    }

    #[test]
    fn a_pull_request_beats_theta_env_only_when_theta_env_is_unset() {
        // Precedence, stated: an operator setting THETA_ENV in a CI job is
        // making a deliberate choice and detection must not second-guess it.
        let context = Fake::default()
            .var("THETA_ENV", "prod")
            .var("GITHUB_EVENT_NAME", "pull_request");

        assert_eq!(detect(&context, None).environment, Environment::Prod);
    }

    #[test]
    fn ci_branch_context_is_preferred_over_a_detached_head() {
        // A PR build checks out a detached merge commit, so git reports no
        // branch while CI knows exactly which one it is.
        let context = Fake::default()
            .var("GITHUB_HEAD_REF", "feature/thing")
            .default_branch("main");

        let resolved = detect(&context, None);
        assert_eq!(resolved.environment, Environment::Preview);
        assert!(
            resolved.because.contains("feature/thing"),
            "{}",
            resolved.because
        );
    }

    #[test]
    fn every_resolution_says_why() {
        // A resolution a user cannot explain is one they cannot correct.
        for context in [
            Fake::default(),
            Fake::default().branch("main").default_branch("main"),
            Fake::default().branch("feature").default_branch("main"),
            Fake::default().var("GITHUB_EVENT_NAME", "pull_request"),
            Fake::default().var("THETA_ENV", "preview"),
        ] {
            let resolved = detect(&context, None);
            assert!(
                !resolved.because.trim().is_empty(),
                "{:?} was resolved without a reason",
                resolved.environment
            );
        }
    }

    #[test]
    fn other_ci_systems_are_recognised_too() {
        // A ThetaBase project should not be GitHub-only by accident.
        for key in [
            "CHANGE_ID",
            "CI_MERGE_REQUEST_IID",
            "BITBUCKET_PR_ID",
            "SYSTEM_PULLREQUEST_PULLREQUESTID",
            "CIRCLE_PULL_REQUEST",
        ] {
            let context = Fake::default()
                .var(key, "42")
                .branch("main")
                .default_branch("main");
            assert_eq!(
                detect(&context, None).environment,
                Environment::Preview,
                "{key} was not recognised as a pull request"
            );
        }
    }
}
