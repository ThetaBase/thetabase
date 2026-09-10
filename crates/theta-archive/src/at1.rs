//! The AT-1 backend.
//!
//! Drives the `at1` CLI (`npm install -g @tinyfiles/cli`), which is the
//! documented interface and has a useful side effect: a subprocess is not a
//! linked HTTP client, so nothing here can drag a network stack into a
//! dependency closure that must not have one.
//!
//! # What is trusted and what is checked
//!
//! AT-1's decompression is documented as byte-identical, and `at1 integrity`
//! checks the container's own SHA-256. Neither is taken as sufficient. Every
//! segment is decompressed and compared against a digest taken *before* the
//! compressor ever saw it — see [`crate::sweep`]. That is not distrust of AT-1
//! specifically; it is that "the backup was fine" is a claim no backup system
//! should make about itself, and the check costs one decompression against a
//! restore nobody can perform.
//!
//! # Encoding needs an account, reading does not
//!
//! `at1` meters encoding and leaves decoding, querying and verifying free and
//! account-less. That asymmetry is worth more here than the free tier is: a
//! restore has to work when the billing relationship does not — an expired card
//! must never be a reason a customer cannot get their data back.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{ArchiveBackend, ArchiveError, StoredRef};

/// Which AT-1 codec to use.
///
/// `auto` picks by structural fingerprint with a verified bake-off, which is
/// the right default for log segments: their contents change shape as a
/// project's schema does, and pinning a codec would mean re-picking it by hand
/// every time that happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    /// Line-delimited JSON.
    QJson,
    /// Tabular, and queryable in place.
    QColumnar,
}

impl Codec {
    fn as_arg(self) -> &'static str {
        match self {
            Codec::Auto => "auto",
            Codec::QJson => "qjson",
            Codec::QColumnar => "qcolumnar",
        }
    }
}

/// Stores segments as AT-1 containers in a directory.
///
/// The directory is the archive: on a server it is a mount of object storage,
/// which is how AT-1 is designed to be used ("a queryable file format over
/// object storage rather than a database to install"). Keeping the object-store
/// details out of this crate means the archive can be S3, R2, GCS or a disk
/// without a line changing here.
#[derive(Debug)]
pub struct At1Archive {
    root: PathBuf,
    codec: Codec,
    /// The `at1` executable. Overridable so a test can point at a stub.
    program: PathBuf,
}

impl At1Archive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            codec: Codec::Auto,
            program: PathBuf::from("at1"),
        }
    }

    pub fn with_codec(mut self, codec: Codec) -> Self {
        self.codec = codec;
        self
    }

    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Whether the CLI is present and runnable.
    ///
    /// Checked up front so a missing tool is reported once, at start, rather
    /// than as a confusing failure on every segment of the first sweep.
    pub fn available(&self) -> bool {
        Command::new(&self.program)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// The account AT-1 will meter encoding against, if any.
    ///
    /// `None` means reads and verification still work and encoding will not —
    /// which is a coherent state, not a broken one, and the caller is told
    /// rather than left to discover it per segment.
    pub fn account(&self) -> Option<String> {
        let output = Command::new(&self.program).arg("whoami").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && !text.is_empty() && !text.contains("no connected account"))
            .then_some(text)
    }

    fn object_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn run(&self, args: &[&str]) -> Result<String, ArchiveError> {
        let output = Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| ArchiveError::Unavailable(format!("could not run `at1`: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(ArchiveError::Backend(format!(
                "`at1 {}` failed: {}",
                args.join(" "),
                match stderr.trim().is_empty() {
                    true => stdout.trim(),
                    false => stderr.trim(),
                }
            )));
        }

        // `at1 compress` exits 0 and writes nothing when no account is
        // connected, so a zero exit is not on its own evidence of success. The
        // caller checks the output file exists; this catches the specific case
        // early enough to say something useful about it.
        if stdout.contains("no connected account") || stderr.contains("no connected account") {
            return Err(ArchiveError::Unavailable(
                "AT-1 has no connected account, so encoding is unavailable. Run `at1 login \
                 --key <key>`. Reads, restores and verification do not need one — an \
                 archive already written stays readable."
                    .into(),
            ));
        }
        Ok(stdout)
    }
}

impl ArchiveBackend for At1Archive {
    fn store(&mut self, key: &str, source: &Path) -> Result<StoredRef, ArchiveError> {
        let destination = self.object_path(key);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ArchiveError::Io {
                path: parent.to_path_buf(),
                detail: e.to_string(),
            })?;
        }

        self.run(&[
            "compress",
            self.codec.as_arg(),
            &source.to_string_lossy(),
            &destination.to_string_lossy(),
        ])?;

        // A zero exit with no file is the shape of an unmetered refusal, so the
        // absence is checked rather than assumed away.
        let stored_bytes = std::fs::metadata(&destination)
            .map_err(|_| {
                ArchiveError::Backend(format!(
                    "`at1 compress` reported success but wrote no container at {}",
                    destination.display()
                ))
            })?
            .len();

        Ok(StoredRef {
            key: key.to_string(),
            stored_bytes,
            container_checksum: self
                .run(&["integrity", &destination.to_string_lossy()])
                .ok()
                .map(|s| s.trim().to_string()),
        })
    }

    fn fetch(&self, stored: &StoredRef, dest: &Path) -> Result<(), ArchiveError> {
        let source = self.object_path(&stored.key);
        if !source.exists() {
            return Err(ArchiveError::Backend(format!(
                "the archive has no object at {}",
                source.display()
            )));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ArchiveError::Io {
                path: parent.to_path_buf(),
                detail: e.to_string(),
            })?;
        }

        self.run(&[
            "decompress",
            &source.to_string_lossy(),
            &dest.to_string_lossy(),
        ])?;
        Ok(())
    }

    fn check_integrity(&self, stored: &StoredRef) -> Result<bool, ArchiveError> {
        let path = self.object_path(&stored.key);
        Ok(self.run(&["integrity", &path.to_string_lossy()]).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_cli_is_reported_as_unavailable_rather_than_as_corruption() {
        // The distinction matters operationally: "the tool is not installed" is
        // a deploy problem, and "the archive is corrupt" is an incident.
        let archive = At1Archive::new("/tmp").with_program("definitely-not-a-real-program");
        assert!(!archive.available());

        let err = archive
            .check_integrity(&StoredRef {
                key: "x".into(),
                stored_bytes: 0,
                container_checksum: None,
            })
            .err();
        // `check_integrity` answers false rather than erroring, so the caller
        // does not have to distinguish. `store` is where it must be loud.
        assert!(err.is_none());
    }

    #[test]
    fn storing_through_a_missing_cli_says_so_plainly() {
        let mut archive = At1Archive::new("/tmp").with_program("definitely-not-a-real-program");
        let err = archive
            .store("k", Path::new("/etc/hostname"))
            .expect_err("a missing CLI cannot store anything");

        assert!(
            matches!(err, ArchiveError::Unavailable(_)),
            "got {err:?}, which does not tell an operator to install anything"
        );
    }

    #[test]
    fn fetching_something_never_stored_is_an_error_and_not_an_empty_file() {
        // Silently producing an empty file here would mean a restore that
        // "succeeded" with a truncated log.
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = At1Archive::new(dir.path());

        let err = archive
            .fetch(
                &StoredRef {
                    key: "seg/nope.at1".into(),
                    stored_bytes: 0,
                    container_checksum: None,
                },
                &dir.path().join("out"),
            )
            .expect_err("nothing was stored under that key");

        assert!(matches!(err, ArchiveError::Backend(_)));
        assert!(!dir.path().join("out").exists());
    }
}
