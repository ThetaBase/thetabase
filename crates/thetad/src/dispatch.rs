//! Request dispatch: one wire [`Request`] in, one [`Response`] out.
//!
//! Deliberately a plain function over `&mut Engine` rather than a method on the
//! server. Everything the protocol does can then be tested without a socket, and
//! the server is left with only the concerns that genuinely need a network:
//! framing, handshakes, and backpressure.

use theta_core::schema::SchemaChange;
use theta_core::{Author, BranchId, Value};
use theta_proto::wire::{
    AuditEntryWire, BranchInfoWire, ChangeDiffWire, ChangeStateWire, ConflictRefWire, GateRuleWire,
    GateWire, MergeResultWire, MergeStatus, ProjectStatusWire, RemedyWire, TxAction,
    ValidationCheckWire, WireError,
};
use theta_proto::{Request, RequestBody, Response, ResponseBody, StatusCode, PROTOCOL_VERSION};
use theta_safety::diff::{ChangeDiff, ChangeId, Gate};
use theta_safety::rationale::{GateRule, Remedy};
use theta_storage::merge::MergeOutcome;

use theta_core::branch::BranchKind;
use theta_safety::audit::{AuditEntry, RiskLevel};
use theta_safety::signed::SignedPolicy;

use crate::engine::{Engine, EngineError, Proposal, TxWrite, TxWriteAction};
use theta_identity::TokenScope;

/// Handle one request against the engine.
///
/// Never panics on bad input: a malformed value or an unknown change id is an
/// error response, because a client that can crash the engine with a bad
/// payload is a denial-of-service hole.
/// Learn this session's public signing key, if the token carries one.
///
/// On every request rather than once at connect: there is no connect step to
/// hang it off, and an instance that restarts mid-session would otherwise never
/// learn a key it is about to be asked to check against.
///
/// This is not a channel a caller can use to install a key for somebody else's
/// session. The key arrives inside a payload the project key signed, and the
/// session id it binds to is covered by that same signature — so a caller can
/// only ever register the key the Control Plane issued to them.
fn register_session_key(
    engine: &mut Engine,
    token: &TokenScope,
) -> std::result::Result<(), EngineError> {
    match token.signing_key.as_deref() {
        Some(hex) => engine.register_session_key(&token.session_id, hex),
        None => Ok(()),
    }
}

pub fn dispatch(
    engine: &mut Engine,
    request: Request,
    token: &TokenScope,
    now_ms: i64,
) -> Response {
    let request_id = request.request_id;
    let branch = BranchId(request.branch_id);
    let author = Author::agent(token.session_id.clone(), token.user_id.clone());

    // Customer traffic keeps the instance awake; the Control Plane's own
    // housekeeping must not.
    //
    // This is load-bearing rather than tidy. The courier polls `Status` on a
    // timer to collect usage and push revocations, so counting its visits as
    // activity would mean every instance reports itself busy forever and
    // nothing ever hibernates — the feature would look implemented and save
    // nothing.
    if is_customer_traffic(&request.body) {
        engine.mark_active();
    }

    // Registration shares the one error path rather than opening a second, so a
    // malformed key is reported the way every other bad request is.
    let handled = register_session_key(engine, token)
        .and_then(|()| handle(engine, request.body, branch, author, now_ms));

    let body = match handled {
        Ok(body) => body,
        Err(err) => ResponseBody::Error(to_wire_error(err)),
    };
    Response { request_id, body }
}

/// Whether this request is somebody using the database, as opposed to the
/// Control Plane looking after it.
///
/// Named as an allow-list of the housekeeping rather than of the traffic: a
/// request kind added later is somebody's use of the product until proven
/// otherwise, and the failure that direction is an instance staying up too
/// long. The other direction hibernates a database somebody is using.
fn is_customer_traffic(body: &RequestBody) -> bool {
    !matches!(
        body,
        RequestBody::Status | RequestBody::PushRevocations { .. } | RequestBody::PushPolicy { .. }
    )
}

