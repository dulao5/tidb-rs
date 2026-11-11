//! go-file: pkg/util/collate/bin.go

use std::any::Any;

use crate::stringutil;

use super::{Collator, WildcardPattern, truncate_tailing_space};

/// binCollator match pattern in bytes
#[derive(Debug, Clone, Copy, Default)]
pub struct BinCollator;

impl Collator for BinCollator {
    /// Compare implement Collator interface.
    // go: binCollator.Compare L1
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        super::cmp_compare(a, b)
    }

    /// Key implement Collator interface.
    // go: binCollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        str.to_vec()
    }

    /// KeyWithoutTrimRightSpace implement Collator interface.
    // go: binCollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        str.to_vec()
    }

    /// Pattern implements Collator interface.
    // go: binCollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(BinPattern::default())
    }

    /// Clone implements Collator interface.
    // go: binCollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(BinCollator)
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `derivedBinCollator` embeds `binCollator`: every method but `Pattern` is promoted from
/// it (so `Clone` returns a `binCollator`, as in Go).
#[derive(Debug, Clone, Copy, Default)]
pub struct DerivedBinCollator {
    bin_collator: BinCollator,
}

impl DerivedBinCollator {
    // glue: Go struct literal &derivedBinCollator{}
    pub const fn new() -> DerivedBinCollator {
        DerivedBinCollator {
            bin_collator: BinCollator,
        }
    }
}

impl Collator for DerivedBinCollator {
    // glue: promoted method binCollator.Compare
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        self.bin_collator.compare(a, b)
    }

    // glue: promoted method binCollator.Key
    fn key(&self, str: &[u8]) -> Vec<u8> {
        self.bin_collator.key(str)
    }

    // glue: promoted method binCollator.KeyWithoutTrimRightSpace
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        self.bin_collator.key_without_trim_right_space(str)
    }

    /// Pattern implements Collator interface.
    // go: derivedBinCollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(DerivedBinPattern::default())
    }

    // glue: promoted method binCollator.Clone
    fn clone_collator(&self) -> Box<dyn Collator> {
        self.bin_collator.clone_collator()
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// binPaddingCollator: `binary` comparison with PAD SPACE.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinPaddingCollator;

impl Collator for BinPaddingCollator {
    // go: binPaddingCollator.Compare L1
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        super::cmp_compare(truncate_tailing_space(a), truncate_tailing_space(b))
    }

    // go: binPaddingCollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        truncate_tailing_space(str).to_vec()
    }

    /// KeyWithoutTrimRightSpace implement Collator interface.
    // go: binPaddingCollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        str.to_vec()
    }

    /// Pattern implements Collator interface.
    /// Notice that trailing spaces are significant.
    // go: binPaddingCollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        Box::new(DerivedBinPattern::default())
    }

    /// Clone implements Collator interface.
    // go: binPaddingCollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(BinPaddingCollator)
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DerivedBinPattern {
    pat_chars: Vec<char>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for DerivedBinPattern {
    /// Compile implements WildcardPattern interface.
    // go: derivedBinPattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        // CompilePattern = CompilePatternInner; the buffers are reused (allocation only).
        stringutil::compile_pattern_inner_into(pattern_str, escape, &mut self.pat_chars, &mut self.pat_types);
    }

    /// DoMatch implements WildcardPattern interface.
    // go: derivedBinPattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match(str, &self.pat_chars, &self.pat_types)
    }
}

#[derive(Debug, Clone, Default)]
struct BinPattern {
    pat_chars: Vec<u8>,
    pat_types: Vec<u8>,
}

impl WildcardPattern for BinPattern {
    /// Compile implements WildcardPattern interface.
    // go: binPattern.Compile L1
    fn compile(&mut self, pattern_str: &[u8], escape: u8) {
        (self.pat_chars, self.pat_types) = stringutil::compile_pattern_binary(pattern_str, escape);
    }

    /// DoMatch implements WildcardPattern interface.
    // go: binPattern.DoMatch L1
    fn do_match(&self, str: &[u8]) -> bool {
        stringutil::do_match_binary(str, &self.pat_chars, &self.pat_types)
    }
}
