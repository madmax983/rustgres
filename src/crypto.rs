//! Pure-std cryptography for SCRAM-SHA-256 authentication (v0.11).
//!
//! Implements from scratch, with zero external crates:
//! - SHA-256 (FIPS 180-4)
//! - HMAC-SHA-256 (RFC 2104)
//! - PBKDF2-HMAC-SHA-256, i.e. SCRAM's `Hi` (RFC 5802 / RFC 2898)
//! - base64 encode/decode (RFC 4648, standard alphabet)
//!
//! Deviations from PostgreSQL, documented honestly:
//! - **No SASLprep normalization** of passwords/usernames (RFC 4013).
//!   Postgres applies SASLprep; we require valid UTF-8 and use the raw
//!   bytes. Non-ASCII passwords therefore hash differently than in
//!   Postgres.
//! - **Salt randomness** comes from `SystemTime` nanoseconds mixed with a
//!   process counter and the pid — not a cryptographic RNG. Fine for a
//!   from-scratch engine, but do not rely on it for production secrecy.
//! - The default SCRAM iteration count is 4096, matching PostgreSQL.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Padding: 0x80, zeros, then 64-bit big-endian bit length.
    let mut msg = Vec::with_capacity(((data.len() + 9 + 63) / 64) * 64);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// HMAC-SHA-256 and PBKDF2
// ---------------------------------------------------------------------------

/// HMAC-SHA-256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let h = sha256(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(64 + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// PBKDF2-HMAC-SHA-256 with a single 32-byte block (SCRAM's `Hi`).
/// SCRAM only ever needs dkLen = 32, so only block index 1 is computed.
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut salt_block = Vec::with_capacity(salt.len() + 4);
    salt_block.extend_from_slice(salt);
    salt_block.extend_from_slice(&1u32.to_be_bytes()); // INT(1)
    let mut u = hmac_sha256(password, &salt_block);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for i in 0..32 {
            out[i] ^= u[i];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// base64 (RFC 4648 standard alphabet)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64. Returns `None` on any invalid character or
/// bad padding.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                n <<= 6;
            } else {
                if pad > 0 {
                    return None; // padding in the middle
                }
                n = (n << 6) | b64_val(c)?;
            }
            let _ = i;
        }
        if pad > 2 {
            return None;
        }
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// SCRAM-SHA-256 server helpers (RFC 5802)
// ---------------------------------------------------------------------------

/// Default iteration count, matching PostgreSQL.
pub const SCRAM_DEFAULT_ITERATIONS: u32 = 4096;

/// A stored SCRAM verifier: everything the server needs to authenticate
/// a role, without ever holding the password.
#[derive(Clone, Debug)]
pub struct ScramVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl ScramVerifier {
    /// PostgreSQL's on-disk/textual verifier format:
    /// `SCRAM-SHA-256$<iterations>:<salt_b64>$<stored_b64>:<server_b64>`.
    pub fn encode(&self) -> String {
        format!(
            "SCRAM-SHA-256${}:{}${}:{}",
            self.iterations,
            base64_encode(&self.salt),
            base64_encode(&self.stored_key),
            base64_encode(&self.server_key),
        )
    }
}

/// A dummy verifier for unknown users during the SCRAM exchange, so
/// authentication attempts against nonexistent roles are not enumerable
/// by timing (like PostgreSQL).
pub fn dummy_verifier() -> ScramVerifier {
    verifier_for_password_with_salt("dummy-password-for-unknown-user", &random_bytes(16), 4096)
}

/// Derive a verifier from a cleartext password (used by CREATE/ALTER
/// ROLE ... PASSWORD). The salt is 16 bytes from our non-crypto RNG.
pub fn verifier_for_password(password: &str) -> ScramVerifier {
    let salt = random_bytes(16);
    verifier_for_password_with_salt(password, &salt, SCRAM_DEFAULT_ITERATIONS)
}