fn handle(
    engine: &mut Engine,
    body: RequestBody,
    branch: BranchId,
    author: Author,
    now_ms: i64,
) -> Result<ResponseBody, EngineError> {
    match body {
        RequestBody::Get { key } => {
            // CRDT-typed fields resolve through their state; plain fields read
            // straight out of the view. Both are hot-path reads with no model
            // call anywhere (`05-prd.md` §3).
            let value = engine
                .get(branch, &key)
                .cloned()
                .or_else(|| engine.get_crdt(branch, &key));
            Ok(ResponseBody::Get {
                found: value.is_some(),
                value_json: value.map(encode_value).unwrap_or_default(),
                // The row's version, not the branch's commit counter (M10.5).
                //
                // This used to be `commits_applied()` — a number that moved
                // whenever any *other* key was written, in a field named for
                // this one. Anything comparing it across two reads of one key
                // was comparing the wrong thing, and a conditional write built
                // on it would have failed for reasons unrelated to the row.
                //
                // Zero for an absent row; `found` is what distinguishes that
                // from a row genuinely written by commit zero.
                version_id: engine.version_of(branch, &key).unwrap_or(0),
            })
        }

        RequestBody::Put {
            key, value_json, ..
        } => {
            let value = decode_value(&value_json)?;
            let hash = engine.put(branch, &key, value, author, now_ms)?;
            Ok(ResponseBody::Put {
                commit_id: hash.to_hex(),
            })
        }

        RequestBody::SignedWrite {
            commit_id,
            timestamp_ms,
            signature,
            op,
        } => {
            let hash = engine.append_signed(
                branch,
                op,
                author,
                crate::engine::SignedEnvelope {
                    commit_id,
                    timestamp_ms,
                    signature,
                },
                now_ms,
            )?;
            Ok(ResponseBody::Put {
                commit_id: hash.to_hex(),
            })
        }

        RequestBody::Crdt { key, mutation } => {
            let hash = engine.apply_crdt(branch, &key, &mutation, author, now_ms)?;
            // Answered as a `Put` because it is one: a row was written and the
            // caller wants the commit. A distinct response would make every
            // client match on two shapes for the same fact.
            Ok(ResponseBody::Put {
                commit_id: hash.to_hex(),
            })
        }

        RequestBody::PutIf {
            key,
            value_json,
            expect,
            ..
        } => {
            let value = decode_value(&value_json)?;
            match engine.put_if(branch, &key, value, expect, author, now_ms) {
                Ok(hash) => Ok(ResponseBody::Put {
                    commit_id: hash.to_hex(),
                }),
                // Answered as its own response rather than as an error. The
                // request was well-formed and the server did what it was asked;
                // the row was simply not in the state required. Returning an
                // error here would make an ordinary lost-update retry look like
                // a fault, and a contended key look like an outage.
                Err(EngineError::PreconditionFailed { key, found, actual }) => {
                    Ok(ResponseBody::PreconditionFailed { key, found, actual })
                }
                Err(other) => Err(other),
            }
        }

        RequestBody::Transaction { ops } => {
            // Decoded before anything is attempted, so a bad value in the last
            // operation cannot be discovered after the earlier ones have been
            // type-checked against the branch. The engine takes decoded values
            // for this reason.
            let mut writes = Vec::with_capacity(ops.len());
            for op in ops {
                let action = match op.action {
                    TxAction::Put { value_json, ttl } => TxWriteAction::Put {
                        value: decode_value(&value_json)?,
                        ttl,
                    },
                    TxAction::Delete => TxWriteAction::Delete,
                };
                writes.push(TxWrite {
                    key: op.key,
                    expect: op.expect,
                    action,
                });
            }

            match engine.transaction(branch, writes, author, now_ms) {
                Ok(hash) => Ok(ResponseBody::Transaction {
                    commit_id: hash.to_hex(),
                }),
                // The same treatment `put_if` gives it, and for the same reason:
                // the request was well-formed and the server did what it was
                // asked. A contended transaction is a retry, not a fault.
                Err(EngineError::PreconditionFailed { key, found, actual }) => {
                    Ok(ResponseBody::PreconditionFailed { key, found, actual })
                }
                Err(other) => Err(other),
            }
        }

        RequestBody::Delete { key } => {
            let hash = engine.delete(branch, &key, author, now_ms)?;
            Ok(ResponseBody::Delete {
                commit_id: hash.to_hex(),
            })
        }

        RequestBody::Query(plan) => {
            let (parsed, bindings) = plan_from_wire(&plan)?;
            let result = engine.query(branch, &parsed, &bindings)?;
            let row_count = result.row_count() as u64;
            let result_set = result.to_arrow_ipc().map_err(|e| EngineError::BadRequest {
                detail: format!("cannot encode result set: {e}"),
            })?;
            Ok(ResponseBody::Query {
                result_set,
                plan_hash: parsed.hash().0,
                row_count,
            })
        }

        RequestBody::Explain(plan) => {
            // EXPLAIN never executes. It is what the Safety Layer and a human
            // reviewer read before deciding whether a query should run at all
            // (`02-api-wire-protocol.md` §4).
            let (parsed, _) = plan_from_wire(&plan)?;
            let explain = engine.explain(branch, &parsed);
            Ok(ResponseBody::Explain {
                explanation_json: serde_json::to_string(&explain).unwrap_or_else(|_| "{}".into()),
            })
        }

        RequestBody::ProposeSchemaChange { change_json } => {
            let change = decode_change(&change_json)?;
            // `propose` also does whatever the gate demands — for a change at
            // the shadow gate that means opening the branch and validating it,
            // so the diff that comes back already says what the checks found
            // (`07-agent-safety-layer.md` §5, §8).
            let proposal = engine.propose(branch, change, author, now_ms)?;
            Ok(ResponseBody::Propose(diff_to_wire(&proposal.diff)))
        }

        RequestBody::ApplySchemaChange { change_id, confirm } => {
            // Note what is *not* passed: the change body and the branch. Both
            // come from the proposal this id names, so the gate that classified
            // it governs what actually runs.
            let hash = engine.apply_schema_change(&ChangeId(change_id), confirm, author, now_ms)?;
            Ok(ResponseBody::Apply {
                commit_id: hash.to_hex(),
            })
        }

        RequestBody::CreateBranch { name, from } => {
            let id = engine.create_branch(&name, BranchId(from), author, now_ms)?;
            Ok(ResponseBody::Branch { branch_id: id.0 })
        }

        RequestBody::Merge {
            source_branch,
            target_branch,
        } => {
            let outcome = engine.merge(
                BranchId(source_branch),
                BranchId(target_branch),
                author,
                now_ms,
            )?;
            Ok(ResponseBody::Merge(merge_to_wire(outcome)))
        }

        RequestBody::Status => Ok(ResponseBody::Status(status_of(engine, branch))),

        RequestBody::Audit { limit, min_risk } => {
            let floor = risk_from_u8(min_risk);
            let entries = engine
                .audit()
                .review(floor, limit.clamp(1, 1_000) as usize)
                .into_iter()
                .map(audit_to_wire)
                .collect();
            Ok(ResponseBody::Audit { entries })
        }

        RequestBody::Describe {
            table,
            include_examples,
            example_limit,
        } => Ok(ResponseBody::Description(engine.describe(
            branch,
            &crate::describe::DescribeRequest {
                table,
                include_examples,
                example_limit,
            },
        ))),

        // The engine's own queue, not the audit trail. The two answer different
        // questions — what has not happened yet, and what did — and answering
        // the first with the second is how a reviewer concludes the queue is
        // empty because nothing was logged.
        RequestBody::ReviewQueue => Ok(ResponseBody::ReviewQueue {
            batches: engine
                .review_queue()
                .iter()
                .map(|batch| theta_proto::wire::ReviewBatchWire {
                    key: batch.key.0.clone(),
                    gate: match batch.gate {
                        Gate::AutoApply => 0,
                        Gate::Confirm => 1,
                        Gate::ShadowValidate => 2,
                    },
                    changes: batch.changes.iter().map(diff_to_wire).collect(),
                    rows_affected: batch.rows_affected,
                    cost: batch.cost,
                    cost_if_unbatched: batch.cost_if_unbatched,
                    reason: batch.reason.clone(),
                })
                .collect(),
        }),

        RequestBody::ListBranches => {
            let mut branches: Vec<BranchInfoWire> =
                engine.branches().iter().map(branch_to_wire).collect();
            branches.sort_by_key(|b| b.branch_id);
            Ok(ResponseBody::Branches { branches })
        }

        RequestBody::DiscardBranch { name } => {
            engine.discard_branch(&name, author, now_ms)?;
            Ok(ResponseBody::Ok)
        }

        RequestBody::ShowChange { change_id } => {
            let change_id = ChangeId(change_id);
            let proposal = engine
                .pending_change(&change_id)
                .ok_or(EngineError::UnknownChange(change_id))?;
            Ok(ResponseBody::Change(change_state_to_wire(&proposal)))
        }

        RequestBody::PromoteChange { change_id } => {
            let outcome = engine.promote_shadow(&ChangeId(change_id), author, now_ms)?;
            Ok(ResponseBody::Merge(merge_to_wire(outcome)))
        }

        RequestBody::RejectChange { change_id, reason } => {
            engine.reject_shadow(&ChangeId(change_id), &reason, author, now_ms)?;
            Ok(ResponseBody::Ok)
        }

        // Handled before dispatch reaches the engine — see `service`. Reaching
        // here means the connection layer failed to intercept it, which is a
        // bug rather than a request to serve.
        // The policy is verified against the project keyset the instance already
        // holds. Nothing about the connection decides whether it is accepted —
        // only the signature does — so this needs no separate authorization and
        // an agent session token has no way to produce one
        // (`07-agent-safety-layer.md` §7).
        RequestBody::PushPolicy {
            payload,
            signature,
            key_id,
        } => {
            let signed = SignedPolicy {
                payload,
                signature,
                key_id: theta_identity::keys::KeyId::new(key_id),
            };
            let accepted = engine.apply_signed_policy(&signed, now_ms).map_err(|e| {
                EngineError::BadRequest {
                    detail: e.to_string(),
                }
            })?;
            Ok(ResponseBody::Policy {
                version: engine.policy_version(),
                accepted,
            })
        }

        RequestBody::PushRevocations { .. } => Err(EngineError::BadRequest {
            detail: "revocation pushes are handled at the connection layer".into(),
        }),
    }
}

