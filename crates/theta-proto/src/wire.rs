//! Typed Rust view of the wire messages, and their Cap'n Proto encoding.
//!
//! The generated bindings are the encoding; these types are what the rest of the
//! codebase works with. Keeping the two separate means the server and the client
//! pattern-match on ordinary Rust enums, while the bytes on the wire stay
//! exactly what `schema/theta.capnp` says they are — and a schema change that
//! breaks the mapping fails to compile rather than silently changing behaviour.
//!
//! Values cross the wire as canonical JSON text rather than as a Cap'n Proto
//! union of value types. That keeps schema evolution out of the protocol: a new
//! `Value` variant is a `theta-core` change, not a wire-format bump
//! (`02-api-wire-protocol.md` §5).

use crate::frame::{FrameError, Result};
use crate::StatusCode;

use crate::theta_capnp as generated;

use capnp::message::{Builder, ReaderOptions};
use capnp::serialize;

fn malformed(what: impl std::fmt::Display) -> FrameError {
    FrameError::Malformed(what.to_string())
}

/// Parse a framed payload into a Cap'n Proto message reader.
fn read_root(bytes: &[u8]) -> Result<capnp::message::Reader<serialize::OwnedSegments>> {
    serialize::read_message(bytes, ReaderOptions::new()).map_err(malformed)
}

// ---- handshake --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: u32,
    pub session_token: String,
    pub client_name: String,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut message = Builder::new_default();
        {
            let mut root = message.init_root::<generated::hello::Builder>();
            root.set_protocol_version(self.protocol_version);
            root.set_session_token(self.session_token.as_str());
            root.set_client_name(self.client_name.as_str());
        }
        serialize::write_message_to_words(&message)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let message = read_root(bytes)?;
        let root: generated::hello::Reader = message.get_root().map_err(malformed)?;
        Ok(Self {
            protocol_version: root.get_protocol_version(),
            session_token: text(root.get_session_token())?,
            client_name: text(root.get_client_name())?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub protocol_version: u32,
    pub project_id: String,
    pub server_name: String,
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut message = Builder::new_default();
        {
            let mut root = message.init_root::<generated::welcome::Builder>();
            root.set_protocol_version(self.protocol_version);
            root.set_project_id(self.project_id.as_str());
            root.set_server_name(self.server_name.as_str());
        }
        serialize::write_message_to_words(&message)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let message = read_root(bytes)?;
        let root: generated::welcome::Reader = message.get_root().map_err(malformed)?;
        Ok(Self {
            protocol_version: root.get_protocol_version(),
            project_id: text(root.get_project_id())?,
            server_name: text(root.get_server_name())?,
        })
    }
}

// ---- requests ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub request_id: u64,
    /// Branch to operate on. Zero is `main`.
    pub branch_id: u64,
    pub body: RequestBody,
}

/// What a signed write actually writes.
///
/// The data operations, and only those. Schema changes go through the proposal
/// flow, where the record that matters is the classifier's decision rather than
/// the entry's authorship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignedOp {
    Put { key: String, value_json: String },
    Delete { key: String },
    Crdt { key: String, mutation: CrdtMutation },
}

/// Identity of one element of a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElemId {
    pub counter: u64,
    pub replica: u64,
}