/// Deterministic variant (tests).
pub fn verifier_for_password_with_salt(
    password: &str,
    salt: &[u8],
    iterations: u32,
) -> ScramVerifier {
    let salted = pbkdf2_hmac_sha256(password.as_bytes(), salt, iterations);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256(&client_key);
    let server_key = hmac_sha256(&salted, b"Server Key");
    ScramVerifier {
        iterations,
        salt: salt.to_vec(),
        stored_key,
        server_key,
    }
}

/// Generate `n` pseudo-random bytes. NOT a cryptographic RNG (see module
/// docs): SystemTime nanoseconds mixed with a counter and the pid.
pub fn random_bytes(n: usize) -> Vec<u8> {
    static CTR: AtomicU64 = AtomicU64::new(0x9e3779b97f4a7c15);
    let mut out = Vec::with_capacity(n);
    let pid = std::process::id() as u64;
    while out.len() < n {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 32))
            .unwrap_or(0xdeadbeef);
        let c = CTR.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed);
        // xorshift64* mix
        let mut x = t ^ c ^ pid.wrapping_mul(0x9e3779b97f4a7c15);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545f4914f6cdd1d);
        out.extend_from_slice(&x.to_be_bytes());
    }
    out.truncate(n);
    out
}

/// A server-side SCRAM-SHA-256 exchange in progress.
pub struct ScramExchange {
    verifier: ScramVerifier,
    server_nonce: String,
    client_first_bare: String,
    server_first: String,
}

impl ScramExchange {
    /// Begin an exchange for `client_first` (the client's initial
    /// response data, e.g. `n,,n=alice,r=...`). The caller has already
    /// decided which verifier to use (a fake one for unknown users).
    /// Returns the exchange plus the server-first message to send.
    pub fn begin(verifier: ScramVerifier, client_first: &str) -> Result<(Self, String), String> {
        // Strip the gs2 header (`n,,` or `y,,` or `p,,`).
        let bare = client_first
            .strip_prefix("n,,")
            .or_else(|| client_first.strip_prefix("y,,"))
            .or_else(|| client_first.strip_prefix("p,,"))
            .ok_or_else(|| "malformed SCRAM client-first message".to_string())?;
        let mut user: Option<&str> = None;
        let mut nonce: Option<&str> = None;
        for attr in bare.split(',') {
            if let Some(v) = attr.strip_prefix("n=") {
                user = Some(v);
            } else if let Some(v) = attr.strip_prefix("r=") {
                nonce = Some(v);
            }
        }
        let client_nonce = nonce
            .filter(|n| !n.is_empty())
            .ok_or_else(|| "SCRAM client-first message has no nonce".to_string())?
            .to_string();
        let _ = user; // the startup-packet user was already authenticated
        let server_nonce = format!("{}{}", client_nonce, base64_encode(&random_bytes(18)));
        let server_first = format!(
            "r={},s={},i={}",
            server_nonce,
            base64_encode(&verifier.salt),
            verifier.iterations
        );
        Ok((
            ScramExchange {
                verifier,
                server_nonce,
                client_first_bare: bare.to_string(),
                server_first: server_first.clone(),
            },
            server_first,
        ))
    }

