//! Runs the shared conformance suite from Rust, against a live thetad.
//!
//! Prints one JSON document on stdout. The Node, Python and Go runners print
//! the same document from the same cases, and `make conformance` diffs all
//! four: bindings that each pass their own suite prove nothing about whether
//! they agree.
//!
//! # What this one proves, and what it does not
//!
//! It reaches the same protocol core the others do, and it links it rather than
//! loading it through a WASM runtime. So it does not exercise `abi.rs` — the
//! byte-buffer boundary — which the other three exercise three times over. What
//! it does exercise, and they cannot yet, is the *client*: `theta-scribe` is
//! Rust, so this is the only binding whose `get`, `put` and `query` are real.
//!
//! Blocking I/O and no async runtime. The runner is sequential by nature — one
//! request, one response, in order — and an executor here would be ceremony
//! around a loop.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use theta_scribe_wasm::HostRequest;

/// One case's outcome.
///
/// A struct rather than a map, because serde writes a struct's fields in
/// declaration order and a map in sorted order — and the other three runners
/// emit `name`, `status`, then the body. The comparison is byte for byte, so
/// key order is part of the contract.
#[derive(Serialize)]
struct Outcome {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendered: Option<RenderedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl Outcome {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            status: None,
            response: None,
            rendered: None,
            message: None,
        }
    }
}

/// A rendered query, as the other runners print it.
///
/// A struct rather than a `serde_json::Value`, because a `Value::Object` is a
/// `BTreeMap` and serde writes it in sorted key order — `params` would come
/// before `sql`, where the other three emit `sql` first. The comparison is byte
/// for byte, so key order is part of the contract.
#[derive(Serialize)]
struct RenderedJson {
    sql: String,
    params: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    protocol_version: u32,
    results: Vec<Outcome>,
    queries: Vec<Outcome>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (address, token) = match args.as_slice() {
        [address, token, ..] => (address.clone(), token.clone()),
        _ => return Err("usage: conformance <host:port> <token>".into()),
    };

    let root = repo_root()?;
    let suite: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("sdk/conformance/cases.json"))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let mut socket = Socket::connect(&address)?;

    // Handshake first: the server refuses anything else until it has one.
    let hello = theta_scribe_wasm::encode_hello(&token, "conformance-rust")?;
    socket.write(&hello)?;
    let welcome = theta_scribe_wasm::decode_welcome(&socket.frame()?)?;

    // Request ids start at 1 and follow the case's position, matching the other
    // three runners. They are compared rather than redacted: two bindings
    // disagreeing about how many requests they sent is worth catching.
    let mut results = Vec::new();
    for (index, case) in suite["cases"]
        .as_array()
        .ok_or("cases is not an array")?
        .iter()
        .enumerate()
    {
        results.push(exchange(&mut socket, case, index as u64 + 1));
    }
    drop(socket);

    // The typed builder renders rather than calls, so these need no server.
    // Each binding builds with its own API; the rendered output is compared.
    let mut queries = Vec::new();
    for case in suite["queries"]
        .as_array()
        .ok_or("queries is not an array")?
    {
        queries.push(render(case));
    }

    let report = Report {
        protocol_version: welcome["protocolVersion"].as_u64().unwrap_or(0) as u32,
        results,
        queries,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn exchange(socket: &mut Socket, case: &Value, request_id: u64) -> Outcome {
    let mut outcome = Outcome::new(case["name"].as_str().unwrap_or_default());

    let attempt = (|| -> Result<Value, Refusal> {
        // The suite's requests are host JSON, which is exactly what the core's
        // `HostRequest` deserializes — so a case naming a field the core does
        // not have fails here, in the binding, rather than reaching the server.
        // From the compact *text* rather than from the parsed `Value`: serde
        // reports a line and column when it reads a string and cannot when it
        // reads a tree, and the other three runners all hand the core a string.
        // A refusal that named no position here would differ from theirs by the
        // words "at line 1 column 32" and fail the comparison over nothing.
        let text =
            serde_json::to_string(&case["request"]).map_err(|e| Refusal::client(e.to_string()))?;
        let host: HostRequest = serde_json::from_str(&text).map_err(|e| Refusal {
            client: true,
            message: format!("not a request this Scribe understands: {e}"),
        })?;
        let framed = theta_scribe_wasm::encode(request_id, 0, &host).map_err(Refusal::client)?;
        socket.write(&framed).map_err(Refusal::transport)?;
        let body = socket.frame().map_err(Refusal::transport)?;
        theta_scribe_wasm::decode(&body).map_err(Refusal::client)
    })();

    match attempt {
        Ok(response) => {
            outcome.status = Some("ok");
            outcome.response = Some(redact(response, &case["redact"]));
        }
        Err(refusal) => {
            outcome.status = Some(match refusal.client {
                true => "clientError",
                false => "transportError",
            });
            outcome.message = Some(first_line(&refusal.message));
        }
    }
    outcome
}

fn render(case: &Value) -> Outcome {
    let mut outcome = Outcome::new(case["name"].as_str().unwrap_or_default());
    let spec = &case["build"];

    let mut query = thetabase::query::table(spec["table"].as_str().unwrap_or_default());
    if let Some(columns) = spec["select"].as_array() {
        query = query.select(
            columns
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string)),
        );
    }
    for clause in spec["where"].as_array().into_iter().flatten() {
        let op = clause[0].as_str().unwrap_or_default();
        let column = clause[1].as_str().unwrap_or_default();
        match predicate(op, column, clause.get(2).cloned().unwrap_or(Value::Null)) {
            Some(p) => query = query.filter(p),
            None => {
                outcome.status = Some("error");
                outcome.message = Some(format!(
                    "the suite names an operator this SDK does not have: {op:?}"
                ));
                return outcome;
            }
        }
    }
    for clause in spec["orderBy"].as_array().into_iter().flatten() {
        query = query.order_by(
            clause[0].as_str().unwrap_or_default(),
            clause[1].as_bool().unwrap_or(false),
        );
    }
    if let Some(limit) = spec["limit"].as_u64() {
        query = query.limit(limit);
    }
    if let Some(offset) = spec["offset"].as_u64() {
        query = query.offset(offset);
    }

    match query.render() {
        Ok(rendered) => {
            outcome.status = Some("ok");
            outcome.rendered = Some(RenderedJson {
                sql: rendered.sql,
                params: rendered.params.into_iter().collect(),
            });
        }
        Err(e) => {
            outcome.status = Some("clientError");
            outcome.message = Some(first_line(&e.to_string()));
        }
    }
    outcome
}

