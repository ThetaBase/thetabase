//! Bring your own object storage (ROADMAP M10).
//!
//! An S3-compatible [`ArchiveBackend`], so a customer can keep their archive in
//! a bucket they control. **S3-compatible** rather than S3: the same API is
//! spoken by R2, B2, MinIO, Ceph and every on-premises appliance worth having,
//! so one implementation covers self-hosted and cloud both — which matters more
//! here than usual, because the entire point is that the customer controls it.
//!
//! # Why this exists, beyond the feature
//!
//! While AT-1 was the only real backend, "AT-1 is recommended" was not a
//! recommendation — it was the absence of a choice, and AT-1 is owned by
//! ThetaBase's founder (`DISTRIBUTION.md`). A suggestion a customer cannot weigh
//! against an alternative is a conflict wearing a preference's clothes. With a
//! bucket they control as a supported option, the recommendation becomes a
//! claim they can check.
//!
//! # The invariant does not move
//!
//! A segment is released only once the archive has been *proved* to return it
//! byte for byte, against a digest taken before any compressor saw it. That
//! belongs to [`crate::sweep`], not to a backend, so it holds for a bucket
//! exactly as it holds for AT-1. Adding a backend is not adding a way around
//! it — `tests/backend_contract.rs` runs the same invariant suite against every
//! backend for that reason.
//!
//! # `check_integrity` reports unknown rather than guessing
//!
//! The easiest thing to get wrong here. An object's ETag is an MD5 for a
//! single-part upload and **not a content hash at all** for a multipart one, so
//! treating it as a checksum reports "intact" for an object nothing has
//! checked. This asks for `x-amz-checksum-sha256`, which the store computes
//! server-side, and reports [`Integrity::Unknown`] where the store does not
//! provide one.
//!
//! A weaker answer is fine. `check_integrity` is documented as cheaper and
//! weaker than the round trip and never a substitute for it, so "I cannot tell"
//! costs nothing — it just means the round trip is the only proof, which it
//! always was. A confident wrong answer is the failure this method exists to
//! prevent.
//!
//! # zstd rather than raw
//!
//! Uncompressed log segments in a customer's bucket is a bill we would be
//! handing them. zstd is boring, ubiquitous and independently audited, and it
//! keeps the comparison with AT-1 specific rather than rhetorical: the honest
//! pitch is the ratio, plus WORM verification of append-only journals, plus
//! query-in-place over HTTP Range — not "we compress and they do not".

use std::path::Path;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tokio::runtime::Handle;

use crate::{ArchiveBackend, ArchiveError, StoredRef};

/// The environment variable naming the bucket.
pub const BUCKET_ENV: &str = "THETA_ARCHIVE_BUCKET";
/// The endpoint, for anything that is not AWS. Absent means AWS.
pub const ENDPOINT_ENV: &str = "THETA_ARCHIVE_ENDPOINT";

/// zstd level.
///
/// 10 rather than the default 3 or the maximum 22. Archiving is a background
/// sweep with no latency budget, so trading CPU for bytes is nearly free — but
/// the top levels cost several times the time for a few percent, and this is a
/// customer's bill either way.
const ZSTD_LEVEL: i32 = 10;

/// What a store could tell us about an object without fetching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// The store's own checksum matches what we recorded.
    Intact,
    /// The store's checksum disagrees. The object is not what was written.
    Corrupt,
    /// The store offers no checksum we can compare. Not a failure — it means
    /// the round trip is the only proof, which it always was.
    Unknown,
}

/// An archive in a bucket the customer controls.
#[derive(Debug, Clone)]
pub struct S3Archive {
    client: Client,
    bucket: String,
    /// Key prefix, so one bucket can hold several projects without collision.
    prefix: String,
    /// A handle to the runtime this was built on.
    ///
    /// `ArchiveBackend` is synchronous and the SDK is not, so calls are bridged
    /// the way `secrets_kms.rs` bridges KMS. Held rather than looked up per
    /// call because `Handle::current` panics off-runtime, and a backend
    /// constructed on a runtime should keep working wherever it is used.
    handle: Handle,
}

impl S3Archive {
    /// Build from the environment.
    ///
    /// Credentials come from the SDK's own chain — environment, profile,
    /// instance metadata — and never from the instance's data directory. A
    /// credential beside the data it protects is defeated by anything that
    /// copies the volume, which is the same argument SEC-2 makes about the data
    /// key.
    pub async fn from_env(prefix: impl Into<String>) -> Result<Self, ArchiveError> {
        let bucket = std::env::var(BUCKET_ENV).map_err(|_| {
            ArchiveError::Unavailable(format!("no archive bucket configured; set {BUCKET_ENV}"))
        })?;

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        // Anything that is not AWS: R2, B2, MinIO, Ceph, an appliance.
        if let Ok(endpoint) = std::env::var(ENDPOINT_ENV) {
            loader = loader.endpoint_url(endpoint);
        }
        let config = loader.load().await;

        Ok(Self {
            client: Client::new(&config),
            bucket,
            prefix: prefix.into(),
            handle: Handle::current(),
        })
    }

