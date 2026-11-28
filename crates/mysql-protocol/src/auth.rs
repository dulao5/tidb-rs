//! Authentication: salt generation, `mysql_native_password`, and `caching_sha2_password`
//! verification (Go `pkg/parser/auth`, `pkg/util/auth/caching_sha2.go`).

use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::constants::auth_plugin;

/// Length of the authentication salt (scramble) sent in the handshake.
pub const SALT_LEN: usize = 20;

/// Length of a MySQL `caching_sha2_password` `$A$NNN$<20-byte-salt><43-byte-b64>` hash string.
const CACHING_SHA2_HASH_LEN: usize = 70;
const CACHING_SHA2_DIGEST_LEN: usize = 43;
/// Minimum iteration count accepted by MySQL/TiDB (`$A$005$` = 5000 rounds).
const CACHING_SHA2_MIN_ROUNDS: u32 = 5_000;
/// Maximum iteration count representable in 3 hex digits (`$A$FFF$` = 4,095,000 rounds,
/// matching MySQL and Go TiDB `pkg/parser/auth/caching_sha2.go`).
const CACHING_SHA2_MAX_ROUNDS: u32 = 0xFFF * 1_000;

/// Parsed `caching_sha2_password` hash (`$A$NNN$<20-byte-salt><43-byte-crypt-b64-digest>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachingSha2Hash {
    pub iterations: u32,
    pub salt: [u8; SALT_LEN],
    pub digest: [u8; CACHING_SHA2_DIGEST_LEN],
}

/// Stored credential of an account, as the server-side user lookup returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// `mysql_native_password`. `None` means the account has an empty password; otherwise the
    /// stored hash `SHA1(SHA1(password))` (what `mysql.user.authentication_string` holds as
    /// `*HEX`).
    NativePassword(Option<[u8; 20]>),
    /// `caching_sha2_password`. `None` means the account has an empty password
    /// (`mysql.user.authentication_string == ""`); otherwise the parsed `$A$NNN$...` SHA-256
    /// crypt hash.
    CachingSha2Password(Option<CachingSha2Hash>),
}

/// Account metadata resolved during handshake authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountAuth {
    pub credential: Credential,
    /// Exact `mysql.user.User` matched during authentication (bound verbatim in `open_session`).
    pub auth_user: Option<String>,
    /// Exact `mysql.user.Host` matched during authentication (bound verbatim in `open_session`).
    pub auth_host: Option<String>,
    /// True when `mysql.user.Account_locked == 'Y'` (checked after password verification -> 3118).
    pub account_locked: bool,
}

impl From<Credential> for AccountAuth {
    fn from(credential: Credential) -> Self {
        Self {
            credential,
            auth_user: None,
            auth_host: None,
            account_locked: false,
        }
    }
}

