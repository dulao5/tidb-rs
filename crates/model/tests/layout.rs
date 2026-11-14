//! Memory layout of the long-lived model structs (spec N13 §9, issue #229 batch E).
//!
//! Each `TableInfo` / `ColumnInfo` / `IndexInfo` of the infoschema is one `Arc` allocation:
//! `ArcInner<T>` = two reference counts (16 B) + `T`. The server's global allocator is
//! jemalloc (`crates/poc/server/src/main.rs`, every platform), which rounds a request up to a
//! size class: 16 B steps up to 128 B, then four classes per doubling (…, 256, 320, 384, 448,
//! 512, 640, …). Shrinking a struct saves memory only when `ArcInner<T>` crosses into a lower
//! class, and growing it by a single byte past a class boundary costs a whole class step on
//! every column of every table. These tests pin each struct to its current class.
//!
//! The second half checks that the fields boxed to get there (`Option<Box<…>>`, spec N13 §9)
//! keep the Go JSON bytes for nil, non-nil empty and non-empty values.

use std::collections::BTreeMap;
use std::mem::size_of;

use tidb_model::{ColumnInfo, IndexInfo, TableInfo};

/// The `ArcInner` header: strong + weak counts.
const ARC_HEADER: usize = 16;

/// ColumnInfo: `ArcInner<ColumnInfo>` must stay in jemalloc's 384 B class (ColumnInfo ≤ 368 B;
/// 400 B before batch E put it in the 448 B class, +64 B per column). A new field or a wider
/// type that breaks this must box a rarely set field instead (spec N13 §9.2).
#[test]
fn column_info_fits_the_384_byte_class() {
    assert!(
        size_of::<ColumnInfo>() <= 368,
        "size_of::<ColumnInfo>() = {}",
        size_of::<ColumnInfo>()
    );
    assert!(size_of::<ColumnInfo>() + ARC_HEADER <= 384);
}

/// IndexInfo: `ArcInner<IndexInfo>` stays in the 256 B class (not shrunk yet; spec N13 §9.5).
#[test]
fn index_info_fits_the_256_byte_class() {
    assert!(
        size_of::<IndexInfo>() + ARC_HEADER <= 256,
        "size_of::<IndexInfo>() = {}",
        size_of::<IndexInfo>()
    );
}

/// TableInfo: `ArcInner<TableInfo>` stays in the 512 B class.
#[test]
fn table_info_fits_the_512_byte_class() {
    assert!(
        size_of::<TableInfo>() + ARC_HEADER <= 512,
        "size_of::<TableInfo>() = {}",
        size_of::<TableInfo>()
    );
}

/// The three boxed members of a ColumnInfo's JSON, in encoding order.
fn boxed_members(c: &ColumnInfo) -> (String, String, String) {
    let j = c.marshal_json().unwrap();
    let member = |name: &str| {
        let key = format!("\"{name}\":");
        let start = j.find(&key).unwrap() + key.len();
        let rest = &j[start..];
        // The values here are null, a base64 string or a flat object of empty objects.
        let end = if rest.starts_with('{') {
            rest.find("}}")
                .map_or_else(|| rest.find('}').unwrap() + 1, |i| i + 2)
        } else {
            rest.find(',').unwrap()
        };
        rest[..end].to_string()
    };
    (
        member("origin_default_bit"),
        member("default_bit"),
        member("dependences"),
    )
}

fn decoded(json: &str) -> ColumnInfo {
    let mut c = ColumnInfo::default();
    c.unmarshal_json(json.as_bytes()).unwrap();
    c
}

/// nil (`None`) encodes as `null` and `null` decodes to `None`.
#[test]
fn boxed_fields_nil() {
    let c = ColumnInfo::default();
    assert_eq!(c.origin_default_value_bit, None);
    assert_eq!(c.dependences, None);
    assert_eq!(
        boxed_members(&c),
        ("null".into(), "null".into(), "null".into())
    );
    let mut f =
        decoded(r#"{"origin_default_bit":"AQ==","default_bit":"AQ==","dependences":{"a":{}}}"#);
    f.unmarshal_json(br#"{"origin_default_bit":null,"default_bit":null,"dependences":null}"#)
        .unwrap();
    assert_eq!(
        (
            f.origin_default_value_bit,
            f.default_value_bit,
            f.dependences
        ),
        (None, None, None)
    );
}

/// Non-nil empty (`Some` of an empty value) encodes as `""` / `{}`, not `null`, and decodes back
/// to `Some` of an empty value.
#[test]
fn boxed_fields_empty() {
    let c = ColumnInfo {
        origin_default_value_bit: Some(Box::default()),
        default_value_bit: Some(Vec::new().into_boxed_slice()),
        dependences: Some(Box::default()),
        ..Default::default()
    };
    let members = ("\"\"".to_string(), "\"\"".to_string(), "{}".to_string());
    assert_eq!(boxed_members(&c), members);
    let d = decoded(r#"{"origin_default_bit":"","default_bit":[],"dependences":{}}"#);
    assert_eq!(d.origin_default_value_bit.as_deref(), Some(&[][..]));
    assert_eq!(d.default_value_bit.as_deref(), Some(&[][..]));
    assert_eq!(d.dependences.as_deref(), Some(&BTreeMap::new()));
    assert_eq!(boxed_members(&d), members);
}

/// Non-empty values encode as base64 / a sorted object; decoding an object into an existing map
/// merges (encoding/json), and bytes replace the old value.
#[test]
fn boxed_fields_non_empty() {
    let c = ColumnInfo {
        origin_default_value_bit: Some(vec![3, 255].into_boxed_slice()),
        default_value_bit: Some(vec![1, 2].into_boxed_slice()),
        dependences: Some(Box::new(BTreeMap::from([
            ("b".to_string(), ()),
            ("a".to_string(), ()),
        ]))),
        ..Default::default()
    };
    assert_eq!(
        boxed_members(&c),
        (
            "\"A/8=\"".into(),
            "\"AQI=\"".into(),
            r#"{"a":{},"b":{}}"#.into()
        )
    );
    let mut d = decoded(&c.marshal_json().unwrap());
    assert_eq!(d, c);
    d.unmarshal_json(br#"{"origin_default_bit":[9],"default_bit":"Bw==","dependences":{"c":{}}}"#)
        .unwrap();
    assert_eq!(d.origin_default_value_bit.as_deref(), Some(&[9u8][..]));
    assert_eq!(d.default_value_bit.as_deref(), Some(&[7u8][..]));
    assert_eq!(boxed_members(&d).2, r#"{"a":{},"b":{},"c":{}}"#.to_string());
}