fn risk_from_u8(level: u8) -> RiskLevel {
    match level {
        0 => RiskLevel::Info,
        1 => RiskLevel::Low,
        2 => RiskLevel::Medium,
        // Anything above the known range floors at the strictest level rather
        // than the loosest: an unknown value must not widen what is returned.
        _ => RiskLevel::High,
    }
}

fn risk_to_u8(level: RiskLevel) -> u8 {
    match level {
        RiskLevel::Info => 0,
        RiskLevel::Low => 1,
        RiskLevel::Medium => 2,
        RiskLevel::High => 3,
    }
}

fn audit_to_wire(entry: &AuditEntry) -> AuditEntryWire {
    AuditEntryWire {
        risk: risk_to_u8(entry.risk),
        summary: entry.summary.clone(),
        author_json: serde_json::to_string(&entry.author).unwrap_or_else(|_| "null".into()),
        timestamp_ms: entry.timestamp_ms,
        detail_json: serde_json::to_string(&entry.detail).unwrap_or_else(|_| "{}".into()),
    }
}

fn branch_to_wire(branch: &theta_core::Branch) -> BranchInfoWire {
    BranchInfoWire {
        branch_id: branch.id.0,
        name: branch.name.clone(),
        kind: match branch.kind {
            BranchKind::Protected => 0,
            BranchKind::Standard => 1,
            BranchKind::Shadow => 2,
        },
        head: branch.head.to_hex(),
        protected: branch.is_protected(),
        // Read from the branch rather than derived from its head, because they
        // are different facts: the head is what has landed, this is what lands
        // next, and a caller signing a write needs the second.
        next_commit: branch.next_commit.0,
    }
}

