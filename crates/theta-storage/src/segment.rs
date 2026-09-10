//! On-disk segment format.
//!
//! A segment is an append-only file of length-prefixed, checksummed records.
//! Nothing about a record is trusted until its checksum verifies, because the
//! one thing this format exists to survive is a process that died partway
//! through a write (`01-system-architecture.md` §7).
//!
//! ```text
//! Segment header (32 bytes)
//!   magic            8   b"THETASEG"
//!   format_version   4   u32 LE
//!   sequence         8   u64 LE   segment number within the log
//!   flags            4   u32 LE   bit 0: payloads are encrypted
//!   reserved         8   zeroed
//!
//! Record (repeated)
//!   length           4   u32 LE   payload length in bytes
//!   checksum         4   u32 LE   CRC32 of the payload as stored
//!   payload      length            encoded LogEntry, encrypted if flagged
//! ```
//!
//! The payload encoding is JSON in format version 1. It is deliberately
//! versioned in the header so a compact encoding can replace it without
//! breaking recovery of segments already on disk — old segments keep declaring
//! version 1 and keep being readable.
//!
//! # Encryption (SEC-2)
//!
//! The flag lives in the header rather than the format version because it is
//! not a format change: an encrypted segment has the same records in the same
//! places, and only the payload bytes differ. Keeping the version at 1 means a
//! database that turns encryption on keeps every segment it already wrote, and
//! only new segments are sealed. Recovery reads a mixed log without being told
//! which is which, because each segment says so itself.
//!
//! The checksum covers the payload **as stored** — the ciphertext, not the
//! plaintext. That is what lets recovery detect a torn record without holding
//! the key, and it keeps the meaning of a checksum failure the same in both
//! cases: the bytes on disk are not the bytes that were written.
//!
//! A decryption failure is therefore never reported as corruption. The checksum
//! has already proved the bytes are intact, so the only remaining explanation
//! is the wrong key, and treating that as a torn record would truncate the log
//! — destroying exactly the data encryption was added to protect.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use theta_core::LogEntry;

use crate::encryption::DataKey;
use crate::error::{Result, StorageError};

pub const MAGIC: &[u8; 8] = b"THETASEG";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_LEN: u64 = 32;
const RECORD_PREFIX_LEN: usize = 8;

/// Header flag: record payloads in this segment are sealed with the project's
/// data key.
const FLAG_ENCRYPTED: u32 = 1 << 0;

/// What a segment's header says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Segment number within the log.
    pub sequence: u64,
    /// Whether payloads are sealed. Read from the file rather than from
    /// configuration, so a log holding both kinds recovers correctly.
    pub encrypted: bool,
}

/// A record refused during recovery, and why. Recovery reports these rather
/// than silently dropping them, so a truncation always shows up in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TornRecord {
    /// The file ended mid-record.
    Truncated {
        offset: u64,
        expected_len: u64,
        available: u64,
    },
    /// The record was fully written but its checksum does not verify.
    ChecksumMismatch {
        offset: u64,
        expected: u32,
        actual: u32,
    },
    /// The payload verified but does not decode — a format-level corruption.
    Undecodable { offset: u64, detail: String },
    /// The segment is too short to hold a header: the crash landed during
    /// segment creation, so the file holds no records.
    HeaderIncomplete { available: u64 },
}

impl TornRecord {
    pub fn offset(&self) -> u64 {
        match self {
            TornRecord::Truncated { offset, .. }
            | TornRecord::ChecksumMismatch { offset, .. }
            | TornRecord::Undecodable { offset, .. } => *offset,
            TornRecord::HeaderIncomplete { .. } => 0,
        }
    }
}

/// Result of scanning one segment.
#[derive(Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<LogEntry>,
    /// Byte offset one past the last record that verified. Anything at or after
    /// this offset is unusable and is what recovery truncates to.
    pub valid_end: u64,
    /// Populated when the scan stopped early. At most one: a segment is
    /// append-only, so the first bad record makes everything after it
    /// unreachable regardless of whether those bytes happen to verify.
    pub torn: Option<TornRecord>,
}

/// Rewrite a segment's header, discarding whatever was there.
///
/// Only valid for a segment whose header is incomplete. The header is written
/// and fsynced when a segment is created, before any record can be appended, so
/// a partial header proves the crash happened during creation and that the file
/// holds no acknowledged records.
pub fn rewrite_header(path: &Path, sequence: u64, encrypted: bool) -> Result<()> {
    let mut file = File::create(path)?; // truncates
    write_header(&mut file, sequence, encrypted)?;
    file.sync_all()?;
    Ok(())
}

pub fn write_header(file: &mut File, sequence: u64, encrypted: bool) -> Result<()> {
    let mut header = [0u8; HEADER_LEN as usize];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..20].copy_from_slice(&sequence.to_le_bytes());
    let flags = if encrypted { FLAG_ENCRYPTED } else { 0 };
    header[20..24].copy_from_slice(&flags.to_le_bytes());
    file.write_all(&header)?;
    Ok(())
}

