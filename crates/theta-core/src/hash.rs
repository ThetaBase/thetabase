//! Content addressing. Every log entry is named by the BLAKE3 hash of its
//! canonical encoding, which is what makes the log a verifiable Merkle DAG
//! rather than an ordered list we merely promise not to rewrite.

use std::fmt;

use serde::{Deserialize, Serialize};

pub const HASH_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentHash(#[serde(with = "hex_bytes")] pub [u8; HASH_LEN]);

impl ContentHash {
    /// The hash every genesis entry chains onto.
    pub const ZERO: ContentHash = ContentHash([0u8; HASH_LEN]);

    pub fn of(bytes: &[u8]) -> Self {
        ContentHash(*blake3::hash(bytes).as_bytes())
    }

    /// Hash of a sequence of fields, length-prefixed so that
    /// `["ab", "c"]` and `["a", "bc"]` cannot collide.
    pub fn of_fields(fields: &[&[u8]]) -> Self {
        let mut hasher = blake3::Hasher::new();
        for f in fields {
            hasher.update(&(f.len() as u64).to_le_bytes());
            hasher.update(f);
        }
        ContentHash(*hasher.finalize().as_bytes())
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; HASH_LEN]
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != HASH_LEN * 2 {
            return None;
        }
        let mut out = [0u8; HASH_LEN];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(ContentHash(out))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{ContentHash, HASH_LEN};

    pub fn serialize<S: Serializer>(v: &[u8; HASH_LEN], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&ContentHash(*v).to_hex())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; HASH_LEN], D::Error> {
        let s = String::deserialize(d)?;
        ContentHash::from_hex(&s)
            .map(|h| h.0)
            .ok_or_else(|| serde::de::Error::custom("invalid content hash"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_hashing_is_unambiguous() {
        assert_ne!(
            ContentHash::of_fields(&[b"ab", b"c"]),
            ContentHash::of_fields(&[b"a", b"bc"])
        );
    }

    #[test]
    fn hex_roundtrips() {
        let h = ContentHash::of(b"thetabase");
        assert_eq!(ContentHash::from_hex(&h.to_hex()), Some(h));
    }
}
