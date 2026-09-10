//! PKCE (RFC 7636).
//!
//! A CLI is a public client: it cannot keep a client secret, so possession of
//! the authorization code would otherwise be enough to redeem it. PKCE binds the
//! code to a random verifier that only the process which started the flow knows,
//! so an intercepted code is useless on its own.
//!
//! Only `S256` is supported. The spec also allows `plain`, where the challenge
//! *is* the verifier — which provides no protection against an attacker who saw
//! the authorization request. Offering it would mean a downgrade is available.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// The secret half, kept in the client.
#[derive(Clone, PartialEq, Eq)]
pub struct PkceVerifier(String);

// Manual, so a verifier cannot reach a log line through `{:?}`.
impl std::fmt::Debug for PkceVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PkceVerifier(<redacted>)")
    }
}

/// The public half, sent in the authorization request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceChallenge(String);

impl PkceVerifier {
    /// Generate a verifier from OS entropy.
    ///
    /// 32 bytes, base64url-encoded to 43 characters — within RFC 7636's 43..128
    /// range and at its recommended entropy.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the OS must provide entropy for PKCE");
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn challenge(&self) -> PkceChallenge {
        let digest = blake3_sha256(self.0.as_bytes());
        PkceChallenge(URL_SAFE_NO_PAD.encode(digest))
    }

    /// The verifier, for the token exchange. Named so every call site is easy
    /// to find and check.
    pub fn expose_for_exchange(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PkceChallenge {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The method name to send. Always `S256`; see the module docs on why
    /// `plain` is not offered.
    pub const METHOD: &'static str = "S256";
}

/// SHA-256, as RFC 7636 requires for `S256`.
///
/// Implemented here rather than pulled in as a dependency: this crate sits in
/// `thetad`'s dependency closure, which the hot-path guard checks, and one
/// well-understood hash is cheaper to audit than another crate.
fn blake3_sha256(input: &[u8]) -> [u8; 32] {
    sha256(input)
}

/// SHA-256 (FIPS 180-4).
fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pad: a 1 bit, zeros, then the length in bits as a big-endian u64.
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((message.len() as u64) * 8).to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// A random `state` value, binding a callback to the attempt that started it.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("the OS must provide entropy for state");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from RFC 7636 Appendix B.
    #[test]
    fn the_challenge_matches_the_rfc_test_vector() {
        let verifier = PkceVerifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(
            verifier.challenge().as_str(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        // Empty input and the classic "abc", so an off-by-one in padding shows.
        let hex = |bytes: [u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Long enough to span two blocks, where padding gets interesting.
        assert_eq!(
            hex(sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_generated_verifier_is_within_the_rfc_length_range() {
        let verifier = PkceVerifier::generate();
        assert!(
            (43..=128).contains(&verifier.len()),
            "RFC 7636 requires 43..128 characters, got {}",
            verifier.len()
        );
    }

    #[test]
    fn two_verifiers_are_never_the_same() {
        // A predictable verifier is no verifier: the whole point is that only
        // this process knows it.
        let a = PkceVerifier::generate();
        let b = PkceVerifier::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn the_challenge_is_not_the_verifier() {
        // Which is exactly what `plain` would give, and why it is not offered.
        let verifier = PkceVerifier::generate();
        assert_ne!(
            verifier.challenge().as_str(),
            verifier.expose_for_exchange()
        );
    }

    #[test]
    fn a_verifier_never_renders_itself_into_a_log_line() {
        let verifier = PkceVerifier::generate();
        let rendered = format!("{verifier:?}");
        assert_eq!(rendered, "PkceVerifier(<redacted>)");
        assert!(!rendered.contains(verifier.expose_for_exchange()));
    }

    #[test]
    fn state_values_are_random_and_url_safe() {
        let a = generate_state();
        let b = generate_state();
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }
}
