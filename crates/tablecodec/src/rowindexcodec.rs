//! go-file: pkg/tablecodec/rowindexcodec/rowindexcodec.go
//!
//! The minimal key-kind test used by the resource group tagger.

/// KeyKind is a specific type of key, mainly used to distinguish row/index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// KeyKindUnknown indicates that this key is unknown type.
    KeyKindUnknown,
    /// KeyKindRow means that this key belongs to row.
    KeyKindRow,
    /// KeyKindIndex means that this key belongs to index.
    KeyKindIndex,
}

const TABLE_PREFIX: &[u8] = b"t";
const ROW_PREFIX: &[u8] = b"_r";
const INDEX_PREFIX: &[u8] = b"_i";

/// GetKeyKind returns the KeyKind that matches the key in the minimalist way.
// go: GetKeyKind L1
pub fn get_key_kind(key: &[u8]) -> KeyKind {
    // [ TABLE_PREFIX | TABLE_ID | ROW_PREFIX (INDEX_PREFIX) | ROW_ID (INDEX_ID) | ... ]   (name)
    // [      1       |    8     |            2              |         8         | ... ]   (byte)
    if key.len() < 11 {
        return KeyKind::KeyKindUnknown;
    }
    if !key.starts_with(TABLE_PREFIX) {
        return KeyKind::KeyKindUnknown;
    }
    let key = &key[9..];
    if key.starts_with(ROW_PREFIX) {
        return KeyKind::KeyKindRow;
    }
    if key.starts_with(INDEX_PREFIX) {
        return KeyKind::KeyKindIndex;
    }
    KeyKind::KeyKindUnknown
}
