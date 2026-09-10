//! Local credential storage.
//!
//! `06-provisioning-identity-flow.md` §2 asks for the OS keychain "where
//! available". Availability is a runtime fact, not a compile-time one — a Linux
//! box with no Secret Service, a CI container, a headless server — so the store
//! picks a backend at runtime and, importantly, *says which one it used*.
//!
//! A silent downgrade from keychain to file is the failure worth designing
//! against: the user believes their credential is in the keychain, and it is
//! actually in their home directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no credential stored; run `theta login` first")]
    NotFound,

    #[error("credential store is unreadable: {0}")]
    Io(#[from] io::Error),

    #[error("stored credential is not valid: {0}")]
    Encoding(#[from] serde_json::Error),

    #[error(
        "refusing to read {path}: it is readable by other users (mode {mode:o}). \
         Fix with `chmod 600 {path}`, then run `theta login` again."
    )]
    Permissive { path: String, mode: u32 },

    #[error(
        "the OS keychain refused the credential: {0}. \
         Use `--credentials <path>` to store it in a file instead."
    )]
    Keychain(String),
}

/// Which backend actually holds the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The OS keychain.
    Keychain,
    /// A file under the user's config directory, mode 0600.
    ///
    /// Weaker than a keychain: anything running as this user can read it. Used
    /// only where no keychain is available, and always reported so the
    /// difference is visible.
    File,
}

impl Backend {
    pub fn describe(self) -> &'static str {
        match self {
            Backend::Keychain => "OS keychain",
            Backend::File => "local file (no OS keychain available)",
        }
    }
}

/// Why the keychain was not used.
///
/// Kept alongside the backend so the reason can be printed once at login. "No
/// OS keychain available" with no explanation is the kind of message a user
/// cannot act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileReason {
    /// The user named a file explicitly with `--credentials`.
    Requested,
    /// No keychain on this platform, or none reachable — a headless Linux box
    /// with no Secret Service, a CI container, an SSH session with no session
    /// bus.
    Unavailable(String),
}

/// The long-lived user identity token, plus the contexts resolved recently.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// Minted once at login, scoped to the user and never to a project
    /// (`04-threat-model-security.md` §2).
    pub identity_token: String,
    pub user_id: String,
    pub control_plane_url: String,
    /// Recently resolved contexts, so an ambiguous hint is not re-asked in one
    /// session (`06-provisioning-identity-flow.md` §4).
    #[serde(default)]
    pub contexts: Vec<CachedContext>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedContext {
    pub org_hint: String,
    pub project_hint: String,
    pub org_id: String,
    pub project_id: String,
    pub environment: String,
    pub address: String,
    /// The scoped session token. Short-lived, and re-minted rather than
    /// refreshed when it expires.
    pub token: String,
    pub expires_at_ms: i64,
}

impl Credentials {
    pub fn context(&self, org_hint: &str, project_hint: &str) -> Option<&CachedContext> {
        self.contexts
            .iter()
            .find(|c| c.org_hint == org_hint && c.project_hint == project_hint)
    }

    /// Most recently resolved context — what an unqualified command acts on.
    pub fn current(&self) -> Option<&CachedContext> {
        self.contexts.last()
    }

    pub fn remember(&mut self, context: CachedContext) {
        self.contexts.retain(|c| {
            !(c.org_hint == context.org_hint && c.project_hint == context.project_hint)
        });
        self.contexts.push(context);
    }

    /// Drop contexts whose tokens have expired. They cannot be refreshed —
    /// re-resolving mints a new one — so keeping them only invites a confusing
    /// failure later.
    pub fn prune(&mut self, now_ms: i64) {
        self.contexts.retain(|c| c.expires_at_ms > now_ms);
    }
}

const SERVICE: &str = "thetabase";
const ACCOUNT: &str = "identity";

/// Reads and writes the local credential.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
    backend: Backend,
    /// Set whenever `backend` is `File`, so the reason can be shown.
    reason: Option<FileReason>,
}

