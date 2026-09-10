//! The WebAssembly boundary.
//!
//! Deliberately primitive: pointers and lengths into linear memory, and no
//! `wasm-bindgen`. Three reasons.
//!
//! * `wasm-bindgen` emits JavaScript glue, and glue is code that has to be kept
//!   in step with the Rust it wraps — a second thing to drift, in the crate
//!   whose whole purpose is that bindings cannot drift.
//! * Python, Deno and Cloudflare Workers cannot use that glue anyway. A raw
//!   module loads identically everywhere, and the amount of host code needed to
//!   drive it is a few dozen lines per language.
//! * The interface is JSON in, JSON out, so a host needs no generated binding to
//!   call it — only a JSON encoder it already has.
//!
//! # Memory protocol
//!
//! `alloc(len)` returns a pointer the host may write `len` bytes into.
//! Every call takes `(ptr, len)` and returns a pointer to a result buffer laid
//! out as `[u32 length][bytes]`, little-endian. The host reads the length,
//! reads the bytes, then calls `free_result(ptr)`. Results are leaked until
//! freed, on purpose: returning a pointer into a buffer Rust still owns would
//! be a use-after-free the moment the host called anything else.

use std::alloc::{alloc as raw_alloc, dealloc, Layout};

use crate::HostRequest;

/// Reserve `len` bytes for the host to write into.
///
/// # Safety
/// The host must write exactly `len` bytes and pass the same `len` back.
#[no_mangle]
pub extern "C" fn theta_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let layout = Layout::from_size_align(len, 1).expect("a byte layout is always valid");
    // SAFETY: `len` is non-zero, and the layout is well-formed.
    unsafe { raw_alloc(layout) }
}

/// Release a buffer obtained from [`theta_alloc`].
///
/// # Safety
/// `ptr` must have come from `theta_alloc` with this same `len`.
#[no_mangle]
pub unsafe extern "C" fn theta_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let layout = Layout::from_size_align(len, 1).expect("a byte layout is always valid");
    dealloc(ptr, layout);
}

/// Encode a request. Input is `HostRequest` JSON, output is a framed message.
///
/// # Safety
/// `ptr`/`len` must describe a buffer the host wrote.
#[no_mangle]
pub unsafe extern "C" fn theta_encode(
    ptr: *const u8,
    len: usize,
    request_id: u64,
    branch_id: u64,
) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    let result = serde_json::from_slice::<HostRequest>(input)
        .map_err(|e| format!("not a request this Scribe understands: {e}"))
        .and_then(|host| crate::encode(request_id, branch_id, &host));

    match result {
        Ok(bytes) => into_result(Tag::Ok, &bytes),
        Err(message) => into_result(Tag::Err, message.as_bytes()),
    }
}

/// Decode a wire response into JSON.
///
/// # Safety
/// `ptr`/`len` must describe a response body the host read off the socket.
#[no_mangle]
pub unsafe extern "C" fn theta_decode(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    match crate::decode(input) {
        Ok(value) => into_result(Tag::Ok, value.to_string().as_bytes()),
        Err(message) => into_result(Tag::Err, message.as_bytes()),
    }
}

/// How many bytes of body follow a 4-byte length prefix.
///
/// Returns `u32::MAX` for a prefix the framing rejects, which the host must
/// treat as a protocol error rather than a length.
///
/// # Safety
/// `ptr` must point to at least 4 readable bytes.
#[no_mangle]
pub unsafe extern "C" fn theta_body_length(ptr: *const u8) -> u32 {
    let mut prefix = [0u8; crate::LENGTH_PREFIX_BYTES];
    prefix.copy_from_slice(std::slice::from_raw_parts(ptr, crate::LENGTH_PREFIX_BYTES));

    match crate::body_length(prefix) {
        Ok(len) => u32::try_from(len).unwrap_or(u32::MAX),
        Err(_) => u32::MAX,
    }
}

/// Whether a request must drop cached reads, and which.
///
/// Returns JSON: `{"key": "..."} `, `{"all": true}`, or `{}`. The rule lives in
/// the core so that no host can acknowledge a write before dropping the key.
///
/// # Safety
/// `ptr`/`len` must describe `HostRequest` JSON.
#[no_mangle]
pub unsafe extern "C" fn theta_invalidation(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    let json = match serde_json::from_slice::<HostRequest>(input) {
        Err(e) => return into_result(Tag::Err, e.to_string().as_bytes()),
        Ok(host) if crate::invalidates_everything(&host) => serde_json::json!({ "all": true }),
        Ok(host) => match crate::invalidates(&host) {
            Some(key) => serde_json::json!({ "key": key }),
            None => serde_json::json!({}),
        },
    };
    into_result(Tag::Ok, json.to_string().as_bytes())
}

