//! Per-project signing keys.
//!
//! The Control Plane holds a [`SigningKeyset`] — every project's private key.
//! A `thetad` instance holds a [`PublicKeyset`] containing *only* its own
//! project's public key, which is what makes cross-project forgery impossible
//! rather than merely disallowed.

use std::collections::BTreeMap;

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Generate a signing key from OS entropy.
///
/// Taken straight from the OS rather than through an RNG abstraction, so there
/// is no seedable generator anywhere on this path — a deterministically seeded
/// RNG here would make every project's key predictable, and the indirection is
/// exactly what hides that in review.
fn generate_key() -> SigningKey {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("the OS must provide entropy for key generation");
    SigningKey::from_bytes(&secret)
}

/// How a key renders in a `Debug`: present or absent, never its value.
fn redacted(present: bool) -> &'static str {
    if present {
        "<redacted>"
    } else {
        "none"
    }
}

/// Names one key within a project, so a project can rotate without a flag day:
/// the new key is published, tokens start being signed with it, and the old key
/// stays verifiable until every token signed with it has expired.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyId(pub String);

impl KeyId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One project's keys, as the Control Plane holds them.
#[derive(Clone)]
pub struct ProjectKeys {
    pub project_id: String,
    pub active: KeyId,
    keys: BTreeMap<KeyId, SigningKey>,
    /// The project's data-encryption key (SEC-2), once it has one.
    ///
    /// Held here so it rides the existing wrap-and-persist path rather than
    /// getting a store of its own: one wrapped blob per project means one
    /// thing to write, and no way to persist the signing keys but not this.
    /// A project whose signing keys survived a restart and whose data key did
    /// not is a project whose data is gone.
    ///
    /// Private, and reachable only through [`ProjectKeys::data_key`].
    data_key: Option<[u8; 32]>,
}

// A manual impl so a private key cannot reach a log line through `{:?}`.
impl std::fmt::Debug for ProjectKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectKeys")
            .field("project_id", &self.project_id)
            .field("active", &self.active)
            .field("keys", &format!("<{} redacted>", self.keys.len()))
            .field("data_key", &redacted(self.data_key.is_some()))
            .finish()
    }
}

impl ProjectKeys {
    /// Generate a fresh keypair for a project.
    pub fn generate(project_id: impl Into<String>) -> Self {
        let project_id = project_id.into();
        let active = KeyId::new(format!("{project_id}-k1"));
        let mut keys = BTreeMap::new();
        keys.insert(active.clone(), generate_key());
        Self {
            project_id,
            active,
            keys,
            // Not generated here. A project gets a data key when someone
            // decides it should be encrypted, and generating one unasked would
            // make `data_key()` return `Some` for every project — turning "is
            // this project encrypted" from a fact into a guess.
            data_key: None,
        }
    }

    /// The project's data-encryption key, if it has one.
    pub fn data_key(&self) -> Option<&[u8; 32]> {
        self.data_key.as_ref()
    }

    /// Generate a data key if the project has none, and return it either way.
    ///
    /// Idempotent on purpose: this sits on the provisioning path, which retries.
    /// A second call that minted a second key would leave the instance holding
    /// a key that does not open the data written under the first one.
    pub fn ensure_data_key(&mut self) -> &[u8; 32] {
        self.data_key.get_or_insert_with(|| {
            let mut key = [0u8; 32];
            getrandom::fill(&mut key).expect("the OS must provide entropy for key generation");
            key
        })
    }

    /// Add a new key and make it active. The previous key stays verifiable, so
    /// tokens already in flight keep working until they expire.
    pub fn rotate(&mut self) -> KeyId {
        let next = KeyId::new(format!("{}-k{}", self.project_id, self.keys.len() + 1));
        self.keys.insert(next.clone(), generate_key());
        self.active = next.clone();
        next
    }

    /// Forget a key entirely. Every token signed with it stops verifying
    /// immediately — the emergency path for a leaked signing key, distinct from
    /// the ordinary rotation above.
    pub fn revoke_key(&mut self, id: &KeyId) {
        self.keys.remove(id);
    }

