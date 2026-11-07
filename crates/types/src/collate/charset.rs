//! go-file: pkg/util/collate/charset.go

use tidb_parser::gocharset::{
    CharsetGBK, CollationGBKBin, CollationGBKChineseCI, character_set_infos,
};

/// switchDefaultCollation switch the default collation for charset according to the new collation config.
// go: switchDefaultCollation L1
pub(super) fn switch_default_collation(flag: bool) {
    let infos = character_set_infos().read().unwrap();
    let gbk = &infos[CharsetGBK];
    if flag {
        gbk.set_default_collation(CollationGBKChineseCI);
    } else {
        gbk.set_default_collation(CollationGBKBin);
    }
    let collations = gbk.collations();
    collations[CollationGBKBin].set_is_default(!flag);
    collations[CollationGBKChineseCI].set_is_default(flag);
}
