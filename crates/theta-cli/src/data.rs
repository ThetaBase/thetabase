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

use std::sync::Arc;

use theta_proto::frame::{self, LENGTH_PREFIX_BYTES};
use theta_proto::{Hello, Request, RequestBody, Response, ResponseBody, Welcome, PROTOCOL_VERSION};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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

/// Everything the framing needs from a socket.
///
/// A trait object rather than a generic parameter on `DataClient`: the client
/// is constructed from an address string at runtime, so the choice cannot be
/// made at a type level by anything that calls it.
trait Duplex: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Duplex for T {}

/// The socket, with or without TLS around it.
///
/// Both arms carry the same framing, which is the point of putting the choice
/// here rather than at every call site.
enum Transport {
    /// A local or self-hosted instance, reached in the clear.
    Plain(TcpStream),
    /// A provisioned instance, reached through Fly's TLS proxy.
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Transport {
    fn stream(&mut self) -> &mut (dyn Duplex + Send) {
        match self {
            Transport::Plain(stream) => stream,
            Transport::Tls(stream) => stream.as_mut(),
        }
    }
}

/// Whether this address should be dialled with TLS.
///
/// `THETA_TLS` wins when it is set, so a self-hosted instance behind a
/// terminating proxy on some other port can say so -- and so can a developer
/// tunnelling 443 to a local plaintext process.
///
/// Otherwise: port 443 means TLS. That is the port Fly's proxy listens on and
/// the port `FlyRuntime::address` hands out, and it is the only port in this
/// product's vocabulary that implies a terminator in front of `thetad`. Local
/// instances are on 7700 and upwards.
///
/// Inferred from the address rather than carried beside it, deliberately. The
/// address travels through `THETA_ADDRESS` into every SDK and every `theta
/// exec` child; a second variable that had to agree with it would be a second
/// thing to get wrong, and the symptom of getting it wrong is a hang rather
/// than an error -- a plaintext frame sent to a TLS listener is not rejected,
/// it is simply never answered.
///
/// Public because it is part of the contract rather than an implementation
/// detail: a customer deciding how to write `THETA_ADDRESS` for a self-hosted
/// instance needs to be able to ask what a given address will do, and the
/// answer being guessable from a doc comment is not the same as being
/// checkable.
///
/// It is also the only way to test the rule at all. Every observable check runs
/// against an ephemeral port, where both the rule and its negation produce a
/// plaintext connection -- so a test of `THETA_TLS=0` through `connect` passes
/// whether or not the override is honoured. I planted the override's removal
/// and the suite stayed green.
pub fn wants_tls(address: &str) -> bool {
    match std::env::var("THETA_TLS").ok().as_deref() {
        Some("1") | Some("true") | Some("require") | Some("yes") => return true,
        Some("0") | Some("false") | Some("off") | Some("no") => return false,
        _ => {}
    }

    port_of(address) == Some("443")
}

/// The port, as written. `None` for an address with no colon.
fn port_of(address: &str) -> Option<&str> {
    address.rsplit_once(':').map(|(_, port)| port.trim())
}

/// The host, for SNI and certificate verification.
///
/// A shared IPv4 on Fly is routed *by* SNI, so getting this wrong does not
/// produce a certificate error -- it produces a connection to the wrong app,
/// or to none.
fn sni_host(address: &str) -> &str {
    match address.rsplit_once(':') {
        Some((host, _)) => host.trim_start_matches('[').trim_end_matches(']'),
        None => address,
    }
}

pub struct DataClient {
    transport: Transport,
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
        let tcp = TcpStream::connect(address)
            .await
            .map_err(|e| DataError::Unreachable {
                address: address.to_string(),
                detail: e.to_string(),
            })?;

        // Nagle off. Every exchange here is one small frame and then a wait for
        // the answer, which is the exact shape Nagle delays.
        let _ = tcp.set_nodelay(true);