    pub fn signing_key(&self, id: &KeyId) -> Option<&SigningKey> {
        self.keys.get(id)
    }

    pub fn active_signing_key(&self) -> &SigningKey {
        self.keys
            .get(&self.active)
            .expect("the active key is always present")
    }

    /// The private key material, for persisting it.
    ///
    /// The Control Plane cannot survive a restart without writing these down
    /// somewhere, and it cannot write down what it cannot get out. Deliberately
    /// awkward to reach and deliberately named: anything holding an
    /// [`ExportedKeys`] is holding the ability to mint tokens for this project,
    /// and should be encrypting it before it touches a disk.
    ///
    /// Returned as raw bytes rather than as a `Serialize` type so that no
    /// derive can accidentally put a private key into a JSON body or a log
    /// line — the same reasoning as this type's hand-written `Debug`.
    pub fn export(&self) -> ExportedKeys {
        ExportedKeys {
            project_id: self.project_id.clone(),
            active: self.active.clone(),
            keys: self
                .keys
                .iter()
                .map(|(id, key)| (id.clone(), key.to_bytes()))
                .collect(),
            data_key: self.data_key,
        }
    }

    /// Rebuild from [`ExportedKeys`].
    ///
    /// `None` when the export names an active key it does not contain, which
    /// would produce a keyset that panics on first use — `active_signing_key`
    /// is documented to always find one. Better to refuse a corrupt row than to
    /// start and fail later on a request.
    pub fn import(exported: ExportedKeys) -> Option<Self> {
        if !exported.keys.contains_key(&exported.active) {
            return None;
        }
        Some(Self {
            project_id: exported.project_id,
            active: exported.active,
            keys: exported
                .keys
                .into_iter()
                .map(|(id, bytes)| (id, SigningKey::from_bytes(&bytes)))
                .collect(),
            data_key: exported.data_key,
        })
    }

    /// What this project's `thetad` needs in order to verify — and nothing more.
    pub fn public_keyset(&self) -> PublicKeyset {
        PublicKeyset {
            project_id: self.project_id.clone(),
            keys: self
                .keys
                .iter()
                .map(|(id, key)| (id.clone(), key.verifying_key()))
                .collect(),
        }
    }
}

/// One project's private key material, on its way to or from storage.
///
/// Carries no `Serialize`, no `Clone` and a redacting `Debug`, all on purpose:
/// the only thing that should ever happen to this is being encrypted and
/// written down, and every convenience trait is another way for it to end up
/// somewhere it should not be.
pub struct ExportedKeys {
    pub project_id: String,
    pub active: KeyId,
    pub keys: BTreeMap<KeyId, [u8; 32]>,
    /// The project's data-encryption key (SEC-2), if it has one.
    ///
    /// Kept with the signing keys rather than in a table of its own so there is
    /// one wrapped blob per project and one thing to persist. Two stores means
    /// two chances to write only one of them, and a project whose signing keys
    /// survived a restart but whose data key did not is a project whose data is
    /// unreadable.
    ///
    /// `Option` because projects created before SEC-2 do not have one, and
    /// because generating one is a decision the caller makes rather than a
    /// side effect of loading.
    pub data_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for ExportedKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportedKeys")
            .field("project_id", &self.project_id)
            .field("active", &self.active)
            .field("keys", &format!("<{} redacted>", self.keys.len()))
            .field("data_key", &redacted(self.data_key.is_some()))
            .finish()
    }
}

/// Every project's signing keys. Control Plane only.
#[derive(Debug, Default, Clone)]
pub struct SigningKeyset {
    projects: BTreeMap<String, ProjectKeys>,
}

impl SigningKeyset {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keys for `project_id`, generating them on first use.
    pub fn ensure(&mut self, project_id: &str) -> &ProjectKeys {
        self.projects
            .entry(project_id.to_string())
            .or_insert_with(|| ProjectKeys::generate(project_id))
    }

    /// The same, mutably, for callers that need to add material to a project's
    /// keys rather than only read them — generating a data key, for instance.
    pub fn ensure_mut(&mut self, project_id: &str) -> &mut ProjectKeys {
        self.projects
            .entry(project_id.to_string())
            .or_insert_with(|| ProjectKeys::generate(project_id))
    }

