//! The engine actor.
//!
//! One task owns the [`Engine`]; every connection talks to it through a bounded
//! channel. That is not a convenience — it is what makes the concurrency
//! correct:
//!
//! * **Writes serialize.** The log's compare-and-swap append needs a total order
//!   per branch. A lock would give that too, but a lock held across an fsync
//!   turns every reader into a waiter.
//! * **Read-your-writes holds per connection.** A connection's requests reach
//!   the actor in the order it sent them, so a read issued after a write always
//!   observes it (`03-data-model-consistency.md` §3.1).
//! * **Backpressure is expressible.** A bounded queue has a length, so the
//!   server can say "busy" instead of accumulating unbounded work.

use theta_proto::wire::WireError;
use theta_proto::{Request, Response, ResponseBody, StatusCode};
use tokio::sync::{mpsc, oneshot};

use crate::dispatch::dispatch;
use crate::engine::Engine;
use theta_identity::TokenScope;

/// One unit of work for the engine task.
struct Job {
    request: Request,
    token: TokenScope,
    now_ms: i64,
    reply: oneshot::Sender<Response>,
}

/// A cloneable handle to the engine. Cloning is cheap; every clone talks to the
/// same task.
#[derive(Debug, Clone)]
pub struct EngineHandle {
    jobs: mpsc::Sender<Job>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("request_id", &self.request.request_id)
            .finish()
    }
}

/// How often the engine sweeps expired shadow branches.
///
/// Far shorter than any sensible shadow TTL, so a branch is reclaimed close to
/// its deadline rather than up to a sweep late, and cheap enough at this cadence
/// to be unnoticeable: it walks the open proposals and nothing else.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

impl EngineHandle {
    /// Spawn the engine task. `queue_depth` bounds how much work may be waiting
    /// before callers are told the instance is busy.
    pub fn spawn(engine: Engine, queue_depth: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<Job>(queue_depth.max(1));

        tokio::spawn(async move {
            let mut engine = engine;
            let mut maintenance = tokio::time::interval(MAINTENANCE_INTERVAL);
            // The first tick fires immediately; skip it so startup does not
            // begin with a sweep that has nothing to sweep.
            maintenance.tick().await;

            loop {
                tokio::select! {
                    job = rx.recv() => {
                        let Some(job) = job else { break };
                        let response = dispatch(&mut engine, job.request, &job.token, job.now_ms);
                        // A dropped receiver means the client vanished mid-call.
                        // The write already happened and is already durable;
                        // there is simply nobody to tell, which is not an error.
                        let _ = job.reply.send(response);
                    }
                    // Runs on the task that owns the engine, so it never
                    // contends with a request. Nothing here can land a change:
                    // collection only ever discards proposals that did not.
                    //
                    // **One caveat, stated rather than discovered.** Anchoring
                    // talks to an external sink, and it does so on this task —
                    // so a slow or hanging sink stalls request serving for as
                    // long as it takes. That was not true of the sweep, which
                    // only ever walked open proposals in memory. Publishing off
                    // this task needs the engine back to record the receipt, so
                    // it is a design change rather than a `spawn`, and it is
                    // tracked as one. Until then a sink is expected to have its
                    // own timeout.
                    _ = maintenance.tick() => {
                        let now = crate::now_ms();
                        for reclaimed in engine.collect_expired(now) {
                            tracing::info!(
                                change = %reclaimed.change_id.0,
                                branch = reclaimed.branch.0,
                                age_ms = reclaimed.age_ms,
                                validated = reclaimed.was_validated,
                                "shadow branch expired unpromoted and was reclaimed",
                            );
                        }

                        // Chain verification and anchoring
                        // (`04-threat-model-security.md` §7). Findings are
                        // logged at warn: every one of them is something an
                        // operator has to act on, and an integrity finding that
                        // arrives at info level arrives during the incident.
                        let report = engine.maintain(now);
                        if let Some(e) = &report.chain {
                            tracing::error!(error = %e, "log verification found a broken chain");
                        }
                        if let Some(e) = &report.pace {
                            tracing::warn!(error = %e, "chain verification is not keeping up");
                        }
                        for (branch, receipt) in &report.anchored {
                            tracing::info!(branch = branch.0, %receipt, "branch head anchored");
                        }
                        for (branch, e) in &report.unanchored {
                            tracing::warn!(branch = branch.0, error = %e, "branch head is unanchored");
                        }
                    }
                }
            }

            // The last handle is gone, so no further writes can arrive.
            // Checkpoint on the way out so a restart replays as little as
            // possible; a failure here costs replay time, never correctness.
            if let Err(e) = engine.checkpoint() {
                tracing::warn!(error = %e, "final checkpoint failed; the log is still complete");
            }
            tracing::info!("engine task stopped");
        });

        Self { jobs: tx }
    }

    /// Submit a request and await its response.
    ///
    /// Returns a `Busy` error rather than queueing without bound when the
    /// engine is saturated. Shedding load with a clear reason beats accepting
    /// work the instance cannot finish (`09-sla-performance.md` §3).
    pub async fn call(&self, request: Request, token: TokenScope, now_ms: i64) -> Response {
        let request_id = request.request_id;
        let (reply, wait) = oneshot::channel();

        let job = Job {
            request,
            token,
            now_ms,
            reply,
        };
        if let Err(e) = self.jobs.try_send(job) {
            let (code, message) = match e {
                mpsc::error::TrySendError::Full(_) => (
                    StatusCode::Busy,
                    "engine queue is full; retry shortly".to_string(),
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    (StatusCode::Internal, "engine is shutting down".to_string())
                }
            };
            return error_response(request_id, code, message);
        }

        match wait.await {
            Ok(response) => response,
            Err(_) => error_response(
                request_id,
                StatusCode::Internal,
                "engine dropped the request".to_string(),
            ),
        }
    }

    /// Whether the engine task is still running.
    pub fn is_live(&self) -> bool {
        !self.jobs.is_closed()
    }
}

fn error_response(request_id: u64, code: StatusCode, message: String) -> Response {
    Response {
        request_id,
        body: ResponseBody::Error(WireError {
            code,
            message,
            diff: None,
        }),
    }
}
