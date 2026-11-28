//! Regression tests for the panic fixes that the vendored TiKV fork carries on top of its base
//! (`third_party/tikv/PROVENANCE.md`, "Local patches (where the fork differs from TiKV v8.5.8)").
//!
//! TiKV v8.5.8 panics on both inputs below. The fork's fixes return what Go
//! TiDB returns for the same expression: `MinInt64 % -1` is 0, and U+2CEA1 under
//! `utf8mb4_0900_ai_ci` gets the implicit weight of a code point past the weight table, as in
//! tidb-rs's collator (ruling R3 of `docs/testing/rulings.toml`, the upstream fix of Go's
//! off-by-one). The coprocessor evaluation oracle (`copr_eval_oracle.rs`) covers the same
//! inputs end to end; these tests name the two fixed functions directly.

use std::cmp::Ordering;

use tidb_query_datatype::codec::collation::{Collator, collator::CollatorUtf8Mb40900AiCi};
use tidb_query_expr::impl_arithmetic::{ArithmeticOp, IntIntMod};

/// `IntIntMod::calc` (BIGINT % BIGINT): `i64::MIN % -1` used to panic with "attempt to
/// calculate the remainder with overflow". Go's `a % b` and MySQL both give 0.
#[test]
fn signed_mod_of_min_by_minus_one_is_zero() {
    assert_eq!(IntIntMod::calc(&i64::MIN, &-1).unwrap(), Some(0));
    assert_eq!(IntIntMod::calc(&i64::MIN, &1).unwrap(), Some(0));
    assert_eq!(IntIntMod::calc(&i64::MIN, &7).unwrap(), Some(-1));
    assert_eq!(IntIntMod::calc(&-7, &-1).unwrap(), Some(0));
    assert_eq!(IntIntMod::calc(&7, &0).unwrap(), None);
}

/// `Unicode0900::char_weight`: U+2CEA1 is the first code point past the 0x2CEA1-entry weight
/// table and used to index past its end ("index out of bounds: the len is 183969").
#[test]
fn utf8mb4_0900_ai_ci_compares_u2cea1() {
    let last = "\u{2CEA1}".as_bytes();
    assert_eq!(last, b"\xF0\xAC\xBA\xA1");
    let before = "\u{2CEA0}".as_bytes();
    let after = "\u{2CEA2}".as_bytes();
    let cmp = |a: &[u8], b: &[u8]| CollatorUtf8Mb40900AiCi::sort_compare(a, b, false).unwrap();
    assert_eq!(cmp(last, last), Ordering::Equal);
    assert_eq!(cmp(last, b"a"), Ordering::Greater);
    // Past the table every code point gets its implicit weight, in code point order.
    assert_eq!(cmp(last, after), Ordering::Less);
    assert_ne!(cmp(before, last), Ordering::Equal);
    // The sort key is the same implicit weight tidb-rs's collator gives: 0xFBC5 then 0xCEA1
    // ((r >> 15) + 0xFBC0, then (r & 0x7FFF) | 0x8000), each as two big-endian bytes.
    assert_eq!(
        CollatorUtf8Mb40900AiCi::sort_key(last).unwrap(),
        vec![0xFB, 0xC5, 0xCE, 0xA1]
    );
}
