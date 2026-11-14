//! `getUint64FromNUM`: LIMIT counts and FOR UPDATE WAIT seconds above MaxInt64 (NUM's i64
//! value cannot carry them) are read from the source text, through the grammar that
//! `tools/parser-port/handport.py` generates (#210 review).

use tidb_parser::ast::{Datum, Expr, Stmt};
use tidb_parser::{ParseOptions, parse};

fn select(sql: &str) -> Box<tidb_parser::ast::SelectStmt> {
    match parse(sql, &ParseOptions::default())
        .unwrap()
        .stmts
        .remove(0)
    {
        Stmt::Select(s) => s,
        other => panic!("{other:?}"),
    }
}

#[test]
fn limit_above_max_int64() {
    let s = select("SELECT a FROM t LIMIT 18446744073709551615");
    match &s.limit.as_ref().unwrap().count {
        Expr::Value(v) => assert!(matches!(v.datum, Datum::Uint(u64::MAX)), "{v:?}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn for_update_wait_above_max_int64() {
    for (sql, want) in [
        ("SELECT a FROM t FOR UPDATE WAIT 5", 5),
        (
            "SELECT a FROM t FOR UPDATE WAIT /* c */ 9223372036854775808",
            1 << 63,
        ),
        (
            "SELECT a FROM t FOR UPDATE WAIT 18446744073709551615",
            u64::MAX,
        ),
    ] {
        assert_eq!(
            select(sql).lock_info.as_ref().unwrap().wait_sec,
            want,
            "{sql}"
        );
    }
}