/// Read and validate a segment header.
pub fn read_header(reader: &mut impl Read) -> Result<SegmentHeader> {
    let mut header = [0u8; HEADER_LEN as usize];
    reader
        .read_exact(&mut header)
        .map_err(|_| StorageError::Corrupt {
            detail: "segment is shorter than its header".into(),
        })?;

    if &header[..8] != MAGIC {
        return Err(StorageError::Corrupt {
            detail: "segment magic does not match; not a ThetaBase segment".into(),
        });
    }

    let version = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormat { version });
    }

    let flags = u32::from_le_bytes(header[20..24].try_into().expect("4 bytes"));

    // Unknown flags are refused rather than ignored. Every flag this format
    // will ever gain changes how a payload must be read, so a reader that
    // ignores one it does not understand returns wrong data confidently — the
    // one failure mode a log-structured store cannot tolerate.
    let unknown = flags & !FLAG_ENCRYPTED;
    if unknown != 0 {
        return Err(StorageError::Corrupt {
            detail: format!(
                "segment header sets unknown flags {unknown:#x}; it was written by a newer version of ThetaBase"
            ),
        });
    }

    Ok(SegmentHeader {
        sequence: u64::from_le_bytes(header[12..20].try_into().expect("8 bytes")),
        encrypted: flags & FLAG_ENCRYPTED != 0,
    })
}

