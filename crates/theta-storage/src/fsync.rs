//! Making a *name* durable, as opposed to a file's contents.
//!
//! `File::sync_all` promises the bytes survive a crash. It promises nothing
//! about the directory entry that points at them, so a crash can leave a
//! segment whose data is intact and whose name is not — which reads, on
//! recovery, as a segment that was never written.
//!
//! How that is closed is genuinely different per platform, and the difference
//! is a fact about the filesystems rather than a weaker guarantee on one of
//! them.

use std::path::Path;

use crate::error::Result;

/// Make `dir`'s entries durable.
///
/// **POSIX** requires this explicitly: a `create` or `rename` is not durable
/// until the containing directory is fsynced, however many times the file
/// itself was synced.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Make `dir`'s entries durable.
///
/// **Windows** has no equivalent operation and needs none. `File::open` on a
/// directory is refused outright without `FILE_FLAG_BACKUP_SEMANTICS`, and
/// `FlushFileBuffers` on a directory handle obtained *with* that flag is
/// refused as well: the operation does not exist because it is not the
/// mechanism NTFS uses. Creates and renames are recorded in the NTFS metadata
/// journal, which is replayed on mount, so the directory entry is durable once
/// the call returns.
///
/// Spelled with `cfg` rather than by attempting the open and ignoring the
/// error, because ignoring it would also ignore a real fault on the data
/// directory — exactly the failure an operator needs to see. The path is still
/// probed so that a missing or unreadable data directory fails here too,
/// rather than becoming a difference in what the two platforms notice.
///
/// This was a live bug: the POSIX form ran everywhere, so every test that
/// opened a store failed on Windows with `ERROR_ACCESS_DENIED`, while CI —
/// Linux-only — stayed green.
#[cfg(not(unix))]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::metadata(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncing_a_real_directory_succeeds_on_every_platform() {
        // The regression test for the Windows bug. On Unix this exercises the
        // fsync; on Windows it fails if anyone reintroduces `File::open(dir)`.
        let dir = tempfile::tempdir().expect("tempdir");
        sync_dir(dir.path()).expect("a real directory must sync");
    }

    #[test]
    fn syncing_a_missing_directory_is_an_error_on_every_platform() {
        // Both branches have to agree about this. A platform difference in
        // durability mechanics should not become a platform difference in
        // whether a broken data directory is noticed.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("not-there");
        assert!(
            sync_dir(&missing).is_err(),
            "a missing data directory has to be reported, not passed over"
        );
    }
}