impl CredentialStore {
    /// A store rooted at the user's config directory, using the OS keychain if
    /// one can actually be reached.
    ///
    /// Availability is probed here, once, rather than assumed from the target
    /// platform: a Linux desktop and a Linux CI container compile to the same
    /// binary and differ only at runtime.
    pub fn default_location() -> Self {
        let path = default_path();
        match probe_keychain() {
            Ok(()) => Self {
                path,
                backend: Backend::Keychain,
                reason: None,
            },
            Err(detail) => Self {
                path,
                backend: Backend::File,
                reason: Some(FileReason::Unavailable(detail)),
            },
        }
    }

    /// A store at a path the user named. Always the file backend: naming a file
    /// is a request for that file.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            backend: Backend::File,
            reason: Some(FileReason::Requested),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which backend this store actually uses.
    ///
    /// Reported rather than claimed: saying "keychain" while writing a file is
    /// the silent downgrade this module exists to prevent.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Why the file backend is in use, if it is.
    pub fn file_reason(&self) -> Option<&FileReason> {
        self.reason.as_ref()
    }

    pub fn load(&self) -> Result<Credentials, StoreError> {
        if self.backend == Backend::Keychain {
            match keychain_entry()?.get_password() {
                Ok(json) => return Ok(serde_json::from_str(&json)?),
                Err(keyring_core::Error::NoEntry) => {
                    // Nothing in the keychain. A credential left by an older
                    // build, or by a run where no keychain was reachable, is
                    // moved in rather than ignored — otherwise `theta login`
                    // would appear to have been forgotten.
                    return self.migrate_file_into_keychain();
                }
                Err(e) => return Err(StoreError::Keychain(e.to_string())),
            }
        }

        self.load_file()
    }

    pub fn save(&self, credentials: &Credentials) -> Result<(), StoreError> {
        let json = serde_json::to_string(credentials)?;

        if self.backend == Backend::Keychain {
            keychain_entry()?
                .set_password(&json)
                .map_err(|e| StoreError::Keychain(e.to_string()))?;
            // A copy on disk would quietly outlive the keychain entry and
            // survive `theta logout` on any code path that missed it.
            self.remove_file()?;
            return Ok(());
        }

        self.save_file(credentials)
    }

    pub fn clear(&self) -> Result<(), StoreError> {
        if self.backend == Backend::Keychain {
            match keychain_entry()?.delete_credential() {
                Ok(()) | Err(keyring_core::Error::NoEntry) => {}
                Err(e) => return Err(StoreError::Keychain(e.to_string())),
            }
        }
        // Both, always. A logout that leaves a readable credential behind is
        // not a logout, and the backend may have changed since it was written.
        self.remove_file()
    }

    fn load_file(&self) -> Result<Credentials, StoreError> {
        if !self.path.exists() {
            return Err(StoreError::NotFound);
        }
        self.check_permissions()?;
        Ok(serde_json::from_slice(&fs::read(&self.path)?)?)
    }

    fn save_file(&self, credentials: &Credentials) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Written to a temp file and renamed, so a crash mid-write cannot leave
        // a truncated credential that fails to parse on next use.
        let temp = self.path.with_extension("tmp");
        fs::write(&temp, serde_json::to_vec_pretty(credentials)?)?;
        restrict(&temp)?;
        fs::rename(&temp, &self.path)?;
        Ok(())
    }

    fn remove_file(&self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Move a file-backed credential into the keychain, once.
    ///
    /// An upgrade, not a downgrade — the direction this module guards against
    /// is the other one — but the file is removed so there is exactly one copy
    /// afterwards.
    fn migrate_file_into_keychain(&self) -> Result<Credentials, StoreError> {
        let credentials = self.load_file()?;
        keychain_entry()?
            .set_password(&serde_json::to_string(&credentials)?)
            .map_err(|e| StoreError::Keychain(e.to_string()))?;
        self.remove_file()?;
        Ok(credentials)
    }

    /// Refuse to read a credential file other users can read.
    ///
    /// Reading it anyway would mean carrying on with a credential that may
    /// already be compromised, which is worse than an error the user can fix.
    fn check_permissions(&self) -> Result<(), StoreError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&self.path)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(StoreError::Permissive {
                    path: self.path.display().to_string(),
                    mode,
                });
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> io::Result<()> {
    // Windows inherits the user's profile ACL, which is the equivalent
    // protection. Nothing to tighten here.
    Ok(())
}

fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("thetabase").join("credentials.json")
}

fn keychain_entry() -> Result<keyring_core::Entry, StoreError> {
    keyring_core::Entry::new(SERVICE, ACCOUNT).map_err(|e| StoreError::Keychain(e.to_string()))
}

/// Install the platform credential store, once per process.
///
/// A store already installed — by a test, or by an embedder — is left alone.
fn install_platform_store() -> Result<(), String> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let store = apple_native_keyring_store::keychain::Store::new();

    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new();

    // Linux and the BSDs reach the keychain over D-Bus, and the Secret Service
    // client connects with its own `block_on`. Every `theta` command runs
    // inside a tokio runtime, and a `block_on` there does not fail — it
    // panics, taking the process with it, before the command has done
    // anything. So the connection is made on a plain thread, which has no
    // runtime to collide with.
    //
    // Worth the detour rather than dropped: the machines with no D-Bus session
    // at all — servers, containers, CI — are exactly where `theta eject` runs,
    // and there the probe legitimately fails and the caller falls back to the
    // file store. Panicking is not a way to report that.
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    let store = std::thread::spawn(zbus_secret_service_keyring_store::Store::new)
        .join()
        .map_err(|_| "the platform keychain client panicked while connecting".to_string())?;

    #[cfg(not(any(unix, windows)))]
    return Err("no credential store is available on this platform".to_string());

    #[cfg(any(unix, windows))]
    {
        keyring_core::set_default_store(store.map_err(|e| e.to_string())?);
        Ok(())
    }
}

