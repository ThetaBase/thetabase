//! Cold archive: log segments to object storage, and back again.
//!
//! `01-system-architecture.md` §7 and ROADMAP M10: periodic snapshots to object
//! storage with point-in-time restore, and an archive-unavailable path where
//! writes continue locally while snapshots queue and retry.
//!
//! # The one invariant
//!
//! **A local segment is never released until the archive has been proved to
//! return it byte for byte.**
//!
//! Not "until the upload returned 200". Not "until the compressor said it
//! succeeded". Proved, by decompressing what was stored and comparing the hash
//! to the original. Everything else here is arrangement; this is the part that
//! decides whether the archive is a backup or a story about one.
//!
//! It matters more here than for most databases because of `docs/INVARIANTS.md`
//! invariant 6: the log is the only source of truth, and every branch, view and
//! merge is a fold over it. A segment that archives wrong does not lose a table,
//! it loses the ability to reconstruct anything after that point.
//!
//! # Why AT-1
//!
//! Segments are append-only, highly structured, and never mutated — which is
//! the case a columnar compressor is best at. AT-1 (`tinyfiles.io`) is used as
//! the container format because three of its properties line up with what a log
//! archive actually needs, rather than with what is merely nice:
//!
//! * **Byte-identical decompression.** A lossy archive of a log is not an
//!   archive. This is the requirement, and it is the one we verify rather than
//!   assume.
//! * **SHA-256 integrity on the container**, so corruption in object storage is
//!   detectable without a full restore.
//! * **Queryable in place over HTTP Range**, so locating the segments a
//!   point-in-time restore needs does not mean pulling the whole archive first.
//!
//! Its WORM journal verification is a closer fit still — a ThetaBase log *is* an
//! append-only journal — and is left for M10 to take up once the hosted service
//! is wired in.
//!
//! # Why this is not in `thetad`
//!
//! Archiving talks to the network and the hot path may not
//! (`thetad/tests/no_llm_on_hot_path.rs`). Keeping it in its own crate means the
//! guard stays true by construction rather than by everyone remembering. The
//! archiver reads segments the engine has already released through
//! `archivable_segments()`, so it needs no access to a running engine at all.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod at1;
pub mod custodian;
pub mod drill;
pub mod manifest;
pub mod retention;
#[cfg(feature = "s3")]
pub mod s3;
pub mod schedule;
pub mod sweep;

pub use custodian::{Custodian, SegmentSource, TickReport};
pub use manifest::{ArchivedSegment, Manifest};
pub use retention::{Retention, RetentionError};
#[cfg(feature = "s3")]
pub use s3::{Integrity, S3Archive};
pub use schedule::{LogObserver, TickObserver, DEFAULT_INTERVAL};
pub use sweep::{ArchiveOutcome, Sweep};

/// A digest of a segment's bytes.
///
/// BLAKE3, matching `theta_core::ContentHash`, so a segment's archive identity
/// and its identity inside the engine are the same kind of thing. The AT-1
/// container carries its own SHA-256; that checks the container, this checks
/// the contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Digest(*blake3::hash(bytes).as_bytes())
    }

    pub fn of_file(path: &Path) -> Result<Self, ArchiveError> {
        let bytes = std::fs::read(path).map_err(|e| ArchiveError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        Ok(Self::of(&bytes))
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Enough to identify, short enough to read in a log line.
        write!(f, "{}", &self.to_hex()[..16])
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let hex = String::deserialize(d)?;
        if hex.len() != 64 {
            return Err(serde::de::Error::custom("a digest is 64 hex characters"));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| serde::de::Error::custom("a digest is hexadecimal"))?;
        }
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{path}: {detail}")]
    Io { path: PathBuf, detail: String },

    #[error("the archive is unreachable: {0}")]
    Unavailable(String),

    #[error(
        "segment {sequence} did not survive its round trip: stored {stored}, read back \
         {returned}. The local segment has been kept and nothing was released."
    )]
    RoundTripFailed {
        sequence: u64,
        stored: Digest,
        returned: Digest,
    },

    #[error("the archive has no segment {0}")]
    NotArchived(u64),

    #[error("{0}")]
    Backend(String),
}

/// Where archived segments are stored, and how they are put there.
///
/// A trait because the archive is the one component whose failure mode cannot
/// be exercised by hoping: the tests need an archive that corrupts, one that is
/// unreachable, and one that lies about success, and none of those can be asked
/// of a real service.
pub trait ArchiveBackend {
    /// Compress, verify, and store `source`. Returns a reference to what landed.
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError>;

    /// Retrieve a stored object and write the *original* bytes to `dest`.
    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError>;

    /// Whether the stored container is intact, without a full restore.
    ///
    /// Cheaper than `fetch` and weaker: it checks the container, not that the
    /// container holds what we meant to put in it. Never a substitute for the
    /// round-trip proof.
    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError>;
}

/// What a backend stored, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRef {
    /// The object's key in the archive.
    pub key: String,
    /// Bytes the container occupies, for reporting what compression bought.
    pub stored_bytes: u64,
    /// The container's own checksum, as the backend reports it.
    pub container_checksum: Option<String>,
}
