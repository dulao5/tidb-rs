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
            pos: 7
        })
    );
    // TableName with an invalid schema name.
    assert_eq!(
        err("SELECT * FROM `a `.t"),
        ParseError::Semantic(Diagnostic {
            msg: "Incorrect database name 'a '".into(),
            pos: 14
        })
    );
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