    /// Adopt keys that already exist — restored from storage after a restart,
    /// rather than generated.
    ///
    /// Keyed by the project the keys name, so a caller cannot file one
    /// project's keys under another's id and quietly make every token for both
    /// verify against the wrong half.
    pub fn insert(&mut self, keys: ProjectKeys) {
        self.projects.insert(keys.project_id.clone(), keys);
    }

    pub fn get(&self, project_id: &str) -> Option<&ProjectKeys> {
        self.projects.get(project_id)
    }

    pub fn get_mut(&mut self, project_id: &str) -> Option<&mut ProjectKeys> {
        self.projects.get_mut(project_id)
    }

    pub fn projects(&self) -> impl Iterator<Item = &str> {
        self.projects.keys().map(|s| s.as_str())
    }

    /// Drop a project's keys, for a project that is being deleted.
    ///
    /// This is what makes deletion final rather than cosmetic. Every session
    /// token ever minted for the project was signed with these keys, so
    /// dropping them makes all of them unverifiable at once -- with no
    /// revocation list to maintain, nothing to compact, and no window in which
    /// an outstanding token still works.
    ///
    /// It also closes the reuse hole. A project id is a name a customer chose
    /// and may choose again, and a recreated project must not accept tokens
    /// minted for the one it replaced. Deleting the key material means the
    /// recreated project generates fresh keys, so it cannot.
    ///
    /// Returns what was dropped, so a caller can tell "deleted" from "was
    /// never there" rather than having to guess.
    pub fn forget(&mut self, project_id: &str) -> Option<ProjectKeys> {
        self.projects.remove(project_id)
    }
}

/// The public keys one `thetad` instance holds.
///
/// Scoped to a single project by construction. There is no constructor that
/// produces a keyset spanning two projects, so an instance cannot be
/// accidentally configured to accept another project's tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicKeyset {
    pub project_id: String,
    keys: BTreeMap<KeyId, VerifyingKey>,
}

impl PublicKeyset {
    pub fn verifying_key(&self, id: &KeyId) -> Option<&VerifyingKey> {
        self.keys.get(id)
    }

