//! `gostd::json::Decoder::map` with values whose `UnmarshalJSON` fails (D06.1 part 2 review:
//! the first real user is `DDLReorgMeta.Warnings`, a `map[errors.ErrorID]*terror.Error`).
//!
//! Go: `decodeState.object` decodes each value into a fresh zero value before it stores the
//! entry; an error returned by the value's `UnmarshalJSON` returns at once, so that entry and
//! all later members are dropped while the earlier ones stay. The expected values below are
//! the output of Go 1.25 for
//!
//! ```go
//! m := map[string]*errors.Error{"a": errors.Normalize("old", errors.RFCCodeText("ddl:1"), errors.MySQLErrorCode(1))}
//! err := json.Unmarshal([]byte(text), &m)
//! out, _ := json.Marshal(m)
//! ```
//!
//! (pingcap/errors v0.11.5-0.20250523034308-74f78ae071ee).

use std::collections::BTreeMap;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gostd::json::{self, Decoder, Raw};

fn decode(m: &mut Option<BTreeMap<String, Option<Error>>>, text: &str) -> Option<String> {
    let v = json::unmarshal_raw(text.as_bytes()).unwrap();
    let mut d = Decoder::new();
    d.map(
        &v,
        m,
        "map[string]*errors.Error",
        |_, k| Some(k.to_owned()),
        |d, v, x: &mut Option<Error>| match v {
            Raw::Null => *x = None,
            _ => {
                let e = x.get_or_insert_with(Default::default);
                if let Err(err) = e.unmarshal_json_raw(v) {
                    d.fail(err);
                }
            }
        },
    );
    d.finish().err().map(|e| e.to_string())
}

fn encode(m: &Option<BTreeMap<String, Option<Error>>>) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in m.as_ref().unwrap().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::append_string(&mut out, k);
        out.push(':');
        match v {
            None => out.push_str("null"),
            Some(e) => out.push_str(&e.marshal_json()),
        }
    }
    out.push('}');
    out
}

fn base() -> Option<BTreeMap<String, Option<Error>>> {
    let old = errors::normalize(
        "old",
        [errors::rfc_code_text("ddl:1"), errors::mysql_error_code(1)],
    );
    Some(BTreeMap::from([("a".to_owned(), Some(old))]))
}

#[test]
fn map_value_unmarshaler_error() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            r#"{"b":{"code":2},"c":{"code":"x"},"d":{"code":4}}"#,
            r#"{"a":{"class":2,"code":1,"message":"old","rfccode":"ddl:1"},"b":{"class":0,"code":2,"message":"","rfccode":""}}"#,
            Some("json: cannot unmarshal string into Go struct field jsonError.code of type int"),
        ),
        (
            r#"{"b":5,"c":{"code":3}}"#,
            r#"{"a":{"class":2,"code":1,"message":"old","rfccode":"ddl:1"}}"#,
            Some("json: cannot unmarshal number into Go value of type errors.jsonError"),
        ),
        (
            r#"{"a":null,"b":{"class":2,"code":7}}"#,
            r#"{"a":null,"b":{"class":2,"code":7,"message":"","rfccode":"ddl:7"}}"#,
            None,
        ),
        (
            r#"{"b":{"code":"x"}}"#,
            r#"{"a":{"class":2,"code":1,"message":"old","rfccode":"ddl:1"}}"#,
            Some("json: cannot unmarshal string into Go struct field jsonError.code of type int"),
        ),
    ];
    for (text, want, want_err) in cases {
        let mut m = base();
        let err = decode(&mut m, text);
        assert_eq!(encode(&m), *want, "{text}");
        assert_eq!(err.as_deref(), *want_err, "{text}");
    }
}
