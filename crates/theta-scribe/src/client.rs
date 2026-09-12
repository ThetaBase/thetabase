//! The Scribe client surface.

use std::sync::Arc;

use theta_core::Value;
use theta_proto::wire::{ChangeDiffWire, TxOp};
use theta_proto::{RequestBody, Response, ResponseBody, StatusCode};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::cache::{CacheStats, ReadCache};
use crate::pool::ConnectionPool;
use crate::token::TokenSource;

#[derive(Debug, Error)]
pub enum ScribeError {
    #[error("connection refused: {message}")]
    Refused { code: StatusCode, message: String },

    #[error("handshake failed: {0}")]
    Handshake(String),

    #[error("connection closed mid-request")]
    ConnectionClosed,

    #[error("connection desynchronized: expected response {expected}, got {actual}")]
    Desynchronized { expected: u64, actual: u64 },

    /// The server refused the operation. Distinct from a transport failure:
    /// retrying will not help until the caller changes something.
    #[error("{message}")]
    Rejected {
        code: StatusCode,
        message: String,
        /// Present when the Safety Layer gated a change, so the caller can act
        /// on the diff without asking again.
        diff: Option<Box<ChangeDiffWire>>,
    },

    #[error("unexpected response: {0}")]
    Unexpected(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("wire: {0}")]
    Wire(#[from] theta_proto::FrameError),

    #[error("value encoding: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// A query's results.
///
/// The Arrow bytes are kept as they arrived so a caller that wants zero-copy
/// columnar access can have it; `rows()` decodes into plain values for callers
/// that would rather not think about Arrow.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub arrow_ipc: Vec<u8>,
    pub plan_hash: u64,
    pub row_count: u64,
}

#[derive(Debug, Clone)]
pub struct ScribeConfig {
    pub addr: String,
    pub client_name: String,
    pub pool_size: usize,
    pub cache_capacity: usize,
    /// How long a cached read may be served. Zero disables the cache — which is
    /// the right choice for anything that cannot tolerate another writer's
    /// update arriving late.
    pub cache_ttl_ms: i64,
    /// Branch every request targets unless overridden. Zero is `main`.
    pub branch_id: u64,
}

impl ScribeConfig {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            client_name: concat!("scribe/", env!("CARGO_PKG_VERSION")).to_string(),
            pool_size: 4,
            cache_capacity: 1_024,
            cache_ttl_ms: 1_000,
            branch_id: 0,
        }
    }
}

/// The edge runtime. Cheap to clone; clones share one pool and one cache.
#[derive(Debug, Clone)]
pub struct Scribe {
    pool: Arc<ConnectionPool>,
    cache: Arc<Mutex<ReadCache>>,
    branch_id: u64,
}

impl Scribe {
    pub fn connect(config: ScribeConfig, token: Arc<dyn TokenSource>) -> Self {
        Self {
            pool: Arc::new(ConnectionPool::new(
                config.addr,
                config.client_name,
                config.pool_size,
                token,
            )),
            cache: Arc::new(Mutex::new(ReadCache::new(
                config.cache_capacity,
                config.cache_ttl_ms,
            ))),
            branch_id: config.branch_id,
        }
    }