/// Encode a handshake. Input is `{"token": "...", "clientName": "..."}`.
///
/// # Safety
/// `ptr`/`len` must describe a buffer the host wrote.
#[no_mangle]
pub unsafe extern "C" fn theta_encode_hello(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct HelloArgs {
        token: String,
        #[serde(default = "default_client")]
        client_name: String,
    }
    fn default_client() -> String {
        format!("theta-scribe-wasm/{}", env!("CARGO_PKG_VERSION"))
    }

    let result = serde_json::from_slice::<HelloArgs>(input)
        .map_err(|e| e.to_string())
        .and_then(|args| crate::encode_hello(&args.token, &args.client_name));

    match result {
        Ok(bytes) => into_result(Tag::Ok, &bytes),
        Err(message) => into_result(Tag::Err, message.as_bytes()),
    }
}

/// Decode the server's reply to a handshake.
///
/// # Safety
/// `ptr`/`len` must describe a message body the host read off the socket.
#[no_mangle]
pub unsafe extern "C" fn theta_decode_welcome(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    match crate::decode_welcome(input) {
        Ok(value) => into_result(Tag::Ok, value.to_string().as_bytes()),
        Err(message) => into_result(Tag::Err, message.as_bytes()),
    }
}

/// Render a typed query AST to SQL-subset source and bound parameters.
///
/// Each binding offers its own fluent builder, because that is what makes a
/// query pleasant to write. What the builder produces is rendered here, once,
/// so a query built the same way in two languages reaches the server as the
/// same bytes — and so the rule that a value never becomes query text has one
/// implementation rather than three.
///
/// # Safety
/// `ptr`/`len` must describe a `QueryAst` JSON document.
#[no_mangle]
pub unsafe extern "C" fn theta_render_query(ptr: *const u8, len: usize) -> *mut u8 {
    let input = std::slice::from_raw_parts(ptr, len);

    let result = serde_json::from_slice::<crate::query::QueryAst>(input)
        .map_err(|e| format!("not a query this Scribe understands: {e}"))
        .and_then(|ast| crate::query::render(&ast).map_err(|e| e.to_string()));

    match result {
        Ok(rendered) => into_result(
            Tag::Ok,
            serde_json::to_string(&rendered)
                .unwrap_or_default()
                .as_bytes(),
        ),
        Err(message) => into_result(Tag::Err, message.as_bytes()),
    }
}

/// Release a result buffer returned by any of the calls above.
///
/// # Safety
/// `ptr` must be a result pointer this module returned, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn theta_free_result(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let mut header = [0u8; 4];
    header.copy_from_slice(std::slice::from_raw_parts(ptr, 4));
    let len = u32::from_le_bytes(header) as usize;

    let total = RESULT_HEADER + len;
    let layout = Layout::from_size_align(total, 1).expect("a byte layout is always valid");
    dealloc(ptr, layout);
}

/// `[u32 length][u8 tag][bytes]`.
const RESULT_HEADER: usize = 5;

#[derive(Clone, Copy)]
enum Tag {
    Ok = 0,
    Err = 1,
}

/// Lay a result out in linear memory for the host to read.
///
/// Leaked deliberately — the host frees it with [`theta_free_result`]. Handing
/// back a pointer into a buffer Rust still owned would dangle the moment the
/// host called anything else.
fn into_result(tag: Tag, bytes: &[u8]) -> *mut u8 {
    let total = RESULT_HEADER + bytes.len();
    let layout = Layout::from_size_align(total, 1).expect("a byte layout is always valid");

    // SAFETY: `total` is at least RESULT_HEADER, so never zero.
    let out = unsafe { raw_alloc(layout) };
    if out.is_null() {
        return out;
    }

    // SAFETY: `out` has room for the header and the payload.
    unsafe {
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        std::ptr::copy_nonoverlapping(len.to_le_bytes().as_ptr(), out, 4);
        *out.add(4) = tag as u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(RESULT_HEADER), bytes.len());
    }
    out
}
