//! go-file: pkg/util/collate/general_ci.go
//!
//! The plane tables (`planeXX`, `planeTable`) are generated (`general_ci_data.rs`).

use std::any::Any;

use tidb_base::gostd::decode_rune;

use crate::stringutil;

use super::general_ci_data::planeTable;
use super::{Collator, WildcardPattern, compare_common, truncate_tailing_space};

/// `utf8mb4_general_ci` / `utf8_general_ci`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeneralCICollator;

impl Collator for GeneralCICollator {
    /// Compare implements Collator interface.
    // go: generalCICollator.Compare L1
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        compare_common(a, b, convert_rune_general_ci)
    }

    /// Key implements Collator interface.
    // go: generalCICollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        self.key_without_trim_right_space(truncate_tailing_space(str))
    }

    /// KeyWithoutTrimRightSpace implements Collator interface.
    // go: generalCICollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(str.len());
        let mut i = 0;
        while i < str.len() {
            // When the byte sequence is not a valid UTF-8 encoding of a rune, Golang returns RuneError('�') and size 1.
            // See https://pkg.go.dev/unicode/utf8#DecodeRune for more details.
            // Here we check both the size and rune to distinguish between invalid byte sequence and valid '�'.
            let (r, r_len) = decode_rune(&str[i..]);
            let invalid = r == char::REPLACEMENT_CHARACTER && r_len == 1;
            if invalid {
                return buf;
            }

            i += r_len;
            let u16 = convert_rune_general_ci(r);
            buf.push((u16 >> 8) as u8);
            buf.push(u16 as u8);
        }
        buf
    }

    /// Pattern implements Collator interface.
    // go: generalCICollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(CiPattern::default())
    }

    /// Clone implements Collator interface.
    // go: generalCICollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(GeneralCICollator)
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone, Default)]
struct CiPattern {
    pat_chars: Vec<char>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for CiPattern {
    /// Compile implements WildcardPattern interface.
    // go: ciPattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        (self.pat_chars, self.pat_types) = stringutil::compile_pattern_inner(pattern_str, escape);
    }

    /// Compile implements WildcardPattern interface.
    // go: ciPattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match_customized(str, &self.pat_chars, &self.pat_types, |a, b| {
            convert_rune_general_ci(a) == convert_rune_general_ci(b)
        })
    }
}

// go: convertRuneGeneralCI L1
fn convert_rune_general_ci(r: char) -> u32 {
    let r = r as u32;
    if r > 0xFFFF {
        return 0xFFFD;
    }
    match planeTable[(r >> 8) as usize] {
        None => r,
        Some(plane) => plane[(r & 0xFF) as usize] as u32,
    }
}
