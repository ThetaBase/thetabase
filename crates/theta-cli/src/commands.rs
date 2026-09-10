//! The data-plane command bodies: `status`, `audit`, `branch`, `schema`.
//!
//! These are the surface a human uses to review what an agent has been doing
//! (`07-agent-safety-layer.md` §5, §7). The reader is someone with five minutes
//! and a question — "is anything waiting on me, and is any of it alarming" — so
//! the output is prose first and structure second.

use theta_cli::data::DataClient;
use theta_cli::CachedContext;
use theta_eject::verify::Severity;
use theta_proto::wire::{
    AuditEntryWire, BranchInfoWire, ChangeStateWire, GateWire, MergeStatus, RequestBody,
    ResponseBody,
};

/// Risk levels, as the wire encodes them.
const RISK_NAMES: [&str; 4] = ["info", "low", "medium", "high"];

pub fn risk_name(level: u8) -> &'static str {
    RISK_NAMES.get(level as usize).copied().unwrap_or("unknown")
}

/// Parse a `--min-risk` value into its wire encoding.
pub fn parse_risk(name: &str) -> Result<u8, String> {
    RISK_NAMES
        .iter()
        .position(|r| r.eq_ignore_ascii_case(name))
        .map(|i| i as u8)
        .ok_or_else(|| format!("unknown risk level `{name}` (info, low, medium, high)"))
}

pub async fn connect(context: &CachedContext) -> Result<DataClient, String> {
    DataClient::connect(&context.address, &context.token)
        .await
        .map_err(|e| e.to_string())
}

// ---- status ----------------------------------------------------------------

pub async fn status(context: &CachedContext) -> Result<(), String> {
    let mut client = connect(context).await?;
    let body = client
        .call(0, RequestBody::Status)
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Status(status) = body else {
        return Err("the instance did not answer with a status".into());
    };

    println!("project   {}", status.project_id);
    println!("branch    {}", status.branch);
    println!("commits   {}", status.commits_applied);
    println!("written   {:.1} MB", status.write_volume_mb);
    println!(
        "breaker   {}",
        match status.circuit_breaker_tripped {
            // Named as a consequence rather than a state: "tripped" alone does
            // not tell someone why their writes started failing.
            true => format!(
                "TRIPPED — writes are being refused ({} rows in the current window)",
                status.breaker_window_rows
            ),
            false => format!(
                "ok ({} rows in the current window)",
                status.breaker_window_rows
            ),
        }
    );
    if !status.replica_regions.is_empty() {
        println!("replicas  {}", status.replica_regions.join(", "));
    }
    println!("protocol  v{}", status.protocol_version);
    Ok(())
}

// ---- audit -----------------------------------------------------------------