        let mut transport = match wants_tls(address) {
            false => Transport::Plain(tcp),
            true => Transport::Tls(Box::new(tls_connect(tcp, address).await?)),
        };
        let stream = transport.stream();

        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            session_token: session_token.to_string(),
            client_name: concat!("theta-cli/", env!("CARGO_PKG_VERSION")).to_string(),
        };
        write_frame(stream, &hello.encode(), address).await?;

        let payload = read_frame(stream, address)
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
            transport,
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
        let stream = self.transport.stream();
        write_frame(stream, &request.encode(), &address).await?;

        let payload = read_frame(stream, &address)
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

/// Wrap a connected socket in TLS.
///
/// Roots from `webpki-roots` rather than the platform store. Fly's certificates
/// are Let's Encrypt, the set is compiled in, and a client whose behaviour
/// depends on whichever CA bundle a container happens to ship is a client that
/// works on one machine and not the next.
///
/// The crypto provider is named, never defaulted. Two backends reach this
/// workspace -- `reqwest` brings ring, the `kms` feature brings aws-lc-rs --
/// and `ClientConfig::builder()` resolves a process-level default that *panics*
/// when the choice is ambiguous rather than picking one. That exact panic
/// reached production once already, in `store_postgres.rs`.
async fn tls_connect(
    tcp: TcpStream,
    address: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let unreachable = |detail: String| DataError::Unreachable {
        address: address.to_string(),
        detail,
    };

    let mut roots = tokio_rustls::rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };

    // A deployment's own CA, added to the public roots rather than replacing
    // them. A customer running `thetad` behind their own terminating proxy
    // signs with their own CA, and a client that trusts only the compiled-in
    // set cannot reach it -- and "turn TLS off instead" is not an answer for a
    // database.
    //
    // Added rather than substituted, and said plainly because the difference
    // matters: this widens what the client will accept, it does not pin. A
    // deployment that wants only its own CA to be acceptable is asking for
    // something this does not provide.
    //
    // Failures are refusals, never warnings. An operator who pointed this at
    // the wrong path has said "trust this CA", and continuing without it would
    // silently be a different, laxer decision than the one they made.
    if let Ok(path) = std::env::var("THETA_TLS_CA") {
        let pem = std::fs::read(&path).map_err(|e| {
            unreachable(format!(
                "THETA_TLS_CA points at `{path}`, which cannot be read: {e}"
            ))
        })?;

        let mut cursor = std::io::BufReader::new(std::io::Cursor::new(pem));
        let mut added = 0usize;
        for entry in rustls_pemfile::certs(&mut cursor) {
            let cert = entry.map_err(|e| {
                unreachable(format!("THETA_TLS_CA `{path}` is not readable PEM: {e}"))
            })?;
            roots.add(cert).map_err(|e| {
                unreachable(format!(
                    "a certificate in THETA_TLS_CA `{path}` was refused: {e}"
                ))
            })?;
            added += 1;
        }

        if added == 0 {
            return Err(unreachable(format!(
                "THETA_TLS_CA `{path}` contains no certificates. An empty \
                 trust file is almost certainly the wrong file, and ignoring \
                 it would mean connecting under a trust policy nobody chose."
            )));
        }
    }

    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| unreachable(format!("tls: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let host = sni_host(address);
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| {
            unreachable(format!(
                "`{host}` is not a valid TLS server name. A provisioned \
                 instance is reached by hostname, because a shared address is \
                 routed by the name in the TLS handshake -- an IP address \
                 cannot say which instance is wanted."
            ))
        })?;

    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .map_err(|e| unreachable(format!("tls handshake: {e}")))
}

async fn write_frame(
    stream: &mut (dyn Duplex + Send),
    payload: &[u8],
    address: &str,
) -> Result<()> {
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

async fn read_frame(stream: &mut (dyn Duplex + Send), address: &str) -> Result<Option<Vec<u8>>> {
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