    /// A Scribe pointed at another branch, sharing this one's pool.
    ///
    /// The cache is *not* shared: two branches can hold different values for the
    /// same key, so one cache serving both would return whichever was read last.
    pub fn on_branch(&self, branch_id: u64) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            cache: Arc::new(Mutex::new(ReadCache::new(1_024, 1_000))),
            branch_id,
        }
    }

    pub async fn cache_stats(&self) -> CacheStats {
        self.cache.lock().await.stats()
    }

    pub async fn idle_connections(&self) -> usize {
        self.pool.idle_count().await
    }

    // ---- hot path ----------------------------------------------------------

    /// Point lookup, served from the local cache when a fresh entry exists.
    pub async fn get(&self, key: &str) -> Result<Option<Value>, ScribeError> {
        if let Some(hit) = self.cache.lock().await.get(key, now_ms()) {
            return Ok(hit);
        }

        let response = self
            .call(RequestBody::Get {
                key: key.to_string(),
            })
            .await?;
        let value = match expect_body(response)? {
            ResponseBody::Get { found: false, .. } => None,
            ResponseBody::Get { value_json, .. } => Some(serde_json::from_str(&value_json)?),
            other => return Err(ScribeError::Unexpected(format!("{other:?}"))),
        };

        self.cache.lock().await.put(key, value.clone(), now_ms());
        Ok(value)
    }

    pub async fn put(&self, key: &str, value: &Value) -> Result<String, ScribeError> {
        // Invalidate before the write, not after. If the write succeeds and the
        // process dies before an after-the-fact invalidation, the cache would
        // still be serving the old value — read-your-writes broken by a crash
        // window that never had to exist.
        self.cache.lock().await.invalidate(key);

        let response = self
            .call(RequestBody::Put {
                key: key.to_string(),
                value_json: serde_json::to_string(value)?,
                ttl: 0,
            })
            .await?;

        match expect_body(response)? {
            ResponseBody::Put { commit_id } => Ok(commit_id),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<String, ScribeError> {
        self.cache.lock().await.invalidate(key);
        let response = self
            .call(RequestBody::Delete {
                key: key.to_string(),
            })
            .await?;
        match expect_body(response)? {
            ResponseBody::Delete { commit_id } => Ok(commit_id),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Write several keys in one round trip.
    ///
    /// Not a transaction: each write is its own commit and they are not
    /// all-or-nothing. This batches the *network*, not the durability boundary,
    /// and saying so plainly matters because the two look identical from the
    /// call site.
    pub async fn put_many(&self, writes: &[(String, Value)]) -> Result<Vec<String>, ScribeError> {
        if writes.is_empty() {
            return Ok(Vec::new());
        }

        {
            let mut cache = self.cache.lock().await;
            for (key, _) in writes {
                cache.invalidate(key);
            }
        }

        let mut bodies = Vec::with_capacity(writes.len());
        for (key, value) in writes {
            bodies.push(RequestBody::Put {
                key: key.clone(),
                value_json: serde_json::to_string(value)?,
                ttl: 0,
            });
        }

        let mut conn = self.pool.acquire().await?;
        let responses = match conn.pipeline(self.branch_id, bodies).await {
            Ok(responses) => responses,
            Err(e) => return Err(e), // connection dropped, not returned to the pool
        };
        self.pool.release(conn).await;

        let mut commits = Vec::with_capacity(responses.len());
        for response in responses {
            match expect_body(response)? {
                ResponseBody::Put { commit_id } => commits.push(commit_id),
                other => return Err(ScribeError::Unexpected(format!("{other:?}"))),
            }
        }
        Ok(commits)
    }

    /// Run a query and get the rows back.
    ///
    /// Results arrive as Arrow IPC and are decoded here rather than at the call
    /// site, so an SDK binding does not have to carry an Arrow dependency to
    /// read a query.
    /// Several writes as one commit, or none of them.
    ///
    /// Unlike [`put_many`], this *is* a transaction: it batches the durability
    /// boundary and not merely the network. Each operation may carry a
    /// precondition, and every one is checked against the branch before any
    /// write is applied — so a refused transaction changes nothing.
    ///
    /// Returns `Ok(None)` when a precondition was not met. That is not an
    /// error: the request was well-formed and the server did what it was asked,
    /// and a contended transaction is a retry rather than a fault.
    ///
    /// [`put_many`]: Self::put_many
    pub async fn transaction(&self, ops: Vec<TxOp>) -> Result<Option<String>, ScribeError> {
        {
            // Invalidated before the call, for the reason `put` gives: an
            // invalidation that happens after a successful write leaves a crash
            // window in which the cache serves a value the database no longer
            // holds. Every key, including those on operations that may turn out
            // to be refused — invalidating too much costs a read, and
            // invalidating too little costs correctness.
            let mut cache = self.cache.lock().await;
            for op in &ops {
                cache.invalidate(&op.key);
            }
        }

        let response = self.call(RequestBody::Transaction { ops }).await?;
        match expect_body(response)? {
            ResponseBody::Transaction { commit_id } => Ok(Some(commit_id)),
            ResponseBody::PreconditionFailed { .. } => Ok(None),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    pub async fn query(&self, sql: &str) -> Result<QueryResult, ScribeError> {
        self.query_with(sql, &[]).await
    }

    /// Run a query with bound parameters.
    ///
    /// Parameters are sent alongside the query, never spliced into it. There is
    /// no method on this type that takes an already-interpolated string, which
    /// is deliberate: an SDK that offers one invites exactly the injection the
    /// typed path exists to prevent.
    pub async fn query_with(
        &self,
        sql: &str,
        params: &[(&str, Value)],
    ) -> Result<QueryResult, ScribeError> {
        let mut context_vars = Vec::with_capacity(params.len());
        for (name, value) in params {
            context_vars.push(((*name).to_string(), serde_json::to_string(value)?));
        }

        let response = self
            .call(RequestBody::Query(theta_proto::wire::QueryPlanWire {
                plan_hash: 0,
                raw_query: sql.to_string(),
                context_vars,
            }))
            .await?;

        match expect_body(response)? {
            ResponseBody::Query {
                result_set,
                plan_hash,
                row_count,
            } => Ok(QueryResult {
                arrow_ipc: result_set,
                plan_hash,
                row_count,
            }),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    /// EXPLAIN a query without running it.
    pub async fn explain(&self, sql: &str) -> Result<serde_json::Value, ScribeError> {
        self.explain_with(sql, &[]).await
    }

    /// EXPLAIN a query with its bound parameters.
    ///
    /// The parameters matter to the answer, so leaving them out is not a
    /// simplification. A plan for `WHERE email = $p0` with nothing bound to
    /// `$p0` is a plan for a different query than the one that will run — and
    /// `explain` is what a reviewer reads before approving, so an explanation of
    /// something adjacent is worse than none.
    ///
    /// `explain` keeps its no-parameter form because a query with no bindings is
    /// the common case and threading an empty slice through every call site
    /// would be noise.
    pub async fn explain_with(
        &self,
        sql: &str,
        params: &[(&str, Value)],
    ) -> Result<serde_json::Value, ScribeError> {
        let mut context_vars = Vec::with_capacity(params.len());
        for (name, value) in params {
            context_vars.push(((*name).to_string(), serde_json::to_string(value)?));
        }

        let response = self
            .call(RequestBody::Explain(theta_proto::wire::QueryPlanWire {
                plan_hash: 0,
                raw_query: sql.to_string(),
                context_vars,
            }))
            .await?;

        match expect_body(response)? {
            ResponseBody::Explain { explanation_json } => {
                Ok(serde_json::from_str(&explanation_json)?)
            }
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    // ---- schema and branches ------------------------------------------------

    /// Propose a schema change and get back its diff.
    ///
    /// Nothing about impact is sent. The server measures it against the
    /// branch's own view and returns the number in the diff, so this call
    /// cannot influence how the change is classified.
    pub async fn propose(
        &self,
        change: &theta_core::schema::SchemaChange,
    ) -> Result<ChangeDiffWire, ScribeError> {
        let response = self
            .call(RequestBody::ProposeSchemaChange {
                change_json: serde_json::to_string(change)?,
            })
            .await?;
        match expect_body(response)? {
            ResponseBody::Propose(diff) => Ok(diff),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Confirm a pending change by id.
    ///
    /// The change body is deliberately not a parameter: the instance applies
    /// what it classified under this id, so a caller cannot confirm one change
    /// and have another run (`07-agent-safety-layer.md` §4).
    pub async fn apply(&self, change_id: &str, confirm: bool) -> Result<String, ScribeError> {
        // A schema change can alter any key's meaning, and working out which is
        // not worth the risk of getting it wrong.
        self.cache.lock().await.clear();

        let response = self
            .call(RequestBody::ApplySchemaChange {
                change_id: change_id.to_string(),
                confirm,
            })
            .await?;
        match expect_body(response)? {
            ResponseBody::Apply { commit_id } => Ok(commit_id),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    pub async fn create_branch(&self, name: &str, from: u64) -> Result<u64, ScribeError> {
        let response = self
            .call(RequestBody::CreateBranch {
                name: name.to_string(),
                from,
            })
            .await?;
        match expect_body(response)? {
            ResponseBody::Branch { branch_id } => Ok(branch_id),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    pub async fn merge(
        &self,
        source_branch: u64,
        target_branch: u64,
    ) -> Result<theta_proto::wire::MergeResultWire, ScribeError> {
        // A merge can move any key on the target.
        self.cache.lock().await.clear();

        let response = self
            .call(RequestBody::Merge {
                source_branch,
                target_branch,
            })
            .await?;
        match expect_body(response)? {
            ResponseBody::Merge(result) => Ok(result),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    pub async fn status(&self) -> Result<theta_proto::wire::ProjectStatusWire, ScribeError> {
        let response = self.call(RequestBody::Status).await?;
        match expect_body(response)? {
            ResponseBody::Status(status) => Ok(status),
            other => Err(ScribeError::Unexpected(format!("{other:?}"))),
        }
    }

    async fn call(&self, body: RequestBody) -> Result<Response, ScribeError> {
        let mut conn = self.pool.acquire().await?;
        match conn.call(self.branch_id, body).await {
            Ok(response) => {
                self.pool.release(conn).await;
                Ok(response)
            }
            // A connection whose stream state is unknown is worse than a
            // reconnect, so it is dropped rather than returned to the pool.
            Err(e) => Err(e),
        }
    }
}

/// Turn a server-side error response into a `Rejected` error, leaving anything
/// else for the caller to match on.
fn expect_body(response: Response) -> Result<ResponseBody, ScribeError> {
    match response.body {
        ResponseBody::Error(e) => Err(ScribeError::Rejected {
            code: e.code,
            message: e.message,
            diff: e.diff.map(Box::new),
        }),
        other => Ok(other),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
