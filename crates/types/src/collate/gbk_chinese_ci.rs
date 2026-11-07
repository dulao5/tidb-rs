//! go-file: pkg/util/collate/gbk_chinese_ci.go
//!
//! The sort key table `gbkChineseCISortKeyTable` (gbk_chinese_ci_data.go) is generated
//! (`gbk_chinese_ci_data.rs`).

use std::any::Any;

use tidb_base::gostd::decode_rune;

use crate::stringutil;

use super::gbk_chinese_ci_data::gbkChineseCISortKeyTable;
use super::{Collator, WildcardPattern, compare_common, truncate_tailing_space};

/// `gbk_chinese_ci`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GbkChineseCICollator;

impl Collator for GbkChineseCICollator {
    /// Compare implements Collator interface.
    // go: gbkChineseCICollator.Compare L1
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        compare_common(a, b, gbk_chinese_ci_sort_key)
    }

    /// Key implements Collator interface.
    // go: gbkChineseCICollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        self.key_without_trim_right_space(truncate_tailing_space(str))
    }

    /// KeyWithoutTrimRightSpace implement Collator interface.
    // go: gbkChineseCICollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(str.len() * 2);
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
            let u16 = gbk_chinese_ci_sort_key(r);
            if u16 > 0xFF {
                buf.push((u16 >> 8) as u8);
            }
            buf.push(u16 as u8);
        }
        buf
    }

    /// Pattern implements Collator interface.
    // go: gbkChineseCICollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(GbkChineseCIPattern::default())
    }

    /// Clone implements Collator interface.
    // go: gbkChineseCICollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(GbkChineseCICollator)
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone, Default)]
struct GbkChineseCIPattern {
    pat_chars: Vec<char>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for GbkChineseCIPattern {
    /// Compile implements WildcardPattern interface.
    // go: gbkChineseCIPattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        (self.pat_chars, self.pat_types) = stringutil::compile_pattern_inner(pattern_str, escape);
    }

    /// DoMatch implements WildcardPattern interface.
    // go: gbkChineseCIPattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match_customized(str, &self.pat_chars, &self.pat_types, |a, b| {
            gbk_chinese_ci_sort_key(a) == gbk_chinese_ci_sort_key(b)
        })
    }
}

// go: gbkChineseCISortKey L1
fn gbk_chinese_ci_sort_key(r: char) -> u32 {
    let r = r as u32;
    if r > 0xFFFF {
        return 0x3F;
    }

    gbkChineseCISortKeyTable[r as usize] as u32
}
