//! Entries signed by the identity that wrote them (ROADMAP-V3 M20).
//!
//! # What this changes, precisely
//!
//! Today `author` is something the *server* records. A client says who it is by
//! presenting a token, the server verifies the token and writes the author down.
//! The record is therefore exactly as good as the server: an operator with disk
//! access can write an entry attributed to anyone.
//!
//! A signature makes the author a claim the log can check. The entry hash is
//! signed by a key the server never holds, so an entry attributed to a session
//! either carries that session's signature or does not.
//!
//! # What it does not change, which matters more
//!
//! **It protects a customer from us. It does not protect them from their own
//! agent.**
//!
//! A signature proves the holder of the session key produced the entry. It does
//! not establish which human or which model was behind the key, and a stolen
//! session key signs exactly as well as an honest one. So this upgrades "the
//! server says this session wrote it" to "the session key holder wrote it, and
//! the server could not have forged it" — a real upgrade, and a narrower one
//! than "signed commits" tends to suggest.
//!
//! It pairs with `04-threat-model-security.md` §6: an agent's *self-report* is
//! unverifiable, and its *signature* is not. Together they say "this key holder
//! made this change and claimed to be that agent while doing it", which is what
//! makes an attribution query evidence rather than a filter.
//!
//! # The signature covers what the entry says, not where it sits
//!
//! `LogEntry::hash` commits to `prev_hash`, which is what makes the chain
//! tamper-evident. Signing *that* would sign a chain position, and a chain
//! position changes the moment an entry is legitimately re-appended — which is
//! what synchronising two independent logs does to every entry it carries
//! (M22). Signed commits and sync would have been mutually exclusive, and the
//! incompatibility would have surfaced only when somebody used both.
//!
//! So signatures cover `LogEntry::content_hash`: the op, the author, the commit
//! id, the branch and the timestamp. A signature therefore survives an entry
//! being carried to another log, which is the point.
//!
//! Reordering is still caught. Moving a signed entry changes its `prev_hash`,
//! and the *chain* notices even though the signature does not — the two
//! mechanisms answer different questions and neither has to answer both.
//!
//! # Why the signature is beside the entry rather than inside it
//!
//! A signature cannot be inside the thing it signs. Adding a signature field to
//! `LogEntry` would mean either signing an entry whose hash does not include the
//! signature — leaving the field free to be swapped — or a circular definition.
//!
//! So signatures live in a sidecar keyed by entry hash. That has a consequence
//! worth stating rather than discovering: **a missing signature is not a broken
//! chain.** Stripping one is undetectable from the log alone, which is why
//! [`SignatureBook::verify_all`] takes the set of entries that are *expected* to
//! be signed rather than checking only what it happens to hold.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use theta_core::hash::ContentHash;
use theta_core::log::{Author, LogEntry};

/// A signature over one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntrySignature {
    /// The entry's *content* hash — what it says, not where it sits.
    ///
    /// See the module docs: signing a chain position would make a signature die
    /// on any legitimate re-append, including the one sync performs.
    pub entry_hash: ContentHash,
    /// Which key. A session id, so the verifier knows which public key to ask
    /// for — and so a signature by the wrong session is visible rather than
    /// merely invalid.
    pub key_id: String,
    /// Raw ed25519 signature bytes.
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SigningError {
    #[error("no public key registered for `{key_id}`")]
    UnknownKey { key_id: String },

    #[error("the signature for entry {entry_hash} is not valid for key `{key_id}`")]
    BadSignature { entry_hash: String, key_id: String },

    #[error(
        "entry {entry_hash} is attributed to `{author_key}` and signed by `{key_id}`; \
         a signature by another key does not make the attribution true"
    )]
    WrongSigner {
        entry_hash: String,
        author_key: String,
        key_id: String,
    },

    #[error(
        "entry {entry_hash} is attributed to a session and carries no signature. \
         The log alone cannot tell a stripped signature from one never made, \
         which is why this is checked against what was expected rather than \
         against what is present."
    )]
    Missing { entry_hash: String },
}

/// Public keys, by session.
#[derive(Debug, Clone, Default)]
pub struct KeyRegistry {
    keys: BTreeMap<String, VerifyingKey>,
}

