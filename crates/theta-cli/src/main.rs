//! The `theta` CLI.
//!
//! Everyday work happens here, not in a dashboard
//! (`06-provisioning-identity-flow.md` §5). Two rules shape the surface:
//!
//! * **A scoped token is never printed by default.** `theta use` resolves a
//!   context and stores it; `theta exec` injects it into a child process's
//!   environment. Printing one is an explicit, logged action
//!   (`04-threat-model-security.md` §2).
//! * **Never a silent guess.** An ambiguous hint produces one question, and an
//!   unknown project produces one confirmation.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
mod commands;

use theta_cli::client::{ClientError, ControlPlaneClient, ResolveOutcome};
use theta_cli::login as login_flow;
use theta_cli::store::{CachedContext, CredentialStore, Credentials, FileReason};
use theta_identity::oauth::ProviderKind;

#[derive(Parser, Debug)]
#[command(
    name = "theta",
    version,
    about = "ThetaBase — the agent-native database"
)]
struct Cli {
    /// Control plane to talk to.
    ///
    /// Defaults to the hosted service, because that is what somebody who just
    /// ran `curl -fsSL https://thetabase.co/install.sh | sh` is trying to
    /// reach. It used to default to `http://127.0.0.1:8080`, so the first
    /// command in the documentation answered "Is it running? Start one locally
    /// with `theta-control --dev-seed`" -- which tells a new customer to run
    /// the server, and is the correct answer only for somebody developing this
    /// repository.
    ///
    /// `THETA_CONTROL_URL` is how that person gets the old behaviour, and it is
    /// set in this repository's own dev stack rather than being the default for
    /// everybody who installs the product.
    #[arg(
        long,
        env = "THETA_CONTROL_URL",
        default_value = "https://thetabase-control.fly.dev"
    )]
    control_url: String,

    /// Credential file to use. Defaults to the user's config directory.
    #[arg(long, env = "THETA_CREDENTIALS")]
    credentials: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Log in once per machine. Opens an OAuth flow in the default browser and
    /// stores a long-lived identity token.
    Login {
        /// Which identity provider to use.
        ///
        /// Defaults to the deployment's, not to a guess. `theta login` used to
        /// default to `google` while the hosted Control Plane offered only
        /// `github`, so the first command in the documentation failed for every
        /// new customer -- with an error pointing them at provider settings
        /// they do not administer, on a deployment that was working correctly.
        #[arg(long)]
        provider: Option<String>,
    },

    /// Forget the stored identity and every cached context.
    Logout,

    /// Resolve an org/project context and make it current.
    ///
    /// The everyday entry point: `theta use "Company B" churn-dashboard`.
    Use {
        org: String,
        project: String,
        #[arg(long)]
        env: Option<String>,
        /// Create the project if it does not exist, without asking first.
        #[arg(long)]
        create: bool,
    },

    /// List the contexts resolved so far.
    Contexts,

    /// Show or change what this organisation is paying for.
    ///
    /// Here rather than only in the portal because the product's interface is
    /// the CLI, and making somebody open a browser to buy is friction we chose
    /// rather than friction we inherited.
    #[command(subcommand)]
    Plan(PlanCommand),

    /// Read and answer what is waiting for a human.
    ///
    /// The non-interactive commands cost twenty invocations to review five
    /// changes, and a person stops reading carefully around the third — which
    /// matters, because the Safety Layer's correctness rests on somebody
    /// actually reading what they confirm.
    Review,

    /// Seed a worked example in the current project.
    ///
    /// A new database is empty, and an empty database has nothing to propose a
    /// destructive change against — so the one thing that distinguishes ThetaBase
    /// cannot be shown until somebody has modelled a domain. This creates a
    /// table, some rows, and a change file that will be gated, so the next
    /// command demonstrates the product.
    Demo,

    /// Run a command with the current context injected into its environment.
    ///
    /// This is how a token reaches a runtime without ever being written to a
    /// file the user manages.
    Exec {
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Print a scoped token.
    ///
    /// An explicit escape hatch for runtimes that cannot accept environment
    /// injection — never the normal path, and logged as a deliberate action.
    Token {
        #[command(subcommand)]
        action: TokenCommand,
    },

    /// Project status: branch, write volume, circuit-breaker state.
    Status,

    /// Human-legible audit summary, ranked by risk.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Hide anything below this risk: info, low, medium, high.
        #[arg(long, default_value = "info")]
        min_risk: String,
    },

    /// Project operations.
    #[command(subcommand)]
    Project(ProjectCommand),

    /// Branch operations.
    #[command(subcommand)]
    Branch(BranchCommand),

    /// Schema proposals: propose, inspect a diff, confirm, or promote.
    #[command(subcommand)]
    Schema(SchemaCommand),

    /// Migrate an existing Postgres/Supabase project into ThetaBase.
    Eject {
        /// Postgres connection URL to read from. Read-only: `eject` never
        /// writes to the source.
        #[arg(long)]
        from: String,

        /// Schema to migrate.
        #[arg(long, default_value = "public")]
        schema: String,

        /// Migrate, rather than only showing the plan.
        ///
        /// Showing the plan is the default, deliberately. The interesting
        /// failures of a migration are decisions - a `numeric` that will become
        /// a float, a table with no primary key - and a decision is cheap to
        /// change before the run and expensive after it.
        ///
        /// Only this needs a logged-in context: reading a Postgres schema is
        /// not something ThetaBase should demand credentials for.
        #[arg(long)]
        run: bool,

        /// Rows per batch. Peak memory is a batch, not a table.
        #[arg(long, default_value_t = 1_000)]
        batch_size: usize,

        /// Leave a table behind. Repeatable.
        ///
        /// A table with no primary key blocks the migration, and sometimes the
        /// right answer is that it is a log nobody wants moved. Excluding is
        /// reported rather than silent.
        #[arg(long = "exclude")]
        exclude: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum TokenCommand {
    /// Print the current context's token to stdout.
    Print,
    /// Revoke a token, session, user, or org.
    Revoke {
        #[arg(long, value_parser = ["token_id", "session_id", "user_id", "org_id"])]
        kind: String,
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectCommand {
    /// Delete a project, its instances and its data. Irreversible.
    ///
    /// Here rather than only in the portal because the quota refusal that
    /// sends most people looking for this arrives in the CLI, and it tells
    /// them to delete a project. An instruction with no command behind it is
    /// not an instruction.
    Delete {
        /// The project, as `theta contexts` prints it -- or a hint that
        /// matches exactly one resolved context.
        project: String,

        /// Skip the confirmation prompt. For scripts, and for nothing else.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum BranchCommand {
    Create {
        name: String,
        #[arg(long)]
        from: Option<String>,
    },
    List,
    Merge {
        source: String,
        #[arg(long, default_value = "main")]
        into: String,
    },
    Discard {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum PlanCommand {
    /// What this organisation is on, using, and accruing.
    Show,
    /// Move to a different tier.
    ///
    /// An upgrade applies immediately and is charged for the part of the period
    /// that remains. A downgrade is scheduled for the period boundary, because
    /// the current one is already paid for at the higher tier.
    Upgrade {
        /// `pro`, `team` or `scale`.
        plan: String,
    },
    /// Switch between monthly and annual billing.
    Interval {
        /// `monthly` or `annual`.
        interval: String,
    },
}

#[derive(Subcommand, Debug)]
enum SchemaCommand {
    /// Submit a change and print its diff.
    ///
    /// Applies nothing on its own. A change the rules gate is held; one that
    /// needs shadow validation is applied to a shadow branch and validated
    /// there, so what comes back already says what the checks found.
    Propose {
        file: std::path::PathBuf,
        /// Branch to propose against. Defaults to the current branch.
        #[arg(long)]
        branch: Option<String>,
    },
    /// A proposal's diff and what validating it found.
    Show { change_id: String },
    /// Apply a change the rules gate behind a confirmation.
    Confirm { change_id: String },
    /// Land a validated change by merging its shadow branch.
    Promote { change_id: String },
    /// Refuse a change and reclaim its shadow branch.
    Reject {
        change_id: String,
        #[arg(long)]
        reason: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "theta=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let store = match &cli.credentials {
        Some(path) => CredentialStore::at(path),
        None => CredentialStore::default_location(),
    };
    let client = ControlPlaneClient::new(&cli.control_url);

    match cli.command {
        Command::Login { provider } => {
            login(&store, &client, &cli.control_url, provider.as_deref()).await
        }
        Command::Logout => {
            store.clear().map_err(|e| e.to_string())?;
            println!(
                "Logged out. Cached contexts cleared from the {}.",
                store.backend().describe()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Use {
            org,
            project,
            env,
            create,
        } => use_context(&store, &client, &org, &project, env.as_deref(), create).await,
        Command::Contexts => contexts(&store),
        Command::Plan(action) => plan(&store, &client, action).await,
        Command::Review => {
            commands::review(&current_context(&store)?).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Demo => {
            commands::demo(&current_context(&store)?).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Project(action) => project(&store, &client, action).await,
        Command::Exec { command } => exec(&store, &command),
        Command::Token { action } => token(&store, &client, action).await,

        Command::Status => {
            commands::status(&current_context(&store)?).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Audit { limit, min_risk } => {
            let floor = commands::parse_risk(&min_risk)?;
            commands::audit(&current_context(&store)?, limit, floor).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Branch(action) => branch(&store, action).await,
        Command::Schema(action) => schema(&store, action).await,
        Command::Eject {
            from,
            schema,
            run,
            batch_size,
            exclude,
        } => {
            // Resolved only when writing. A dry run inspects somebody else's
            // Postgres and touches nothing here, so requiring a login for it
            // would be asking for a credential to do nothing with.
            let context = match run {
                true => Some(current_context(&store)?),
                false => None,
            };
            commands::eject(&from, &schema, context.as_ref(), batch_size, &exclude)
                .await
                .map(|()| ExitCode::SUCCESS)
        }
    }
}

/// The context every data-plane command acts on.
///
/// Cloned rather than borrowed: these commands are async and the borrow would
/// have to outlive the credential load for no benefit.
/// Plan commands, which act on an *organisation* rather than a project.
///
/// So they take the org from the current context rather than asking for it: a
/// customer who has resolved a project has already said which organisation they
/// mean, and asking again is a question we can answer ourselves.
async fn plan(
    store: &CredentialStore,
    client: &ControlPlaneClient,
    action: PlanCommand,
) -> Result<ExitCode, String> {
    let credentials = store.load().map_err(|e| e.to_string())?;
    let context = current_context(store)?;
    let org = context.org_id.clone();

    match action {
        PlanCommand::Show => {
            let summary = client
                .billing_summary(&credentials.identity_token, &org)
                .await
                .map_err(describe_client_error)?;
            print_plan(&summary);
        }
        PlanCommand::Upgrade { plan } => {
            let result = client
                .change_plan(&credentials.identity_token, &org, &plan)
                .await
                .map_err(describe_client_error)?;
            print_change(&result);
        }
        PlanCommand::Interval { interval } => {
            // Checked here so a typo is a local error naming the two valid
            // answers, rather than a round trip that comes back "invalid body".
            let interval = match interval.to_lowercase().as_str() {
                "monthly" | "month" => "monthly",
                "annual" | "annually" | "year" | "yearly" => "annual",
                other => {
                    return Err(format!(
                        "unknown billing interval `{other}` — try `monthly` or `annual`"
                    ))
                }
            };
            let result = client
                .change_interval(&credentials.identity_token, &org, interval)
                .await
                .map_err(describe_client_error)?;
            print_change(&result);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn money(cents: i64) -> String {
    format!(
        "{}${}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

fn print_plan(summary: &serde_json::Value) {
    let name = summary["plan"]["name"].as_str().unwrap_or("unknown");
    let interval = summary["subscription"]["interval"]
        .as_str()
        .unwrap_or("monthly");
    println!("{name}, billed {interval}");

    let line = |label: &str, key: &str| {
        let used = &summary[key]["used"];
        // An uncollected figure says so. Printing it as 0 would tell a customer
        // they are using nothing, which is a claim this command cannot make.
        let value = match used["state"].as_str() {
            Some("known") => used["value"].to_string(),
            _ => "not measured yet".to_string(),
        };
        let limit = match summary[key]["limit"].as_i64() {
            Some(n) => n.to_string(),
            None => "no limit".to_string(),
        };
        println!("  {label:<22} {value} of {limit}");
    };
    line("production instances", "production_instances");
    line("dev instances", "development_instances");

    // Only when there is something to say. A zero here on every invocation
    // trains people to stop reading the line that matters.
    if let Some(cents) = summary["chargeable_overage_cents"].as_i64() {
        if cents > 0 {
            print!("  {:<22} {}", "overage this period", money(cents));
            match summary["overage_cap_cents"].as_i64() {
                Some(cap) => println!(" of {} cap", money(cap)),
                None => println!(),
            }
        }
    }
}

fn print_change(result: &serde_json::Value) {
    match result["effect"].as_str() {
        Some("immediate") => {
            let charged = result["charged_cents"].as_i64().unwrap_or(0);
            println!("Applied. Charged {}.", money(charged));
        }
        Some("scheduled_at_period_end") => {
            println!(
                "Scheduled for the end of the current period, which you have \
                 already paid for."
            );
        }
        _ => println!("{result}"),
    }
}

fn current_context(store: &CredentialStore) -> Result<CachedContext, String> {
    let credentials = store.load().map_err(|e| e.to_string())?;
    credentials
        .current()
        .cloned()
        .ok_or_else(|| "no current project — run `theta use <org> <project>` first".to_string())
}

async fn branch(store: &CredentialStore, action: BranchCommand) -> Result<ExitCode, String> {
    let context = current_context(store)?;
    match action {
        BranchCommand::List => commands::branch_list(&context).await?,
        BranchCommand::Create { name, from } => {
            commands::branch_create(&context, &name, from.as_deref()).await?
        }
        BranchCommand::Merge { source, into } => {
            commands::branch_merge(&context, &source, &into).await?
        }
        BranchCommand::Discard { name } => commands::branch_discard(&context, &name).await?,
    }
    Ok(ExitCode::SUCCESS)
}

async fn schema(store: &CredentialStore, action: SchemaCommand) -> Result<ExitCode, String> {
    let context = current_context(store)?;
    match action {
        SchemaCommand::Propose { file, branch } => {
            commands::schema_propose(&context, &file, branch.as_deref()).await?
        }
        SchemaCommand::Show { change_id } => commands::schema_show(&context, &change_id).await?,
        SchemaCommand::Confirm { change_id } => {
            commands::schema_confirm(&context, &change_id).await?
        }
        SchemaCommand::Promote { change_id } => {
            commands::schema_promote(&context, &change_id).await?
        }
        SchemaCommand::Reject { change_id, reason } => {
            commands::schema_reject(&context, &change_id, &reason).await?
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `theta login` — the one login the product promises
/// (`06-provisioning-identity-flow.md` §1, §2).
///
/// The provider access token this obtains is spent immediately against the
/// Control Plane and never written down. What is stored is the identity token
/// the Control Plane minted *after* asking the provider who the caller is, so
/// nothing on this machine can assert an identity of its own.
async fn login(
    store: &CredentialStore,
    client: &ControlPlaneClient,
    control_url: &str,
    provider: Option<&str>,
) -> Result<ExitCode, String> {
    // Provider configuration comes from the deployment, not from this machine.
    // Asked before the name is resolved, so the answer can *be* the default.
    let configured = client.providers().await.map_err(describe_client_error)?;

    if configured.is_empty() {
        return Err(
            "this deployment has no identity provider configured, so there is no \
             way to log in. That is a deployment setting, not something this \
             machine can fix."
                .to_string(),
        );
    }

    // Built before `configured` is consumed, so an error can name the choices.
    let offered = provider_list(&configured);

    let config = match provider {
        // Named: honour it, and say what is on offer when it is not there.
        Some(name) => {
            let kind = ProviderKind::parse(name)
                .ok_or_else(|| format!("unknown provider `{name}` — this deployment offers {offered}"))?;
            configured
                .into_iter()
                .find(|c| c.kind == kind)
                .ok_or_else(|| {
                    format!("this deployment does not offer {name} login — it offers {offered}")
                })?
        }
        // Not named, and exactly one on offer: use it. This is the common case
        // and it is the one that used to fail.
        None if configured.len() == 1 => configured.into_iter().next().expect("just checked"),
        // Several: refuse and list them. Picking one would silently tie a
        // person's identity to whichever provider happened to come first in the
        // response, and an identity is not a thing to choose on somebody's
        // behalf.
        None => {
            return Err(format!(
                "this deployment offers {offered} — choose one with \
                 `theta login --provider <name>`"
            ))
        }
    };
    let kind = config.kind;

    let (attempt, listener) = login_flow::begin(&config)
        .map_err(|e| format!("could not open a loopback port for the callback: {e}"))?;

    if !login_flow::open_browser(&attempt.authorize_url) {
        // A login that only works with a local browser does not work over SSH.
        eprintln!(
            "Open this URL to continue:\n\n  {}\n",
            attempt.authorize_url
        );
    } else {
        eprintln!("Continue in your browser. Waiting for the callback...");
    }

    let code = login_flow::await_callback(&listener, &attempt, login_flow::LOGIN_TIMEOUT)
        .map_err(|e| e.to_string())?;

    let http = reqwest::Client::new();
    let (_, tokens) = login_flow::complete(&http, &config, &attempt, &code)
        .await
        .map_err(|e| e.to_string())?;

    // The provider token goes to the Control Plane and no further. The identity
    // this CLI ends up holding was asserted by the provider to that service —
    // not read out of a response this process could have fabricated.
    let session = client
        .login(kind.as_str(), &tokens.access_token)
        .await
        .map_err(describe_client_error)?;

    let credentials = Credentials {
        identity_token: session.token,
        user_id: session.user_id.clone(),
        control_plane_url: control_url.to_string(),
        contexts: Vec::new(),
    };
    store.save(&credentials).map_err(|e| e.to_string())?;

    let who = session
        .display_name
        .or(session.email)
        .unwrap_or(session.user_id);
    println!(
        "Signed in as {who}. Credentials in the {}.",
        store.backend().describe()
    );
    // A downgrade to a file is worth one line of explanation. "No OS keychain
    // available" with no reason is not something a user can act on.
    if let Some(FileReason::Unavailable(detail)) = store.file_reason() {
        eprintln!("note: the OS keychain could not be used ({detail}).");
    }

    if session.orgs.is_empty() {
        println!("No organizations yet.");
    } else {
        // Shown once, so the user learns what one login gave them access to
        // rather than discovering it from a failed `theta use`.
        println!("Organizations:");
        for org in &session.orgs {
            println!("  {org}");
        }
    }
    println!("Pick a context with: theta use <org> <project>");
    Ok(ExitCode::SUCCESS)
}

async fn use_context(
    store: &CredentialStore,
    client: &ControlPlaneClient,
    org: &str,
    project: &str,
    env: Option<&str>,
    create: bool,
) -> Result<ExitCode, String> {
    let mut credentials = store.load().map_err(|e| e.to_string())?;
    let session_id = format!("sess_{}", std::process::id());

    let outcome = client
        .resolve(
            &credentials.identity_token,
            org,
            project,
            env,
            &session_id,
            create,
        )
        .await
        .map_err(describe_client_error)?;

    match outcome {
        ResolveOutcome::Ready {
            project_id,
            org_id,
            environment,
            address,
            token,
            expires_at_ms,
        } => {
            credentials.remember(CachedContext {
                org_hint: org.to_string(),
                project_hint: project.to_string(),
                org_id,
                project_id: project_id.clone(),
                environment: environment.clone(),
                address,
                token,
                expires_at_ms,
            });
            credentials.prune(now_ms());
            store.save(&credentials).map_err(|e| e.to_string())?;

            // The token itself is not printed. It reaches a runtime through
            // `theta exec`, not through the user's clipboard.
            println!("Using {project_id} ({environment}).");
            println!("Run a command against it with: theta exec -- <command>");
            Ok(ExitCode::SUCCESS)
        }

        ResolveOutcome::NeedsClarification {
            question,
            candidates,
        } => {
            // One question, not a form.
            println!("{question}");
            for candidate in candidates {
                println!("  {candidate}");
            }
            Ok(ExitCode::from(2))
        }

        ResolveOutcome::ConfirmCreate { question, .. } => {
            println!("{question}");
            println!("Re-run with --create to confirm.");
            Ok(ExitCode::from(2))
        }
    }
}

/// Delete a project.
///
/// The confirmation is a typed name rather than a y/n, for the reason a y/n is
/// a bad fit: this destroys a database and cannot be undone, and `y` is what
/// somebody presses to get past a prompt. Typing the project's own name is the
/// one answer that cannot be given by accident or by habit.
async fn project(
    store: &CredentialStore,
    client: &ControlPlaneClient,
    action: ProjectCommand,
) -> Result<ExitCode, String> {
    let credentials = store.load().map_err(|e| e.to_string())?;

    match action {
        ProjectCommand::Delete { project, yes } => {
            // Resolved locally, from contexts this machine has already seen.
            // Deliberately not through `/v1/resolve`: that provisions what it
            // cannot find, and a typo in a delete command must not be able to
            // create a project -- let alone create one and then delete it.
            let matches: Vec<&CachedContext> = credentials
                .contexts
                .iter()
                .filter(|c| {
                    c.project_id == project
                        || c.project_hint == project
                        || c.project_id
                            .rsplit('/')
                            .next()
                            .is_some_and(|leaf| leaf == project)
                })
                .collect();

            let project_id = match matches.as_slice() {
                [one] => one.project_id.clone(),
                [] => {
                    return Err(format!(
                        "no resolved context for `{project}`. `theta contexts`                          lists what this machine knows; the argument is a                          project id from that list."
                    ))
                }
                // Names that differ only by org. Refusing is the only safe
                // answer: picking one would delete a database on a coin flip.
                many => {
                    let ids: Vec<&str> = many.iter().map(|c| c.project_id.as_str()).collect();
                    return Err(format!(
                        "`{project}` matches {} projects: {}. Name one exactly.",
                        many.len(),
                        ids.join(", ")
                    ))
                }
            };

            if !yes {
                println!(
                    "This deletes `{project_id}` permanently: every instance,                      every branch, every row, and the key that makes them                      readable. There is no undo and no archive."
                );
                print!("Type the project id to confirm: ");
                use std::io::Write;
                std::io::stdout().flush().ok();

                let mut typed = String::new();
                std::io::stdin()
                    .read_line(&mut typed)
                    .map_err(|e| format!("could not read the confirmation: {e}"))?;

                if typed.trim() != project_id {
                    println!("Not deleted.");
                    return Ok(ExitCode::FAILURE);
                }
            }

            let outcome = client
                .delete_project(&credentials.identity_token, &project_id)
                .await
                .map_err(describe_client_error)?;

            let released = outcome
                .get("instancesReleased")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let sessions = outcome
                .get("sessionsInvalidated")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            println!("Deleted {project_id}.");
            println!("  instances released:    {released}");
            println!("  sessions invalidated:  {sessions}");
            match outcome.get("projectsRemaining").and_then(|v| v.as_u64()) {
                Some(remaining) => {
                    println!("  projects you may create: {remaining}")
                }
                None => println!("  projects you may create: unlimited on this plan"),
            }

            // The local context is stale the moment the server answers, and a
            // stale one is worse than none: `theta status` would dial a
            // destroyed instance and report it as unreachable rather than as
            // gone.
            let mut credentials = credentials;
            credentials.contexts.retain(|c| c.project_id != project_id);
            store.save(&credentials).map_err(|e| e.to_string())?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn contexts(store: &CredentialStore) -> Result<ExitCode, String> {
    let mut credentials = store.load().map_err(|e| e.to_string())?;
    credentials.prune(now_ms());

    if credentials.contexts.is_empty() {
        println!("No contexts yet. Resolve one with: theta use <org> <project>");
        return Ok(ExitCode::SUCCESS);
    }

    let current = credentials.current().map(|c| c.project_id.clone());
    for context in &credentials.contexts {
        let marker = match Some(&context.project_id) == current.as_ref() {
            true => "*",
            false => " ",
        };
        println!(
            "{marker} {:<40} {:<8} expires in {}s",
            context.project_id,
            context.environment,
            (context.expires_at_ms - now_ms()).max(0) / 1000
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn exec(store: &CredentialStore, command: &[String]) -> Result<ExitCode, String> {
    let mut credentials = store.load().map_err(|e| e.to_string())?;
    credentials.prune(now_ms());

    let context = credentials
        .current()
        .ok_or("no current context; resolve one with `theta use <org> <project>`")?;

    let (program, args) = command.split_first().ok_or("no command given")?;

    // Injected into the child's environment and nowhere else: not a file, not
    // the shell's history, not this process's stdout
    // (`06-provisioning-identity-flow.md` §3).
    let status = std::process::Command::new(program)
        .args(args)
        .env("THETA_TOKEN", &context.token)
        .env("THETA_ADDRESS", &context.address)
        .env("THETA_PROJECT", &context.project_id)
        .env("THETA_RESOLVED_ENVIRONMENT", &context.environment)
        .status()
        .map_err(|e| format!("cannot run `{program}`: {e}"))?;

    Ok(match status.code() {
        Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
        // Killed by a signal. 128+n is the shell convention.
        None => ExitCode::from(1),
    })
}

async fn token(
    store: &CredentialStore,
    client: &ControlPlaneClient,
    action: TokenCommand,
) -> Result<ExitCode, String> {
    match action {
        TokenCommand::Print => {
            let mut credentials = store.load().map_err(|e| e.to_string())?;
            credentials.prune(now_ms());
            let context = credentials
                .current()
                .ok_or("no current context; resolve one with `theta use <org> <project>`")?;

            // Logged as an explicit user action, because printing a credential
            // is one (`06-provisioning-identity-flow.md` §6).
            tracing::warn!(
                project = %context.project_id,
                "session token printed by explicit user action"
            );
            eprintln!(
                "warning: printing a scoped token for {}. Prefer `theta exec`.",
                context.project_id
            );
            println!("{}", context.token);
            Ok(ExitCode::SUCCESS)
        }

        TokenCommand::Revoke { kind, id } => {
            let version = client
                .revoke(&kind, &id)
                .await
                .map_err(describe_client_error)?;
            println!("Revoked. Instances will refuse it at revocation list v{version}.");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Turn a client error into something a user can act on.
fn describe_client_error(error: ClientError) -> String {
    match error {
        ClientError::Unreachable { url, detail } => format!(
            "cannot reach the control plane at {url}: {detail}\n\
             Is it running? Start one locally with `theta-control --dev-seed`."
        ),
        other => other.to_string(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The providers a deployment offers, for an error message.
///
/// Named rather than counted: a person told "this deployment offers 1 provider"
/// still has to guess which.
fn provider_list(configured: &[theta_identity::oauth::ProviderConfig]) -> String {
    let names: Vec<&str> = configured.iter().map(|c| c.kind.as_str()).collect();
    match names.as_slice() {
        [] => "no providers".to_string(),
        [one] => format!("`{one}`"),
        many => many
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}