/// A CRDT mutation, in the shape a caller may send one.
///
/// Deliberately **not** `theta_core::CrdtOp`. That type carries an element id on
/// `SeqInsert`, and an id decides both an element's identity and its position
/// among concurrent siblings — a client that chose its own could collide with
/// another writer's element or displace one. The server assigns it from
/// (branch, commit).
///
/// Expressed as a type that cannot hold one rather than as a field the server
/// remembers to ignore. A field that must be ignored is a field somebody
/// eventually reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrdtMutation {
    /// PN-Counter. Negative values decrement.
    Increment {
        by: i64,
    },
    /// LWW-Register, tie-broken by (timestamp, branch, commit).
    SetRegister {
        value_json: String,
    },
    SetAdd {
        element_json: String,
    },
    SetRemove {
        element_json: String,
    },
    /// `after` absent inserts at the head of the list.
    SeqInsert {
        after: Option<ElemId>,
        value_json: String,
    },
    /// Names an element the caller read. The one place an id travels inward.
    SeqRemove {
        id: ElemId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Get {
        key: String,
    },
    Put {
        key: String,
        value_json: String,
        ttl: u64,
    },
    /// A mutation of a CRDT-typed row (`01-system-architecture.md` §3.3).
    ///
    /// Separate from [`RequestBody::Put`] rather than a variant of it. A
    /// CRDT-typed row converges by replaying *operations*; a caller reaching it
    /// through `put` would be writing an absolute value, and two absolute values
    /// cannot be reconciled without choosing one.
    Crdt {
        key: String,
        mutation: CrdtMutation,
    },
    /// What is waiting for a human, grouped so it can be answered together.
    ///
    /// Distinct from [`RequestBody::Audit`], which is what *happened*. This is
    /// what has not happened yet, and reading one for the other is how somebody
    /// concludes the queue is empty because nothing was logged.
    ReviewQueue,

    /// A write the caller signed with its session key
    /// (`04-threat-model-security.md` 7.2).
    ///
    /// Separate from the unsigned requests for the same reason
    /// [`RequestBody::PutIf`] is separate from [`RequestBody::Put`]: a signature
    /// that can be dropped in transit is worse than none, because the caller
    /// believes the record is checkable and it is not.
    SignedWrite {
        commit_id: u64,
        timestamp_ms: i64,
        signature: Vec<u8>,
        op: SignedOp,
    },
    /// A write with a precondition on the row's current version (M10.5).
    ///
    /// A distinct request rather than optional fields on [`RequestBody::Put`],
    /// so an older server refuses the call outright instead of accepting the
    /// write and silently dropping the condition. A precondition that can be
    /// lost in transit is worse than none: the caller believes they are
    /// protected and they are not.
    PutIf {
        key: String,
        value_json: String,
        ttl: u64,
        expect: Precondition,
    },
    Delete {
        key: String,
    },
    /// Several writes that land as one commit, or not at all.
    ///
    /// A separate request rather than a flag on a batched write, for the same
    /// reason [`RequestBody::PutIf`] is separate from [`RequestBody::Put`]: a
    /// server that predates this refuses the call outright instead of applying
    /// the writes one at a time and leaving the caller believing they were
    /// atomic. Atomicity lost in transit is worse than atomicity never offered,
    /// because a partial result looks exactly like a whole one.
    ///
    /// Distinct from any batched write that exists for throughput — that kind
    /// batches the *network*, not the durability boundary.
    Transaction {
        ops: Vec<TxOp>,
    },
    Query(QueryPlanWire),
    Explain(QueryPlanWire),
    /// No impact fields. The server measures what a change would touch against
    /// the branch's own view; a caller that could name its own row count could
    /// name zero and walk a drop past the gate that reads it.
    ProposeSchemaChange {
        change_json: String,
    },
    /// Confirm a pending change.
    ///
    /// Carries the id and the confirmation, and deliberately neither the change
    /// body nor a branch: the server applies what it classified under this id,
    /// on the branch that proposal targeted. See `ApplyRequest` in the schema.
    ApplySchemaChange {
        change_id: String,
        confirm: bool,
    },
    CreateBranch {
        name: String,
        from: u64,
    },
    Merge {
        source_branch: u64,
        target_branch: u64,
    },
    Status,
    /// Control Plane only. Carries a signed revocation list; the instance
    /// verifies it with the project key it already holds, so this needs no
    /// separate authentication mechanism — and no HTTP client on the instance
    /// side, which keeps the hot-path dependency guard intact.
    PushRevocations {
        payload: Vec<u8>,
        signature: Vec<u8>,
        key_id: String,
    },

    // ---- review surface (`07-agent-safety-layer.md` §5, §7) ----
    Audit {
        limit: u32,
        /// Entries below this risk are omitted.
        min_risk: u8,
    },
    ListBranches,
    DiscardBranch {
        name: String,
    },
    ShowChange {
        change_id: String,
    },
    /// Land a validated change by merging its shadow branch.
    ///
    /// Deliberately not a flag on `ApplySchemaChange`: that call is driven by
    /// confirmation, and a change at the shadow gate refuses confirmation
    /// outright. Two names for two different things a caller can ask for.
    PromoteChange {
        change_id: String,
    },
    RejectChange {
        change_id: String,
        reason: String,
    },
    /// What is in here, and where it came from (M19).
    ///
    /// A first-class call rather than something a caller assembles from `query`
    /// and `audit`: an agent arriving at an unfamiliar database asks this
    /// first, and every step it has to take before it can ask is a step it can
    /// get wrong.
    Describe {
        /// Empty describes every table.
        table: String,
        /// Draw example values. Off by default — see the schema comment.
        include_examples: bool,
        /// Zero means the server's default.
        example_limit: u32,
    },
    /// Control Plane only: a safety policy signed with the project key.
    ///
    /// Signed rather than authorized, so an agent session token has no way to
    /// produce one (`07-agent-safety-layer.md` §7).
    PushPolicy {
        payload: Vec<u8>,
        signature: Vec<u8>,
        key_id: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryPlanWire {
    pub plan_hash: u64,
    pub raw_query: String,
    /// Bound parameters, as canonical JSON. Never interpolated into `raw_query`.
    pub context_vars: Vec<(String, String)>,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut message = Builder::new_default();
        {
            let mut root = message.init_root::<generated::request::Builder>();
            root.set_request_id(self.request_id);
            root.set_branch_id(self.branch_id);
            let body = root.init_body();

            match &self.body {
                RequestBody::Get { key } => {
                    body.init_get().set_key(key.as_str());
                }
                RequestBody::Delete { key } => {
                    body.init_delete().set_key(key.as_str());
                }
                RequestBody::Put {
                    key,
                    value_json,
                    ttl,
                } => {
                    let mut b = body.init_put();
                    b.set_key(key.as_str());
                    b.set_value(value_json.as_str());
                    b.set_ttl(*ttl);
                }
                RequestBody::SignedWrite {
                    commit_id,
                    timestamp_ms,
                    signature,
                    op,
                } => {
                    let mut b = body.init_signed_write();
                    b.set_commit_id(*commit_id);
                    b.set_timestamp_ms(*timestamp_ms);
                    b.set_signature(signature);
                    let o = b.init_op();
                    match op {
                        SignedOp::Put { key, value_json } => {
                            let mut p = o.init_put();
                            p.set_key(key.as_str());
                            p.set_value(value_json.as_str());
                        }
                        SignedOp::Delete { key } => o.init_delete().set_key(key.as_str()),
                        SignedOp::Crdt { key, mutation } => {
                            let mut c = o.init_crdt();
                            c.set_key(key.as_str());
                            encode_mutation(c.init_mutation(), mutation);
                        }
                    }
                }
                RequestBody::Crdt { key, mutation } => {
                    let mut b = body.init_crdt();
                    b.set_key(key.as_str());
                    encode_mutation(b.init_mutation(), mutation);
                }
                RequestBody::PutIf {
                    key,
                    value_json,
                    ttl,
                    expect,
                } => {
                    let mut b = body.init_put_if();
                    b.set_key(key.as_str());
                    b.set_value(value_json.as_str());
                    b.set_ttl(*ttl);
                    // The union is a nested group, so the setters live on it
                    // rather than on the request.
                    let mut e = b.get_expect();
                    match expect {
                        Precondition::Absent => e.set_absent(()),
                        Precondition::Version(v) => e.set_version(*v),
                    }
                }
                RequestBody::Transaction { ops } => {
                    let mut b = body.init_transaction();
                    let mut list = b.reborrow().init_ops(ops.len() as u32);
                    for (i, op) in ops.iter().enumerate() {
                        let mut o = list.reborrow().get(i as u32);
                        o.set_key(op.key.as_str());
                        // Written before the action so the borrow of the
                        // union group ends before the next one begins.
                        {
                            let mut e = o.reborrow().get_expect();
                            match &op.expect {
                                None => e.set_any(()),
                                Some(Precondition::Absent) => e.set_absent(()),
                                Some(Precondition::Version(v)) => e.set_version(*v),
                            }
                        }
                        let mut a = o.get_action();
                        match &op.action {
                            TxAction::Put { value_json, ttl } => {
                                let mut put = a.init_put();
                                put.set_value(value_json.as_str());
                                put.set_ttl(*ttl);
                            }
                            TxAction::Delete => a.set_delete(()),
                        }
                    }
                }
                RequestBody::Query(plan) => write_plan(body.init_query().init_plan(), plan),
                RequestBody::Explain(plan) => write_plan(body.init_explain().init_plan(), plan),
                RequestBody::ProposeSchemaChange { change_json } => {
                    let mut b = body.init_propose_schema_change();
                    b.set_change(change_json.as_str());
                }
                RequestBody::ApplySchemaChange { change_id, confirm } => {
                    let mut b = body.init_apply_schema_change();
                    b.set_change_id(change_id.as_str());
                    b.set_confirm(*confirm);
                }
                RequestBody::CreateBranch { name, from } => {
                    let mut b = body.init_create_branch();
                    b.set_name(name.as_str());
                    b.set_from(*from);
                }
                RequestBody::Merge {
                    source_branch,
                    target_branch,
                } => {
                    let mut b = body.init_merge();
                    b.set_source_branch(*source_branch);
                    b.set_target_branch(*target_branch);
                }
                RequestBody::Status => {
                    body.init_status();
                }
                RequestBody::PushRevocations {
                    payload,
                    signature,
                    key_id,
                } => {
                    let mut b = body.init_push_revocations();
                    b.set_payload(payload);
                    b.set_signature(signature);
                    b.set_key_id(key_id.as_str());
                }
                RequestBody::Audit { limit, min_risk } => {
                    let mut b = body.init_audit();
                    b.set_limit(*limit);
                    b.set_min_risk(*min_risk);
                }
                RequestBody::ReviewQueue => {
                    body.init_review_queue();
                }
                RequestBody::ListBranches => {
                    body.init_list_branches();
                }
                RequestBody::DiscardBranch { name } => {
                    body.init_discard_branch().set_name(name.as_str());
                }
                RequestBody::ShowChange { change_id } => {
                    body.init_show_change().set_change_id(change_id.as_str());
                }
                RequestBody::PromoteChange { change_id } => {
                    body.init_promote_change().set_change_id(change_id.as_str());
                }
                RequestBody::Describe {
                    table,
                    include_examples,
                    example_limit,
                } => {
                    let mut b = body.init_describe();
                    b.set_table(table.as_str());
                    b.set_include_examples(*include_examples);
                    b.set_example_limit(*example_limit);
                }
                RequestBody::RejectChange { change_id, reason } => {
                    let mut b = body.init_reject_change();
                    b.set_change_id(change_id.as_str());
                    b.set_reason(reason.as_str());
                }
                RequestBody::PushPolicy {
                    payload,
                    signature,
                    key_id,
                } => {
                    let mut b = body.init_push_policy();
                    b.set_payload(payload);
                    b.set_signature(signature);
                    b.set_key_id(key_id.as_str());
                }
            }
        }
        serialize::write_message_to_words(&message)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        use generated::request::body::Which;

        let message = read_root(bytes)?;
        let root: generated::request::Reader = message.get_root().map_err(malformed)?;

        let body = match root.get_body().which().map_err(malformed)? {
            Which::Describe(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::Describe {
                    table: text(r.get_table())?,
                    include_examples: r.get_include_examples(),
                    example_limit: r.get_example_limit(),
                }
            }
            Which::ReviewQueue(_) => RequestBody::ReviewQueue,
            Which::Crdt(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::Crdt {
                    key: text(r.get_key())?,
                    mutation: decode_mutation(r)?,
                }
            }
            Which::SignedWrite(r) => {
                use generated::signed_write_request::op::Which as O;
                let r = r.map_err(malformed)?;
                let op = match r.get_op().which().map_err(malformed)? {
                    O::Put(p) => {
                        let p = p.map_err(malformed)?;
                        SignedOp::Put {
                            key: text(p.get_key())?,
                            value_json: text(p.get_value())?,
                        }
                    }
                    O::Delete(d) => SignedOp::Delete {
                        key: text(d.map_err(malformed)?.get_key())?,
                    },
                    O::Crdt(c) => {
                        let c = c.map_err(malformed)?;
                        SignedOp::Crdt {
                            key: text(c.get_key())?,
                            mutation: decode_mutation(c)?,
                        }
                    }
                };
                RequestBody::SignedWrite {
                    commit_id: r.get_commit_id(),
                    timestamp_ms: r.get_timestamp_ms(),
                    signature: r.get_signature().map_err(malformed)?.to_vec(),
                    op,
                }
            }
            Which::Get(r) => RequestBody::Get {
                key: text(r.map_err(malformed)?.get_key())?,
            },
            Which::Delete(r) => RequestBody::Delete {
                key: text(r.map_err(malformed)?.get_key())?,
            },
            Which::Put(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::Put {
                    key: text(r.get_key())?,
                    value_json: text(r.get_value())?,
                    ttl: r.get_ttl(),
                }
            }
            Which::PutIf(r) => {
                let r = r.map_err(malformed)?;
                let expect = match r.get_expect().which().map_err(|_| {
                    // A precondition this build does not understand must never
                    // be treated as "no precondition". Refused, so an older
                    // server cannot silently perform an unconditional write.
                    malformed("unknown precondition kind")
                })? {
                    generated::put_if_request::expect::Which::Absent(()) => Precondition::Absent,
                    generated::put_if_request::expect::Which::Version(v) => {
                        Precondition::Version(v)
                    }
                };
                RequestBody::PutIf {
                    key: text(r.get_key())?,
                    value_json: text(r.get_value())?,
                    ttl: r.get_ttl(),
                    expect,
                }
            }
            Which::Transaction(r) => {
                let r = r.map_err(malformed)?;
                let mut ops = Vec::new();
                for o in r.get_ops().map_err(malformed)? {
                    // An unknown precondition or action must never decode to
                    // something weaker. Refused, for the reason `putIf` gives.
                    let expect = match o
                        .get_expect()
                        .which()
                        .map_err(|_| malformed("unknown precondition kind in a transaction"))?
                    {
                        generated::transaction_op::expect::Which::Any(()) => None,
                        generated::transaction_op::expect::Which::Absent(()) => {
                            Some(Precondition::Absent)
                        }
                        generated::transaction_op::expect::Which::Version(v) => {
                            Some(Precondition::Version(v))
                        }
                    };
                    let action = match o
                        .get_action()
                        .which()
                        .map_err(|_| malformed("unknown action kind in a transaction"))?
                    {
                        generated::transaction_op::action::Which::Put(p) => {
                            let p = p.map_err(malformed)?;
                            TxAction::Put {
                                value_json: text(p.get_value())?,
                                ttl: p.get_ttl(),
                            }
                        }
                        generated::transaction_op::action::Which::Delete(()) => TxAction::Delete,
                    };
                    ops.push(TxOp {
                        key: text(o.get_key())?,
                        expect,
                        action,
                    });
                }
                RequestBody::Transaction { ops }
            }
            Which::Query(r) => RequestBody::Query(read_plan(
                r.map_err(malformed)?.get_plan().map_err(malformed)?,
            )?),
            Which::Explain(r) => RequestBody::Explain(read_plan(
                r.map_err(malformed)?.get_plan().map_err(malformed)?,
            )?),
            Which::ProposeSchemaChange(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::ProposeSchemaChange {
                    change_json: text(r.get_change())?,
                }
            }
            Which::ApplySchemaChange(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::ApplySchemaChange {
                    change_id: text(r.get_change_id())?,
                    confirm: r.get_confirm(),
                }
            }
            Which::CreateBranch(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::CreateBranch {
                    name: text(r.get_name())?,
                    from: r.get_from(),
                }
            }
            Which::Merge(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::Merge {
                    source_branch: r.get_source_branch(),
                    target_branch: r.get_target_branch(),
                }
            }
            Which::Status(_) => RequestBody::Status,
            Which::Audit(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::Audit {
                    limit: r.get_limit(),
                    min_risk: r.get_min_risk(),
                }
            }
            Which::ListBranches(_) => RequestBody::ListBranches,
            Which::DiscardBranch(r) => RequestBody::DiscardBranch {
                name: text(r.map_err(malformed)?.get_name())?,
            },
            Which::ShowChange(r) => RequestBody::ShowChange {
                change_id: text(r.map_err(malformed)?.get_change_id())?,
            },
            Which::PromoteChange(r) => RequestBody::PromoteChange {
                change_id: text(r.map_err(malformed)?.get_change_id())?,
            },
            Which::RejectChange(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::RejectChange {
                    change_id: text(r.get_change_id())?,
                    reason: text(r.get_reason())?,
                }
            }
            Which::PushPolicy(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::PushPolicy {
                    payload: r.get_payload().map_err(malformed)?.to_vec(),
                    signature: r.get_signature().map_err(malformed)?.to_vec(),
                    key_id: text(r.get_key_id())?,
                }
            }
            Which::PushRevocations(r) => {
                let r = r.map_err(malformed)?;
                RequestBody::PushRevocations {
                    payload: r.get_payload().map_err(malformed)?.to_vec(),
                    signature: r.get_signature().map_err(malformed)?.to_vec(),
                    key_id: text(r.get_key_id())?,
                }
            }
        };

        Ok(Self {
            request_id: root.get_request_id(),
            branch_id: root.get_branch_id(),
            body,
        })
    }
}

fn write_plan(mut builder: generated::query_plan::Builder, plan: &QueryPlanWire) {
    builder.set_plan_hash(plan.plan_hash);
    builder.set_raw_query(plan.raw_query.as_str());
    let mut vars = builder.init_context_vars(plan.context_vars.len() as u32);
    for (i, (key, value)) in plan.context_vars.iter().enumerate() {
        let mut kv = vars.reborrow().get(i as u32);
        kv.set_key(key.as_str());
        kv.set_value(value.as_bytes());
    }
}

fn read_plan(reader: generated::query_plan::Reader) -> Result<QueryPlanWire> {
    let mut context_vars = Vec::new();
    for kv in reader.get_context_vars().map_err(malformed)?.iter() {
        let value = kv.get_value().map_err(malformed)?;
        context_vars.push((
            text(kv.get_key())?,
            String::from_utf8(value.to_vec()).map_err(malformed)?,
        ));
    }
    Ok(QueryPlanWire {
        plan_hash: reader.get_plan_hash(),
        raw_query: text(reader.get_raw_query())?,
        context_vars,
    })
}

// ---- responses --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub request_id: u64,
    pub body: ResponseBody,
}

