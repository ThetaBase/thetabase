//! Encryption at rest (SEC-2).
//!
//! `specs/04` §3 requires per-project keys for data at rest, with no key shared
//! across projects even inside one organisation. This is the cipher; the key
//! comes from outside (see [`DataKey::from_env`] and `thetad`'s config).
//!
//! # XChaCha20-Poly1305, not AES-GCM
//!
//! AES-GCM is faster where AES-NI exists and is what the Control Plane uses to
//! wrap its signing keys — a handful of small values, wrapped rarely. This is
//! the opposite: millions of records, appended forever, under one key.
//!
//! That difference is entirely about the nonce. GCM's 96-bit nonce makes random
//! selection a birthday problem — at 2^32 records the collision probability is
//! no longer negligible, and a repeated nonce in GCM does not degrade the
//! ciphertext, it hands over the authentication key. Avoiding that needs either
//! a counter, which a crash-and-truncate cycle can replay, or key rotation on a
//! schedule nobody will remember.
//!
//! XChaCha20's 192-bit nonce makes random selection safe for as many records as
//! a database will ever hold, with no counter to get wrong and no rotation
//! deadline. It is also fast without hardware AES, which matters for the
//! embedded and edge targets M12 wants.
//!
//! # What this protects, and what it does not
//!
//! Stated plainly, because SEC-2 exists because the specification previously
//! described a control that was not there.
//!
//! **Protects:** a stolen segment file, a backup or volume snapshot taken
//! without the key, a decommissioned disk, an over-broad replica.
//!
//! **Does not protect:** anyone who can read the key alongside the data. Where
//! the key lives is therefore the whole question, and it is a deployment
//! decision rather than a code one — see [`DataKey::from_env`].

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::CoreError;

type Result<T> = std::result::Result<T, CoreError>;

/// Nonce length for XChaCha20-Poly1305.
const NONCE_BYTES: usize = 24;

/// The environment variable a deployment injects the key through.
///
/// Preferred over a file beside the data: a key on the same volume as the
/// ciphertext it protects is defeated by anything that copies the volume, which
/// is most of what this defends against. An orchestrator's secret, a mounted
/// tmpfs, or the value the Control Plane sets at launch all keep the key off
/// the disk being protected.
///
/// An environment variable is not a theta, and this is a trade rather than a
/// win: the value is readable through `/proc/<pid>/environ` by anything running
/// as the same user, and can reach a crash dump or a process listing. It is
/// chosen over a file in the data directory because the threats this defends
/// against - a copied volume, a lifted backup, a discarded disk - all capture
/// that file and none of them capture a running process. A deployment that
/// wants the remaining gap closed should keep the instance's user unshared and
/// its core dumps off.
///
/// Declared here, next to the type that reads it. The Control Plane sets this
/// variable and the engine reads it; one copy of the string means one fewer
/// chance for the two of them to disagree about its name.
pub use crate::DATA_KEY_ENV;

/// One project's data-encryption key.
#[derive(Clone)]
pub struct DataKey {
    cipher: XChaCha20Poly1305,
}

// Hand-written so the key cannot reach a log line through `{:?}`, the same
// reasoning as `ProjectKeys`.
impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(<redacted>)")
    }
}