impl KeyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, key_id: impl Into<String>, key: VerifyingKey) {
        self.keys.insert(key_id.into(), key);
    }

    /// Register a key given as 32 hex-encoded bytes.
    ///
    /// The shape a key arrives in from a token. Kept here rather than at the
    /// call site so that the one place which turns bytes into a `VerifyingKey`
    /// is the one place that knows what a valid key is — a caller that decoded
    /// it itself would be free to accept 31 bytes and pad.
    pub fn register_hex(&mut self, key_id: &str, hex: &str) -> Result<(), SigningError> {
        let invalid = || SigningError::UnknownKey {
            key_id: key_id.to_string(),
        };
        if hex.len() != 64 {
            return Err(invalid());
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2).ok_or_else(invalid)?, 16)
                .map_err(|_| invalid())?;
        }
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| invalid())?;
        self.register(key_id, key);
        Ok(())
    }

    pub fn get(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(key_id)
    }
}

/// Signatures, by entry hash.
#[derive(Debug, Clone, Default)]
pub struct SignatureBook {
    by_entry: BTreeMap<ContentHash, EntrySignature>,
}

impl SignatureBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_entry.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_entry.is_empty()
    }

    pub fn get(&self, entry_hash: &ContentHash) -> Option<&EntrySignature> {
        self.by_entry.get(entry_hash)
    }

    pub fn insert(&mut self, signature: EntrySignature) {
        self.by_entry.insert(signature.entry_hash, signature);
    }

    /// Sign an entry.
    ///
    /// Takes the entry rather than a hash so the signature cannot be produced
    /// over something that is not an entry — a signing function that accepts an
    /// arbitrary 32 bytes is one an attacker can ask to sign a future entry's
    /// hash.
    pub fn sign(&mut self, entry: &LogEntry, key_id: &str, key: &SigningKey) -> &EntrySignature {
        let entry_hash = entry.content_hash();
        let signature = EntrySignature {
            entry_hash,
            key_id: key_id.to_string(),
            signature: key.sign(&entry_hash.0).to_bytes().to_vec(),
        };
        self.by_entry.insert(entry_hash, signature);
        self.by_entry.get(&entry_hash).expect("just inserted")
    }

    /// Accept a signature that arrived with an entry.
    ///
    /// Verifies **before** storing, and stores only on success. The obvious
    /// shape — insert, then call `verify`, then remove if it failed — leaves a
    /// window in which the book holds a signature nothing has checked, and a
    /// book that can hold one is a book whose contents mean less than they look
    /// like they mean.
    ///
    /// The signature is over the entry's content hash, which the caller
    /// computed from an entry it built. If the server had assembled the entry
    /// instead, this would be checking a signature against something the signer
    /// never saw.
    pub fn accept(
        &mut self,
        entry: &LogEntry,
        key_id: &str,
        signature: Vec<u8>,
        keys: &KeyRegistry,
    ) -> Result<(), SigningError> {
        let entry_hash = entry.content_hash();
        let candidate = EntrySignature {
            entry_hash,
            key_id: key_id.to_string(),
            signature,
        };

        // Checked against a temporary book, so a failed verification cannot
        // leave anything behind.
        let mut probe = SignatureBook::new();
        probe.insert(candidate.clone());
        probe.verify(entry, keys)?;

        self.by_entry.insert(entry_hash, candidate);
        Ok(())
    }

    /// Check one entry's signature.
    pub fn verify(&self, entry: &LogEntry, keys: &KeyRegistry) -> Result<(), SigningError> {
        let entry_hash = entry.content_hash();
        let held = self
            .by_entry
            .get(&entry_hash)
            .ok_or_else(|| SigningError::Missing {
                entry_hash: entry_hash.to_hex(),
            })?;

        // The signer must be the session the entry is attributed to. Without
        // this, any registered key could sign for any author and the signature
        // would verify — which would make the whole mechanism a check that
        // *somebody* signed, and nothing about who.
        if let Some(session) = entry.author.session_id() {
            if held.key_id != session {
                return Err(SigningError::WrongSigner {
                    entry_hash: entry_hash.to_hex(),
                    author_key: session.to_string(),
                    key_id: held.key_id.clone(),
                });
            }
        }

        let key = keys
            .get(&held.key_id)
            .ok_or_else(|| SigningError::UnknownKey {
                key_id: held.key_id.clone(),
            })?;

        let signature =
            Signature::from_slice(&held.signature).map_err(|_| SigningError::BadSignature {
                entry_hash: entry_hash.to_hex(),
                key_id: held.key_id.clone(),
            })?;

        key.verify(&entry_hash.0, &signature)
            .map_err(|_| SigningError::BadSignature {
                entry_hash: entry_hash.to_hex(),
                key_id: held.key_id.clone(),
            })
    }

    /// Check every entry that should have been signed.
    ///
    /// Driven by the entries, not by the book. Iterating the book would verify
    /// only the signatures that are present, which is exactly what an attacker
    /// who stripped one would want: nothing missing means nothing checked.
    ///
    /// `should_be_signed` decides which entries are in scope. `System` entries
    /// are written by the instance itself and have no session key, so a
    /// deployment that signs agent writes must not fail on them.
    pub fn verify_all(
        &self,
        entries: &[LogEntry],
        keys: &KeyRegistry,
        should_be_signed: impl Fn(&Author) -> bool,
    ) -> Result<usize, SigningError> {
        let mut checked = 0;
        for entry in entries {
            if !should_be_signed(&entry.author) {
                continue;
            }
            self.verify(entry, keys)?;
            checked += 1;
        }
        Ok(checked)
    }
}