    /// Verify `client_final` (e.g. `c=biws,r=...,p=...`). On success
    /// returns the server-final message (`v=...`) to send.
    pub fn verify(&self, client_final: &str) -> Result<String, String> {
        let mut cbind: Option<&str> = None;
        let mut nonce: Option<&str> = None;
        let mut proof_b64: Option<&str> = None;
        for attr in client_final.split(',') {
            if let Some(v) = attr.strip_prefix("c=") {
                cbind = Some(v);
            } else if let Some(v) = attr.strip_prefix("r=") {
                nonce = Some(v);
            } else if let Some(v) = attr.strip_prefix("p=") {
                proof_b64 = Some(v);
            }
        }
        // We don't support channel binding: only "biws" (base64("n,,")).
        if cbind != Some("biws") {
            return Err("unsupported SCRAM channel binding".to_string());
        }
        // The nonce must be exactly the server nonce: any tampering with
        // the client nonce is caught here.
        if nonce != Some(self.server_nonce.as_str()) {
            return Err("SCRAM nonce mismatch".to_string());
        }
        let proof = proof_b64
            .and_then(base64_decode)
            .ok_or_else(|| "malformed SCRAM proof".to_string())?;
        if proof.len() != 32 {
            return Err("malformed SCRAM proof".to_string());
        }
        let without_proof = client_final
            .rsplit_once(",p=")
            .map(|(head, _)| head)
            .ok_or_else(|| "malformed SCRAM client-final message".to_string())?;
        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );
        let client_sig = hmac_sha256(&self.verifier.stored_key, auth_message.as_bytes());
        // Expected ClientKey = proof XOR ClientSignature.
        let mut client_key = [0u8; 32];
        for i in 0..32 {
            client_key[i] = proof[i] ^ client_sig[i];
        }
        let stored_key = sha256(&client_key);
        // Constant-time comparison.
        let mut diff = 0u8;
        for i in 0..32 {
            diff |= stored_key[i] ^ self.verifier.stored_key[i];
        }
        if diff != 0 {
            return Err("password authentication failed".to_string());
        }
        let server_sig = hmac_sha256(&self.verifier.server_key, auth_message.as_bytes());
        Ok(format!("v={}", base64_encode(&server_sig)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // "abc"
        let h = sha256(b"abc");
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_empty() {
        let h = sha256(b"");
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 1
        let h = hmac_sha256(&[0x0bu8; 20], b"Hi There");
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn pbkdf2_rfc7914_vector() {
        // PBKDF2-HMAC-SHA256("password", "salt", 1), dkLen=32
        let d = pbkdf2_hmac_sha256(b"password", b"salt", 1);
        let hex: String = d.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn base64_roundtrip() {
        for data in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
        ] {
            let e = base64_encode(data);
            assert_eq!(base64_decode(&e).unwrap(), data);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert!(base64_decode("!!!").is_none());
        assert!(base64_decode("Zm9v").is_some());
    }

    #[test]
    fn scram_roundtrip() {
        // Simulate a full client/server exchange by hand.
        let password = "s3cret";
        let salt = b"fixedsalt12345678";
        let verifier = verifier_for_password_with_salt(password, salt, 4096);
        let client_nonce = "rOprNGfwEbeRWgbRQ";
        let client_first = format!("n,,n=alice,r={}", client_nonce);
        let (ex, server_first) = ScramExchange::begin(verifier, &client_first).unwrap();
        assert!(server_first.starts_with(&format!("r={}", client_nonce)));
        assert!(server_first.contains("s="));
        assert!(server_first.contains("i=4096"));
        // Client side:
        let server_nonce = server_first
            .split(',')
            .find(|a| a.starts_with("r="))
            .unwrap()
            .strip_prefix("r=")
            .unwrap();
        let client_final_wo = format!("c=biws,r={}", server_nonce);
        let auth_message = format!(
            "{},{},{}",
            &client_first[3..],
            server_first,
            client_final_wo
        );
        let salted = pbkdf2_hmac_sha256(password.as_bytes(), salt, 4096);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_sig = hmac_sha256(&stored_key, auth_message.as_bytes());
        let mut proof = [0u8; 32];
        for i in 0..32 {
            proof[i] = client_key[i] ^ client_sig[i];
        }
        let client_final = format!("{},p={}", client_final_wo, base64_encode(&proof));
        let server_final = ex.verify(&client_final).unwrap();
        assert!(server_final.starts_with("v="));
        // Wrong password must fail.
        let (ex2, _) = ScramExchange::begin(
            verifier_for_password_with_salt("nope", salt, 4096),
            &client_first,
        )
        .unwrap();
        // A proof computed for the wrong password must fail against the
        // right verifier — recompute exchange and verify with wrong proof.
        let salted_wrong = pbkdf2_hmac_sha256(b"nope", salt, 4096);
        let ck_wrong = hmac_sha256(&salted_wrong, b"Client Key");
        let mut proof_wrong = [0u8; 32];
        for i in 0..32 {
            proof_wrong[i] = ck_wrong[i] ^ client_sig[i];
        }
        let cf_wrong = format!("{},p={}", client_final_wo, base64_encode(&proof_wrong));
        assert!(ex.verify(&cf_wrong).is_err());
        let _ = ex2;
    }
}
