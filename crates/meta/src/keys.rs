//! Raw TiKV keys of TiDB's meta structures (Go: `pkg/meta/meta.go`, `pkg/structure`).
//!
//! All meta keys start with `m`. A *string* entry `key` lives at
//! `m + EncodeBytes(key) + EncodeUint('s')`; a *hash* `key` stores field `f` at
//! `m + EncodeBytes(key) + EncodeUint('h') + EncodeBytes(f)`. `EncodeBytes` is the
//! memcomparable byte encoding (8-byte groups plus a marker byte) and `EncodeUint` is a
//! big-endian u64.

use codec::prelude::BufferReader;
use codec::prelude::MemComparableByteDecoder;
use codec::prelude::MemComparableByteEncoder;
use codec::prelude::NumberEncoder;

/// Prefix of every meta key (Go: `mMetaPrefix`).
pub const META_PREFIX: &[u8] = b"m";
/// String key holding the global schema version (Go: `mSchemaVersionKey`).
pub const SCHEMA_VERSION_KEY: &[u8] = b"SchemaVersionKey";
/// Hash of all databases, field `DB:{id}` (Go: `mDBs`).
pub const DBS_KEY: &[u8] = b"DBs";

const STRING_DATA: u8 = b's';
const HASH_DATA: u8 = b'h';

fn encode_prefix(key: &[u8], flag: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(META_PREFIX.len() + key.len() + 24);
    out.extend_from_slice(META_PREFIX);
    // Writing into a growable Vec cannot fail.
    out.write_comparable_bytes(key)
        .expect("write to Vec cannot fail");
    out.write_u64(u64::from(flag))
        .expect("write to Vec cannot fail");
    out
}

/// Raw key of the string entry `key` (Go: `TxStructure.EncodeStringDataKey`).
pub fn string_data_key(key: &[u8]) -> Vec<u8> {
    encode_prefix(key, STRING_DATA)
}

/// Common prefix of every field of hash `key` (Go: `TxStructure.hashDataKeyPrefix`).
pub fn hash_data_prefix(key: &[u8]) -> Vec<u8> {
    encode_prefix(key, HASH_DATA)
}

/// Raw key of field `field` of hash `key` (Go: `TxStructure.EncodeHashDataKey`).
pub fn hash_data_key(key: &[u8], field: &[u8]) -> Vec<u8> {
    let mut out = hash_data_prefix(key);
    out.write_comparable_bytes(field)
        .expect("write to Vec cannot fail");
    out
}

/// The field of a hash data key, given the hash's prefix (Go: `decodeHashDataKey`).
pub fn decode_hash_field(raw_key: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    let mut rest = raw_key.strip_prefix(prefix)?;
    let field = rest.read_comparable_bytes().ok()?;
    rest.bytes().is_empty().then_some(field)
}

/// The exclusive end of all keys starting with `prefix` (Go: `kv.Key.PrefixNext`).
pub fn prefix_next(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == u8::MAX {
            out.pop();
        } else {
            *last += 1;
            return out;
        }
    }
    // All bytes were 0xFF: unbounded.
    Vec::new()
}

/// Hash field of a database in [`DBS_KEY`], also the hash key of its tables (Go: `dbKey`).
pub fn db_key(db_id: i64) -> Vec<u8> {
    format!("DB:{db_id}").into_bytes()
}

/// Hash field of a table in its database hash (Go: `tableKey`).
pub fn table_key(table_id: i64) -> Vec<u8> {
    format!("Table:{table_id}").into_bytes()
}

/// String key of the schema diff of `version` (Go: `schemaDiffKey`).
pub fn schema_diff_key(version: i64) -> Vec<u8> {
    format!("Diff:{version}").into_bytes()
}

/// Table id of a `Table:{id}` hash field (Go: `ParseTableKey`); `None` for other fields
/// in the database hash (auto-id counters `TID:`, `IID:`, sequences, ...).
pub fn parse_table_key(field: &[u8]) -> Option<i64> {
    std::str::from_utf8(field)
        .ok()?
        .strip_prefix("Table:")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Vectors printed by a Go program built against TiDB 4cee7f2084: `pkg/util/codec`'s
    // EncodeBytes/EncodeUint composed exactly as `pkg/structure` does
    // (`"m" + EncodeBytes(key) + EncodeUint(flag) [+ EncodeBytes(field)]`).
    #[test]
    fn schema_version_key_matches_go() {
        assert_eq!(
            string_data_key(SCHEMA_VERSION_KEY),
            hex("6d536368656d615665ff7273696f6e4b6579ff0000000000000000f70000000000000073")
        );
    }

    #[test]
    fn schema_diff_key_matches_go() {
        assert_eq!(
            string_data_key(&schema_diff_key(42)),
            hex("6d446966663a343200fe0000000000000073")
        );
    }

    #[test]
    fn hash_keys_match_go() {
        assert_eq!(
            hash_data_key(DBS_KEY, &db_key(2)),
            hex("6d4442730000000000fa000000000000006844423a3200000000fb")
        );
        assert_eq!(
            hash_data_key(&db_key(114), &table_key(120)),
            hex("6d44423a3131340000fd00000000000000685461626c653a3132ff3000000000000000f8")
        );
    }

    #[test]
    fn hash_field_round_trip() {
        let prefix = hash_data_prefix(&db_key(114));
        let key = hash_data_key(&db_key(114), &table_key(120));
        assert!(key.starts_with(&prefix));
        assert_eq!(decode_hash_field(&key, &prefix).unwrap(), b"Table:120");
        // A key of another hash or with trailing garbage is rejected.
        assert_eq!(decode_hash_field(&key, &hash_data_prefix(b"DB:11")), None);
        let mut bad = key.clone();
        bad.push(0);
        assert_eq!(decode_hash_field(&bad, &prefix), None);
    }

    #[test]
    fn prefix_next_works() {
        assert_eq!(prefix_next(b"ab"), b"ac");
        assert_eq!(prefix_next(&[1, 0xFF, 0xFF]), vec![2]);
        assert_eq!(prefix_next(&[0xFF]), Vec::<u8>::new());
    }

    #[test]
    fn table_key_parsing() {
        assert_eq!(parse_table_key(b"Table:120"), Some(120));
        assert_eq!(parse_table_key(b"TID:120"), None);
        assert_eq!(parse_table_key(b"Table:x"), None);
    }
}
