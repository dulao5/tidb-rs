//! go-file: pkg/util/collate/unicode_0900_ai_ci_impl.go

use crate::stringutil;

use super::WildcardPattern;
use super::ucadata::{DUCET0900Table, LongRune8};
use super::unicode_ci::UcaImpl;

/// `unicode0900Impl`: the UCA 9.0.0 weights of `utf8mb4_0900_ai_ci`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unicode0900Impl;

impl UcaImpl for Unicode0900Impl {
    // go: unicode0900Impl.Clone L1
    fn clone_impl(&self) -> Self {
        Unicode0900Impl
    }

    // go: unicode0900Impl.Preprocess L1
    #[inline]
    fn preprocess<'a>(&self, s: &'a [u8]) -> &'a [u8] {
        s
    }

    // go: unicode0900Impl.GetWeight L1
    #[inline]
    fn get_weight(&self, r: char) -> (u64, u64) {
        convert_rune_unicode_ci0900(r)
    }

    // go: unicode0900Impl.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(Unicode0900AICIPattern::default())
    }
}

/// The weights of a rune. Like Go, a rune equal to `len(MapTable4)` (U+2CEA1) indexes past
/// the table and panics (Go checks `int(r) > len(...)`).
// go: convertRuneUnicodeCI0900 L1
#[inline]
fn convert_rune_unicode_ci0900(r: char) -> (u64, u64) {
    let r = r as u32;
    if r as usize > DUCET0900Table.map_table4.len() {
        return (
            (r >> 15) as u64 + 0xFBC0 + ((((r & 0x7FFF) | 0x8000) as u64) << 16),
            0,
        );
    }

    let first = DUCET0900Table.map_table4[r as usize];
    if first == LongRune8 {
        let l = DUCET0900Table.long_rune(r);
        return (l[0], l[1]);
    }
    (first, 0)
}

#[derive(Debug, Clone, Default)]
struct Unicode0900AICIPattern {
    pat_chars: Vec<char>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for Unicode0900AICIPattern {
    /// Compile implements WildcardPattern interface.
    // go: unicode0900AICIPattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        (self.pat_chars, self.pat_types) = stringutil::compile_pattern_inner(pattern_str, escape);
    }

    /// DoMatch implements WildcardPattern interface.
    // go: unicode0900AICIPattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match_customized(str, &self.pat_chars, &self.pat_types, |a, b| {
            let (a_first, a_second) = convert_rune_unicode_ci0900(a);
            let (b_first, b_second) = convert_rune_unicode_ci0900(b);

            a_first == b_first && a_second == b_second
        })
    }
}
