//! Connection pooling.
//!
//! A ThetaBase connection is not cheap: it costs a TCP handshake plus a protocol
//! handshake with token validation. Paying that per request would put `get`
//! nowhere near its 5ms p50 budget, so connections are kept and reused.
//!
//! The pool holds at most `size` connections. Callers check one out, use it, and
//! return it; a connection that errored is dropped rather than returned, because
//! a stream in an unknown state is worse than a reconnect.

use std::collections::VecDeque;
use std::sync::Arc;

use theta_proto::frame::{self, LENGTH_PREFIX_BYTES};
use theta_proto::{Hello, Request, Response, Welcome, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::client::ScribeError;
use crate::token::TokenSource;

/// A connection that has completed the handshake and is ready for requests.
#[derive(Debug)]
pub struct Connection {
    stream: TcpStream,
    next_request_id: u64,
    pub welcome: Welcome,
}

impl Connection {
    /// Open and handshake.
    pub async fn open(addr: &str, token: &str, client_name: &str) -> Result<Self, ScribeError> {
        let mut stream = TcpStream::connect(addr).await?;
        // Request/response latency matters more than write batching here.
        stream.set_nodelay(true)?;

        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            session_token: token.to_string(),
            client_name: client_name.to_string(),
        };
        write_frame(&mut stream, &hello.encode()).await?;

        let payload = read_frame(&mut stream)
            .await?
            .ok_or_else(|| ScribeError::Handshake("server closed during handshake".into()))?;

        match Welcome::decode(&payload) {
            Ok(welcome) => Ok(Self {
                stream,
                next_request_id: 1,
                welcome,
            }),
            Err(_) => {
                // A refusal arrives as an error Response rather than a Welcome,
                // and carries the reason the server declined.
                let refusal = Response::decode(&payload)
                    .map_err(|e| ScribeError::Handshake(format!("unintelligible reply: {e}")))?;
                Err(match refusal.body {
                    theta_proto::ResponseBody::Error(e) => ScribeError::Refused {
                        code: e.code,
                        message: e.message,
                    },
                    other => ScribeError::Handshake(format!("unexpected reply: {other:?}")),
                })
            }
        }
    }

    /// Send one request and await its response.
    pub async fn call(
        &mut self,
        branch_id: u64,
        body: theta_proto::RequestBody,
    ) -> Result<Response, ScribeError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let request = Request {
            request_id,
            branch_id,
            body,
        };
        write_frame(&mut self.stream, &request.encode()).await?;

        let payload = read_frame(&mut self.stream)
            .await?
            .ok_or(ScribeError::ConnectionClosed)?;
        let response = Response::decode(&payload)?;

        if response.request_id != request_id {
            // The stream is now out of step with what we believe about it, so
            // the connection cannot be reused.
            return Err(ScribeError::Desynchronized {
                expected: request_id,
                actual: response.request_id,
            });
        }
        Ok(response)
    }

    /// Send several requests before reading any reply.
    ///
    /// This is the batching win: N requests cost one round trip rather than N,
    /// because the server processes them in order and replies in order on the
    /// same connection.
    pub async fn pipeline(
        &mut self,
        branch_id: u64,
        bodies: Vec<theta_proto::RequestBody>,
    ) -> Result<Vec<Response>, ScribeError> {
        if bodies.is_empty() {
            return Ok(Vec::new());
        }

        let mut ids = Vec::with_capacity(bodies.len());
        let mut out = Vec::with_capacity(bodies.len());

        for body in bodies {
            let request_id = self.next_request_id;
            self.next_request_id += 1;
            ids.push(request_id);
            let request = Request {
                request_id,
                branch_id,
                body,
            };
            // Buffered by the kernel; flushed once below.
            self.stream
                .write_all(&frame::frame(&request.encode())?)
                .await?;
        }
        self.stream.flush().await?;

        for expected in ids {
            let payload = read_frame(&mut self.stream)
                .await?
                .ok_or(ScribeError::ConnectionClosed)?;
            let response = Response::decode(&payload)?;
            if response.request_id != expected {
                return Err(ScribeError::Desynchronized {
                    expected,
                    actual: response.request_id,
                });
            }
            out.push(response);
        }
        Ok(out)
    }
}

/// A fixed-size pool of handshaked connections.
#[derive(Debug)]
pub struct ConnectionPool {
    addr: String,
    client_name: String,
    size: usize,
    idle: Mutex<VecDeque<Connection>>,
    token: Arc<dyn TokenSource>,
}

impl ConnectionPool {
    pub fn new(
        addr: impl Into<String>,
        client_name: impl Into<String>,
        size: usize,
        token: Arc<dyn TokenSource>,
    ) -> Self {
        Self {
            addr: addr.into(),
            client_name: client_name.into(),
            size: size.max(1),
            idle: Mutex::new(VecDeque::new()),
            token,
        }
    }

    /// Take a connection, opening one if none is idle.
    pub async fn acquire(&self) -> Result<Connection, ScribeError> {
        if let Some(conn) = self.idle.lock().await.pop_front() {
            return Ok(conn);
        }
        self.open().await
    }

    /// Return a healthy connection for reuse. Connections beyond the pool size
    /// are closed rather than accumulated.
    pub async fn release(&self, conn: Connection) {
        let mut idle = self.idle.lock().await;
        if idle.len() < self.size {
            idle.push_back(conn);
        }
    }

    pub async fn idle_count(&self) -> usize {
        self.idle.lock().await.len()
    }

    async fn open(&self) -> Result<Connection, ScribeError> {
        let token = self.token.token();
        match Connection::open(&self.addr, &token, &self.client_name).await {
            Ok(conn) => Ok(conn),
            Err(ScribeError::Refused { code, message })
                if code == theta_proto::StatusCode::Unauthorized =>
            {
                // The token may simply have expired. Refresh once and retry;
                // retrying with the same rejected token would be a loop.
                if !self.token.refresh() {
                    return Err(ScribeError::Refused { code, message });
                }
                let token = self.token.token();
                Connection::open(&self.addr, &token, &self.client_name).await
            }
            Err(other) => Err(other),
        }
    }
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<(), ScribeError> {
    stream.write_all(&frame::frame(payload)?).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, ScribeError> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    match stream.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let size = frame::decode_length(prefix)?;
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}