impl Credential {
    /// MySQL authentication plugin name for this credential.
    pub fn plugin_name(&self) -> &'static str {
        match self {
            Credential::NativePassword(_) => auth_plugin::MYSQL_NATIVE_PASSWORD,
            Credential::CachingSha2Password(_) => auth_plugin::CACHING_SHA2_PASSWORD,
        }
    }

    /// Credential for a plain-text password (empty = no password).
    pub fn native_from_password(password: &str) -> Self {
        if password.is_empty() {
            Credential::NativePassword(None)
        } else {
            Credential::NativePassword(Some(native_password_hash(password.as_bytes())))
        }
    }

    /// `caching_sha2_password` credential for a plain-text password (empty = no password;
    /// non-empty hashes with 5000 rounds and a fresh random 20-byte salt).
    pub fn caching_sha2_from_password(password: &str) -> Self {
        if password.is_empty() {
            Credential::CachingSha2Password(None)
        } else {
            let salt = generate_salt();
            let digest = sha256_crypt_digest(password.as_bytes(), &salt, CACHING_SHA2_MIN_ROUNDS);
            Credential::CachingSha2Password(Some(CachingSha2Hash {
                iterations: CACHING_SHA2_MIN_ROUNDS,
                salt,
                digest,
            }))
        }
    }

    /// Formats a `caching_sha2_password` credential back into its MySQL `authentication_string`
    /// form (`""` or `$A$NNN$<20-byte-salt><43-byte-b64>`). Returns `None` for non-SHA2 credentials.
    pub fn to_caching_sha2_authentication_string(&self) -> Option<String> {
        match self {
            Credential::CachingSha2Password(None) => Some(String::new()),
            Credential::CachingSha2Password(Some(h)) => {
                let mut out = Vec::with_capacity(CACHING_SHA2_HASH_LEN);
                out.extend_from_slice(format!("$A${:03X}$", h.iterations / 1000).as_bytes());
                out.extend_from_slice(&h.salt);
                out.extend_from_slice(&h.digest);
                String::from_utf8(out).ok()
            }
            Credential::NativePassword(_) => None,
        }
    }

    /// Parses a `mysql.user.authentication_string` value for `mysql_native_password`: empty or
    /// `*` followed by 40 hex digits.
    pub fn native_from_authentication_string(s: &str) -> Option<Self> {
        if s.is_empty() {
            return Some(Credential::NativePassword(None));
        }
        let hex = s.strip_prefix('*')?;
        if hex.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Credential::NativePassword(Some(out)))
    }

    /// Parses a `mysql.user.authentication_string` value for `caching_sha2_password`:
    /// empty (passwordless account) or exact 70-byte `$A$NNN$<20-byte-salt><43-byte-crypt-b64>`.
    pub fn caching_sha2_from_authentication_string(s: &str) -> Option<Self> {
        if s.is_empty() {
            return Some(Credential::CachingSha2Password(None));
        }
        let b = s.as_bytes();
        if b.len() != CACHING_SHA2_HASH_LEN || &b[0..3] != b"$A$" || b[6] != b'$' {
            return None;
        }
        let rounds_str = std::str::from_utf8(&b[3..6]).ok()?;
        if !rounds_str.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let thousands = u32::from_str_radix(rounds_str, 16).ok()?;
        let iterations = thousands.checked_mul(1000)?;
        if !(CACHING_SHA2_MIN_ROUNDS..=CACHING_SHA2_MAX_ROUNDS).contains(&iterations) {
            return None;
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&b[7..7 + SALT_LEN]);
        let mut digest = [0u8; CACHING_SHA2_DIGEST_LEN];
        digest.copy_from_slice(&b[7 + SALT_LEN..CACHING_SHA2_HASH_LEN]);
        Some(Credential::CachingSha2Password(Some(CachingSha2Hash {
            iterations,
            salt,
            digest,
        })))
    }

    /// Checks a client's `mysql_native_password` auth response against this credential.
    pub fn verify_native(&self, salt: &[u8], auth_response: &[u8]) -> bool {
        match self {
            Credential::NativePassword(None) => auth_response.is_empty(),
            Credential::NativePassword(Some(hash)) => {
                verify_native_password(salt, auth_response, hash)
            }
            Credential::CachingSha2Password(_) => false,
        }
    }

    /// Checks a client's plaintext password (sent over TLS during `caching_sha2_password` full
    /// authentication) against this credential in constant time.
    pub fn verify_caching_sha2_plaintext(&self, plaintext: &[u8]) -> bool {
        match self {
            Credential::CachingSha2Password(None) => plaintext.is_empty(),
            Credential::CachingSha2Password(Some(h)) => {
                let candidate = sha256_crypt_digest(plaintext, &h.salt, h.iterations);
                constant_time_eq(&candidate, &h.digest)
            }
            Credential::NativePassword(_) => false,
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Ulrich Drepper's SHA-256 crypt digest, matching MySQL 8 / Go TiDB `pkg/util/auth/caching_sha2.go`
/// (`Sha256Crypt`), returning the 43-byte crypt-base64 digest suffix.
fn sha256_crypt_digest(
    password: &[u8],
    salt: &[u8; SALT_LEN],
    rounds: u32,
) -> [u8; CACHING_SHA2_DIGEST_LEN] {
    // 1. Alternate digest B = SHA256(password || salt || password).
    let mut ctx_b = Sha256::new();
    ctx_b.update(password);
    ctx_b.update(salt);
    ctx_b.update(password);
    let b = ctx_b.finalize();

    // 2. Digest A = SHA256(password || salt || B repeated len(password) bytes || bit-loop).
    let mut ctx_a = Sha256::new();
    ctx_a.update(password);
    ctx_a.update(salt);
    let mut rem = password.len();
    while rem > 32 {
        ctx_a.update(b);
        rem -= 32;
    }
    ctx_a.update(&b[..rem]);
    let mut n = password.len();
    while n > 0 {
        if n & 1 != 0 {
            ctx_a.update(b);
        } else {
            ctx_a.update(password);
        }
        n >>= 1;
    }
    let mut a = ctx_a.finalize();

    // 3. DP = SHA256(password repeated len(password) times), expanded to len(password) bytes as P.
    let mut ctx_dp = Sha256::new();
    for _ in 0..password.len() {
        ctx_dp.update(password);
    }
    let dp = ctx_dp.finalize();
    let mut p = Vec::with_capacity(password.len());
    let mut rem = password.len();
    while rem > 32 {
        p.extend_from_slice(&dp);
        rem -= 32;
    }
    p.extend_from_slice(&dp[..rem]);

    // 4. DS = SHA256(salt repeated (16 + A[0]) times), truncated to salt.len() (20 <= 32) as S.
    let mut ctx_ds = Sha256::new();
    for _ in 0..(16 + usize::from(a[0])) {
        ctx_ds.update(salt);
    }
    let ds = ctx_ds.finalize();
    let s = &ds[..SALT_LEN];

    // 5. Stretch for `rounds` iterations.
    for i in 0..rounds {
        let mut ctx_c = Sha256::new();
        if i & 1 != 0 {
            ctx_c.update(&p);
        } else {
            ctx_c.update(a);
        }
        if i % 3 != 0 {
            ctx_c.update(s);
        }
        if i % 7 != 0 {
            ctx_c.update(&p);
        }
        if i & 1 != 0 {
            ctx_c.update(a);
        } else {
            ctx_c.update(&p);
        }
        a = ctx_c.finalize();
    }

    // 6. Permuted crypt-base64 encoding (43 bytes).
    const B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = [0u8; CACHING_SHA2_DIGEST_LEN];
    let mut pos = 0usize;
    let mut push = |b2: u8, b1: u8, b0: u8, count: usize| {
        let mut w = (u32::from(b2) << 16) | (u32::from(b1) << 8) | u32::from(b0);
        for _ in 0..count {
            out[pos] = B64[(w & 0x3f) as usize];
            pos += 1;
            w >>= 6;
        }
    };
    push(a[0], a[10], a[20], 4);
    push(a[21], a[1], a[11], 4);
    push(a[12], a[22], a[2], 4);
    push(a[3], a[13], a[23], 4);
    push(a[24], a[4], a[14], 4);
    push(a[15], a[25], a[5], 4);
    push(a[6], a[16], a[26], 4);
    push(a[27], a[7], a[17], 4);
    push(a[18], a[28], a[8], 4);
    push(a[9], a[19], a[29], 4);
    push(0, a[31], a[30], 3);
    out
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// `SHA1(SHA1(password))`, the stored form of a native password.
pub fn native_password_hash(password: &[u8]) -> [u8; 20] {
    sha1(&[&sha1(&[password])])
}

/// Client-side scramble: `SHA1(password) XOR SHA1(salt || SHA1(SHA1(password)))`.
/// Used by tests and tools; an empty password scrambles to an empty response.
pub fn native_password_scramble(password: &[u8], salt: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = sha1(&[password]);
    let stage2 = sha1(&[&stage1]);
    let mix = sha1(&[salt, &stage2]);
    stage1.iter().zip(mix.iter()).map(|(a, b)| a ^ b).collect()
}

/// Verifies a native-password auth response against the stored `SHA1(SHA1(password))`
/// (TiDB `auth.CheckScrambledPassword`).
pub fn verify_native_password(salt: &[u8], auth_response: &[u8], stored: &[u8; 20]) -> bool {
    if auth_response.len() != 20 {
        return false;
    }
    let mix = sha1(&[salt, stored]);
    let mut stage1 = [0u8; 20];
    for i in 0..20 {
        stage1[i] = auth_response[i] ^ mix[i];
    }
    let candidate = sha1(&[&stage1]);
    // Constant-time comparison.
    candidate
        .iter()
        .zip(stored.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Generates a salt of printable ASCII bytes (never NUL, so it survives NUL-terminated
/// framing in the handshake).
pub fn generate_salt() -> [u8; SALT_LEN] {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut raw = [0u8; SALT_LEN];
    if getrandom::fill(&mut raw).is_err() {
        // Fall back to a time-derived seed; the salt only needs to be unpredictable enough to
        // prevent replay, and getrandom failing is not expected on supported platforms.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        raw = sha1(&[&nanos.to_le_bytes()]);
    }
    raw.map(|b| ALPHABET[usize::from(b) % ALPHABET.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_password_hash_matches_mysql_password_function() {
        // MySQL: SELECT PASSWORD('123') -> *23AE809DDACAF96AF0FD78ED04B6A265E05AA257
        let c = Credential::native_from_password("123");
        assert_eq!(
            Credential::native_from_authentication_string(
                "*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"
            ),
            Some(c)
        );
        assert_eq!(
            Credential::native_from_authentication_string(""),
            Some(Credential::NativePassword(None))
        );
        assert_eq!(Credential::native_from_authentication_string("*12"), None);
    }

    #[test]
    fn scramble_verifies() {
        let salt = generate_salt();
        let c = Credential::native_from_password("secret");
        let resp = native_password_scramble(b"secret", &salt);
        assert_eq!(resp.len(), 20);
        assert!(c.verify_native(&salt, &resp));
        assert!(!c.verify_native(&salt, &native_password_scramble(b"Secret", &salt)));
        assert!(!c.verify_native(&salt, &[]));
        let other_salt = [b'x'; SALT_LEN];
        assert!(!c.verify_native(&other_salt, &resp));
    }

    #[test]
    fn scramble_known_vector() {
        // TiDB's TestCheckScramble (pkg/parser/auth/mysql_native_password_test.go).
        let salt = [
            85, 92, 45, 22, 58, 79, 107, 6, 122, 125, 58, 80, 12, 90, 103, 32, 90, 10, 74, 82,
        ];
        let auth = [
            24, 180, 183, 225, 166, 6, 81, 102, 70, 248, 199, 143, 91, 204, 169, 9, 161, 171, 203,
            33,
        ];
        let stored = native_password_hash(b"abc");
        assert!(verify_native_password(&salt, &auth, &stored));
        assert_eq!(native_password_scramble(b"abc", &salt), auth);
        assert!(!verify_native_password(&salt, b"xxyyzz", &stored));
    }

    #[test]
    fn empty_password() {
        let c = Credential::native_from_password("");
        assert!(c.verify_native(b"anything", &[]));
        assert!(!c.verify_native(b"anything", &[1; 20]));
    }

    #[test]
    fn salt_is_printable() {
        let s = generate_salt();
        assert!(s.iter().all(|b| b.is_ascii_alphanumeric()));
    }

    fn decode_hex(hex: &str) -> String {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn caching_sha2_password_matches_go_tidb_v858_vectors() {
        // Real hashes generated by Go TiDB v8.5.8 (`CREATE USER ... IDENTIFIED WITH caching_sha2_password BY ...`).
        let short_hash = decode_hex(
            "24412430303524477808780B3D3F02270A2F0D19541174201C441655394334614F6C7A4B4F634A633039636C5148444D42375A5A6D706C2E354E4C316D4D504846653969662E",
        );
        let c_short = Credential::caching_sha2_from_authentication_string(&short_hash)
            .expect("valid Go v8.5.8 short-password hash");
        assert!(c_short.verify_caching_sha2_plaintext(b"secret123"));
        assert!(!c_short.verify_caching_sha2_plaintext(b"secret124"));
        assert!(!c_short.verify_caching_sha2_plaintext(b""));
        assert!(!c_short.verify_native(b"01234567890123456789", b""));

        let long_pw = b"a_very_long_password_exceeding_32_bytes_0123456789!@#";
        let long_hash = decode_hex(
            "244124303035241F135E5A43725F154B091B073D26506A03423F20745A362E753944344478516C2F4B4E4F43396B4E4A4A4D76777062654570463474344A4E6E5557636F7A34",
        );
        let c_long = Credential::caching_sha2_from_authentication_string(&long_hash)
            .expect("valid Go v8.5.8 long-password hash");
        assert!(c_long.verify_caching_sha2_plaintext(long_pw));
        assert!(!c_long.verify_caching_sha2_plaintext(b"a_very_long_password_exceeding_32_bytes"));

        // Non-default hexadecimal iteration counts generated by Go TiDB v8.5.8 `hashCrypt`:
        // `$A$00A$` = 10,000 rounds, `$A$010$` = 16,000 rounds (not 10,000).
        for (expected_rounds, hash_str) in [
            (
                10_000u32,
                "$A$00A$0123456789abcdefghijG0qJD7XG0O5hOVmIdpxIt5BtXHm/Q3qI/npfYXaVPT0",
            ),
            (
                16_000u32,
                "$A$010$0123456789abcdefghijKgQ0EW/zG9pfo2n4hFAbQKZYj8TLuD82nh7i47bZLuC",
            ),
        ] {
            let cred = Credential::caching_sha2_from_authentication_string(hash_str)
                .expect("valid Go v8.5.8 hex-rounds hash");
            let Credential::CachingSha2Password(Some(ref h)) = cred else {
                panic!("expected CachingSha2Password(Some)");
            };
            assert_eq!(h.iterations, expected_rounds);
            assert!(cred.verify_caching_sha2_plaintext(b"nondefault-rounds-pw"));
            assert!(!cred.verify_caching_sha2_plaintext(b"wrong-pw"));
            assert_eq!(
                cred.to_caching_sha2_authentication_string().as_deref(),
                Some(hash_str)
            );
        }

        // Round-trip helper and empty password.
        let generated = Credential::caching_sha2_from_password("roundtrip-pw");
        let encoded = generated.to_caching_sha2_authentication_string().unwrap();
        assert_eq!(encoded.len(), CACHING_SHA2_HASH_LEN);
        let reparsed = Credential::caching_sha2_from_authentication_string(&encoded).unwrap();
        assert_eq!(reparsed, generated);
        assert!(reparsed.verify_caching_sha2_plaintext(b"roundtrip-pw"));
        assert!(!reparsed.verify_caching_sha2_plaintext(b"wrong"));

        let empty = Credential::caching_sha2_from_authentication_string("").unwrap();
        assert_eq!(empty, Credential::CachingSha2Password(None));
        assert!(empty.verify_caching_sha2_plaintext(b""));
        assert!(!empty.verify_caching_sha2_plaintext(b"x"));

        // Malformed hashes fail closed (`None`).
        for bad in [
            "$A$004$123456789012345678901234567890123456789012345678901234567890123",
            "$B$005$123456789012345678901234567890123456789012345678901234567890123",
            "$A$005X123456789012345678901234567890123456789012345678901234567890123",
            "$A$xyz$123456789012345678901234567890123456789012345678901234567890123",
            "$A$+05$123456789012345678901234567890123456789012345678901234567890123",
            "$A$005$too-short",
            "*23AE809DDACAF96AF0FD78ED04B6A265E05AA257",
        ] {
            assert_eq!(
                Credential::caching_sha2_from_authentication_string(bad),
                None,
                "malformed hash {bad:?} must fail closed"
            );
        }
    }
}