/// Encode one record. Returns the bytes to append.
///
/// With a key, the payload is sealed before it is measured and checksummed, so
/// the length and CRC describe the bytes that actually land on disk. Recovery
/// can then verify a record without the key, and only needs it to read one.
pub fn encode_record(entry: &LogEntry, key: Option<&DataKey>) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(entry)?;
    let payload = match key {
        Some(key) => key.seal(&payload)?,
        None => payload,
    };
    let checksum = crc32fast::hash(&payload);

    let mut out = Vec::with_capacity(RECORD_PREFIX_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Scan a segment from `start_offset`, stopping at the first record that does
/// not verify.
///
/// Stopping at the first bad record is the point: a torn write means the process
/// died there, so any bytes beyond it are from a write that was never
/// acknowledged. Interpreting them would resurrect data the caller was told was
/// lost.
/// Scan one segment.
///
/// `key` may be `None` even for an encrypted segment — the caller learns from
/// the returned error that a key is required, rather than having to know in
/// advance. The reverse is also fine: a key is ignored for a segment that
/// declares itself plaintext, which is what lets a database enable encryption
/// without rewriting the segments it already has.
pub fn scan(path: &Path, start_offset: u64, key: Option<&DataKey>) -> Result<ScanResult> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();

    // A segment too short to hold a header was interrupted during creation, so
    // it contains no records. Reported as torn rather than as an error: an
    // incomplete header is a crash artifact, not an unreadable log.
    if file_len < HEADER_LEN {
        return Ok(ScanResult {
            entries: Vec::new(),
            valid_end: HEADER_LEN,
            torn: Some(TornRecord::HeaderIncomplete {
                available: file_len,
            }),
        });
    }

    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;

    // Refused up front rather than per record. Every record in the segment
    // would fail identically, and a missing key is a configuration mistake the
    // operator should hear about once, in terms they can act on.
    let key = match (header.encrypted, key) {
        (true, None) => {
            return Err(StorageError::Encryption(format!(
                "segment {} is encrypted and no data key is configured; set {}",
                path.display(),
                crate::encryption::DATA_KEY_ENV
            )))
        }
        (true, Some(key)) => Some(key),
        // A key configured for a plaintext segment is not an error: it is a log
        // written before encryption was turned on, and refusing it would make
        // enabling encryption mean losing the history.
        (false, _) => None,
    };

    let mut offset = start_offset.max(HEADER_LEN);
    reader.seek(SeekFrom::Start(offset))?;

    let mut result = ScanResult {
        valid_end: offset,
        ..Default::default()
    };

    loop {
        if offset >= file_len {
            break;
        }

        let mut prefix = [0u8; RECORD_PREFIX_LEN];
        if reader.read_exact(&mut prefix).is_err() {
            result.torn = Some(TornRecord::Truncated {
                offset,
                expected_len: RECORD_PREFIX_LEN as u64,
                available: file_len - offset,
            });
            break;
        }

        let len = u32::from_le_bytes(prefix[..4].try_into().expect("4 bytes")) as usize;
        let expected_crc = u32::from_le_bytes(prefix[4..].try_into().expect("4 bytes"));

        let available = file_len - offset - RECORD_PREFIX_LEN as u64;
        if (len as u64) > available {
            result.torn = Some(TornRecord::Truncated {
                offset,
                expected_len: len as u64,
                available,
            });
            break;
        }

        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).is_err() {
            result.torn = Some(TornRecord::Truncated {
                offset,
                expected_len: len as u64,
                available,
            });
            break;
        }

        let actual_crc = crc32fast::hash(&payload);
        if actual_crc != expected_crc {
            result.torn = Some(TornRecord::ChecksumMismatch {
                offset,
                expected: expected_crc,
                actual: actual_crc,
            });
            break;
        }

        // Unsealed after the checksum, so a failure here cannot be confused
        // with corruption: the bytes are provably the bytes that were written,
        // and the only remaining explanation is the wrong key. Returned as an
        // error rather than recorded as torn, because recovery responds to a
        // torn record by truncating the log — which for a whole segment of
        // undecryptable records would delete the data instead of reporting that
        // the wrong key was supplied.
        let payload = match key {
            Some(key) => key.open(&payload)?,
            None => payload,
        };

        match serde_json::from_slice::<LogEntry>(&payload) {
            Ok(entry) => result.entries.push(entry),
            Err(e) => {
                // Checksum verified, so the bytes are exactly what was written —
                // this is a format problem, not a torn write, and it must not be
                // silently skipped.
                result.torn = Some(TornRecord::Undecodable {
                    offset,
                    detail: e.to_string(),
                });
                break;
            }
        }

        offset += RECORD_PREFIX_LEN as u64 + len as u64;
        result.valid_end = offset;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use theta_core::{Author, BranchId, CommitId, ContentHash, OpType, Value};

    use super::*;

    fn entry(n: u64) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash::ZERO,
            commit_id: CommitId(n),
            branch_id: BranchId::MAIN,
            op: OpType::Put {
                key: format!("k{n}"),
                value: Value::Int(n as i64),
            },
            author: Author::System,
            timestamp_ms: n as i64,
        }
    }

    fn segment_with(entries: &[LogEntry]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("000.seg");
        let mut file = File::create(&path).expect("create");
        write_header(&mut file, 0, false).expect("header");
        for e in entries {
            file.write_all(&encode_record(e, None).expect("encode"))
                .expect("write");
        }
        file.sync_all().expect("sync");
        (dir, path)
    }

    #[test]
    fn a_clean_segment_scans_back_every_record() {
        let entries: Vec<_> = (0..5).map(entry).collect();
        let (_dir, path) = segment_with(&entries);

        let result = scan(&path, 0, None).expect("scan");
        assert_eq!(result.entries, entries);
        assert!(result.torn.is_none());
    }

    #[test]
    fn a_torn_trailing_record_is_truncated_not_interpreted() {
        let entries: Vec<_> = (0..3).map(entry).collect();
        let (_dir, path) = segment_with(&entries);

        // Simulate a process that died mid-write: append a record and lose its tail.
        let partial = encode_record(&entry(99), None).expect("encode");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(&partial[..partial.len() / 2])
            .expect("write");
        file.sync_all().expect("sync");

        let result = scan(&path, 0, None).expect("scan");
        assert_eq!(result.entries, entries, "the torn record must not appear");
        assert!(matches!(result.torn, Some(TornRecord::Truncated { .. })));
        assert!(result.valid_end < file.metadata().expect("meta").len());
    }

    #[test]
    fn a_corrupted_payload_fails_its_checksum_and_stops_the_scan() {
        let entries: Vec<_> = (0..3).map(entry).collect();
        let (_dir, path) = segment_with(&entries);

        // Flip a byte inside the second record's payload.
        let first_len = encode_record(&entries[0], None).expect("encode").len() as u64;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        file.seek(SeekFrom::Start(
            HEADER_LEN + first_len + RECORD_PREFIX_LEN as u64 + 2,
        ))
        .expect("seek");
        file.write_all(b"X").expect("write");
        file.sync_all().expect("sync");

        let result = scan(&path, 0, None).expect("scan");
        assert_eq!(result.entries.len(), 1, "the scan stops at the bad record");
        assert!(matches!(
            result.torn,
            Some(TornRecord::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn a_segment_from_a_future_format_is_refused_rather_than_guessed_at() {
        let mut header = [0u8; HEADER_LEN as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());

        let err = read_header(&mut Cursor::new(header.to_vec())).expect_err("must refuse");
        assert!(matches!(err, StorageError::UnsupportedFormat { .. }));
    }

    #[test]
    fn a_file_that_is_not_a_segment_is_rejected() {
        let err = read_header(&mut Cursor::new(
            b"not a segment at all!!!!!!!!!!!!!".to_vec(),
        ))
        .expect_err("must reject");
        assert!(matches!(err, StorageError::Corrupt { .. }));
    }

    #[test]
    fn scanning_from_an_offset_resumes_mid_segment() {
        let entries: Vec<_> = (0..4).map(entry).collect();
        let (_dir, path) = segment_with(&entries);

        let first_len = encode_record(&entries[0], None).expect("encode").len() as u64;
        let result = scan(&path, HEADER_LEN + first_len, None).expect("scan");
        assert_eq!(result.entries, entries[1..]);
    }
}