impl DataKey {
    /// Build from 32 raw bytes.
    pub fn from_bytes(key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(key.into()),
        }
    }

    /// Build from 64 hex characters.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(CoreError::Encryption(format!(
                "a data key is 64 hex characters (32 bytes); got {}",
                hex.len()
            )));
        }
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| CoreError::Encryption("a data key must be hexadecimal".into()))?;
        }
        Ok(Self::from_bytes(&key))
    }

    /// From the environment, if a key is configured there.
    ///
    /// `None` means no key, which means no encryption — reported by the caller
    /// at startup rather than assumed, so "is this data encrypted" is answered
    /// by a log line instead of by reading the source.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var(DATA_KEY_ENV) {
            Ok(hex) => Self::from_hex(&hex).map(Some),
            Err(_) => Ok(None),
        }
    }

    /// A fresh key from OS entropy.
    pub fn generate() -> ([u8; 32], Self) {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS must provide entropy");
        let cipher = Self::from_bytes(&key);
        (key, cipher)
    }

    /// Encrypt a record payload. The nonce is prepended.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).expect("the OS must provide entropy");

        let ciphertext = self
            .cipher
            .encrypt(&XNonce::from(nonce), plaintext)
            .map_err(|_| CoreError::Encryption("could not encrypt a record".into()))?;

        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt one.
    ///
    /// A failure here is deliberately not a torn record: the segment's checksum
    /// already passed, so the bytes are the bytes that were written and the
    /// problem is the key. Recovery must not treat that as corruption and
    /// truncate the log — that would destroy the data this exists to protect.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() <= NONCE_BYTES {
            return Err(CoreError::Encryption(
                "an encrypted record is too short to hold its nonce".into(),
            ));
        }
        let (nonce, body) = sealed.split_at(NONCE_BYTES);
        let nonce: [u8; NONCE_BYTES] = nonce.try_into().expect("split at NONCE_BYTES");
        self.cipher
            .decrypt(&XNonce::from(nonce), body)
            .map_err(|_| {
                CoreError::Encryption(
                    "a record did not decrypt; this is the wrong data key for this \
                     project, or the record was altered"
                        .into(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> DataKey {
        DataKey::from_hex(&"7a".repeat(32)).expect("valid key")
    }

    #[test]
    fn a_record_survives_a_round_trip() {
        let k = key();
        let plaintext = br#"{"op":"put","key":"users:1"}"#;
        assert_eq!(k.open(&k.seal(plaintext).unwrap()).unwrap(), plaintext);
    }

    #[test]
    fn the_sealed_form_does_not_contain_the_plaintext() {
        // The property the module exists for. A segment file that still holds
        // readable rows is not encrypted at rest whatever the header says.
        let k = key();
        let plaintext = b"a-very-distinctive-row-value";
        let sealed = k.seal(plaintext).unwrap();
        assert!(
            !sealed
                .windows(plaintext.len())
                .any(|w| w == plaintext.as_slice()),
            "the plaintext is readable in the sealed record"
        );
    }

    #[test]
    fn sealing_the_same_record_twice_gives_different_bytes() {
        // Otherwise identical rows are identifiable as identical, which leaks
        // the shape of the data without decrypting any of it.
        let k = key();
        assert_ne!(k.seal(b"same").unwrap(), k.seal(b"same").unwrap());
    }

    #[test]
    fn another_projects_key_cannot_read_it() {
        // `specs/04` §3: no key shared across projects, so this is the property
        // that makes per-project keys mean anything.
        let sealed = key().seal(b"secret").unwrap();
        let other = DataKey::from_hex(&"9c".repeat(32)).unwrap();
        assert!(other.open(&sealed).is_err());
    }

    #[test]
    fn an_altered_record_is_refused_rather_than_returning_wrong_bytes() {
        let k = key();
        let mut sealed = k.seal(b"an important row").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(k.open(&sealed).is_err());
    }

    #[test]
    fn a_truncated_record_is_refused() {
        let k = key();
        assert!(k.open(&[0u8; NONCE_BYTES]).is_err());
        assert!(k.open(&[]).is_err());
    }

    #[test]
    fn a_key_of_the_wrong_shape_is_refused_at_startup() {
        assert!(DataKey::from_hex("abcd").is_err());
        assert!(DataKey::from_hex(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn the_key_does_not_render_itself() {
        assert_eq!(format!("{:?}", key()), "DataKey(<redacted>)");
    }

    #[test]
    fn a_generated_key_round_trips_through_its_bytes() {
        let (bytes, cipher) = DataKey::generate();
        let sealed = cipher.seal(b"row").unwrap();
        assert_eq!(DataKey::from_bytes(&bytes).open(&sealed).unwrap(), b"row");
    }
}