fn change_state_to_wire(proposal: &Proposal) -> ChangeStateWire {
    let outcome = proposal.shadow.as_ref().and_then(|s| s.outcome.as_ref());
    ChangeStateWire {
        diff: diff_to_wire(&proposal.diff),
        shadow_branch_id: proposal.shadow.as_ref().map(|s| s.shadow.0),
        validation_passed: outcome.map(|o| o.passed),
        validation_summary: match outcome {
            Some(o) => o.summary(),
            None => proposal.summary(),
        },
        checks: outcome
            .map(|o| {
                o.checks
                    .iter()
                    .map(|c| ValidationCheckWire {
                        name: c.name.clone(),
                        passed: c.passed,
                        detail: c.detail.clone(),
                        samples_json: serde_json::to_string(&c.samples)
                            .unwrap_or_else(|_| "[]".into()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn status_of(engine: &Engine, branch: BranchId) -> ProjectStatusWire {
    let config = engine.config();
    ProjectStatusWire {
        project_id: config.project_id.clone(),
        branch: engine
            .branches()
            .get(branch)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| branch.0.to_string()),
        write_volume_mb: engine.write_volume_mb(),
        circuit_breaker_tripped: engine.breaker().is_tripped(),
        // Single active region per project at launch; replicas are read-only
        // and cross-region multi-master is out of scope for v1
        // (`01-system-architecture.md` §8).
        replica_regions: engine.regions(),
        breaker_window_rows: engine.breaker().window_rows(),
        protocol_version: PROTOCOL_VERSION,
        commits_applied: engine.commits_applied(),
        storage_bytes: engine.storage_bytes(),
        rows_written: engine.rows_written(),
        idle_ms: engine.idle_ms(),
    }
}

/// Turn a wire query plan into a typed plan plus its parameter bindings.
///
/// A precompiled plan arrives as canonical JSON of the plan IR; otherwise the
/// SQL-subset front end parses the source text *into* plan nodes. Either way
/// what comes out is a typed [`theta_query::Plan`] — there is no path from wire
/// text to execution that does not pass through the IR
/// (`04-threat-model-security.md` §4).
fn plan_from_wire(
    plan: &theta_proto::wire::QueryPlanWire,
) -> Result<(theta_query::Plan, theta_query::Bindings), EngineError> {
    let parsed = match serde_json::from_str::<theta_query::Plan>(&plan.raw_query) {
        Ok(typed) => typed,
        Err(_) => {
            theta_query::sql::compile(&plan.raw_query).map_err(|e| EngineError::BadRequest {
                detail: format!("cannot plan query: {e}"),
            })?
        }
    };

    let mut bindings = theta_query::Bindings::new();
    for (name, json) in &plan.context_vars {
        bindings.insert(name.clone(), decode_value(json)?);
    }
    Ok((parsed, bindings))
}

// ---- encoding helpers -------------------------------------------------------

fn encode_value(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "null".into())
}

fn decode_value(json: &str) -> Result<Value, EngineError> {
    serde_json::from_str(json).map_err(|e| EngineError::BadRequest {
        detail: format!("value is not a valid encoding: {e}"),
    })
}

fn decode_change(json: &str) -> Result<SchemaChange, EngineError> {
    serde_json::from_str(json).map_err(|e| EngineError::BadRequest {
        detail: format!("schema change is not a valid encoding: {e}"),
    })
}

pub fn diff_to_wire(diff: &ChangeDiff) -> ChangeDiffWire {
    ChangeDiffWire {
        change_id: diff.change_id.0.clone(),
        destructive: diff.destructive,
        rows_affected: diff.rows_affected,
        reversible: diff.reversible,
        estimated_cost_ms: diff.estimated_cost_ms,
        requires_confirm: diff.requires_confirm,
        gate: match diff.gate {
            Gate::AutoApply => GateWire::AutoApply,
            Gate::Confirm => GateWire::Confirm,
            Gate::ShadowValidate => GateWire::ShadowValidate,
        },
        rule: match diff.rationale.rule {
            GateRule::IrreversibleOverShadowThreshold { .. } => {
                GateRuleWire::IrreversibleOverShadowThreshold
            }
            GateRule::Destructive { .. } => GateRuleWire::Destructive,
            GateRule::OverRowImpactThreshold { .. } => GateRuleWire::OverRowImpactThreshold,
            GateRule::AutoApprovedByPolicy { .. } => GateRuleWire::AutoApprovedByPolicy,
            GateRule::WithinThresholds { .. } => GateRuleWire::WithinThresholds,
        },
        remedy: match diff.rationale.remedy {
            None => RemedyWire::None,
            Some(Remedy::Confirm) => RemedyWire::Confirm,
            Some(Remedy::ValidateOnShadowBranch) => RemedyWire::ValidateOnShadowBranch,
            Some(Remedy::ReduceBlastRadius { .. }) => RemedyWire::ReduceBlastRadius,
        },
        // The threshold the rule actually compared against. Zero where the rule
        // read none, which is not the same as a threshold of zero — but a rule
        // that reads no threshold also sends no remedy that mentions one, so
        // there is nothing a caller can misread it as.
        rule_threshold: match &diff.rationale.rule {
            GateRule::IrreversibleOverShadowThreshold {
                shadow_threshold, ..
            } => *shadow_threshold,
            GateRule::OverRowImpactThreshold {
                row_impact_threshold,
                ..
            } => *row_impact_threshold,
            GateRule::AutoApprovedByPolicy { max_rows, .. } => *max_rows,
            GateRule::Destructive { .. } | GateRule::WithinThresholds { .. } => 0,
        },
        shadow_branch_id: diff.shadow_branch_id.unwrap_or(0),
        reason: diff.reason.clone(),
        affected_table: diff.affected_schema.table.clone(),
        affected_column: diff.affected_schema.column.clone().unwrap_or_default(),
        change_type: diff.affected_schema.change_type.clone(),
    }
}

fn merge_to_wire(outcome: MergeOutcome) -> MergeResultWire {
    match outcome {
        MergeOutcome::UpToDate => MergeResultWire {
            status: MergeStatus::UpToDate,
            ..Default::default()
        },
        MergeOutcome::Merged { converged, .. } => MergeResultWire {
            status: MergeStatus::Ok,
            converged,
            ..Default::default()
        },
        MergeOutcome::Conflicted { conflicts } => MergeResultWire {
            status: MergeStatus::Conflict,
            conflicts: conflicts
                .into_iter()
                .map(|c| ConflictRefWire {
                    key: c.key,
                    ours_json: c.ours.map(encode_value).unwrap_or_default(),
                    theirs_json: c.theirs.map(encode_value).unwrap_or_default(),
                    reason: c.reason,
                })
                .collect(),
            ..Default::default()
        },
    }
}

fn to_wire_error(err: EngineError) -> WireError {
    let (code, diff) = match &err {
        // The diff travels with the refusal so the caller can act on it without
        // a second round trip — the whole point of propose → diff → confirm.
        EngineError::Gated { diff, .. } => {
            (StatusCode::ConfirmationRequired, Some(diff_to_wire(diff)))
        }
        EngineError::BreakerOpen { .. } => (StatusCode::BreakerOpen, None),
        // Both are the caller's request being malformed in a way no retry of
        // the same request fixes. `Rejected` says "send a corrected one", which
        // is exactly the advice: drop the duplicate, or do not send an empty
        // transaction.
        EngineError::EmptyTransaction | EngineError::DuplicateKeyInTransaction { .. } => {
            (StatusCode::Rejected, None)
        }
        // The caller sent a well-formed request that the data refuses. Rejected
        // rather than mapped to the precondition code: a precondition failure
        // says "retry with what you just read", and retrying this unchanged
        // fails identically. The message names the row already holding the
        // value, which is the only thing that helps.
        EngineError::UniqueViolation { .. } => (StatusCode::Rejected, None),
        // Mapped to the breaker's code rather than to a rejection, and the two
        // genuinely are the same shape of answer: a limit this project set has
        // been reached, nothing is wrong with the request, and retrying it
        // unchanged will fail identically until the window rolls.
        //
        // `Rejected` would tell a caller to fix their request, which is the one
        // thing that cannot help here.
        EngineError::ReviewExhausted { .. } => (StatusCode::BreakerOpen, None),
        // Anchoring is background integrity work, never something a request
        // asked for. Reporting it against the caller's request would blame a
        // client for an operator's problem.
        EngineError::Anchor(_) => (StatusCode::Internal, None),
        // Asking for a point the branch cannot answer is the caller's request
        // being wrong about history, not the instance being broken. Rejected, so
        // the message — which names the horizon — reaches them.
        EngineError::Temporal(_) => (StatusCode::Rejected, None),
        // Routing to a region nothing serves is the caller naming somewhere that
        // does not exist. Rejected, so the message naming the region reaches them.
        EngineError::Region(_) => (StatusCode::Rejected, None),
        // A signature that does not verify is not retryable and is not a scope
        // problem: the caller has to send a corrected request, which is exactly
        // what `Rejected` means.
        EngineError::Signature(_) => (StatusCode::Rejected, None),
        // A race for a position in the log, though. That one *is* retryable, and
        // `Conflict` is the code a client already retries on.
        EngineError::SignedWriteRaced { .. } => (StatusCode::Conflict, None),
        EngineError::TypeMismatch { .. } | EngineError::BadRequest { .. } => {
            (StatusCode::Rejected, None)
        }
        EngineError::UnknownChange(_) => (StatusCode::Rejected, None),
        // Should never reach here: the `PutIf` arm answers it as a
        // `PreconditionFailed` response. Mapped rather than unreachable!()
        // because a future caller of `put_if` that forgets to catch it should
        // get a refusal the client can read, not a panic in the server.
        EngineError::PreconditionFailed { .. } => (StatusCode::Rejected, None),
        // `Core` joins these: a key that will not parse or bytes that will not
        // open are the server's configuration being wrong, not the caller's
        // request. Nothing the caller changes fixes it.
        EngineError::NotImplemented { .. }
        | EngineError::Storage(_)
        | EngineError::Core(_)
        | EngineError::Audit(_) => (StatusCode::Internal, None),
    };
    WireError {
        code,
        message: err.to_string(),
        diff,
    }
}
