//! go-file: pkg/util/collate/unicode_0400_ci_impl.go

use crate::stringutil;

use super::ucadata::DUCET0400Table;
use super::unicode_ci::UcaImpl;
use super::{WildcardPattern, truncate_tailing_space};

// magic number indicate weight has 2 uint64, should get from `longRuneMap`
const longRune: u64 = 0xFFFD;

/// `unicode0400Impl`: the UCA 4.0.0 weights of `utf8mb4_unicode_ci` / `utf8_unicode_ci`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unicode0400Impl;

impl UcaImpl for Unicode0400Impl {
    // go: unicode0400Impl.Clone L1
    fn clone_impl(&self) -> Self {
        Unicode0400Impl
    }

    // go: unicode0400Impl.Preprocess L1
    #[inline]
    fn preprocess<'a>(&self, s: &'a [u8]) -> &'a [u8] {
        truncate_tailing_space(s)
    }

    // go: unicode0400Impl.GetWeight L1
    #[inline]
    fn get_weight(&self, r: char) -> (u64, u64) {
        let r = r as u32;
        if r > 0xFFFF {
            return (0xFFFD, 0);
        }
        if DUCET0400Table.map_table4[r as usize] == longRune {
            let l = DUCET0400Table.long_rune(r);
            return (l[0], l[1]);
        }
        (DUCET0400Table.map_table4[r as usize], 0)
    }

    // go: unicode0400Impl.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(UnicodePattern::default())
    }
}

#[derive(Debug, Clone, Default)]
struct UnicodePattern {
    pat_chars: Vec<char>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for UnicodePattern {
    /// Compile implements WildcardPattern interface.
    // go: unicodePattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        (self.pat_chars, self.pat_types) = stringutil::compile_pattern_inner(pattern_str, escape);
    }

    /// DoMatch implements WildcardPattern interface.
    // go: unicodePattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match_customized(str, &self.pat_chars, &self.pat_types, |a, b| {
            if a as u32 > 0xFFFF || b as u32 > 0xFFFF {
                return a == b;
            }

            let (ar, br) = (
                DUCET0400Table.map_table4[a as usize],
                DUCET0400Table.map_table4[b as usize],
            );
            if ar != br {
                return false;
            }

            if ar == longRune {
                return a == b;
            }

            true
        })
    }
}
