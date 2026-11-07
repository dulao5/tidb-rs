//! go-file: pkg/parser/mysql/charset.go
//!
//! Constants and the tables `CharsetIDs`, `Collations`, `CollationNames` are in
//! `charset_gen.rs` (generated).

// Go identifiers for constants and statics (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use super::*;
use crate::gostd::RangeTable;
use crate::gostd::unicode_tables as u;

/// CharsetNameToID maps charset name to its default collation ID.
// go: CharsetNameToID L1
pub fn charset_name_to_id(charset: &str) -> u8 {
    // Use quick path for TiDB to avoid access CharsetIDs map
    // "SHOW CHARACTER SET;" to see all the supported character sets.
    if charset == "utf8mb4" {
        return UTF8MB4DefaultCollationID;
    } else if charset == "binary" {
        return BinaryDefaultCollationID;
    } else if charset == "utf8" {
        return UTF8DefaultCollationID;
    } else if charset == "ascii" {
        return ASCIIDefaultCollationID;
    } else if charset == "latin1" {
        return Latin1DefaultCollationID;
    }
    CharsetIDs.get(charset).copied().unwrap_or(0)
}

/// IsUTF8Charset checks if charset is utf8, utf8mb4.
// go: IsUTF8Charset L1
pub fn is_utf8_charset(charset: &str) -> bool {
    charset == UTF8Charset || charset == UTF8MB4Charset
}

/// RangeGraph defines valid unicode characters to use in column names. It strictly follows
/// MySQL's definition. See #3994.
pub static RangeGraph: [&RangeTable; 28] = [
    // _MY_PNT
    &u::No,
    &u::Mn,
    &u::Me,
    &u::Pc,
    &u::Pd,
    &u::Pd,
    &u::Ps,
    &u::Pe,
    &u::Pi,
    &u::Pf,
    &u::Po,
    &u::Sm,
    &u::Sc,
    &u::Sk,
    &u::So,
    // _MY_U
    &u::Lu,
    &u::Lt,
    &u::Nl,
    // _MY_L
    &u::Ll,
    &u::Lm,
    &u::Lo,
    &u::Nl,
    &u::Mn,
    &u::Mc,
    &u::Me,
    // _MY_NMR
    &u::Nd,
    &u::Nl,
    &u::No,
];
