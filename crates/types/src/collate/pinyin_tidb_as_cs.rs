//! go-file: pkg/util/collate/pinyin_tidb_as_cs.go

use std::any::Any;

use super::{Collator, WildcardPattern};

/// Collation of utf8mb4_zh_pinyin_tidb_as_cs
#[derive(Debug, Clone, Copy, Default)]
pub struct ZhPinyinTiDBASCSCollator;

impl Collator for ZhPinyinTiDBASCSCollator {
    /// Compare is not implemented.
    // go: zhPinyinTiDBASCSCollator.Compare L1
    fn compare(&self, _: &[u8], _: &[u8]) -> isize {
        panic!("implement me")
    }

    /// Key is not implemented.
    // go: zhPinyinTiDBASCSCollator.Key L1
    fn key(&self, _: &[u8]) -> Vec<u8> {
        panic!("implement me")
    }

    /// KeyWithoutTrimRightSpace is not implemented.
    // go: zhPinyinTiDBASCSCollator.KeyWithoutTrimRightSpace L1
    fn key_without_trim_right_space(&self, _: &[u8]) -> Vec<u8> {
        panic!("implement me")
    }

    /// Pattern is not implemented.
    // go: zhPinyinTiDBASCSCollator.Pattern L1
    fn pattern(&self) -> Box<dyn WildcardPattern> {
        panic!("implement me")
    }

    /// Clone is not implemented.
    // go: zhPinyinTiDBASCSCollator.Clone L1
    fn clone_collator(&self) -> Box<dyn Collator> {
        panic!("implement me")
    }

    // glue: Go type assertion support
    fn as_any(&self) -> &dyn Any {
        self
    }
}
