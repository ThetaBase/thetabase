//! The shared backend contract, run against a backend that needs no service.
//!
//! `contract/mod.rs` holds the invariants; this file supplies something to run
//! them against on a machine with no bucket and no network, which is every CI
//! runner this repository has. `live_s3.rs` runs the identical suite against a
//! real object store.

use std::path::{Path, PathBuf};

use theta_archive::{ArchiveBackend, ArchiveError, Digest, StoredRef};

mod contract;
use contract::contract;

// ---- the backends -----------------------------------------------------------

/// A local directory, standing in for any object store.
///
/// Not a *fake* in the `round_trip.rs` sense: it does not misbehave on demand.
/// It is a real, honest backend over a real filesystem, and it is here so the
/// contract has something to run against on a machine with no bucket and no
/// network — which is every CI runner this repository has.
#[derive(Default)]
struct DirectoryArchive {
    root: Option<tempfile::TempDir>,
}

impl DirectoryArchive {
    fn new() -> Self {
        Self {
            root: Some(tempfile::tempdir().expect("tempdir")),
        }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.root
            .as_ref()
            .expect("root")
            .path()
            .join(key.replace('/', "_"))
    }
}

impl ArchiveBackend for DirectoryArchive {
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError> {
        let bytes = std::fs::read(source).map_err(|e| ArchiveError::Io {
            path: source.to_path_buf(),
            detail: e.to_string(),
        })?;
        let path = self.path(key);
        std::fs::write(&path, &bytes).map_err(|e| ArchiveError::Io {
            path,
            detail: e.to_string(),
        })?;
        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes: bytes.len() as u64,
            container_checksum: Some(Digest::of(&bytes).to_hex()),
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError> {
        let path = self.path(&stored.key);
        let bytes = std::fs::read(&path).map_err(|e| ArchiveError::Io {
            path,
            detail: e.to_string(),
        })?;
        std::fs::write(dest, &bytes).map_err(|e| ArchiveError::Io {
            path: dest.to_path_buf(),
            detail: e.to_string(),
        })
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        let path = self.path(&stored.key);
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(false);
        };
        Ok(match &stored.container_checksum {
            Some(recorded) => &Digest::of(&bytes).to_hex() == recorded,
            // Nothing recorded to compare against. Not a reason to call it
            // broken — the same reasoning `S3Archive` applies to a store that
            // computes no checksum.
            None => true,
        })
    }
}

#[test]
fn a_directory_backend_satisfies_the_contract() {
    contract("directory", DirectoryArchive::new);
}

// AT-1 is exercised against the real service in `live_at1.rs`, which needs the
// CLI installed and is not part of `make gates` for that reason. When it runs,
// it should run this contract too — the point of the contract is that it is the
// same for every backend, and a backend excused from it is the failure this
// file exists to prevent.
//
// `S3Archive` needs a bucket, so its contract run belongs with the other live
// suites rather than here. `make archive-live` is where both go.
