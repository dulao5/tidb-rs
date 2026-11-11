//! go-file: pkg/structure/type.go
//!
//! The key layout: `prefix + EncodeBytes(key) + EncodeUint(flag) [+ ...]`.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_codec::decode_bytes;
use tidb_codec::decode_uint;
use tidb_codec::encode_bytes;
use tidb_codec::encode_int;
use tidb_codec::encode_uint;

use super::ErrInvalidHashKeyFlag;
use super::TxStructure;

/// TypeFlag is for data structure meta/data flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeFlag(pub u8);

impl TypeFlag {
    /// StringMeta is the flag for string meta.
    pub const StringMeta: TypeFlag = TypeFlag(b'S');
    /// StringData is the flag for string data.
    pub const StringData: TypeFlag = TypeFlag(b's');
    /// HashMeta is the flag for hash meta.
    pub const HashMeta: TypeFlag = TypeFlag(b'H');
    /// HashData is the flag for hash data.
    pub const HashData: TypeFlag = TypeFlag(b'h');
    /// ListMeta is the flag for list meta.
    pub const ListMeta: TypeFlag = TypeFlag(b'L');
    /// ListData is the flag for list data.
    pub const ListData: TypeFlag = TypeFlag(b'l');
}

/// `errors.New("invalid encoded hash data key prefix")` (also for string keys, as in Go).
// glue: the plain error of the prefix checks of decodeStringDataKey / decodeHashDataKey
fn err_bad_prefix() -> Error {
    tidb_base::errors::new("invalid encoded hash data key prefix")
}

impl TxStructure {
    /// EncodeStringDataKey will encode string key.
    // go: TxStructure.EncodeStringDataKey L1
    pub fn encode_string_data_key(&self, key: &[u8]) -> Vec<u8> {
        // for codec Encode, we may add extra bytes data, so here and following encode
        // we will use extra length like 4 for a little optimization.
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + 24);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::StringData.0));
        ek
    }

    // go: TxStructure.decodeStringDataKey L1
    pub(crate) fn decode_string_data_key(&self, ek: &[u8]) -> Result<Vec<u8>, Error> {
        if !ek.starts_with(&self.prefix) {
            return Err(err_bad_prefix());
        }
        let ek = &ek[self.prefix.len()..];
        let (ek, key) = decode_bytes(ek, None)?;
        let (_, tp) = decode_uint(ek)?;
        if tp != u64::from(TypeFlag::StringData.0) {
            return Err(ErrInvalidHashKeyFlag.gen_with_stack(
                "invalid encoded string data key flag %c",
                vec![Arg::Uint(u64::from(tp as u8), "uint8")],
            ));
        }
        Ok(key)
    }

    /// EncodeHashMetaKey encodes hash meta key.
    // go: TxStructure.EncodeHashMetaKey L1
    pub fn encode_hash_meta_key(&self, key: &[u8]) -> Vec<u8> {
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + 16);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::HashMeta.0));
        ek
    }

    // go: TxStructure.encodeHashDataKey L1
    pub(crate) fn encode_hash_data_key_inner(&self, key: &[u8], field: &[u8]) -> Vec<u8> {
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + field.len() + 24);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::HashData.0));
        encode_bytes(&mut ek, field);
        ek
    }

    /// EncodeHashDataKey exports for tests.
    // go: TxStructure.EncodeHashDataKey L1
    pub fn encode_hash_data_key(&self, key: &[u8], field: &[u8]) -> Vec<u8> {
        self.encode_hash_data_key_inner(key, field)
    }

    // go: TxStructure.decodeHashDataKey L1
    pub(crate) fn decode_hash_data_key(&self, ek: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error> {
        if !ek.starts_with(&self.prefix) {
            return Err(err_bad_prefix());
        }
        let ek = &ek[self.prefix.len()..];
        let (ek, key) = decode_bytes(ek, None)?;
        let (ek, tp) = decode_uint(ek)?;
        if tp != u64::from(TypeFlag::HashData.0) {
            return Err(ErrInvalidHashKeyFlag.gen_with_stack(
                "invalid encoded hash data key flag %c",
                vec![Arg::Uint(u64::from(tp as u8), "uint8")],
            ));
        }
        let (_, field) = decode_bytes(ek, None)?;
        Ok((key, field))
    }

    // go: TxStructure.hashDataKeyPrefix L1
    pub(crate) fn hash_data_key_prefix(&self, key: &[u8]) -> Vec<u8> {
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + 24);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::HashData.0));
        ek
    }

    // go: TxStructure.encodeListMetaKey L1
    pub(crate) fn encode_list_meta_key(&self, key: &[u8]) -> Vec<u8> {
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + 24);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::ListMeta.0));
        ek
    }

    // go: TxStructure.encodeListDataKey L1
    pub(crate) fn encode_list_data_key(&self, key: &[u8], index: i64) -> Vec<u8> {
        let mut ek = Vec::with_capacity(self.prefix.len() + key.len() + 36);
        ek.extend_from_slice(&self.prefix);
        encode_bytes(&mut ek, key);
        encode_uint(&mut ek, u64::from(TypeFlag::ListData.0));
        encode_int(&mut ek, index);
        ek
    }
}