/// The usual scope: everything an agent wrote.
///
/// A named function rather than a closure at each call site, so "which entries
/// must be signed" is one decision in one place instead of a predicate somebody
/// re-derives and gets subtly wrong.
pub fn agent_entries(author: &Author) -> bool {
    author.is_agent()
}

#[cfg(test)]
mod tests {
    use super::*;
    use theta_core::log::{CommitId, OpType};
    use theta_core::{BranchId, Value};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn a_signature_filed_under_a_key_that_is_not_the_authors_is_refused() {
        // The guard that makes this a check on *who* signed rather than a check
        // that somebody did. It is unreachable through `Engine::append_signed`,
        // which derives the key id from the author and so cannot produce a
        // mismatch — a plant deleting it there changed nothing, because the
        // property is enforced by construction on that path.
        //
        // It is reachable here, through `insert`, which is how a signature would
        // arrive from anywhere other than a live request. Tested where it can
        // actually be exercised rather than where it merely reads as relevant.
        let mallory = key(7);
        let mut keys = KeyRegistry::new();
        keys.register("mallory", mallory.verifying_key());
        keys.register("victim", key(1).verifying_key());

        let entry = entry(1, Author::agent("victim", "alice"));
        let mut book = SignatureBook::new();
        book.insert(EntrySignature {
            entry_hash: entry.content_hash(),
            // Mallory's key, genuinely signing the victim's entry.
            key_id: "mallory".into(),
            signature: mallory.sign(&entry.content_hash().0).to_bytes().to_vec(),
        });

        let err = book
            .verify(&entry, &keys)
            .expect_err("a signature by one session verified for another's entry");
        assert!(
            matches!(err, SigningError::WrongSigner { .. }),
            "the mismatch was reported as something other than the wrong signer: {err}"
        );
    }

    #[test]
    fn a_signature_that_does_not_verify_is_not_left_in_the_book() {
        // `accept` verifies before storing. The obvious shape — insert, verify,
        // remove on failure — leaves the book holding, however briefly, a
        // signature nothing has checked; and a book that can hold one is a book
        // whose contents mean less than they look like they mean.
        //
        // A plant that stored first and verified after returned the same error
        // and passed every test that only looked at the return value.
        let mut keys = KeyRegistry::new();
        keys.register("s1", key(1).verifying_key());

        let entry = entry(1, Author::agent("s1", "alice"));
        let mut book = SignatureBook::new();

        let forged = key(9).sign(&entry.content_hash().0).to_bytes().to_vec();
        assert!(book.accept(&entry, "s1", forged, &keys).is_err());

        assert!(
            book.is_empty(),
            "a signature that failed verification was left in the book"
        );
        assert!(
            book.get(&entry.content_hash()).is_none(),
            "the rejected signature is retrievable"
        );
    }