/// What a conditional write requires of the row it is replacing.
///
/// An enum rather than a nullable version, because "no version supplied" and
/// "this row must not exist" are different requests and the second is a much
/// stronger claim than anyone makes by forgetting a field. A sentinel zero
/// would collapse them — the same shape of mistake as treating a missing
/// subscription as unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// The row must not exist. Create-only.
    Absent,
    /// The row must be at exactly this version — the `version_id` a `Get`
    /// returned.
    Version(u64),
}

/// One operation inside a [`RequestBody::Transaction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOp {
    pub key: String,
    /// Checked against the branch before *any* operation in the transaction is
    /// applied, so a transaction that would violate a condition changes
    /// nothing. This is what lets one call say "record the payment and mark the
    /// invoice paid, and only if neither has been done already".
    pub expect: Option<Precondition>,
    pub action: TxAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxAction {
    Put { value_json: String, ttl: u64 },
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Error(WireError),
    /// One commit id, for the whole transaction. Singular deliberately: a
    /// transaction that returned an id per operation would be describing
    /// something that did not happen.
    Transaction {
        commit_id: String,
    },
    Get {
        found: bool,
        value_json: String,
        version_id: u64,
    },
    Put {
        commit_id: String,
    },
    Delete {
        commit_id: String,
    },
    Query {
        result_set: Vec<u8>,
        plan_hash: u64,
        row_count: u64,
    },
    Explain {
        explanation_json: String,
    },
    Propose(ChangeDiffWire),
    Apply {
        commit_id: String,
    },
    Branch {
        branch_id: u64,
    },
    Merge(MergeResultWire),
    Status(ProjectStatusWire),
    Revocations {
        version: u64,
        accepted: bool,
    },
    Audit {
        entries: Vec<AuditEntryWire>,
    },
    Branches {
        branches: Vec<BranchInfoWire>,
    },
    /// What is waiting for a human, grouped.
    ReviewQueue {
        batches: Vec<ReviewBatchWire>,
    },
    Change(ChangeStateWire),
    /// The call succeeded and has nothing to report.
    Ok,
    Policy {
        version: u64,
        accepted: bool,
    },
    /// A conditional write whose precondition did not hold (M10.5).
    ///
    /// Deliberately not an [`ResponseBody::Error`]. The request was well-formed
    /// and the server did exactly what it was asked; the row simply was not in
    /// the state the caller required. Conflating the two would make an ordinary
    /// lost-update retry indistinguishable from a fault, in logs and in error
    /// budgets alike — and a contended key would look like an outage.
    PreconditionFailed {
        key: String,
        /// True when the row exists but is at a different version.
        found: bool,
        /// The row's current version, so a retry needs no second round trip.
        /// Meaningless when `found` is false.
        actual: u64,
    },
    /// What is in here, and where it came from (M19).
    Description(SchemaDescriptionWire),
}