    /// Build from parts, for tests and for a caller wiring its own config.
    pub fn new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
            handle: Handle::current(),
        }
    }

    fn object_key(&self, key: &str) -> String {
        match self.prefix.is_empty() {
            true => key.to_string(),
            false => format!("{}/{}", self.prefix.trim_end_matches('/'), key),
        }
    }

    /// What the store says about an object, without fetching it.
    pub fn integrity(&self, stored: &StoredRef) -> Result<Integrity, ArchiveError> {
        let recorded = match &stored.container_checksum {
            Some(checksum) => checksum.clone(),
            // Nothing to compare against. Stored before checksums were
            // recorded, or by a store that offered none.
            None => return Ok(Integrity::Unknown),
        };

        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let key = self.object_key(&stored.key);

        let head = tokio::task::block_in_place(|| {
            self.handle.block_on(async move {
                client
                    .head_object()
                    .bucket(bucket)
                    .key(key)
                    .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
                    .send()
                    .await
            })
        })
        .map_err(|e| ArchiveError::Unavailable(format!("head_object: {e}")))?;

        // Deliberately *not* the ETag. An ETag is an MD5 for a single-part
        // upload and is not a content hash at all for a multipart one, so
        // comparing against it would report "intact" for an object nothing had
        // checked.
        match head.checksum_sha256() {
            Some(actual) if actual == recorded => Ok(Integrity::Intact),
            Some(_) => Ok(Integrity::Corrupt),
            None => Ok(Integrity::Unknown),
        }
    }
}

impl ArchiveBackend for S3Archive {
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError> {
        let plain = std::fs::read(source).map_err(|e| ArchiveError::Io {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })?;

        let compressed =
            zstd::encode_all(plain.as_slice(), ZSTD_LEVEL).map_err(|e| ArchiveError::Io {
                path: source.to_path_buf(),
                detail: format!("compressing: {e}"),
            })?;
        let stored_bytes = compressed.len() as u64;

        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let object_key = self.object_key(key);

        let checksum = tokio::task::block_in_place(|| {
            self.handle.block_on(async move {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(object_key)
                    // Asked for explicitly, so the store computes and records a
                    // real content hash rather than leaving us with an ETag.
                    .checksum_algorithm(aws_sdk_s3::types::ChecksumAlgorithm::Sha256)
                    .body(ByteStream::from(compressed))
                    .send()
                    .await
            })
        })
        .map_err(|e| ArchiveError::Unavailable(format!("put_object: {e}")))?;

        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes,
            // `None` when the store did not return one — recorded honestly, so
            // `integrity` reports Unknown rather than comparing against nothing.
            container_checksum: checksum.checksum_sha256().map(str::to_string),
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let object_key = self.object_key(&stored.key);

        let compressed = tokio::task::block_in_place(|| {
            self.handle.block_on(async move {
                let out = client
                    .get_object()
                    .bucket(bucket)
                    .key(object_key)
                    .send()
                    .await
                    .map_err(|e| ArchiveError::Unavailable(format!("get_object: {e}")))?;
                out.body
                    .collect()
                    .await
                    .map(|b| b.into_bytes().to_vec())
                    .map_err(|e| ArchiveError::Unavailable(format!("reading body: {e}")))
            })
        })?;

        let plain = zstd::decode_all(compressed.as_slice()).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: format!("decompressing: {e}"),
        })?;

        std::fs::write(dest, &plain).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: e.to_string(),
        })
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        // `Unknown` answers `true` here, and the distinction is deliberate.
        //
        // The trait's question is "is there a reason to think this is broken",
        // and "the store cannot tell me" is not one. Answering `false` would
        // report a healthy archive as corrupt on every store that does not
        // compute SHA-256 — an alert that fires constantly is an alert that
        // gets switched off.
        //
        // Nothing is lost: this method is documented as weaker than the round
        // trip and never a substitute for it, and the round trip is what
        // releasing a segment actually turns on. Callers wanting the three-way
        // answer have `S3Archive::integrity`.
        Ok(!matches!(self.integrity(stored)?, Integrity::Corrupt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_keeps_two_projects_out_of_each_others_keys() {
        // One bucket, several projects. Without the prefix, `segments/000.at1`
        // is the same object for every project sharing it — and the failure is
        // silent, because each one round-trips its own bytes back.
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = handle.enter();

        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .build();
        let client = Client::from_conf(config);

        let a = S3Archive::new(client.clone(), "bucket", "org_a/checkout");
        let b = S3Archive::new(client, "bucket", "org_b/analytics");

        assert_eq!(
            a.object_key("segments/000.at1"),
            "org_a/checkout/segments/000.at1"
        );
        assert_ne!(
            a.object_key("segments/000.at1"),
            b.object_key("segments/000.at1")
        );
    }

    #[test]
    fn an_unrecorded_checksum_reads_as_unknown_rather_than_intact() {
        // The property the whole `Integrity` enum exists for. Nothing to
        // compare against must not read as a successful comparison.
        let handle = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = handle.enter();

        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .build();
        let archive = S3Archive::new(Client::from_conf(config), "bucket", "p");

        let stored = StoredRef {
            key: "segments/000.at1".into(),
            stored_bytes: 1,
            container_checksum: None,
        };
        assert_eq!(
            archive.integrity(&stored).expect("no call made"),
            Integrity::Unknown
        );
    }

    #[test]
    fn zstd_round_trips_a_log_shaped_segment() {
        // The codec itself, so a failure here is not confused with a transport
        // one. Log-shaped input, because that is what the ratio is measured on.
        let segment: Vec<u8> = (0..2000)
            .flat_map(|n| {
                format!("{{\"commitId\":{n},\"op\":\"put\",\"key\":\"k{n}\"}}\n").into_bytes()
            })
            .collect();

        let compressed = zstd::encode_all(segment.as_slice(), ZSTD_LEVEL).expect("compress");
        assert!(
            compressed.len() < segment.len() / 4,
            "log-shaped data should compress well: {} -> {}",
            segment.len(),
            compressed.len()
        );
        assert_eq!(
            zstd::decode_all(compressed.as_slice()).expect("decompress"),
            segment,
            "zstd must return exactly what went in, or the round-trip proof \
             would fail for a reason that is not the archive's fault"
        );
    }
}
