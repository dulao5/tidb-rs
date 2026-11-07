//! go-file: pkg/util/collate/unicode_0400_ci_generated.go
//! go-file: pkg/util/collate/unicode_0900_ai_ci_generated.go
//! go-file: pkg/util/collate/ucaimpl/main.go
//!
//! Go generates `unicode_0400_ci_generated.go` and `unicode_0900_ai_ci_generated.go` from
//! one template (`ucaimpl/main.go`, `unicode_ci.go.tpl`) "rather than using other
//! polymorphism method (e.g. generics, interfaces, if/else...) to make sure every call to
//! the `GetWeight` and `Preprocess` is inlined". A Rust generic gives the same code: it is
//! monomorphized per impl, so `UnicodeCollator<Unicode0400Impl>` and
//! `UnicodeCollator<Unicode0900Impl>` are the two generated collators with the calls
//! inlined.

use std::any::Any;

use tidb_base::gostd::decode_rune;

use super::unicode_0400_ci_impl::Unicode0400Impl;
use super::unicode_0900_ai_ci_impl::Unicode0900Impl;
use super::{Collator, WildcardPattern, sign};

/// The template parameter `ImplName` of `unicode_ci.go.tpl`.
pub(crate) trait UcaImpl: Copy + Send + Sync + 'static {
    /// `Clone`.
    // glue: the template parameter ImplName of unicode_ci.go.tpl
    fn clone_impl(&self) -> Self;
    /// `Preprocess`: the string the weights are taken from.
    // glue: the template parameter ImplName of unicode_ci.go.tpl
    fn preprocess<'a>(&self, s: &'a [u8]) -> &'a [u8];
    /// `GetWeight`: up to 8 16-bit collation elements, low element first (`first` holds the
    /// first four).
    // glue: the template parameter ImplName of unicode_ci.go.tpl
    fn get_weight(&self, r: char) -> (u64, u64);
    /// `Pattern`.
    // glue: the template parameter ImplName of unicode_ci.go.tpl
    fn pattern(&self) -> Box<dyn WildcardPattern>;
}

/// The collator of `unicode_ci.go.tpl` over `I`.
#[derive(Debug, Clone, Copy, Default)]
#[allow(private_bounds)]
pub struct UnicodeCollator<I: UcaImpl> {
    imp: I,
}

/// unicodeCICollator implements UCA. see http://unicode.org/reports/tr10/
pub type UnicodeCICollator = UnicodeCollator<Unicode0400Impl>;
/// `utf8mb4_0900_ai_ci`.
pub type Unicode0900AICICollator = UnicodeCollator<Unicode0900Impl>;

#[allow(private_bounds)]
impl<I: UcaImpl> UnicodeCollator<I> {
    /// One instance of the template (Go: one generated file, named by `main`).
    // go: pkg/util/collate/ucaimpl/main.go generateFile L2 (the template is a Rust generic, monomorphized per impl like the generated files)
    // go: pkg/util/collate/ucaimpl/main.go main L2 (the two instances are the type aliases UnicodeCICollator and Unicode0900AICICollator)
    fn with_impl(imp: I) -> Self {
        UnicodeCollator { imp }
    }
}

#[allow(private_bounds)]
impl<I: UcaImpl + Default> Collator for UnicodeCollator<I> {
    /// Clone implements Collator interface.
    // go: unicodeCICollator.Clone L1
    // go: unicode0900AICICollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(Self::with_impl(self.imp.clone_impl()))
    }

    /// Compare implements Collator interface.
    // go: unicodeCICollator.Compare L1 [M-INT]
    // go: unicode0900AICICollator.Compare L1 [M-INT]
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        let a = self.imp.preprocess(a);
        let b = self.imp.preprocess(b);

        // weight of a, b. weight in unicode_ci may have 8 uint16s. xn indicate first 4 u16s, xs indicate last 4 u16s
        let (mut an, mut bn) = (0u64, 0u64);
        let (mut as_, mut bs) = (0u64, 0u64);
        // decode index of a, b
        let (mut ai, mut bi) = (0usize, 0usize);
        loop {
            if an == 0 {
                if as_ == 0 {
                    while an == 0 && ai < a.len() {
                        // When the byte sequence is not a valid UTF-8 encoding of a rune, Golang returns RuneError('�') and size 1.
                        // See https://pkg.go.dev/unicode/utf8#DecodeRune for more details.
                        // Here we check both the size and rune to distinguish between invalid byte sequence and valid '�'.
                        let (ar, ar_len) = decode_rune(&a[ai..]);
                        let invalid = ar == char::REPLACEMENT_CHARACTER && ar_len == 1;
                        if invalid {
                            return 0;
                        }
                        ai += ar_len;
                        (an, as_) = self.imp.get_weight(ar);
                    }
                } else {
                    an = as_;
                    as_ = 0;
                }
            }

            if bn == 0 {
                if bs == 0 {
                    while bn == 0 && bi < b.len() {
                        // When the byte sequence is not a valid UTF-8 encoding of a rune, Golang returns RuneError('�') and size 1.
                        // See https://pkg.go.dev/unicode/utf8#DecodeRune for more details.
                        // Here we check both the size and rune to distinguish between invalid byte sequence and valid '�'.
                        let (br, br_len) = decode_rune(&b[bi..]);
                        let invalid = br == char::REPLACEMENT_CHARACTER && br_len == 1;
                        if invalid {
                            return 0;
                        }
                        bi += br_len;
                        (bn, bs) = self.imp.get_weight(br);
                    }
                } else {
                    bn = bs;
                    bs = 0;
                }
            }

            if an == 0 || bn == 0 {
                // Go: sign(int(an) - int(bn)), in wrapping int64 arithmetic.
                return sign((an as i64).wrapping_sub(bn as i64) as isize);
            }

            if an == bn {
                (an, bn) = (0, 0);
                continue;
            }

            while an != 0 && bn != 0 {
                if (an ^ bn) & 0xFFFF != 0 {
                    return sign((an & 0xFFFF) as isize - (bn & 0xFFFF) as isize);
                }
                an >>= 16;
                bn >>= 16;
            }
        }
    }

    /// Key implements Collator interface.
    // go: unicodeCICollator.Key L1
    // go: unicode0900AICICollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        self.key_without_trim_right_space(self.imp.preprocess(str))
    }

    /// KeyWithoutTrimRightSpace implements Collator interface.
    // go: unicodeCICollator.KeyWithoutTrimRightSpace L1
    // go: unicode0900AICICollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, str: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(str.len() * 2);
        let mut si = 0; // decode index of s

        while si < str.len() {
            let (r, r_len) = decode_rune(&str[si..]);
            let invalid = r == char::REPLACEMENT_CHARACTER && r_len == 1;
            if invalid {
                return buf;
            }

            si += r_len;
            // weight of str. weight in unicode_ci may has 8 uint16s. sn indicate first 4 u16s, ss indicate last 4 u16s
            let (mut sn, mut ss) = self.imp.get_weight(r);

            while sn != 0 {
                buf.push(((sn & 0xFF00) >> 8) as u8);
                buf.push(sn as u8);
                sn >>= 16;
            }
            while ss != 0 {
                buf.push(((ss & 0xFF00) >> 8) as u8);
                buf.push(ss as u8);
                ss >>= 16;
            }
        }
        buf
    }

    /// Pattern implements Collator interface.
    // go: unicodeCICollator.Pattern L1
    // go: unicode0900AICICollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        self.imp.pattern()
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}