/// One line of the human-legible audit trail (`07-agent-safety-layer.md` §7).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditEntryWire {
    /// 0 info, 1 low, 2 medium, 3 high.
    pub risk: u8,
    pub summary: String,
    /// Canonical JSON of `theta_core::Author`.
    pub author_json: String,
    pub timestamp_ms: i64,
    pub detail_json: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchInfoWire {
    pub branch_id: u64,
    pub name: String,
    /// 0 protected, 1 standard, 2 shadow.
    pub kind: u8,
    pub head: String,
    pub protected: bool,
    /// The commit id the next entry on this branch will carry.
    ///
    /// What a caller signing a write needs: a signed entry commits to its own
    /// position, so the position has to be known before it can be signed for.
    pub next_commit: u64,
}

/// A group of changes a reviewer can answer together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewBatchWire {
    /// The grouping key: the table the changes are about.
    pub key: String,
    /// The gate the batch carries — the strongest of its members. A batch is
    /// never gated below its worst change, which is what stops batching from
    /// becoming a discount on risk.
    pub gate: u8,
    pub changes: Vec<ChangeDiffWire>,
    pub rows_affected: u64,
    pub cost: u32,
    pub cost_if_unbatched: u32,
    /// Why the batch carries the gate it does, naming the member responsible.
    /// A reviewer's next question after "this needs shadow validation" is always
    /// *which one*.
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationCheckWire {
    pub name: String,
    pub passed: bool,
    pub detail: String,
    /// Canonical JSON of the sampled rows.
    pub samples_json: String,
}

/// A proposal's current state: its diff, and what validating it found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangeStateWire {
    pub diff: ChangeDiffWire,
    pub shadow_branch_id: Option<u64>,
    /// `None` until verification has run.
    ///
    /// Modelled as an option rather than a bare bool so a client cannot read
    /// "not yet run" as "failed", or a default `true` as a pass.
    pub validation_passed: Option<bool>,
    pub validation_summary: String,
    pub checks: Vec<ValidationCheckWire>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WireError {
    pub code: StatusCode,
    pub message: String,
    /// Present when the Safety Layer refused a change, so the caller can act on
    /// the diff without a second round trip.
    pub diff: Option<ChangeDiffWire>,
}

