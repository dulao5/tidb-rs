//! go-file: pkg/util/collate/gbk_bin.go

use std::any::Any;

use tidb_parser::gocharset::encoding_gbk::{Encoder, new_custom_gbk_encoder};

use super::bin::DerivedBinPattern;
use super::{Collator, WildcardPattern, rune_len, sign, truncate_tailing_space};

/// gbkBinCollator is collator for gbk_bin.
#[derive(Debug, Clone, Copy)]
pub struct GbkBinCollator {
    e: Encoder,
}

impl GbkBinCollator {
    /// `&gbkBinCollator{charset.NewCustomGBKEncoder()}`.
    // glue: Go struct literal
    pub fn new() -> GbkBinCollator {
        GbkBinCollator {
            e: new_custom_gbk_encoder(),
        }
    }
}

impl Default for GbkBinCollator {
    // glue: Go struct literal
    fn default() -> Self {
        GbkBinCollator::new()
    }
}

impl Collator for GbkBinCollator {
    /// Clone implements Collator interface.
    // go: gbkBinCollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        Box::new(GbkBinCollator {
            e: new_custom_gbk_encoder(),
        })
    }

    /// Compare implement Collator interface.
    // go: gbkBinCollator.Compare L1
    fn compare(&self, a: &[u8], b: &[u8]) -> isize {
        let mut a = truncate_tailing_space(a);
        let mut b = truncate_tailing_space(b);

        // compare the character one by one.
        while !a.is_empty() && !b.is_empty() {
            let a_len = a.len().min(rune_len(a[0]));
            let b_len = b.len().min(rune_len(b[0]));
            // if convert error happened, we use '?'(0x3F) replace it.
            // It should not happen.
            let a_gbk = self.e.bytes(&a[..a_len]).unwrap_or_else(|_| vec![0x3F]);
            let b_gbk = self.e.bytes(&b[..b_len]).unwrap_or_else(|_| vec![0x3F]);

            let compare = super::cmp_compare(a_gbk.as_slice(), b_gbk.as_slice());
            if compare != 0 {
                return compare;
            }
            a = &a[a_len..];
            b = &b[b_len..];
        }

        sign(a.len() as isize - b.len() as isize)
    }

    /// Key implement Collator interface.
    // go: gbkBinCollator.Key L1
    fn key(&self, str: &[u8]) -> Vec<u8> {
        self.key_without_trim_right_space(truncate_tailing_space(str))
    }

    /// KeyWithoutTrimRightSpace implement Collator interface.
    // go: gbkBinCollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, mut str: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(str.len());
        while !str.is_empty() {
            let l = str.len().min(rune_len(str[0]));
            match self.e.bytes(&str[..l]) {
                Err(_) => buf.push(b'?'),
                Ok(gbk) => buf.extend_from_slice(&gbk),
            }
            str = &str[l..];
        }

        buf
    }

    /// Pattern implements Collator interface.
    // go: gbkBinCollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        // use binPattern directly, they are totally same.
        // (Go: gbkBinPattern embeds derivedBinPattern.)
        Box::new(DerivedBinPattern::default())
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}
