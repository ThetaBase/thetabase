//! Host JSON to wire messages, and back.
//!
//! This is the step that must happen exactly once. A binding that built wire
//! messages in its own language would be a second implementation of the
//! protocol, and the first schema change would leave it describing something
//! the server no longer accepts — which is the drift `make sdk-check` catches
//! for types and this prevents for behaviour.

use theta_proto::wire::QueryPlanWire;
use theta_proto::{RequestBody, Response, ResponseBody};

use crate::HostRequest;

pub fn to_wire(host: &HostRequest) -> Result<RequestBody, String> {
    Ok(match host {
        HostRequest::Get { key } => RequestBody::Get { key: key.clone() },

        HostRequest::Put { key, value } => RequestBody::Put {
            key: key.clone(),
            value_json: encode_value(value)?,
            ttl: 0,
        },

        HostRequest::PutIf { key, value, expect } => RequestBody::PutIf {
            key: key.clone(),
            value_json: encode_value(value)?,
            ttl: 0,
            expect: match expect {
                crate::Expect::Absent => theta_proto::wire::Precondition::Absent,
                crate::Expect::Version { value } => {
                    theta_proto::wire::Precondition::Version(*value)
                }
            },
        },

        HostRequest::Delete { key } => RequestBody::Delete { key: key.clone() },

        HostRequest::Transaction { ops } => RequestBody::Transaction {
            ops: ops
                .iter()
                .map(|op| {
                    Ok(theta_proto::wire::TxOp {
                        key: op.key.clone(),
                        expect: op.expect.as_ref().map(|e| match e {
                            crate::Expect::Absent => theta_proto::wire::Precondition::Absent,
                            crate::Expect::Version { value } => {
                                theta_proto::wire::Precondition::Version(*value)
                            }
                        }),
                        action: match &op.action {
                            crate::HostTxAction::Put { value } => {
                                theta_proto::wire::TxAction::Put {
                                    value_json: encode_value(value)?,
                                    ttl: 0,
                                }
                            }
                            crate::HostTxAction::Delete => theta_proto::wire::TxAction::Delete,
                        },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        },

        HostRequest::Query { sql, params } => RequestBody::Query(plan(sql, params)?),
        HostRequest::Explain { sql, params } => RequestBody::Explain(plan(sql, params)?),

        HostRequest::ProposeSchemaChange { change } => RequestBody::ProposeSchemaChange {
            change_json: serde_json::to_string(change).map_err(|e| e.to_string())?,
        },

        HostRequest::ApplySchemaChange { change_id, confirm } => RequestBody::ApplySchemaChange {
            change_id: change_id.clone(),
            confirm: *confirm,
        },

        HostRequest::ShowChange { change_id } => RequestBody::ShowChange {
            change_id: change_id.clone(),
        },
        HostRequest::PromoteChange { change_id } => RequestBody::PromoteChange {
            change_id: change_id.clone(),
        },
        HostRequest::RejectChange { change_id, reason } => RequestBody::RejectChange {
            change_id: change_id.clone(),
            reason: reason.clone(),
        },

        HostRequest::CreateBranch { name, from } => RequestBody::CreateBranch {
            name: name.clone(),
            // A branch is named on the wire by id. The host resolves a name to
            // an id through `ListBranches` before asking, because guessing
            // would mean branching from whatever happened to match.
            from: from
                .as_deref()
                .map(parse_branch_id)
                .transpose()?
                .unwrap_or(0),
        },

        HostRequest::ListBranches => RequestBody::ListBranches,

        HostRequest::DiscardBranch { name } => RequestBody::DiscardBranch { name: name.clone() },

        HostRequest::Merge { source, into } => RequestBody::Merge {
            source_branch: parse_branch_id(source)?,
            target_branch: parse_branch_id(into)?,
        },

        HostRequest::Audit { limit, min_risk } => RequestBody::Audit {
            limit: *limit,
            min_risk: u8::try_from(*min_risk)
                .map_err(|_| format!("minRisk {min_risk} is outside 0-255"))?,
        },

        HostRequest::Status => RequestBody::Status,
    })
}

/// A host's plain JSON as the row encoding the server reads.
///
/// Canonical, and produced here rather than by the host: two hosts serialising
/// the same value differently would give the same row two representations on
/// the wire.
///
/// The conversion through [`theta_core::Value`] is the part that was missing.
/// This wrote the host's JSON straight out — `{"email":"alice@example.com"}` —
/// and the server decodes a row as a `Value`, whose encoding is adjacently
/// tagged because a value's type is never inferred after the fact. Every write
/// from every SDK was refused with "missing field `kind`", and the conformance
/// suite reported three bindings in perfect agreement.
fn encode_value(value: &serde_json::Value) -> Result<String, String> {
    serde_json::to_string(&theta_core::Value::from_json(value)).map_err(|e| e.to_string())
}

fn plan(
    sql: &str,
    params: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<QueryPlanWire, String> {
    // Parameters are bound, never interpolated — the invariant that makes a SQL
    // literal incapable of becoming syntax (`04-threat-model-security.md` §4).
    // Sorted so the same query with the same bindings produces the same bytes
    // whatever order the host's map iterates in.
    //
    // Encoded as `theta_core::Value`, which is what the server decodes each
    // binding as. This used to forward the host's plain JSON unchanged, and the
    // server refused every parameterised query with "value is not a valid
    // encoding" — a host writes `30`, and the wire encoding is
    // `{"kind":"int","value":30}` because a value's type is never inferred after
    // the fact. Converting here rather than in each binding is the whole reason
    // this core exists.
    let mut context_vars: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| {
            Ok((
                k.clone(),
                serde_json::to_string(&theta_core::Value::from_json(v))
                    .map_err(|e: serde_json::Error| e.to_string())?,
            ))
        })
        .collect::<Result<_, String>>()?;
    context_vars.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(QueryPlanWire {
        plan_hash: 0,
        raw_query: sql.to_string(),
        context_vars,
    })
}

fn parse_branch_id(name: &str) -> Result<u64, String> {
    match name {
        "main" => Ok(0),
        other => other.parse::<u64>().map_err(|_| {
            format!(
                "`{other}` is not a branch id. Branches are named on the wire by id; \
                 resolve the name through listBranches first rather than guessing."
            )
        }),
    }
}

/// A response, as JSON the host hands back to its caller.
///
/// The shape mirrors the generated `ResponseBody` union: a `kind` and a value.
/// A host that already has the generated types can narrow on `kind` and get the
/// right one.
pub fn from_wire(response: &Response) -> serde_json::Value {
    let (kind, value) = match &response.body {
        ResponseBody::Get {
            found,
            value_json,
            version_id,
        } => (
            "get",
            serde_json::json!({
                "found": found,
                "versionId": version_id,
                // Back to plain JSON, matching what the host wrote. The
                // server stores and returns the tagged `Value` encoding; a host
                // that wrote `{"email": "..."}` and read back
                // `{"kind":"map","value":{...}}` has not round-tripped its own
                // row. `to_json` documents the two types that do not survive.
                "value": serde_json::from_str::<theta_core::Value>(value_json)
                    .map(|v| v.to_json())
                    .unwrap_or(serde_json::Value::Null),
            }),
        ),
        ResponseBody::Put { commit_id }
        | ResponseBody::Delete { commit_id }
        | ResponseBody::Apply { commit_id }
        // One commit id for the whole transaction, reported the same way a
        // single write is. A host that has to branch on which kind of write it
        // performed to read a commit id would be doing the core's job.
        | ResponseBody::Transaction { commit_id } => {
            ("commit", serde_json::json!({ "commitId": commit_id }))
        }
        // Its own kind, not folded into an error. A host narrowing on `kind`
        // should be able to retry a lost update without parsing a message, and
        // an ordinary retry must not read as a fault.
        ResponseBody::PreconditionFailed { key, found, actual } => (
            "preconditionFailed",
            serde_json::json!({
                "key": key,
                "found": found,
                "actual": actual,
            }),
        ),
        ResponseBody::Query {
            result_set,
            plan_hash,
            row_count,
        } => (
            "query",
            serde_json::json!({
                // Arrow IPC stays binary. Re-encoding rows as JSON here would
                // undo the reason result sets are Arrow at all, so the bytes
                // cross once and the host decodes them with an Arrow reader.
                "resultSetBase64": base64(result_set),
                "planHash": plan_hash,
                "rowCount": row_count,
            }),
        ),
        ResponseBody::Explain { explanation_json } => (
            "explain",
            serde_json::from_str::<serde_json::Value>(explanation_json)
                .unwrap_or(serde_json::Value::Null),
        ),
        ResponseBody::Propose(diff) => (
            "propose",
            serde_json::to_value(DiffJson::from(diff)).unwrap_or(serde_json::Value::Null),
        ),
        ResponseBody::Branch { branch_id } => {
            ("branch", serde_json::json!({ "branchId": branch_id }))
        }
        ResponseBody::Branches { branches } => (
            "branches",
            serde_json::json!(branches
                .iter()
                .map(|b| serde_json::json!({
                    "branchId": b.branch_id,
                    "name": b.name,
                    "kind": b.kind,
                    "head": b.head,
                    "protected": b.protected,
                }))
                .collect::<Vec<_>>()),
        ),
        ResponseBody::Merge(result) => (
            "merge",
            serde_json::json!({
                "status": format!("{:?}", result.status),
                // The conflicts themselves, not just a count: a non-CRDT
                // conflict goes to a human, and a human needs to see which
                // field on which row (03-data-model-consistency.md §4).
                "conflictCount": result.conflicts.len(),
                "conflicts": result.conflicts
                    .iter()
                    .map(|c| serde_json::json!({
                        "key": c.key,
                        "reason": c.reason,
                        "ours": c.ours_json,
                        "theirs": c.theirs_json,
                    }))
                    .collect::<Vec<_>>(),
                "converged": result.converged,
            }),
        ),
        ResponseBody::Status(status) => (
            "status",
            serde_json::json!({
                "projectId": status.project_id,
                "branch": status.branch,
                "writeVolumeMB": status.write_volume_mb,
                "circuitBreakerTripped": status.circuit_breaker_tripped,
                "breakerWindowRows": status.breaker_window_rows,
                "commitsApplied": status.commits_applied,
                "storageBytes": status.storage_bytes,
                "rowsWritten": status.rows_written,
            }),
        ),
        ResponseBody::Audit { entries } => (
            "audit",
            serde_json::json!(entries
                .iter()
                .map(|e| serde_json::json!({
                    "risk": e.risk,
                    "summary": e.summary,
                    "author": serde_json::from_str::<serde_json::Value>(&e.author_json)
                        .unwrap_or(serde_json::Value::Null),
                    "timestampMs": e.timestamp_ms,
                    "detail": serde_json::from_str::<serde_json::Value>(&e.detail_json)
                        .unwrap_or(serde_json::Value::Null),
                }))
                .collect::<Vec<_>>()),
        ),
        // A gate refusal is an *answer*, not a transport failure: it carries the
        // diff the caller has to act on, so it stays a response body here
        // rather than being flattened into a connection error.
        ResponseBody::Error(e) => (
            "error",
            serde_json::json!({ "code": format!("{:?}", e.code), "message": e.message }),
        ),
        // The call did what was asked and has nothing to report. A host that
        // could not tell this from an unrecognised response would have to guess
        // whether its write happened.
        ResponseBody::Ok => ("ok", serde_json::Value::Null),

        // `describe` — what is in here and where it came from. `llms.txt` calls
        // this the first thing an agent arriving at an unfamiliar database
        // asks, and until this arm existed the answer was a debug string.
        ResponseBody::Description(description) => (
            "description",
            serde_json::json!({
                "tables": description.tables.iter().map(|t| serde_json::json!({
                    "name": t.name,
                    "columns": t.columns.iter().map(|c| serde_json::json!({
                        "name": c.name,
                        "type": c.ty,
                        "nullable": c.nullable,
                        "crdt": c.crdt,
                        // Whether an agent has written this column, which is
                        // the question a reviewer asks about a column they do
                        // not recognise.
                        "touchedByAgent": c.touched_by_agent,
                        "examples": c.examples,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                // Reported rather than silently omitted: a caller shown no
                // examples should be able to tell "there are none" from "you
                // may not see them".
                "examplesWithheld": description.examples_withheld,
                "withheldReason": description.withheld_reason,
            }),
        ),

        // A proposal's current state, including what validating it found.
        ResponseBody::Change(change) => (
            "change",
            serde_json::json!({
                "diff": DiffJson::from(&change.diff),
                "shadowBranchId": change.shadow_branch_id,
                "validationPassed": change.validation_passed,
                "validationSummary": change.validation_summary,
                "checks": change.checks.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "passed": c.passed,
                    "detail": c.detail,
                })).collect::<Vec<_>>(),
            }),
        ),

        // What is waiting for a human. A batch carries the strongest gate of
        // its members, and the reason names which member is responsible — a
        // reviewer's next question after "this needs validation" is *which one*.
        ResponseBody::ReviewQueue { batches } => (
            "reviewQueue",
            serde_json::json!({
                "batches": batches.iter().map(|b| serde_json::json!({
                    "key": b.key,
                    "gate": b.gate,
                    "rowsAffected": b.rows_affected,
                    "cost": b.cost,
                    "costIfUnbatched": b.cost_if_unbatched,
                    "reason": b.reason,
                })).collect::<Vec<_>>(),
            }),
        ),

        // **A catch-all, and it hides things.** A response variant added to the
        // wire without an arm above compiles cleanly and reaches every binding
        // as `kind: "raw"` with a debug string in it — which is what happened
        // to `Transaction` before the arm above existed. It is kept because a
        // host talking to a *newer* server must degrade rather than panic, and
        // that is a real requirement; but anything added on our side must be
        // given an arm, and `translate_covers_every_response.rs` is what makes
        // forgetting fail.
        other => ("raw", serde_json::json!({ "debug": format!("{other:?}") })),
    };

    serde_json::json!({
        "requestId": response.request_id,
        "kind": kind,
        "value": value,
    })
}

/// The diff, in the field names the generated bindings use.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffJson {
    change_id: String,
    destructive: bool,
    rows_affected: u64,
    reversible: bool,
    estimated_cost_ms: u32,
    requires_confirm: bool,
    shadow_branch_id: u64,
    reason: String,
    affected_table: String,
    affected_column: String,
    change_type: String,
    gate: String,
}

impl From<&theta_proto::wire::ChangeDiffWire> for DiffJson {
    fn from(d: &theta_proto::wire::ChangeDiffWire) -> Self {
        Self {
            change_id: d.change_id.clone(),
            destructive: d.destructive,
            rows_affected: d.rows_affected,
            reversible: d.reversible,
            estimated_cost_ms: d.estimated_cost_ms,
            requires_confirm: d.requires_confirm,
            shadow_branch_id: d.shadow_branch_id,
            reason: d.reason.clone(),
            affected_table: d.affected_table.clone(),
            affected_column: d.affected_column.clone(),
            change_type: d.change_type.clone(),
            gate: format!("{:?}", d.gate),
        }
    }
}

/// Base64, written out rather than pulled in: this crate compiles to WebAssembly
/// and every dependency is bytes the caller downloads.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(match chunk.len() > 1 {
            true => ALPHABET[(n >> 6 & 63) as usize] as char,
            false => '=',
        });
        out.push(match chunk.len() > 2 {
            true => ALPHABET[(n & 63) as usize] as char,
            false => '=',
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