pub async fn audit(context: &CachedContext, limit: usize, min_risk: u8) -> Result<(), String> {
    let mut client = connect(context).await?;
    let body = client
        .call(
            0,
            RequestBody::Audit {
                limit: limit as u32,
                min_risk,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Audit { entries } = body else {
        return Err("the instance did not answer with an audit trail".into());
    };

    if entries.is_empty() {
        println!(
            "Nothing at {} risk or above. (`--min-risk info` shows everything.)",
            risk_name(min_risk)
        );
        return Ok(());
    }

    println!("{} event(s), most serious first:\n", entries.len());
    for entry in &entries {
        print_audit_entry(entry);
    }
    Ok(())
}

fn print_audit_entry(entry: &AuditEntryWire) {
    println!(
        "  [{}] {}",
        risk_name(entry.risk).to_uppercase(),
        entry.summary
    );
}

// ---- branch ----------------------------------------------------------------

pub async fn branch_list(context: &CachedContext) -> Result<(), String> {
    let mut client = connect(context).await?;
    let body = client
        .call(0, RequestBody::ListBranches)
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Branches { branches } = body else {
        return Err("the instance did not answer with a branch list".into());
    };

    for branch in &branches {
        println!("{}", describe_branch(branch));
    }
    Ok(())
}

fn describe_branch(branch: &BranchInfoWire) -> String {
    let kind = match branch.kind {
        0 => "protected",
        2 => "shadow",
        _ => "",
    };
    let head = match branch.head.len() >= 12 {
        true => &branch.head[..12],
        false => branch.head.as_str(),
    };
    match kind.is_empty() {
        true => format!("  {:<24} {head}", branch.name),
        false => format!("  {:<24} {head}  ({kind})", branch.name),
    }
}

pub async fn branch_create(
    context: &CachedContext,
    name: &str,
    from: Option<&str>,
) -> Result<(), String> {
    let mut client = connect(context).await?;
    let from_id = match from {
        None => 0,
        Some(name) => resolve_branch(&mut client, name).await?,
    };

    let body = client
        .call(
            0,
            RequestBody::CreateBranch {
                name: name.to_string(),
                from: from_id,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Branch { branch_id } = body else {
        return Err("the instance did not answer with a branch".into());
    };
    println!("Created `{name}` (branch {branch_id}).");
    Ok(())
}

pub async fn branch_discard(context: &CachedContext, name: &str) -> Result<(), String> {
    let mut client = connect(context).await?;
    client
        .call(
            0,
            RequestBody::DiscardBranch {
                name: name.to_string(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    println!("Discarded `{name}`. Its commits remain in the log.");
    Ok(())
}

pub async fn branch_merge(context: &CachedContext, source: &str, into: &str) -> Result<(), String> {
    let mut client = connect(context).await?;
    let source_id = resolve_branch(&mut client, source).await?;
    let target_id = resolve_branch(&mut client, into).await?;

    let body = client
        .call(
            0,
            RequestBody::Merge {
                source_branch: source_id,
                target_branch: target_id,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Merge(result) = body else {
        return Err("the instance did not answer with a merge result".into());
    };

    match result.status {
        MergeStatus::Ok => {
            println!("Merged `{source}` into `{into}`.");
            if !result.converged.is_empty() {
                println!(
                    "  {} field(s) converged automatically: {}",
                    result.converged.len(),
                    result.converged.join(", ")
                );
            }
            Ok(())
        }
        MergeStatus::UpToDate => {
            println!("`{into}` already has everything from `{source}`.");
            Ok(())
        }
        MergeStatus::Conflict => {
            // Never auto-resolved and never guessed at: both sides are printed
            // and a human decides (`03-data-model-consistency.md` §2.4).
            println!(
                "{} conflict(s) between `{source}` and `{into}`. Nothing was merged.\n",
                result.conflicts.len()
            );
            for conflict in &result.conflicts {
                println!("  {}", conflict.key);
                println!("    {}", conflict.reason);
                println!("    on `{into}`:     {}", conflict.ours_json);
                println!("    on `{source}`:   {}", conflict.theirs_json);
            }
            Err("resolve the conflicts and merge again".into())
        }
        MergeStatus::Blocked => Err("the merge was refused".into()),
    }
}

/// Look up a branch id by name.
async fn resolve_branch(client: &mut DataClient, name: &str) -> Result<u64, String> {
    if name == "main" {
        return Ok(0);
    }
    let body = client
        .call(0, RequestBody::ListBranches)
        .await
        .map_err(|e| e.to_string())?;
    let ResponseBody::Branches { branches } = body else {
        return Err("the instance did not answer with a branch list".into());
    };
    branches
        .iter()
        .find(|b| b.name == name)
        .map(|b| b.branch_id)
        .ok_or_else(|| format!("no branch named `{name}`"))
}

// ---- schema ----------------------------------------------------------------

/// Open the review screen.
///
/// The queue is read once before the terminal is touched. Somebody with nothing
/// waiting should be told so in their shell rather than shown an empty screen
/// they have to work out how to leave.
pub async fn review(context: &CachedContext) -> Result<(), String> {
    use theta_cli::review::app::{Action, Authority, ReviewApp};

    let batches = fetch_review_queue(context).await?;
    if batches.is_empty() {
        println!("Nothing is waiting for review.");
        return Ok(());
    }

    // The role is not known to the CLI, so this asks the instance by trying
    // nothing — the refusal comes back from the server when an action is taken.
    // Assuming authority here and being refused later is the honest order:
    // hiding the actions on a guess would tell somebody they lack a permission
    // they may well have.
    let app = ReviewApp::new(batches, Authority::MayReview);

    // Both closures re-connect per call. A held connection across a review
    // session would keep the instance awake for as long as the screen is open,
    // which is what `should_poll` going dormant exists to avoid — and holding
    // one open while somebody reads a diff for ten minutes is the same cost.
    let project = context.project_id.clone();
    let fetch_ctx = context.clone();
    let act_ctx = context.clone();
    let runtime = tokio::runtime::Handle::current();

    let fetch = move || {
        let ctx = fetch_ctx.clone();
        tokio::task::block_in_place(|| {
            runtime.block_on(async move { fetch_review_queue(&ctx).await })
        })
    };

    let runtime = tokio::runtime::Handle::current();
    let act = move |action: Action| {
        let ctx = act_ctx.clone();
        tokio::task::block_in_place(|| {
            runtime.block_on(async move {
                match action {
                    Action::Confirm(id) => schema_confirm(&ctx, &id)
                        .await
                        .map(|()| format!("Confirmed {id}.")),
                    Action::Promote(id) => schema_promote(&ctx, &id)
                        .await
                        .map(|()| format!("Promoted {id}.")),
                    Action::Reject { change_id, reason } => {
                        schema_reject(&ctx, &change_id, &reason)
                            .await
                            .map(|()| format!("Rejected {change_id}."))
                    }
                    // Handled by the loop itself.
                    Action::Refresh | Action::Quit => Ok(String::new()),
                }
            })
        })
    };

    theta_cli::review::run::run(&project, app, fetch, act)
}

/// Read the review queue from the instance.
async fn fetch_review_queue(
    context: &CachedContext,
) -> Result<Vec<theta_proto::wire::ReviewBatchWire>, String> {
    let mut client = connect(context).await?;
    match client
        .call(0, RequestBody::ReviewQueue)
        .await
        .map_err(|e| e.to_string())?
    {
        ResponseBody::ReviewQueue { batches } => Ok(batches),
        ResponseBody::Error(e) => Err(e.message),
        _ => Err("the instance did not answer with a review queue".into()),
    }
}

/// The file `demo` writes, and the change the quickstart proposes.
const DEMO_CHANGE_FILE: &str = "drop-legacy-ref.json";

/// The demo's table and the column it will offer to drop.
///
/// Named once because three places have to agree: the table the demo creates,
/// the rows it writes into that table, and the change file it hands the user.
/// Two of those matching while the third drifts produces a quickstart that
/// fails on its last command, which is the worst place for it to fail.
const DEMO_TABLE: &str = "customers";
const DEMO_COLUMN: &str = "legacy_ref";

/// Seed a worked example, so the Safety Layer has something to stop.
///
/// This exists because of a gap in the first minute of using ThetaBase. A new
/// database is empty, and an empty database has nothing to propose a
/// destructive change against — so the one thing that distinguishes this
/// product cannot be demonstrated until somebody has modelled a domain and
/// written rows. That is a long way to ask a person to walk on trust.
///
/// So: one table, some rows, and a change file that will be gated. The user's
/// next command is the one that shows the gate firing.
pub async fn demo(context: &CachedContext) -> Result<(), String> {
    use theta_core::schema::{FieldDef, SchemaChange, TableDef};
    use theta_core::ValueType;

    let mut client = connect(context).await?;

    let field = |name: &str, ty: ValueType, nullable: bool| {
        (
            name.to_string(),
            FieldDef {
                name: name.to_string(),
                ty,
                nullable,
                crdt: None,
                declared_at: None,
            },
        )
    };

    let table = TableDef {
        name: DEMO_TABLE.into(),
        fields: [
            field("email", ValueType::Text, false),
            field("plan", ValueType::Text, false),
            // The column the quickstart drops. Nullable and unused-looking,
            // which is exactly the shape of the column somebody deletes on a
            // Friday afternoon.
            field(DEMO_COLUMN, ValueType::Text, true),
        ]
        .into_iter()
        .collect(),
        indexes: Vec::new(),
    };

    // Adding a table is a safe change, so this applies without a gate. That is
    // worth the user seeing: the Safety Layer is not a speed bump on everything,
    // it is a rule about what is destructive.
    let added = client
        .call(
            0,
            RequestBody::ProposeSchemaChange {
                change_json: serde_json::to_string(&SchemaChange::AddTable { table })
                    .map_err(|e| e.to_string())?,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    match added {
        ResponseBody::Propose(_) => {}
        ResponseBody::Error(e) if e.message.contains("exists") => {
            // Re-running the demo is not a failure. Somebody who ran it twice
            // wants the change file again, not a lecture.
        }
        ResponseBody::Error(e) => return Err(e.message),
        _ => return Err("the instance did not answer with a diff".into()),
    }

    let rows = [
        ("cus_001", "ada@example.com", "team", "LEG-4471"),
        ("cus_002", "grace@example.com", "pro", "LEG-8812"),
        ("cus_003", "alan@example.com", "pro", "LEG-1093"),
        ("cus_004", "katherine@example.com", "team", "LEG-2250"),
        ("cus_005", "edsger@example.com", "free", "LEG-7736"),
    ];

    for (id, email, plan, legacy_ref) in rows {
        let value = serde_json::json!({
            "email": email,
            "plan": plan,
            "legacy_ref": legacy_ref,
        });
        let written = client
            .call(
                0,
                RequestBody::Put {
                    key: format!("{DEMO_TABLE}:{id}"),
                    value_json: value.to_string(),
                    ttl: 0,
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        if let ResponseBody::Error(e) = written {
            return Err(format!("seeding {id}: {}", e.message));
        }
    }

    // Written to disk rather than proposed here, so the *user* runs the command
    // that triggers the gate. Watching it happen to you is the demonstration;
    // being told it happened is a claim.
    let change = SchemaChange::DropColumn {
        table: DEMO_TABLE.into(),
        column: DEMO_COLUMN.into(),
    };
    let json = serde_json::to_string_pretty(&change).map_err(|e| e.to_string())?;
    std::fs::write(DEMO_CHANGE_FILE, &json)
        .map_err(|e| format!("cannot write {DEMO_CHANGE_FILE}: {e}"))?;

    println!("Seeded `{DEMO_TABLE}` with {} rows.", rows.len());
    println!();
    println!("Wrote {DEMO_CHANGE_FILE} — it drops a column those rows use.");
    println!();
    println!("Now run:");
    println!("  theta schema propose {DEMO_CHANGE_FILE}");
    Ok(())
}

pub async fn schema_propose(
    context: &CachedContext,
    file: &std::path::Path,
    branch: Option<&str>,
) -> Result<(), String> {
    let source = std::fs::read_to_string(file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    // Validated here so a typo is a local error naming the file, rather than a
    // wire round trip that comes back saying "malformed change".
    let change: theta_core::schema::SchemaChange = serde_json::from_str(&source)
        .map_err(|e| format!("{} is not a schema change: {e}", file.display()))?;

    let mut client = connect(context).await?;
    let branch_id = match branch {
        None => 0,
        Some(name) => resolve_branch(&mut client, name).await?,
    };

    let body = client
        .call(
            branch_id,
            RequestBody::ProposeSchemaChange {
                change_json: serde_json::to_string(&change).map_err(|e| e.to_string())?,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Propose(diff) = body else {
        return Err("the instance did not answer with a diff".into());
    };

    println!("{}", diff.change_id);
    println!(
        "  {} on {}{} — {} row(s), {}",
        diff.change_type.replace('_', " "),
        diff.affected_table,
        match diff.affected_column.is_empty() {
            true => String::new(),
            false => format!(".{}", diff.affected_column),
        },
        diff.rows_affected,
        match diff.reversible {
            true => "reversible",
            false => "irreversible",
        },
    );
    println!("  {}", diff.reason);
    println!();
    println!(
        "{}",
        next_step(&diff.gate, &diff.change_id, diff.shadow_branch_id)
    );
    Ok(())
}

/// What the caller has to do next, named as a command they can run.
fn next_step(gate: &GateWire, change_id: &str, shadow_branch_id: u64) -> String {
    match gate {
        GateWire::AutoApply => "Applied.".to_string(),
        GateWire::Confirm => {
            format!("To apply: theta schema confirm {change_id}")
        }
        GateWire::ShadowValidate => format!(
            "Validated on shadow branch {shadow_branch_id}. \
             Review it, then:\n  \
             theta schema show {change_id}\n  \
             theta schema promote {change_id}\n  \
             theta schema reject {change_id} --reason \"...\""
        ),
    }
}

pub async fn schema_show(context: &CachedContext, change_id: &str) -> Result<(), String> {
    let mut client = connect(context).await?;
    let body = client
        .call(
            0,
            RequestBody::ShowChange {
                change_id: change_id.to_string(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Change(state) = body else {
        return Err("the instance did not answer with a change".into());
    };
    print_change_state(&state);
    Ok(())
}

fn print_change_state(state: &ChangeStateWire) {
    let diff = &state.diff;
    println!("{}", diff.change_id);
    println!(
        "  {} on {}{} — {} row(s), {}",
        diff.change_type.replace('_', " "),
        diff.affected_table,
        match diff.affected_column.is_empty() {
            true => String::new(),
            false => format!(".{}", diff.affected_column),
        },
        diff.rows_affected,
        match diff.reversible {
            true => "reversible",
            false => "irreversible",
        },
    );
    println!("  {}", diff.reason);

    let Some(shadow) = state.shadow_branch_id else {
        return;
    };
    println!("\n  shadow branch {shadow}");

    match state.validation_passed {
        // Distinguishing "not yet run" from "failed" matters: treating the
        // first as the second would hide a change that is still in flight, and
        // treating it as a pass would be very much worse.
        None => println!("  not yet validated"),
        Some(passed) => {
            println!(
                "  {}",
                match passed {
                    true => "validation PASSED",
                    false => "validation FAILED",
                }
            );
            for check in &state.checks {
                println!(
                    "    {} {}: {}",
                    match check.passed {
                        true => "ok  ",
                        false => "FAIL",
                    },
                    check.name,
                    check.detail
                );
            }
            if passed {
                println!("\n  To land it:  theta schema promote {}", diff.change_id);
            }
        }
    }
}

pub async fn schema_confirm(context: &CachedContext, change_id: &str) -> Result<(), String> {
    let mut client = connect(context).await?;

    // Only the id travels. The instance applies the change it classified under
    // it, on the branch that proposal targeted — there is no body or branch on
    // this call to disagree with the gate
    // (`07-agent-safety-layer.md` §4).
    let body = client
        .call(
            0,
            RequestBody::ApplySchemaChange {
                change_id: change_id.to_string(),
                confirm: true,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Apply { commit_id } = body else {
        return Err("the instance did not answer with a commit".into());
    };
    println!(
        "Applied {change_id} ({}).",
        &commit_id[..12.min(commit_id.len())]
    );
    Ok(())
}

pub async fn schema_promote(context: &CachedContext, change_id: &str) -> Result<(), String> {
    let mut client = connect(context).await?;
    let body = client
        .call(
            0,
            RequestBody::PromoteChange {
                change_id: change_id.to_string(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let ResponseBody::Merge(result) = body else {
        return Err("the instance did not answer with a merge result".into());
    };
    match result.status {
        MergeStatus::Ok | MergeStatus::UpToDate => {
            println!("Promoted {change_id}. The validated change is now on the target branch.");
            Ok(())
        }
        MergeStatus::Conflict => Err(format!(
            "{} conflict(s) between the shadow branch and its target; nothing was merged",
            result.conflicts.len()
        )),
        MergeStatus::Blocked => Err("the promotion was refused".into()),
    }
}

pub async fn schema_reject(
    context: &CachedContext,
    change_id: &str,
    reason: &str,
) -> Result<(), String> {
    let mut client = connect(context).await?;
    client
        .call(
            0,
            RequestBody::RejectChange {
                change_id: change_id.to_string(),
                reason: reason.to_string(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    println!("Rejected {change_id}. Its shadow branch has been reclaimed.");
    Ok(())
}

// ---- eject ------------------------------------------------------------------

/// `theta eject --from <postgres-url>`.
///
/// Reflect, plan, report — and only then, if asked, run. The plan is printed in
/// full before anything is written, because a migration's failures are
/// decisions rather than crashes: a `numeric` becoming a float changes money by
/// fractions of a cent and reports success.
///
/// Nothing here writes to the source. A migration that could alter what it is
/// reading is one nobody can safely re-run, and re-running is exactly what
/// happens when the first attempt is interrupted.
pub async fn eject(
    from: &str,
    schema_name: &str,
    target: Option<&CachedContext>,
    batch_size: usize,
    exclude: &[String],
) -> Result<(), String> {
    let client = theta_eject::connect(from)
        .await
        .map_err(|e| format!("{e}"))?;

    let reflected = theta_eject::reflect::reflect(&client, schema_name)
        .await
        .map_err(|e| format!("{e}"))?;
    let plan = theta_eject::plan_excluding(&reflected, exclude);

    println!(
        "Reflected `{schema_name}`: {} table(s).",
        reflected.tables.len()
    );
    println!();

    for table in &plan.tables {
        println!(
            "  {} — {} column(s), key ({}), ~{} row(s)",
            table.source_name,
            table.columns.len(),
            table.primary_key.join(", "),
            table.estimated_rows
        );
        for column in &table.columns {
            let flag = if column.mapping.is_lossy() { "!" } else { " " };
            println!(
                "    {flag} {:<24} {:<14} -> {:?}",
                column.source_name, column.source_type, column.mapping.ty
            );
        }
        println!();
    }

    if !plan.warnings.is_empty() {
        println!("Changes of meaning ({}):", plan.warnings.len());
        for warning in &plan.warnings {
            let where_ = match &warning.column {
                Some(column) => format!("{}.{column}", warning.table),
                None => warning.table.clone(),
            };
            println!("  ! {where_}: {}", warning.message);
        }
        println!();
    }

    if !plan.blockers.is_empty() {
        // Listed rather than summarised: each one needs a different decision,
        // and "3 blockers" tells nobody which three.
        println!("Blockers ({}):", plan.blockers.len());
        for blocker in &plan.blockers {
            println!("  x {}: {}", blocker.table, blocker.message);
        }
        return Err(format!(
            "{} table(s) cannot be migrated as they stand. Nothing was written.",
            plan.blockers.len()
        ));
    }

    let Some(context) = target else {
        println!(
            "Dry run: nothing was written. Re-run with --run to migrate, \
             --batch-size to tune ({batch_size} rows per batch)."
        );
        return Ok(());
    };

    run_migration(&client, &plan, &reflected, context, batch_size).await
}

/// A live `thetad` as a migration target.
///
/// The adapter is all the CLI adds: the ordering that makes a migration
/// correct — schema before rows, the cursor after the batch, verification
/// against a re-read of the source — lives in `theta_eject::migrate`, where it
/// can be tested. Keeping a second copy here is how the two would drift.
struct Instance {
    client: DataClient,
}

impl theta_eject::Target for Instance {
    async fn apply_schema(
        &mut self,
        change: &theta_core::schema::SchemaChange,
    ) -> Result<theta_eject::SchemaOutcome, String> {
        let change_json = serde_json::to_string(change).map_err(|e| e.to_string())?;
        // Proposed, not applied, so the Safety Layer sees every change. An
        // `eject` that installed its schema behind the gate would be the one
        // caller allowed to skip the review everything else submits to — and a
        // migration is exactly when a destructive change is most likely to be
        // accidental.
        let body = self
            .client
            .call(0, RequestBody::ProposeSchemaChange { change_json })
            .await
            .map_err(|e| e.to_string())?;

        let ResponseBody::Propose(diff) = body else {
            return Err("the instance did not answer a schema proposal with a diff".into());
        };

        Ok(match diff.gate {
            GateWire::AutoApply => theta_eject::SchemaOutcome::Applied,
            gate => theta_eject::SchemaOutcome::Gated {
                table: diff.affected_table.clone(),
                next_step: next_step(&gate, &diff.change_id, diff.shadow_branch_id),
            },
        })
    }

    async fn put(&mut self, key: &str, value: &theta_core::Value) -> Result<(), String> {
        let value_json = serde_json::to_string(value).map_err(|e| e.to_string())?;
        let body = self
            .client
            .call(
                0,
                RequestBody::Put {
                    key: key.to_string(),
                    value_json,
                    ttl: 0,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        match body {
            ResponseBody::Put { .. } => Ok(()),
            ResponseBody::Error(e) => Err(format!("writing `{key}` was refused: {}", e.message)),
            other => Err(format!(
                "writing `{key}` got an unexpected answer: {other:?}"
            )),
        }
    }

    async fn get(&mut self, key: &str) -> Result<Option<theta_core::Value>, String> {
        let body = self
            .client
            .call(
                0,
                RequestBody::Get {
                    key: key.to_string(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(match body {
            ResponseBody::Get {
                found: true,
                value_json,
                ..
            } => serde_json::from_str(&value_json).ok(),
            _ => None,
        })
    }
}

/// Prints what is happening, because silence and a hang look the same.
struct Printing;

impl theta_eject::Progress for Printing {
    fn phase(&mut self, what: &str) {
        println!("{what}...");
    }
    fn table_progress(&mut self, table: &str, rows: u64) {
        println!("  {table} — {rows} row(s)");
    }
}

async fn run_migration(
    source: &theta_eject::SourceClient,
    plan: &theta_eject::Plan,
    reflected: &theta_eject::Schema,
    context: &CachedContext,
    batch_size: usize,
) -> Result<(), String> {
    let mut target = Instance {
        client: connect(context).await?,
    };
    println!("Target {}", context.address);

    let outcome = theta_eject::migrate(
        source,
        plan,
        reflected,
        &mut target,
        batch_size,
        &mut Printing,
    )
    .await?;

    let report = &outcome.report;
    println!();
    println!(
        "Migrated {} row(s) across {} table(s); compared {}.",
        outcome.rows_written, report.tables_compared, report.rows_compared
    );

    let expected = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Expected)
        .count();
    if expected > 0 {
        println!("{expected} predicted change(s) of meaning, as warned above.");
    }

    let unexpected: Vec<_> = report.unexpected().collect();
    if unexpected.is_empty() {
        println!("No unexpected mismatches.");
        return Ok(());
    }

    // Listed, not counted. Each is a different thing to look at, and the whole
    // point of the pass is that a change of meaning is never silent.
    println!();
    println!("Unexpected mismatches ({}):", unexpected.len());
    for finding in &unexpected {
        let where_ = match (&finding.column, &finding.primary_key) {
            (Some(c), Some(k)) => format!("{}.{c} [{k}]", finding.table),
            (Some(c), None) => format!("{}.{c}", finding.table),
            (None, Some(k)) => format!("{} [{k}]", finding.table),
            (None, None) => finding.table.clone(),
        };
        println!("  x {where_}: {}", finding.message);
    }
    Err(format!(
        "{} unexpected mismatch(es). The data is written; the verification pass \
         did not agree that it means the same thing.",
        unexpected.len()
    ))
}
