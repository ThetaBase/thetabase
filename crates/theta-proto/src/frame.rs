//! Framing: `[u32 length][message]` over a persistent connection
//! (`02-api-wire-protocol.md` §1).
//!
//! The length prefix is little-endian and counts only the payload. A frame
//! larger than [`MAX_FRAME_BYTES`] is refused *before* any allocation — an
//! attacker who can open a socket must not be able to make the server allocate
//! four gigabytes by sending four bytes.

use std::io;

use thiserror::Error;

/// Largest frame either side will send or accept. Generous for a schema change
/// or a batched write, far below anything that threatens the process.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

pub const LENGTH_PREFIX_BYTES: usize = 4;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame of {size} bytes exceeds the {max}-byte limit")]
    TooLarge { size: u32, max: u32 },

    #[error("connection closed mid-frame: expected {expected} bytes, read {actual}")]
    Truncated { expected: usize, actual: usize },

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("malformed message: {0}")]
    Malformed(String),
}

pub type Result<T> = std::result::Result<T, FrameError>;

/// Prefix `payload` with its length, ready to write.
pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    let size = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        size: u32::MAX,
        max: MAX_FRAME_BYTES,
    })?;
    check_size(size)?;

    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decode a length prefix, refusing an oversized frame before allocating.
pub fn decode_length(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Result<usize> {
    let size = u32::from_le_bytes(prefix);
    check_size(size)?;
    Ok(size as usize)
}

fn check_size(size: u32) -> Result<()> {
    if size > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size,
            max: MAX_FRAME_BYTES,
        });
    }
    Ok(())
}

/// Read one frame from a blocking reader. Used by tests and by the sync client;
/// the server reads frames asynchronously.
pub fn read_frame(reader: &mut impl io::Read) -> Result<Vec<u8>> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    reader.read_exact(&mut prefix)?;
    let size = decode_length(prefix)?;

    let mut payload = vec![0u8; size];
    reader
        .read_exact(&mut payload)
        .map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => FrameError::Truncated {
                expected: size,
                actual: 0,
            },
            _ => FrameError::Io(e),
        })?;
    Ok(payload)
}

pub fn write_frame(writer: &mut impl io::Write, payload: &[u8]) -> Result<()> {
    writer.write_all(&frame(payload)?)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let payload = b"hello thetabase".to_vec();
        let framed = frame(&payload).expect("frame");
        assert_eq!(read_frame(&mut Cursor::new(framed)).expect("read"), payload);
    }

    #[test]
    fn an_oversized_length_prefix_is_refused_before_allocating() {
        let prefix = (MAX_FRAME_BYTES + 1).to_le_bytes();
        assert!(matches!(
            decode_length(prefix),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_connection_closed_mid_frame_is_an_error_not_a_short_read() {
        let mut framed = frame(b"a longer payload here").expect("frame");
        framed.truncate(8); // prefix plus four bytes
        assert!(read_frame(&mut Cursor::new(framed)).is_err());
    }

    #[test]
    fn an_empty_payload_is_a_valid_frame() {
        let framed = frame(b"").expect("frame");
        assert_eq!(framed.len(), LENGTH_PREFIX_BYTES);
        assert!(read_frame(&mut Cursor::new(framed))
            .expect("read")
            .is_empty());
    }

    #[test]
    fn frames_are_read_back_in_order_from_one_stream() {
        let mut stream = Vec::new();
        for msg in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            stream.extend_from_slice(&frame(msg).expect("frame"));
        }
        let mut cursor = Cursor::new(stream);
        assert_eq!(read_frame(&mut cursor).expect("read"), b"one");
        assert_eq!(read_frame(&mut cursor).expect("read"), b"two");
        assert_eq!(read_frame(&mut cursor).expect("read"), b"three");
    }
}