/// Whether a keychain is actually reachable right now.
///
/// The probe reads an entry that is not expected to exist: a store that answers
/// "no such entry" is working. Nothing is written, so probing cannot leave
/// anything behind on a machine where login is never completed.
fn probe_keychain() -> Result<(), String> {
    install_platform_store()?;

    let entry = keyring_core::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(_) | Err(keyring_core::Error::NoEntry) => Ok(()),
        // Ambiguous means several matching credentials exist, which still means
        // the store answered.
        Err(keyring_core::Error::Ambiguous(_)) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            identity_token: "identity-token".into(),
            user_id: "u_1".into(),
            control_plane_url: "http://localhost:8080".into(),
            contexts: Vec::new(),
        }
    }

    fn context(org: &str, project: &str, expires: i64) -> CachedContext {
        CachedContext {
            org_hint: org.into(),
            project_hint: project.into(),
            org_id: format!("org_{org}"),
            project_id: format!("{org}/{project}"),
            environment: "dev".into(),
            address: "127.0.0.1:7700".into(),
            token: "scoped-token".into(),
            expires_at_ms: expires,
        }
    }

    #[test]
    fn credentials_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("creds.json"));

        let mut creds = credentials();
        creds.remember(context("a", "b", 1_000));
        store.save(&creds).expect("save");

        assert_eq!(store.load().expect("load"), creds);
    }

    #[test]
    fn loading_before_login_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("missing.json"));
        assert!(matches!(store.load(), Err(StoreError::NotFound)));
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("creds.json"));
        store.save(&credentials()).expect("save");

        let mode = fs::metadata(store.path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a credential readable by other users is a leak"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_permissive_credential_file_is_refused_rather_than_used() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("creds.json"));
        store.save(&credentials()).expect("save");

        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).expect("chmod");

        // Carrying on with a credential that may already be compromised is
        // worse than an error the user can act on.
        match store.load() {
            Err(StoreError::Permissive { mode, .. }) => assert_eq!(mode, 0o644),
            other => panic!("a world-readable credential was accepted: {other:?}"),
        }
    }

    // ---- the keychain backend ------------------------------------------
    //
    // Run against `keyring_core`'s in-memory store, so the real save/load/clear
    // paths execute rather than a stand-in. The store is process-global and
    // this crate's entry name is fixed, so these serialize on a mutex.

    static KEYCHAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn keychain_store(dir: &Path) -> (std::sync::MutexGuard<'static, ()>, CredentialStore) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        let guard = KEYCHAIN.lock().unwrap_or_else(|e| e.into_inner());
        INSTALL.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
        });

        let store = CredentialStore {
            path: dir.join("credentials.json"),
            backend: Backend::Keychain,
            reason: None,
        };
        store.clear().expect("start from nothing");
        (guard, store)
    }

    #[test]
    fn a_keychain_credential_is_not_also_written_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_guard, store) = keychain_store(dir.path());

        store.save(&credentials()).expect("save");

        // A copy on disk would outlive the keychain entry and be readable by
        // anything running as this user — the exact protection the keychain buys.
        assert!(
            !store.path().exists(),
            "the keychain backend must not leave a credential file behind"
        );
        assert_eq!(store.load().expect("load"), credentials());
    }

    #[test]
    fn logging_out_clears_the_keychain_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_guard, store) = keychain_store(dir.path());
        store.save(&credentials()).expect("save");

        store.clear().expect("clear");

        assert!(matches!(store.load(), Err(StoreError::NotFound)));
    }

    #[test]
    fn logging_out_also_removes_a_credential_file_left_by_an_earlier_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_guard, store) = keychain_store(dir.path());
        // Written when no keychain was reachable; the backend may differ now.
        store.save_file(&credentials()).expect("save file");

        store.clear().expect("clear");

        assert!(
            !store.path().exists(),
            "a logout that leaves a readable credential behind is not a logout"
        );
    }

    #[test]
    fn a_file_credential_moves_into_the_keychain_the_first_time_it_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_guard, store) = keychain_store(dir.path());
        store.save_file(&credentials()).expect("save file");

        // Ignoring it would make a completed `theta login` look forgotten.
        assert_eq!(store.load().expect("load"), credentials());
        assert!(
            !store.path().exists(),
            "after migrating there must be exactly one copy"
        );
        // And it is now in the keychain, not merely gone from disk.
        assert_eq!(store.load().expect("load again"), credentials());
    }

    #[test]
    fn clearing_a_keychain_that_holds_nothing_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_guard, store) = keychain_store(dir.path());
        assert!(store.clear().is_ok());
    }

    #[test]
    fn naming_a_credentials_path_means_the_file_backend_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("creds.json"));

        assert_eq!(store.backend(), Backend::File);
        assert_eq!(store.file_reason(), Some(&FileReason::Requested));
    }

    #[test]
    fn remembering_a_context_replaces_the_previous_one_for_that_hint() {
        let mut creds = credentials();
        creds.remember(context("a", "b", 1_000));
        creds.remember(context("a", "b", 2_000));

        assert_eq!(
            creds.contexts.len(),
            1,
            "one context per hint, not a growing list"
        );
        assert_eq!(
            creds.context("a", "b").expect("cached").expires_at_ms,
            2_000
        );
    }

    #[test]
    fn the_most_recent_context_is_the_current_one() {
        // Switching companies mid-session is a first-class case, not an edge
        // one (`06-provisioning-identity-flow.md` §4).
        let mut creds = credentials();
        creds.remember(context("a", "b", 1_000));
        creds.remember(context("c", "d", 1_000));

        assert_eq!(creds.current().expect("current").org_hint, "c");
        // ...and the earlier context is still usable, not evicted.
        assert!(creds.context("a", "b").is_some());
    }

    #[test]
    fn expired_contexts_are_pruned_rather_than_failing_later() {
        let mut creds = credentials();
        creds.remember(context("live", "x", 10_000));
        creds.remember(context("dead", "y", 1_000));

        creds.prune(5_000);
        assert!(creds.context("live", "x").is_some());
        assert!(
            creds.context("dead", "y").is_none(),
            "a stale token cannot be refreshed, only re-minted"
        );
    }

    #[test]
    fn the_backend_reports_what_it_actually_uses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::at(dir.path().join("creds.json"));
        store.save(&credentials()).expect("save");

        // Claiming a keychain while writing a file is the silent downgrade this
        // module exists to prevent.
        assert!(store.path().exists(), "it really did write a file");
        assert_eq!(store.backend(), Backend::File);
        assert!(store.backend().describe().contains("no OS keychain"));
    }

    #[test]
    fn clearing_a_store_that_was_never_written_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        CredentialStore::at(dir.path().join("nothing.json"))
            .clear()
            .expect("clear is idempotent");
    }
}