    #[test]
    fn a_signature_that_verifies_is_stored_and_retrievable() {
        // So the test above is not passing merely because `accept` never stores.
        let signer = key(1);
        let mut keys = KeyRegistry::new();
        keys.register("s1", signer.verifying_key());

        let entry = entry(1, Author::agent("s1", "alice"));
        let mut book = SignatureBook::new();
        let good = signer.sign(&entry.content_hash().0).to_bytes().to_vec();

        book.accept(&entry, "s1", good, &keys).expect("accept");
        assert_eq!(book.len(), 1);
        assert!(book.get(&entry.content_hash()).is_some());
    }

    fn entry(commit: u64, author: Author) -> LogEntry {
        LogEntry {
            prev_hash: ContentHash([commit as u8; 32]),
            commit_id: CommitId(commit),
            branch_id: BranchId(0),
            op: OpType::Put {
                key: format!("a:{commit}"),
                value: Value::Int(commit as i64),
            },
            author,
            timestamp_ms: commit as i64,
        }
    }

    fn registry(pairs: &[(&str, &SigningKey)]) -> KeyRegistry {
        let mut keys = KeyRegistry::new();
        for (id, key) in pairs {
            keys.register(*id, key.verifying_key());
        }
        keys
    }

    #[test]
    fn a_signed_entry_verifies() {
        let signing = key(1);
        let keys = registry(&[("sess_a", &signing)]);
        let entry = entry(1, Author::agent("sess_a", "alice"));

        let mut book = SignatureBook::new();
        book.sign(&entry, "sess_a", &signing);
        assert!(book.verify(&entry, &keys).is_ok());
    }

    #[test]
    fn an_entry_edited_after_signing_no_longer_verifies() {
        // The property that makes the author a claim the log can check rather
        // than one it records: the signature is over the entry hash, so any
        // change to the entry invalidates it.
        let signing = key(1);
        let keys = registry(&[("sess_a", &signing)]);
        let original = entry(1, Author::agent("sess_a", "alice"));

        let mut book = SignatureBook::new();
        book.sign(&original, "sess_a", &signing);

        let edited = LogEntry {
            op: OpType::Put {
                key: "a:1".into(),
                value: Value::Int(999),
            },
            ..original.clone()
        };

        // The edited entry hashes differently, so it has no signature at all —
        // which is the honest report rather than a bad-signature error.
        assert!(matches!(
            book.verify(&edited, &keys),
            Err(SigningError::Missing { .. })
        ));
    }

    #[test]
    fn moving_a_signed_entry_in_the_chain_does_not_invalidate_its_signature() {
        // Deliberate, and the reason sync works at all: a signature covers what
        // the entry says, not where it sits. Re-appending an entry to another
        // log gives it a new `prev_hash` and must not destroy the proof of who
        // wrote it.
        //
        // Reordering is still caught — by the *chain*, which notices the
        // changed `prev_hash`. The two mechanisms answer different questions and
        // neither has to answer both.
        let signing = key(1);
        let keys = registry(&[("sess_a", &signing)]);
        let original = entry(1, Author::agent("sess_a", "alice"));

        let mut book = SignatureBook::new();
        book.sign(&original, "sess_a", &signing);

        let moved = LogEntry {
            prev_hash: ContentHash([99; 32]),
            ..original.clone()
        };
        assert_ne!(
            original.hash(),
            moved.hash(),
            "it is at a different position"
        );
        assert!(
            book.verify(&moved, &keys).is_ok(),
            "the signature must survive the entry being carried to another log"
        );
    }

    #[test]
    fn a_signature_by_another_session_does_not_make_the_attribution_true() {
        // Without this check, any registered key could sign for any author and
        // the signature would verify — making the mechanism a check that
        // *somebody* signed and nothing about who.
        let mallory = key(9);
        let keys = registry(&[("sess_mallory", &mallory)]);
        let entry = entry(1, Author::agent("sess_alice", "alice"));

        let mut book = SignatureBook::new();
        book.sign(&entry, "sess_mallory", &mallory);

        match book.verify(&entry, &keys) {
            Err(SigningError::WrongSigner {
                author_key, key_id, ..
            }) => {
                assert_eq!(author_key, "sess_alice");
                assert_eq!(key_id, "sess_mallory");
            }
            other => panic!("expected a wrong-signer failure, got {other:?}"),
        }
    }