fn predicate(op: &str, column: &str, value: Value) -> Option<theta_scribe_wasm::query::Predicate> {
    use thetabase::query as q;
    Some(match op {
        "eq" => q::eq(column, value),
        "ne" => q::ne(column, value),
        "lt" => q::lt(column, value),
        "lte" => q::lte(column, value),
        "gt" => q::gt(column, value),
        "gte" => q::gte(column, value),
        "isNull" => q::is_null(column),
        "notNull" => q::not_null(column),
        _ => return None,
    })
}

/// Blank out values that legitimately differ between runs.
fn redact(mut response: Value, paths: &Value) -> Value {
    for dotted in paths.as_array().into_iter().flatten() {
        let Some(dotted) = dotted.as_str() else {
            continue;
        };
        let parts: Vec<&str> = dotted.split('.').collect();
        // A `pointer_mut` walk rather than a loop of `get_mut`: the borrow
        // checker refuses to let a loop reassign a `&mut` from the thing it is
        // already borrowing, and the JSON-pointer form says the same thing in
        // one call.
        let pointer = format!("/{}", parts.join("/"));
        if let Some(target) = response.pointer_mut(&pointer) {
            *target = Value::String("<redacted>".into());
        }
    }
    response
}

struct Refusal {
    /// Whether the core refused, as opposed to the transport failing. A caller
    /// may retry the second and must not retry the first.
    client: bool,
    message: String,
}

impl Refusal {
    fn client(message: String) -> Self {
        Self {
            client: true,
            message,
        }
    }
    fn transport(message: String) -> Self {
        Self {
            client: false,
            message,
        }
    }
}

struct Socket {
    stream: TcpStream,
}

impl Socket {
    fn connect(address: &str) -> Result<Self, String> {
        Ok(Self {
            stream: TcpStream::connect(address).map_err(|e| e.to_string())?,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write_all(bytes).map_err(|e| e.to_string())
    }

    fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; n];
        self.stream
            .read_exact(&mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Read one length-prefixed frame.
    fn frame(&mut self) -> Result<Vec<u8>, String> {
        let prefix = self.read_exact(4)?;
        let length = theta_scribe_wasm::body_length(
            prefix.as_slice().try_into().map_err(|_| "short prefix")?,
        )?;
        self.read_exact(length)
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn repo_root() -> Result<PathBuf, String> {
    let mut dir: &Path = Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("sdk/conformance/cases.json").is_file() {
            return Ok(dir.to_path_buf());
        }
        dir = dir
            .parent()
            .ok_or("no ThetaBase workspace above the SDK directory")?;
    }
}
