//! Authentication: salt generation and `mysql_native_password` verification.

use sha1::{Digest, Sha1};

/// Length of the authentication salt (scramble) sent in the handshake.
pub const SALT_LEN: usize = 20;

/// Stored credential of an account, as the server-side user lookup returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// `mysql_native_password`. `None` means the account has an empty password; otherwise the
    /// stored hash `SHA1(SHA1(password))` (what `mysql.user.authentication_string` holds as
    /// `*HEX`).
    NativePassword(Option<[u8; 20]>),
}

impl Credential {
    /// Credential for a plain-text password (empty = no password).
    pub fn native_from_password(password: &str) -> Self {
        if password.is_empty() {
            Credential::NativePassword(None)
        } else {
            Credential::NativePassword(Some(native_password_hash(password.as_bytes())))
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

    /// Checks a client's `mysql_native_password` auth response against this credential.
    pub fn verify_native(&self, salt: &[u8], auth_response: &[u8]) -> bool {
        match self {
            Credential::NativePassword(None) => auth_response.is_empty(),
            Credential::NativePassword(Some(hash)) => {
                verify_native_password(salt, auth_response, hash)
            }
        }
    }
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
}