    #[test]
    fn a_forged_signature_from_the_right_session_id_still_fails() {
        // Naming the right session is not the same as holding its key.
        let alice = key(1);
        let mallory = key(9);
        let keys = registry(&[("sess_alice", &alice)]);
        let entry = entry(1, Author::agent("sess_alice", "alice"));

        // Mallory signs with her own key but labels it as Alice's session.
        let mut book = SignatureBook::new();
        book.insert(EntrySignature {
            entry_hash: entry.content_hash(),
            key_id: "sess_alice".into(),
            signature: mallory.sign(&entry.content_hash().0).to_bytes().to_vec(),
        });

        assert!(matches!(
            book.verify(&entry, &keys),
            Err(SigningError::BadSignature { .. })
        ));
    }

    #[test]
    fn a_stripped_signature_is_caught_because_verification_iterates_the_entries() {
        // The failure mode of a sidecar. Iterating the *book* would verify only
        // what is present, which is exactly what an attacker who removed one
        // would want: nothing missing means nothing checked.
        let signing = key(1);
        let keys = registry(&[("sess_a", &signing)]);
        let entries: Vec<LogEntry> = (1..=3)
            .map(|i| entry(i, Author::agent("sess_a", "alice")))
            .collect();

        let mut book = SignatureBook::new();
        for e in &entries {
            book.sign(e, "sess_a", &signing);
        }
        assert_eq!(book.verify_all(&entries, &keys, agent_entries).unwrap(), 3);

        // Strip the middle one.
        book.by_entry.remove(&entries[1].content_hash());

        let err = book.verify_all(&entries, &keys, agent_entries).unwrap_err();
        assert!(matches!(err, SigningError::Missing { .. }), "{err:?}");
    }

    #[test]
    fn entries_out_of_scope_are_not_required_to_be_signed() {
        // `System` entries are written by the instance and have no session key.
        // A deployment that signs agent writes must not fail on them.
        let signing = key(1);
        let keys = registry(&[("sess_a", &signing)]);
        let entries = vec![
            entry(1, Author::agent("sess_a", "alice")),
            entry(2, Author::System),
            entry(
                3,
                Author::Human {
                    user_id: "carol".into(),
                },
            ),
        ];

        let mut book = SignatureBook::new();
        book.sign(&entries[0], "sess_a", &signing);

        assert_eq!(
            book.verify_all(&entries, &keys, agent_entries).unwrap(),
            1,
            "only the agent entry was in scope"
        );
    }

    #[test]
    fn an_unknown_key_is_reported_as_unknown_rather_than_as_a_bad_signature() {
        // Different problems: one is a registry that has not caught up, the
        // other is a forgery. Collapsing them would send somebody hunting an
        // attacker for a deployment error, or the reverse.
        let signing = key(1);
        let entry = entry(1, Author::agent("sess_a", "alice"));
        let mut book = SignatureBook::new();
        book.sign(&entry, "sess_a", &signing);

        let empty = KeyRegistry::new();
        assert!(matches!(
            book.verify(&entry, &empty),
            Err(SigningError::UnknownKey { .. })
        ));
    }

    #[test]
    fn signing_takes_an_entry_so_nothing_can_ask_for_an_arbitrary_hash_to_be_signed() {
        // Structural, and asserted because the alternative signature —
        // `sign(hash, key)` — would let a caller obtain a signature over a hash
        // it constructed, including a future entry's. There is no such call.
        //
        // What this test can check is the consequence: the signature is over the
        // entry's own hash and nothing else.
        let signing = key(1);
        let entry = entry(1, Author::agent("sess_a", "alice"));
        let mut book = SignatureBook::new();
        let produced = book.sign(&entry, "sess_a", &signing).clone();

        assert_eq!(produced.entry_hash, entry.content_hash());
        let expected = signing.sign(&entry.content_hash().0).to_bytes().to_vec();
        assert_eq!(produced.signature, expected);
    }
}
