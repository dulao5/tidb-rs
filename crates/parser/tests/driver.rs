//! Focused tests for the LR driver: error positions, action aborts, stack reuse.
use tidb_parser::{Diagnostic, ParseError, ParseOptions, Parser, parse};

fn err(sql: &str) -> ParseError {
    parse(sql, &ParseOptions::default()).expect_err(sql)
}

#[test]
fn syntax_error_reports_offending_token() {
    assert_eq!(
        err("SELECT * FORM t"),
        ParseError::Syntax {
            pos: 9,
            near: "FORM".into()
        }
    );
}

#[test]
fn syntax_error_at_end_of_input() {
    assert_eq!(
        err("SELECT 1 +"),
        ParseError::Syntax {
            pos: 10,
            near: String::new()
        }
    );
    assert_eq!(
        err("SELECT a FROM t WHERE"),
        ParseError::Syntax {
            pos: 21,
            near: String::new()
        }
    );
}

#[test]
fn action_error_aborts_with_position() {
    // goyacc: `yylex.AppendError(ErrWrongUsage...); return 1` in SelectStmtOpts.
    assert_eq!(
        err("SELECT ALL DISTINCT a FROM t"),
        ParseError::Semantic(Diagnostic {
            msg: "Incorrect usage of ALL and DISTINCT".into(),
            pos: 7,
            code: 0
        })
    );
    // TableName with an invalid schema name.
    assert_eq!(
        err("SELECT * FROM `a `.t"),
        ParseError::Semantic(Diagnostic {
            msg: "Incorrect database name 'a '".into(),
            pos: 14,
            code: 0
        })
    );
}

#[test]
fn charset_and_collation_actions_carry_their_code() {
    // goyacc: `CharsetName` appends `ErrUnknownCharacterSet` (1115), `CollationName` the
    // `ErrUnknownCollation` of `charset.GetCollationByName` (1273).
    assert_eq!(
        err("SET NAMES nosuch"),
        ParseError::Semantic(Diagnostic {
            msg: "Unknown character set: 'nosuch'".into(),
            pos: 10,
            code: 1115
        })
    );
    assert!(matches!(
        err("SELECT 'a' COLLATE nosuch"),
        ParseError::Semantic(Diagnostic { code: 1273, .. })
    ));
    // The name is cut at 64 characters (`'%-.64s'` of `MySQLErrName`).
    let long = "x".repeat(70);
    assert_eq!(
        err(&format!("SET NAMES {long}")),
        ParseError::Semantic(Diagnostic {
            msg: format!("Unknown character set: '{}'", &long[..64]),
            pos: 10,
            code: 1115
        })
    );
    // A known introducer without a legacy collation (Go's TestUnderscoreCharset) and
    // `UNICODE` (ucs2) are `ErrUnknownCharacterSet` too.
    assert!(matches!(
        err("select hex(_ujis '3F')"),
        ParseError::Semantic(Diagnostic { code: 1115, ref msg, .. })
            if msg == "Unsupported character introducer: 'ujis'"
    ));
    assert!(matches!(
        err("CREATE TABLE t (a TEXT UNICODE)"),
        ParseError::Semantic(Diagnostic { code: 1115, ref msg, .. })
            if msg == "Unknown character set: 'ucs2'"
    ));
}

#[test]
fn abort_stops_before_later_syntax_error() {
    // The driver stops right after the aborting reduction, like goyacc's `return 1`, so the
    // syntax error after it is never reached.
    assert!(matches!(
        err("SELECT ALL DISTINCT 1 FROM FROM"),
        ParseError::Semantic(Diagnostic { pos: 7, .. })
    ));
}

#[test]
fn lex_error() {
    assert!(matches!(
        err("SELECT 'abc"),
        ParseError::Lex { pos: 11, .. }
    ));
}

#[test]
fn statement_list() {
    let o = ParseOptions::default();
    assert_eq!(parse("SELECT 1;;SELECT 2;", &o).unwrap().stmts.len(), 2);
    assert_eq!(parse("", &o).unwrap().stmts.len(), 0);
    assert_eq!(parse(";", &o).unwrap().stmts.len(), 0);
}

#[test]
fn parser_is_reusable_after_errors() {
    let o = ParseOptions::default();
    let mut p = Parser::new();
    for _ in 0..3 {
        assert!(p.parse("SELECT ALL DISTINCT 1 FROM FROM", &o).is_err());
        assert!(p.parse("SELECT 1 +", &o).is_err());
        let out = p
            .parse("SELECT a, b FROM t WHERE a > 1 ORDER BY b", &o)
            .unwrap();
        assert_eq!(out.stmts.len(), 1);
        assert!(out.warnings.is_empty());
    }
}

#[test]
fn counts_the_parameter_markers_it_builds() {
    let count = |sql: &str| parse(sql, &ParseOptions::default()).unwrap().param_markers;
    assert_eq!(count("SELECT 1"), 0);
    // A `?` inside a string or a comment is not a marker.
    assert_eq!(count("SELECT '?' /* ? */ FROM t"), 0);
    assert_eq!(count("SELECT c FROM t WHERE id BETWEEN ? AND ?"), 2);
    // Every production that builds a marker: an expression, LIMIT, a window frame bound,
    // LEAD / LAG's offset.
    assert_eq!(count("SELECT a FROM t LIMIT ?, ?"), 2);
    assert_eq!(
        count("SELECT SUM(a) OVER (ORDER BY a ROWS BETWEEN ? PRECEDING AND ? FOLLOWING) FROM t"),
        2
    );
    assert_eq!(count("SELECT LAG(a, ?) OVER (ORDER BY a) FROM t"), 1);
    // Over all the statements of the input; a fresh count on every parse.
    assert_eq!(count("SELECT ?; SELECT ?, ?"), 3);
    let mut p = Parser::new();
    let opts = ParseOptions::default();
    assert_eq!(p.parse("SELECT ?, ?", &opts).unwrap().param_markers, 2);
    assert_eq!(p.parse("SELECT ?", &opts).unwrap().param_markers, 1);
}