/// What must happen before a change may land. Mirrors
/// `theta_safety::diff::Gate`; kept here so `theta-proto` stays free of a
/// dependency on the safety crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GateWire {
    #[default]
    AutoApply,
    Confirm,
    ShadowValidate,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangeDiffWire {
    pub change_id: String,
    pub destructive: bool,
    pub rows_affected: u64,
    pub reversible: bool,
    pub estimated_cost_ms: u32,
    pub requires_confirm: bool,
    /// What must actually happen before this lands. `requires_confirm` alone
    /// cannot distinguish a confirmable change from one where confirmation is
    /// not sufficient.
    pub gate: GateWire,
    /// Zero means no shadow branch exists yet.
    pub shadow_branch_id: u64,
    pub reason: String,
    pub affected_table: String,
    pub affected_column: String,
    pub change_type: String,
    /// Which rule produced the gate, and what would unblock it (M19).
    ///
    /// `reason` above is rendered *from* these rather than written beside them.
    /// Parsing the sentence to recover any of this is parsing English to get
    /// back something that was structured a moment earlier.
    pub rule: GateRuleWire,
    pub remedy: RemedyWire,
    /// The threshold the rule compared against. Zero when it read none.
    ///
    /// Sent because "you are over the limit" without the limit is a refusal a
    /// caller cannot act on.
    pub rule_threshold: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GateRuleWire {
    IrreversibleOverShadowThreshold,
    Destructive,
    OverRowImpactThreshold,
    AutoApprovedByPolicy,
    #[default]
    WithinThresholds,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RemedyWire {
    #[default]
    None,
    Confirm,
    ValidateOnShadowBranch,
    ReduceBlastRadius,
}

/// What a `describe` answers with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaDescriptionWire {
    pub tables: Vec<TableDescriptionWire>,
    /// True when examples were asked for and withheld.
    ///
    /// Reported rather than silently omitted, because a client that asked and
    /// got nothing must be able to tell "there were none" from "we would not
    /// give them to you" — those lead to different next steps and look
    /// identical in an empty list.
    pub examples_withheld: bool,
    pub withheld_reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableDescriptionWire {
    pub name: String,
    pub row_count: u64,
    pub columns: Vec<ColumnDescriptionWire>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnDescriptionWire {
    pub name: String,
    pub ty: String,
    pub nullable: bool,
    /// The CRDT kind, or empty. The most useful field here for an agent
    /// deciding whether two writes can race: with one, concurrent modification
    /// converges; without, it becomes a conflict a human resolves.
    pub crdt: String,
    /// Hex commit hash, or empty when outside the visible log.
    pub declared_at: String,
    pub touched_by_agent: bool,
    /// Canonical JSON values. Empty unless asked for and permitted.
    pub examples: Vec<String>,
    pub null_basis_points: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergeResultWire {
    pub status: MergeStatus,
    pub conflicts: Vec<ConflictRefWire>,
    pub converged: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MergeStatus {
    #[default]
    Ok,
    Conflict,
    Blocked,
    UpToDate,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConflictRefWire {
    pub key: String,
    pub ours_json: String,
    pub theirs_json: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectStatusWire {
    pub project_id: String,
    pub branch: String,
    pub write_volume_mb: f32,
    pub circuit_breaker_tripped: bool,
    pub replica_regions: Vec<String>,
    pub breaker_window_rows: u64,
    pub protocol_version: u32,
    pub commits_applied: u64,
    /// Bytes on disk. `None` when the figure could not be read.
    ///
    /// An option rather than a bare `u64` because a billing surface has to tell
    /// "nobody measured" from "zero" — rendering an unmeasured figure as zero
    /// tells a customer they are using nothing.
    pub storage_bytes: Option<u64>,
    /// Rows written since this instance started.
    pub rows_written: u64,
    /// Milliseconds since this instance last served a request, running from
    /// instance start when it has served none.
    ///
    /// A duration rather than a timestamp: hibernation must not depend on the
    /// Control Plane and the instance agreeing about the time.
    pub idle_ms: u64,
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut message = Builder::new_default();
        {
            let mut root = message.init_root::<generated::response::Builder>();
            root.set_request_id(self.request_id);
            let body = root.init_body();

            match &self.body {
                ResponseBody::Error(err) => {
                    let mut b = body.init_error();
                    b.set_code(err.code as u8);
                    b.set_message(err.message.as_str());
                    b.set_has_diff(err.diff.is_some());
                    if let Some(diff) = &err.diff {
                        write_diff(b.init_diff(), diff);
                    }
                }
                ResponseBody::Get {
                    found,
                    value_json,
                    version_id,
                } => {
                    let mut b = body.init_get();
                    b.set_found(*found);
                    b.set_value(value_json.as_str());
                    b.set_version_id(*version_id);
                }
                ResponseBody::Put { commit_id } => {
                    body.init_put().set_commit_id(commit_id.as_str());
                }
                ResponseBody::Transaction { commit_id } => {
                    body.init_transaction().set_commit_id(commit_id.as_str());
                }
                ResponseBody::Delete { commit_id } => {
                    body.init_delete().set_commit_id(commit_id.as_str());
                }
                ResponseBody::Apply { commit_id } => {
                    body.init_apply().set_commit_id(commit_id.as_str());
                }
                ResponseBody::Query {
                    result_set,
                    plan_hash,
                    row_count,
                } => {
                    let mut b = body.init_query();
                    b.set_result_set(result_set);
                    b.set_plan_hash(*plan_hash);
                    b.set_row_count(*row_count);
                }
                ResponseBody::Explain { explanation_json } => {
                    body.init_explain()
                        .set_explanation(explanation_json.as_str());
                }
                ResponseBody::Propose(diff) => write_diff(body.init_propose(), diff),
                ResponseBody::Branch { branch_id } => {
                    body.init_branch().set_branch_id(*branch_id);
                }
                ResponseBody::Merge(result) => write_merge(body.init_merge(), result),
                ResponseBody::Status(status) => write_status(body.init_status(), status),
                ResponseBody::Revocations { version, accepted } => {
                    let mut b = body.init_revocations();
                    b.set_version(*version);
                    b.set_accepted(*accepted);
                }
                ResponseBody::Audit { entries } => {
                    let mut list = body.init_audit().init_entries(entries.len() as u32);
                    for (i, entry) in entries.iter().enumerate() {
                        let mut b = list.reborrow().get(i as u32);
                        b.set_risk(entry.risk);
                        b.set_summary(entry.summary.as_str());
                        b.set_author(entry.author_json.as_str());
                        b.set_timestamp_ms(entry.timestamp_ms);
                        b.set_detail(entry.detail_json.as_str());
                    }
                }
                ResponseBody::Branches { branches } => {
                    let mut list = body.init_branches().init_branches(branches.len() as u32);
                    for (i, branch) in branches.iter().enumerate() {
                        let mut b = list.reborrow().get(i as u32);
                        b.set_branch_id(branch.branch_id);
                        b.set_name(branch.name.as_str());
                        b.set_kind(branch.kind);
                        b.set_head(branch.head.as_str());
                        b.set_protected(branch.protected);
                        b.set_next_commit(branch.next_commit);
                    }
                }
                ResponseBody::ReviewQueue { batches } => {
                    let mut list = body.init_review_queue().init_batches(batches.len() as u32);
                    for (i, batch) in batches.iter().enumerate() {
                        let mut b = list.reborrow().get(i as u32);
                        b.set_key(batch.key.as_str());
                        b.set_gate(batch.gate);
                        b.set_rows_affected(batch.rows_affected);
                        b.set_cost(batch.cost);
                        b.set_cost_if_unbatched(batch.cost_if_unbatched);
                        b.set_reason(batch.reason.as_str());
                        let mut changes = b.init_changes(batch.changes.len() as u32);
                        for (j, change) in batch.changes.iter().enumerate() {
                            write_diff(changes.reborrow().get(j as u32), change);
                        }
                    }
                }
                ResponseBody::Description(description) => {
                    let mut b = body.init_description();
                    b.set_examples_withheld(description.examples_withheld);
                    b.set_withheld_reason(description.withheld_reason.as_str());
                    let mut tables = b.init_tables(description.tables.len() as u32);
                    for (i, table) in description.tables.iter().enumerate() {
                        let mut t = tables.reborrow().get(i as u32);
                        t.set_name(table.name.as_str());
                        t.set_row_count(table.row_count);
                        let mut columns = t.init_columns(table.columns.len() as u32);
                        for (j, column) in table.columns.iter().enumerate() {
                            let mut c = columns.reborrow().get(j as u32);
                            c.set_name(column.name.as_str());
                            c.set_type(column.ty.as_str());
                            c.set_nullable(column.nullable);
                            c.set_crdt(column.crdt.as_str());
                            c.set_declared_at(column.declared_at.as_str());
                            c.set_touched_by_agent(column.touched_by_agent);
                            c.set_null_basis_points(column.null_basis_points);
                            let mut examples = c.init_examples(column.examples.len() as u32);
                            for (k, example) in column.examples.iter().enumerate() {
                                examples.set(k as u32, example.as_str());
                            }
                        }
                    }
                }
                ResponseBody::Change(state) => {
                    let mut b = body.init_change();
                    write_diff(b.reborrow().init_diff(), &state.diff);
                    b.set_has_shadow(state.shadow_branch_id.is_some());
                    b.set_shadow_branch_id(state.shadow_branch_id.unwrap_or(0));
                    b.set_has_validation(state.validation_passed.is_some());
                    b.set_validation_passed(state.validation_passed.unwrap_or(false));
                    b.set_validation_summary(state.validation_summary.as_str());
                    let mut list = b.init_checks(state.checks.len() as u32);
                    for (i, check) in state.checks.iter().enumerate() {
                        let mut c = list.reborrow().get(i as u32);
                        c.set_name(check.name.as_str());
                        c.set_passed(check.passed);
                        c.set_detail(check.detail.as_str());
                        c.set_samples(check.samples_json.as_str());
                    }
                }
                ResponseBody::Ok => {
                    body.init_ok();
                }
                ResponseBody::Policy { version, accepted } => {
                    let mut b = body.init_policy();
                    b.set_version(*version);
                    b.set_accepted(*accepted);
                }
                ResponseBody::PreconditionFailed { key, found, actual } => {
                    let mut b = body.init_precondition();
                    b.set_key(key.as_str());
                    b.set_found(*found);
                    b.set_actual(*actual);
                }
            }
        }
        serialize::write_message_to_words(&message)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        use generated::response::body::Which;

        let message = read_root(bytes)?;
        let root: generated::response::Reader = message.get_root().map_err(malformed)?;

        let body = match root.get_body().which().map_err(malformed)? {
            Which::Error(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::Error(WireError {
                    code: StatusCode::from_u8(r.get_code()),
                    message: text(r.get_message())?,
                    diff: match r.get_has_diff() {
                        true => Some(read_diff(r.get_diff().map_err(malformed)?)?),
                        false => None,
                    },
                })
            }
            Which::Description(r) => {
                let r = r.map_err(malformed)?;
                let mut tables = Vec::new();
                for table in r.get_tables().map_err(malformed)?.iter() {
                    let mut columns = Vec::new();
                    for column in table.get_columns().map_err(malformed)?.iter() {
                        let mut examples = Vec::new();
                        for example in column.get_examples().map_err(malformed)?.iter() {
                            examples.push(text(example)?);
                        }
                        columns.push(ColumnDescriptionWire {
                            name: text(column.get_name())?,
                            ty: text(column.get_type())?,
                            nullable: column.get_nullable(),
                            crdt: text(column.get_crdt())?,
                            declared_at: text(column.get_declared_at())?,
                            touched_by_agent: column.get_touched_by_agent(),
                            examples,
                            null_basis_points: column.get_null_basis_points(),
                        });
                    }
                    tables.push(TableDescriptionWire {
                        name: text(table.get_name())?,
                        row_count: table.get_row_count(),
                        columns,
                    });
                }
                ResponseBody::Description(SchemaDescriptionWire {
                    tables,
                    examples_withheld: r.get_examples_withheld(),
                    withheld_reason: text(r.get_withheld_reason())?,
                })
            }
            Which::Get(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::Get {
                    found: r.get_found(),
                    value_json: text(r.get_value())?,
                    version_id: r.get_version_id(),
                }
            }
            Which::Transaction(r) => ResponseBody::Transaction {
                commit_id: text(r.map_err(malformed)?.get_commit_id())?,
            },
            Which::Put(r) => ResponseBody::Put {
                commit_id: text(r.map_err(malformed)?.get_commit_id())?,
            },
            Which::Delete(r) => ResponseBody::Delete {
                commit_id: text(r.map_err(malformed)?.get_commit_id())?,
            },
            Which::Apply(r) => ResponseBody::Apply {
                commit_id: text(r.map_err(malformed)?.get_commit_id())?,
            },
            Which::Query(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::Query {
                    result_set: r.get_result_set().map_err(malformed)?.to_vec(),
                    plan_hash: r.get_plan_hash(),
                    row_count: r.get_row_count(),
                }
            }
            Which::Explain(r) => ResponseBody::Explain {
                explanation_json: text(r.map_err(malformed)?.get_explanation())?,
            },
            Which::Propose(r) => ResponseBody::Propose(read_diff(r.map_err(malformed)?)?),
            Which::Branch(r) => ResponseBody::Branch {
                branch_id: r.map_err(malformed)?.get_branch_id(),
            },
            Which::Merge(r) => ResponseBody::Merge(read_merge(r.map_err(malformed)?)?),
            Which::Status(r) => ResponseBody::Status(read_status(r.map_err(malformed)?)?),
            Which::Audit(r) => {
                let entries = r.map_err(malformed)?.get_entries().map_err(malformed)?;
                ResponseBody::Audit {
                    entries: entries
                        .iter()
                        .map(|e| {
                            Ok(AuditEntryWire {
                                risk: e.get_risk(),
                                summary: text(e.get_summary())?,
                                author_json: text(e.get_author())?,
                                timestamp_ms: e.get_timestamp_ms(),
                                detail_json: text(e.get_detail())?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                }
            }
            Which::ReviewQueue(r) => {
                let batches = r.map_err(malformed)?.get_batches().map_err(malformed)?;
                ResponseBody::ReviewQueue {
                    batches: batches
                        .iter()
                        .map(|b| {
                            Ok(ReviewBatchWire {
                                key: text(b.get_key())?,
                                gate: b.get_gate(),
                                rows_affected: b.get_rows_affected(),
                                cost: b.get_cost(),
                                cost_if_unbatched: b.get_cost_if_unbatched(),
                                reason: text(b.get_reason())?,
                                changes: b
                                    .get_changes()
                                    .map_err(malformed)?
                                    .iter()
                                    .map(read_diff)
                                    .collect::<Result<Vec<_>>>()?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                }
            }
            Which::Branches(r) => {
                let branches = r.map_err(malformed)?.get_branches().map_err(malformed)?;
                ResponseBody::Branches {
                    branches: branches
                        .iter()
                        .map(|b| {
                            Ok(BranchInfoWire {
                                branch_id: b.get_branch_id(),
                                name: text(b.get_name())?,
                                kind: b.get_kind(),
                                head: text(b.get_head())?,
                                protected: b.get_protected(),
                                next_commit: b.get_next_commit(),
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                }
            }
            Which::Change(r) => {
                let r = r.map_err(malformed)?;
                let checks = r.get_checks().map_err(malformed)?;
                ResponseBody::Change(ChangeStateWire {
                    diff: read_diff(r.get_diff().map_err(malformed)?)?,
                    shadow_branch_id: match r.get_has_shadow() {
                        true => Some(r.get_shadow_branch_id()),
                        false => None,
                    },
                    // The `has` flag is what separates "not yet run" from
                    // "failed"; reading the bool without it would turn an
                    // unvalidated change into a passed one on the default.
                    validation_passed: match r.get_has_validation() {
                        true => Some(r.get_validation_passed()),
                        false => None,
                    },
                    validation_summary: text(r.get_validation_summary())?,
                    checks: checks
                        .iter()
                        .map(|c| {
                            Ok(ValidationCheckWire {
                                name: text(c.get_name())?,
                                passed: c.get_passed(),
                                detail: text(c.get_detail())?,
                                samples_json: text(c.get_samples())?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                })
            }
            Which::Ok(_) => ResponseBody::Ok,
            Which::Policy(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::Policy {
                    version: r.get_version(),
                    accepted: r.get_accepted(),
                }
            }
            Which::Precondition(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::PreconditionFailed {
                    key: text(r.get_key())?,
                    found: r.get_found(),
                    actual: r.get_actual(),
                }
            }
            Which::Revocations(r) => {
                let r = r.map_err(malformed)?;
                ResponseBody::Revocations {
                    version: r.get_version(),
                    accepted: r.get_accepted(),
                }
            }
        };

        Ok(Self {
            request_id: root.get_request_id(),
            body,
        })
    }
}

fn write_diff(mut b: generated::change_diff::Builder, diff: &ChangeDiffWire) {
    b.set_change_id(diff.change_id.as_str());
    b.set_destructive(diff.destructive);
    b.set_rows_affected(diff.rows_affected);
    b.set_reversible(diff.reversible);
    b.set_estimated_cost_ms(diff.estimated_cost_ms);
    b.set_requires_confirm(diff.requires_confirm);
    b.set_gate(match diff.gate {
        GateWire::AutoApply => generated::Gate::AutoApply,
        GateWire::Confirm => generated::Gate::Confirm,
        GateWire::ShadowValidate => generated::Gate::ShadowValidate,
    });
    b.set_shadow_branch_id(diff.shadow_branch_id);
    b.set_reason(diff.reason.as_str());
    b.set_affected_table(diff.affected_table.as_str());
    b.set_affected_column(diff.affected_column.as_str());
    b.set_change_type(diff.change_type.as_str());
    b.set_rule(match diff.rule {
        GateRuleWire::IrreversibleOverShadowThreshold => {
            generated::GateRule::IrreversibleOverShadowThreshold
        }
        GateRuleWire::Destructive => generated::GateRule::Destructive,
        GateRuleWire::OverRowImpactThreshold => generated::GateRule::OverRowImpactThreshold,
        GateRuleWire::AutoApprovedByPolicy => generated::GateRule::AutoApprovedByPolicy,
        GateRuleWire::WithinThresholds => generated::GateRule::WithinThresholds,
    });
    b.set_remedy(match diff.remedy {
        RemedyWire::None => generated::Remedy::None,
        RemedyWire::Confirm => generated::Remedy::Confirm,
        RemedyWire::ValidateOnShadowBranch => generated::Remedy::ValidateOnShadowBranch,
        RemedyWire::ReduceBlastRadius => generated::Remedy::ReduceBlastRadius,
    });
    b.set_rule_threshold(diff.rule_threshold);
}

/// Decode a rule, failing closed like [`gate_from_wire`].
///
/// A rule this build does not recognise comes from a newer peer, and it reads as
/// the strictest one. The alternative — reading it as `withinThresholds` —
/// would tell a client that a change a newer server gated was inside every
/// threshold, which is a confident wrong answer about the one decision this
/// product exists to get right.
fn rule_from_wire(
    read: std::result::Result<generated::GateRule, capnp::NotInSchema>,
) -> GateRuleWire {
    match read {
        Ok(generated::GateRule::IrreversibleOverShadowThreshold) => {
            GateRuleWire::IrreversibleOverShadowThreshold
        }
        Ok(generated::GateRule::Destructive) => GateRuleWire::Destructive,
        Ok(generated::GateRule::OverRowImpactThreshold) => GateRuleWire::OverRowImpactThreshold,
        Ok(generated::GateRule::AutoApprovedByPolicy) => GateRuleWire::AutoApprovedByPolicy,
        Ok(generated::GateRule::WithinThresholds) => GateRuleWire::WithinThresholds,
        Err(capnp::NotInSchema(_)) => GateRuleWire::IrreversibleOverShadowThreshold,
    }
}

/// Decode a remedy, failing closed.
///
/// An unrecognised remedy reads as `validateOnShadowBranch`, the one that cannot
/// be short-circuited. Reading it as `none` would be the dangerous direction:
/// `none` means the change *applied*, so a client would stop waiting for a
/// decision that is still pending.
fn remedy_from_wire(
    read: std::result::Result<generated::Remedy, capnp::NotInSchema>,
) -> RemedyWire {
    match read {
        Ok(generated::Remedy::None) => RemedyWire::None,
        Ok(generated::Remedy::Confirm) => RemedyWire::Confirm,
        Ok(generated::Remedy::ValidateOnShadowBranch) => RemedyWire::ValidateOnShadowBranch,
        Ok(generated::Remedy::ReduceBlastRadius) => RemedyWire::ReduceBlastRadius,
        Err(capnp::NotInSchema(_)) => RemedyWire::ValidateOnShadowBranch,
    }
}

/// Decode a gate, failing closed.
///
/// A gate this build does not recognise comes from a newer peer, and it reads
/// as the strongest one. Guessing `autoApply` would let a future destructive
/// classification through as safe; guessing `shadowValidate` costs an
/// unnecessary review. Only one of those is recoverable.
fn gate_from_wire(read: std::result::Result<generated::Gate, capnp::NotInSchema>) -> GateWire {
    match read {
        Ok(generated::Gate::AutoApply) => GateWire::AutoApply,
        Ok(generated::Gate::Confirm) => GateWire::Confirm,
        Ok(generated::Gate::ShadowValidate) => GateWire::ShadowValidate,
        Err(capnp::NotInSchema(_)) => GateWire::ShadowValidate,
    }
}

fn read_diff(r: generated::change_diff::Reader) -> Result<ChangeDiffWire> {
    Ok(ChangeDiffWire {
        change_id: text(r.get_change_id())?,
        destructive: r.get_destructive(),
        rows_affected: r.get_rows_affected(),
        reversible: r.get_reversible(),
        estimated_cost_ms: r.get_estimated_cost_ms(),
        requires_confirm: r.get_requires_confirm(),
        gate: gate_from_wire(r.get_gate()),
        shadow_branch_id: r.get_shadow_branch_id(),
        reason: text(r.get_reason())?,
        affected_table: text(r.get_affected_table())?,
        affected_column: text(r.get_affected_column())?,
        change_type: text(r.get_change_type())?,
        rule: rule_from_wire(r.get_rule()),
        remedy: remedy_from_wire(r.get_remedy()),
        rule_threshold: r.get_rule_threshold(),
    })
}

fn write_merge(mut b: generated::merge_result::Builder, result: &MergeResultWire) {
    use generated::merge_result::Status;
    b.set_status(match result.status {
        MergeStatus::Ok => Status::Ok,
        MergeStatus::Conflict => Status::Conflict,
        MergeStatus::Blocked => Status::Blocked,
        MergeStatus::UpToDate => Status::UpToDate,
    });
    b.set_conflict_count(result.conflicts.len() as u32);

    {
        let mut list = b.reborrow().init_conflicts(result.conflicts.len() as u32);
        for (i, c) in result.conflicts.iter().enumerate() {
            let mut item = list.reborrow().get(i as u32);
            item.set_key(c.key.as_str());
            item.set_ours(c.ours_json.as_str());
            item.set_theirs(c.theirs_json.as_str());
            item.set_reason(c.reason.as_str());
        }
    }

    let mut converged = b.init_converged(result.converged.len() as u32);
    for (i, key) in result.converged.iter().enumerate() {
        converged.set(i as u32, key.as_str());
    }
}

fn read_merge(r: generated::merge_result::Reader) -> Result<MergeResultWire> {
    use generated::merge_result::Status;

    let status = match r.get_status().map_err(malformed)? {
        Status::Ok => MergeStatus::Ok,
        Status::Conflict => MergeStatus::Conflict,
        Status::Blocked => MergeStatus::Blocked,
        Status::UpToDate => MergeStatus::UpToDate,
    };

    let mut conflicts = Vec::new();
    for c in r.get_conflicts().map_err(malformed)?.iter() {
        conflicts.push(ConflictRefWire {
            key: text(c.get_key())?,
            ours_json: text(c.get_ours())?,
            theirs_json: text(c.get_theirs())?,
            reason: text(c.get_reason())?,
        });
    }

    let mut converged = Vec::new();
    for key in r.get_converged().map_err(malformed)?.iter() {
        converged.push(text(key)?);
    }

    Ok(MergeResultWire {
        status,
        conflicts,
        converged,
    })
}

fn write_status(mut b: generated::project_status::Builder, status: &ProjectStatusWire) {
    b.set_project_id(status.project_id.as_str());
    b.set_branch(status.branch.as_str());
    b.set_write_volume_m_b(status.write_volume_mb);
    b.set_circuit_breaker_tripped(status.circuit_breaker_tripped);
    b.set_breaker_window_rows(status.breaker_window_rows);
    b.set_protocol_version(status.protocol_version);
    b.set_commits_applied(status.commits_applied);
    b.set_has_storage_bytes(status.storage_bytes.is_some());
    b.set_storage_bytes(status.storage_bytes.unwrap_or(0));
    b.set_rows_written(status.rows_written);
    b.set_idle_ms(status.idle_ms);

    let mut regions = b.init_replica_regions(status.replica_regions.len() as u32);
    for (i, region) in status.replica_regions.iter().enumerate() {
        regions.set(i as u32, region.as_str());
    }
}

fn read_status(r: generated::project_status::Reader) -> Result<ProjectStatusWire> {
    let mut replica_regions = Vec::new();
    for region in r.get_replica_regions().map_err(malformed)?.iter() {
        replica_regions.push(text(region)?);
    }
    Ok(ProjectStatusWire {
        project_id: text(r.get_project_id())?,
        branch: text(r.get_branch())?,
        write_volume_mb: r.get_write_volume_m_b(),
        circuit_breaker_tripped: r.get_circuit_breaker_tripped(),
        replica_regions,
        breaker_window_rows: r.get_breaker_window_rows(),
        protocol_version: r.get_protocol_version(),
        commits_applied: r.get_commits_applied(),
        storage_bytes: match r.get_has_storage_bytes() {
            true => Some(r.get_storage_bytes()),
            false => None,
        },
        rows_written: r.get_rows_written(),
        idle_ms: r.get_idle_ms(),
    })
}

/// Write a CRDT mutation into a builder.
///
/// Shared by the plain and the signed request, so the two cannot encode the
/// same mutation differently - which would make a signature computed over one
/// fail to verify against the other for no reason a caller could see.
fn encode_mutation(mut m: generated::crdt_request::mutation::Builder<'_>, mutation: &CrdtMutation) {
    match mutation {
        CrdtMutation::Increment { by } => m.set_increment(*by),
        CrdtMutation::SetRegister { value_json } => m.set_set_register(value_json.as_str()),
        CrdtMutation::SetAdd { element_json } => m.set_set_add(element_json.as_str()),
        CrdtMutation::SetRemove { element_json } => m.set_set_remove(element_json.as_str()),
        CrdtMutation::SeqInsert { after, value_json } => {
            let mut ins = m.init_seq_insert();
            ins.set_value(value_json.as_str());
            if let Some(after) = after {
                let mut a = ins.init_after();
                a.set_counter(after.counter);
                a.set_replica(after.replica);
            }
        }
        CrdtMutation::SeqRemove { id } => {
            let mut e = m.init_seq_remove();
            e.set_counter(id.counter);
            e.set_replica(id.replica);
        }
    }
}

/// Read a CRDT mutation off a request.
///
/// Every arm is spelled out. A `_ =>` catch-all here would accept a mutation
/// this build does not understand and silently turn it into something it does,
/// which is a converging field quietly diverging.
fn decode_mutation(r: generated::crdt_request::Reader<'_>) -> Result<CrdtMutation> {
    use generated::crdt_request::mutation::Which as M;

    fn elem(r: generated::elem_id::Reader<'_>) -> ElemId {
        ElemId {
            counter: r.get_counter(),
            replica: r.get_replica(),
        }
    }

    Ok(match r.get_mutation().which().map_err(malformed)? {
        M::Increment(by) => CrdtMutation::Increment { by },
        M::SetRegister(v) => CrdtMutation::SetRegister {
            value_json: text(v)?,
        },
        M::SetAdd(v) => CrdtMutation::SetAdd {
            element_json: text(v)?,
        },
        M::SetRemove(v) => CrdtMutation::SetRemove {
            element_json: text(v)?,
        },
        M::SeqInsert(ins) => {
            let ins = ins.map_err(malformed)?;
            CrdtMutation::SeqInsert {
                // Absent means the head of the list, which is a real position
                // and not a missing field.
                after: match ins.has_after() {
                    true => Some(elem(ins.get_after().map_err(malformed)?)),
                    false => None,
                },
                value_json: text(ins.get_value())?,
            }
        }
        M::SeqRemove(id) => CrdtMutation::SeqRemove {
            id: elem(id.map_err(malformed)?),
        },
    })
}

fn text(result: std::result::Result<capnp::text::Reader<'_>, capnp::Error>) -> Result<String> {
    Ok(result
        .map_err(malformed)?
        .to_str()
        .map_err(malformed)?
        .to_string())
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn every_gate_survives_the_wire_unchanged() {
        for gate in [
            GateWire::AutoApply,
            GateWire::Confirm,
            GateWire::ShadowValidate,
        ] {
            let encoded = match gate {
                GateWire::AutoApply => generated::Gate::AutoApply,
                GateWire::Confirm => generated::Gate::Confirm,
                GateWire::ShadowValidate => generated::Gate::ShadowValidate,
            };
            assert_eq!(gate_from_wire(Ok(encoded)), gate);
        }
    }

    #[test]
    fn a_gate_this_build_does_not_know_reads_as_the_strongest_one() {
        // A newer peer classifying something in a way this build has no name
        // for must not be read as "apply it". Failing closed costs a review
        // that was not needed; failing open applies a destructive change.
        assert_eq!(
            gate_from_wire(Err(capnp::NotInSchema(99))),
            GateWire::ShadowValidate
        );
    }

    #[test]
    fn a_rule_this_build_does_not_know_reads_as_the_strictest_one() {
        // The alternative — reading it as `withinThresholds` — would tell a
        // client that a change a newer server gated was inside every threshold,
        // which is a confident wrong answer about the one decision this product
        // exists to get right.
        assert_eq!(
            rule_from_wire(Err(capnp::NotInSchema(99))),
            GateRuleWire::IrreversibleOverShadowThreshold
        );
    }

    #[test]
    fn a_remedy_this_build_does_not_know_never_reads_as_nothing_to_do() {
        // The dangerous direction here is specific: `none` means the change
        // *applied*. A client reading an unknown remedy as `none` stops waiting
        // for a decision that is still pending, and reports success for a change
        // that has not landed.
        let unknown = remedy_from_wire(Err(capnp::NotInSchema(99)));
        assert_ne!(unknown, RemedyWire::None);
        assert_eq!(unknown, RemedyWire::ValidateOnShadowBranch);
    }

    #[test]
    fn every_known_rule_and_remedy_survives_the_round_trip() {
        // Fail-closed defaults are only safe if the *known* values do not
        // silently take them. A mapping that returned the strictest value for
        // everything would pass both tests above and be useless.
        for rule in [
            GateRuleWire::IrreversibleOverShadowThreshold,
            GateRuleWire::Destructive,
            GateRuleWire::OverRowImpactThreshold,
            GateRuleWire::AutoApprovedByPolicy,
            GateRuleWire::WithinThresholds,
        ] {
            let mut diff = ChangeDiffWire {
                rule,
                ..Default::default()
            };
            diff.change_id = "chg_1".into();
            let response = Response {
                request_id: 1,
                body: ResponseBody::Propose(diff.clone()),
            };
            let decoded = Response::decode(&response.encode()).expect("decode");
            assert_eq!(decoded.body, ResponseBody::Propose(diff), "{rule:?}");
        }

        for remedy in [
            RemedyWire::None,
            RemedyWire::Confirm,
            RemedyWire::ValidateOnShadowBranch,
            RemedyWire::ReduceBlastRadius,
        ] {
            let diff = ChangeDiffWire {
                remedy,
                change_id: "chg_1".into(),
                ..Default::default()
            };
            let response = Response {
                request_id: 1,
                body: ResponseBody::Propose(diff.clone()),
            };
            let decoded = Response::decode(&response.encode()).expect("decode");
            assert_eq!(decoded.body, ResponseBody::Propose(diff), "{remedy:?}");
        }
    }
}
