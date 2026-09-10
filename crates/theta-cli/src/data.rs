//! Talking to a `thetad` instance.
//!
//! The Control Plane client in `client.rs` handles identity and provisioning
//! over HTTP. This is the other half: the data plane, over the length-prefixed
//! Cap'n Proto framing every SDK binding agrees on
//! (`02-api-wire-protocol.md` §1).
//!
//! One connection per command. The CLI is not a long-lived client and a
//! connection pool would only add a way for a stale socket to produce a
//! confusing error.

use theta_proto::frame::{self, LENGTH_PREFIX_BYTES};
use theta_proto::{Hello, Request, RequestBody, Response, ResponseBody, Welcome, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("cannot reach the instance at {address}: {detail}")]
    Unreachable { address: String, detail: String },

    #[error("{0}")]
    Refused(String),

    #[error("unexpected response from the instance: {0}")]
    Unexpected(String),
}

type Result<T> = std::result::Result<T, DataError>;

pub struct DataClient {
    stream: TcpStream,
    address: String,
    next_request_id: u64,
    pub project_id: String,
}

impl DataClient {
    /// Connect and complete the handshake.
    ///
    /// The scoped session token goes in the `Hello`, never a raw project
    /// credential (`04-threat-model-security.md` §2).
    pub async fn connect(address: &str, session_token: &str) -> Result<Self> {
        let mut stream = TcpStream::connect(address)
            .await
            .map_err(|e| DataError::Unreachable {
                address: address.to_string(),
                detail: e.to_string(),
            })?;

        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            session_token: session_token.to_string(),
            client_name: concat!("theta-cli/", env!("CARGO_PKG_VERSION")).to_string(),
        };
        write_frame(&mut stream, &hello.encode(), address).await?;

        let payload = read_frame(&mut stream, address)
            .await?
            .ok_or_else(|| DataError::Unexpected("the instance closed during handshake".into()))?;

        // A refusal arrives as an error Response rather than a Welcome, so try
        // that shape before reporting the frame as malformed — otherwise a
        // clear "invalid session token" surfaces as a parse error.
        let welcome = match Welcome::decode(&payload) {
            Ok(welcome) => welcome,
            Err(_) => return Err(refusal_from(&payload)),
        };

        Ok(Self {
            stream,
            address: address.to_string(),
            next_request_id: 1,
            project_id: welcome.project_id,
        })
    }

    /// Send one request on `branch` and await its response body.
    pub async fn call(&mut self, branch: u64, body: RequestBody) -> Result<ResponseBody> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let request = Request {
            request_id,
            branch_id: branch,
            body,
        };
        let address = self.address.clone();
        write_frame(&mut self.stream, &request.encode(), &address).await?;

        let payload = read_frame(&mut self.stream, &address)
            .await?
            .ok_or_else(|| DataError::Unexpected("the instance closed mid-call".into()))?;
        let response =
            Response::decode(&payload).map_err(|e| DataError::Unexpected(e.to_string()))?;

        match response.body {
            ResponseBody::Error(err) => Err(DataError::Refused(err.message)),
            body => Ok(body),
        }
    }
}

/// Read a refusal that arrived where a `Welcome` was expected.
fn refusal_from(payload: &[u8]) -> DataError {
    match Response::decode(payload) {
        Ok(Response {
            body: ResponseBody::Error(err),
            ..
        }) => DataError::Refused(err.message),
        _ => DataError::Unexpected("the instance sent neither a Welcome nor an error".into()),
    }
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8], address: &str) -> Result<()> {
    let framed = frame::frame(payload).map_err(|e| DataError::Unexpected(e.to_string()))?;
    stream
        .write_all(&framed)
        .await
        .map_err(|e| DataError::Unreachable {
            address: address.to_string(),
            detail: e.to_string(),
        })?;
    stream.flush().await.map_err(|e| DataError::Unreachable {
        address: address.to_string(),
        detail: e.to_string(),
    })
}

async fn read_frame(stream: &mut TcpStream, address: &str) -> Result<Option<Vec<u8>>> {
    let unreachable = |e: std::io::Error| DataError::Unreachable {
        address: address.to_string(),
        detail: e.to_string(),
    };

    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    match stream.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(unreachable(e)),
    }

    let size = frame::decode_length(prefix).map_err(|e| DataError::Unexpected(e.to_string()))?;
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await.map_err(unreachable)?;
    Ok(Some(payload))
}
