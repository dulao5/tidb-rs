//! Ports of Go unit tests of `pkg/types/json_binary_test.go`, `json_binary_functions_test.go`
//! and `json_path_expr_test.go` (test files are not in the tracker's func_blocks.tsv, so the
//! tests carry `glue:` annotations naming the Go test).
//!
//! The Go unit tests whose assertions the govec `json` domain cannot express (they call
//! unexported functions or inspect internal state); every other test of the three files is
//! covered by govec records with the test function as `src` (literals mined with
//! `cmd/minelits`, expected values from running Go). See specs/D01.5-json/spec.md §5.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use super::json_binary::{BinaryJSON, JSONValue, Opaque, create_binary_json};
use super::json_binary_functions::{
    decode_escaped_unicode, peek_bytes_as_json, unquote_json_string,
};
use super::json_constants::JSONTypeCodeOpaque;
use super::json_path_expr::{
    ArraySelection, PathExpressionFlagExt, json_path_array_index_from_start, parse_json_path_expr,
};

// glue: port of the Go unit test TestContainsAnyAsterisk
#[test]
fn contains_any_asterisk() {
    for (expression, contains) in [
        ("$.a[1]", false),
        ("$.a[*]", true),
        ("$.*[1]", true),
        ("$**.a[1]", true),
    ] {
        let pe = parse_json_path_expr(expression.as_bytes()).unwrap();
        assert_eq!(pe.flags.contains_any_asterisk(), contains, "{expression}");
    }
}

// glue: port of the Go unit test TestPushBackOneIndexLeg
#[test]
fn push_back_one_index_leg() {
    for (expression, index, expected, multi) in [
        ("$", 1, "$[1]", false),
        ("$.a[1]", 1, "$.a[1][1]", false),
        ("$.a[*]", 10, "$.a[*][10]", true),
        ("$.*[2]", 2, "$.*[2][2]", true),
        ("$**.a[3]", 3, "$**.a[3][3]", true),
        ("$.a[1 to 3]", 3, "$.a[1 to 3][3]", true),
        ("$.a[last-3 to last-3]", 3, "$.a[last-3 to last-3][3]", true),
        ("$**.a[3]", -3, "$**.a[3][last-2]", true),
    ] {
        let pe = parse_json_path_expr(expression.as_bytes()).unwrap();
        let pe = pe.push_back_one_array_selection_leg(ArraySelection::Index(
            json_path_array_index_from_start(index),
        ));
        assert_eq!(String::from_utf8(pe.string()).unwrap(), expected);
        assert_eq!(pe.could_match_multiple_values(), multi, "{expression}");
    }
}

// glue: port of the Go unit test TestPushBackOneKeyLeg
#[test]
fn push_back_one_key_leg() {
    for (expression, key, expected, multi) in [
        ("$", "aa", "$.aa", false),
        ("$.a[1]", "aa", "$.a[1].aa", false),
        ("$.a[1]", "*", "$.a[1].*", true),
        ("$.a[*]", "k", "$.a[*].k", true),
        ("$.*[2]", "bb", "$.*[2].bb", true),
        ("$**.a[3]", "cc", "$**.a[3].cc", true),
    ] {
        let pe = parse_json_path_expr(expression.as_bytes()).unwrap();
        let pe = pe.push_back_one_key_leg(key.as_bytes().to_vec());
        assert_eq!(String::from_utf8(pe.string()).unwrap(), expected);
        assert_eq!(pe.could_match_multiple_values(), multi, "{expression}");
    }
}

// glue: port of the Go unit test TestDecodeEscapedUnicode
#[test]
fn decode_escaped_unicode_test() {
    let (r, size) = decode_escaped_unicode(b"597d").unwrap();
    assert_eq!(&r, "好\x00".as_bytes());
    assert_eq!(size, 3);
}

// glue: port of the Go unit test TestFunctions
#[test]
fn functions() {
    let test_byte = b"\\bfnrtuz0";
    assert_eq!(unquote_json_string(test_byte).unwrap(), b"\x08fnrtuz0");
    let err = peek_bytes_as_json(test_byte).unwrap_err();
    assert_eq!(err.to_string(), "Invalid JSON bytes");
    let err = peek_bytes_as_json(b"").unwrap_err();
    assert_eq!(err.to_string(), "Cant peek from empty bytes");
}

// glue: port of the Go unit test TestBinaryJSONOpaque
#[test]
fn binary_json_opaque() {
    let mut long = vec![233, 0x80, 0x01];
    long.extend_from_slice(&[0; 128]);
    let cases = [
        (
            vec![233, 1, b'9'],
            vec![b'9'],
            r#""base64:type233:OQ==""#.to_owned(),
        ),
        (long, vec![0; 128], r#""base64:type233:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=""#.to_owned()),
    ];
    for (value, buf, output) in cases {
        let bj = BinaryJSON::owned(JSONTypeCodeOpaque, value);
        let expected = Opaque {
            type_code: 233,
            buf: Cow::Owned(buf),
        };
        assert_eq!(bj.get_opaque_field_type(), expected.type_code);
        assert_eq!(bj.get_opaque(), expected);
        assert_eq!(
            String::from_utf8(bj.marshal_to(Vec::new()).unwrap()).unwrap(),
            output
        );
    }
}

// glue: port of the Go unit test TestHashValue
#[test]
fn hash_value_distinct() {
    let arr = |v: Vec<JSONValue<'static>>| JSONValue::Array(v);
    let bj = |v: JSONValue<'static>| JSONValue::BinaryJSON(create_binary_json(v));
    let jsons = [
        create_binary_json(arr(vec![])),
        create_binary_json(arr(vec![bj(arr(vec![]))])),
        create_binary_json(arr(vec![bj(arr(vec![bj(arr(vec![]))]))])),
        create_binary_json(JSONValue::Object(BTreeMap::new())),
        create_binary_json(arr(vec![bj(JSONValue::Bool(false))])),
        create_binary_json(arr(vec![bj(JSONValue::Bool(true))])),
        create_binary_json(arr(vec![bj(JSONValue::Nil)])),
    ];
    let distinct: HashSet<Vec<u8>> = jsons.iter().map(|j| j.hash_value(Vec::new())).collect();
    assert_eq!(distinct.len(), jsons.len());
}
