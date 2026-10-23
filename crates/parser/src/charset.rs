//! Charset / collation lookups used by the lexer and actions (port of the parser-relevant part of
//! TiDB `pkg/parser/charset/charset.go`; tables in `charset_gen.rs`).
use crate::charset_gen::{COLLATIONS, KNOWN_CHARSETS, SUPPORTED_CHARSETS};

/// `charset.CharsetBin` / `charset.CollationBin`.
pub const CHARSET_BIN: &str = "binary";
pub const COLLATION_BIN: &str = "binary";
/// `mysql.DefaultCharset`.
pub const DEFAULT_CHARSET: &str = "utf8mb4";
/// `charset.CharsetUTF8MB4` / `charset.CollationUTF8MB4`.
pub const CHARSET_UTF8MB4: &str = "utf8mb4";
pub const COLLATION_UTF8MB4: &str = "utf8mb4_bin";
/// `charset.CharsetLatin1`.
pub const CHARSET_LATIN1: &str = "latin1";

/// Error of [`get_charset_info`], worded like TiDB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharsetError {
    /// A MySQL charset TiDB does not support (Go returns the info *and* this error).
    Unsupported(&'static str),
    Unknown,
}

fn lookup(
    tbl: &'static [(&'static str, &'static str)],
    cs: &str,
) -> Option<&'static (&'static str, &'static str)> {
    tbl.iter().find(|(n, _)| n.eq_ignore_ascii_case(cs))
}

fn normalize(cs: &str) -> &str {
    if cs.eq_ignore_ascii_case("utf8mb3") {
        "utf8"
    } else {
        cs
    }
}

/// `charset.GetCharsetInfo`: canonical (lower-case) name and default collation.
pub fn get_charset_info(cs: &str) -> Result<(&'static str, &'static str), CharsetError> {
    let cs = normalize(cs);
    if let Some(&(n, c)) = lookup(SUPPORTED_CHARSETS, cs) {
        return Ok((n, c));
    }
    match lookup(KNOWN_CHARSETS, cs) {
        Some(&(n, _)) => Err(CharsetError::Unsupported(n)),
        None => Err(CharsetError::Unknown),
    }
}

/// The lexer's `handleIdent`: `_name` is a charset introducer when `GetCharsetInfo` returns a
/// charset at all (supported or not). Returns the canonical name.
pub fn introducer_charset(cs: &str) -> Option<&'static str> {
    match get_charset_info(cs) {
        Ok((n, _)) | Err(CharsetError::Unsupported(n)) => Some(n),
        Err(CharsetError::Unknown) => None,
    }
}

/// `charset.GetDefaultCollation`.
pub fn get_default_collation(cs: &str) -> Option<&'static str> {
    get_charset_info(cs).ok().map(|(_, c)| c)
}

/// `charset.GetDefaultCollationLegacy` (charset introducers).
pub fn get_default_collation_legacy(cs: &str) -> Option<&'static str> {
    let l = cs.to_ascii_lowercase();
    match l.as_str() {
        "utf8mb3" => get_default_collation("utf8"),
        "utf8" | "utf8mb4" | "ascii" | "latin1" | "binary" => get_default_collation(cs),
        _ => None,
    }
}

/// `charset.GetCollationByName`: canonical collation name.
pub fn get_collation_by_name(name: &str) -> Option<&'static str> {
    let l = name.to_ascii_lowercase();
    let l = match l.as_str() {
        "utf8mb3_bin" => "utf8_bin",
        "utf8mb3_unicode_ci" => "utf8_unicode_ci",
        "utf8mb3_general_ci" => "utf8_general_ci",
        other => other,
    };
    COLLATIONS
        .iter()
        .find(|(_, _, n)| *n == l)
        .map(|(_, _, n)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert_eq!(get_charset_info("UTF8MB3"), Ok(("utf8", "utf8_bin")));
        assert_eq!(
            get_charset_info("big5"),
            Err(CharsetError::Unsupported("big5"))
        );
        assert_eq!(get_charset_info("nope"), Err(CharsetError::Unknown));
        assert_eq!(introducer_charset("Latin1"), Some("latin1"));
        assert_eq!(get_default_collation_legacy("gbk"), None);
        assert_eq!(
            get_collation_by_name("UTF8MB4_GENERAL_CI"),
            Some("utf8mb4_general_ci")
        );
        assert_eq!(get_collation_by_name("utf8mb3_bin"), Some("utf8_bin"));
        assert_eq!(get_collation_by_name("nope"), None);
    }
}