    pub fn key_ids(&self) -> impl Iterator<Item = &KeyId> {
        self.keys.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Serializable form, for shipping a keyset to an instance at startup.
    pub fn to_wire(&self) -> WireKeyset {
        WireKeyset {
            project_id: self.project_id.clone(),
            keys: self
                .keys
                .iter()
                .map(|(id, key)| (id.clone(), key.to_bytes().to_vec()))
                .collect(),
        }
    }

    pub fn from_wire(wire: &WireKeyset) -> Result<Self, KeysetError> {
        let mut keys = BTreeMap::new();
        for (id, bytes) in &wire.keys {
            let bytes: [u8; 32] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| KeysetError::Malformed {
                        key: id.clone(),
                        detail: format!("expected 32 bytes, got {}", bytes.len()),
                    })?;
            let key = VerifyingKey::from_bytes(&bytes).map_err(|e| KeysetError::Malformed {
                key: id.clone(),
                detail: e.to_string(),
            })?;
            keys.insert(id.clone(), key);
        }
        Ok(Self {
            project_id: wire.project_id.clone(),
            keys,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireKeyset {
    pub project_id: String,
    pub keys: Vec<(KeyId, Vec<u8>)>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum KeysetError {
    #[error("key `{key}` is malformed: {detail}")]
    Malformed { key: KeyId, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_project_gets_a_distinct_key() {
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-b");
        assert_ne!(
            a.active_signing_key().to_bytes(),
            b.active_signing_key().to_bytes(),
            "two projects sharing a key would make cross-project forgery trivial"
        );
    }

    #[test]
    fn generating_twice_for_one_project_gives_different_keys() {
        // Keys come from the OS RNG, so this catches a stubbed or seeded
        // generator sneaking in.
        let a = ProjectKeys::generate("project-a");
        let b = ProjectKeys::generate("project-a");
        assert_ne!(
            a.active_signing_key().to_bytes(),
            b.active_signing_key().to_bytes()
        );
    }

    #[test]
    fn rotation_keeps_the_old_key_verifiable() {
        let mut keys = ProjectKeys::generate("p");
        let old = keys.active.clone();
        let new = keys.rotate();

        assert_ne!(old, new);
        assert_eq!(keys.active, new);
        // Tokens already signed with the old key must keep working until they
        // expire, or every rotation is an outage.
        assert!(keys.signing_key(&old).is_some());
        assert_eq!(keys.public_keyset().key_ids().count(), 2);
    }

    #[test]
    fn revoking_a_key_stops_it_verifying_immediately() {
        let mut keys = ProjectKeys::generate("p");
        let leaked = keys.active.clone();
        keys.rotate();
        keys.revoke_key(&leaked);

        // The emergency path: a leaked signing key must not stay valid for the
        // lifetime of the tokens it signed.
        assert!(keys.signing_key(&leaked).is_none());
        assert!(keys.public_keyset().verifying_key(&leaked).is_none());
    }

    #[test]
    fn a_public_keyset_carries_no_private_material() {
        let keys = ProjectKeys::generate("p");
        let public = keys.public_keyset();
        let wire = public.to_wire();

        // Every published byte must be a public key, and a public key is not
        // enough to sign with.
        for (_, bytes) in &wire.keys {
            assert_eq!(bytes.len(), 32);
            assert_ne!(
                bytes.as_slice(),
                keys.active_signing_key().to_bytes().as_slice(),
                "a private key reached the published keyset"
            );
        }
    }

    #[test]
    fn a_keyset_round_trips_through_the_wire() {
        let keys = ProjectKeys::generate("p");
        let public = keys.public_keyset();
        assert_eq!(PublicKeyset::from_wire(&public.to_wire()), Ok(public));
    }

    #[test]
    fn a_malformed_keyset_is_refused_rather_than_partially_loaded() {
        let wire = WireKeyset {
            project_id: "p".into(),
            keys: vec![(KeyId::new("k1"), vec![0u8; 7])],
        };
        assert!(matches!(
            PublicKeyset::from_wire(&wire),
            Err(KeysetError::Malformed { .. })
        ));
    }

    #[test]
    fn debug_output_never_contains_private_key_material() {
        let keys = ProjectKeys::generate("p");
        let rendered = format!("{keys:?}");
        // A private key reaching a log line is a breach, and `{:?}` on a config
        // struct is how it usually happens.
        assert!(rendered.contains("redacted"), "got: {rendered}");
        let secret = keys.active_signing_key().to_bytes();
        assert!(!rendered.contains(&format!("{}", secret[0])) || !rendered.contains("SigningKey"));
    }

    #[test]
    fn the_keyset_generates_keys_on_first_use() {
        let mut keyset = SigningKeyset::new();
        assert!(keyset.get("new-project").is_none());
        keyset.ensure("new-project");
        assert!(keyset.get("new-project").is_some());
        assert_eq!(keyset.projects().count(), 1);
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn keys_survive_an_export_and_import() {
        let mut original = ProjectKeys::generate("org/project");
        original.rotate();
        let active = original.active.clone();

        let restored = ProjectKeys::import(original.export()).expect("valid export");

        assert_eq!(restored.project_id, "org/project");
        assert_eq!(restored.active, active);
        // The public halves match, which is what makes a token minted before a
        // restart still verify after one.
        assert_eq!(restored.public_keyset(), original.public_keyset());
    }

    #[test]
    fn an_export_missing_its_active_key_is_refused() {
        // `active_signing_key` is documented to always find one, so a keyset
        // that cannot is a panic waiting for the first request. Refuse the row
        // instead of starting and failing later.
        let keys = ProjectKeys::generate("org/project");
        let mut exported = keys.export();
        exported.keys.remove(&exported.active.clone());
        assert!(ProjectKeys::import(exported).is_none());
    }

    #[test]
    fn exported_keys_do_not_render_their_private_material() {
        let keys = ProjectKeys::generate("org/project");
        let rendered = format!("{:?}", keys.export());
        assert!(rendered.contains("redacted"), "got: {rendered}");
        assert!(rendered.contains("org/project"));
    }
}
