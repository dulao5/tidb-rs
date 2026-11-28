//! Parity tests for `ParseOptions::sql_mode` against Go TiDB v8.5.8 (`parser.SetSQLMode`).
//!
//! Covers all six parser/lexer `sql_mode` bits read by Go v8.5.8 (`ANSI_QUOTES`,
//! `PIPES_AS_CONCAT`, `HIGH_NOT_PRECEDENCE`, `NO_BACKSLASH_ESCAPES`, `IGNORE_SPACE`,
//! `REAL_AS_FLOAT`), combinations (`ANSI`, multi-flag), and edge cases verified against live Go
//! TiDB v8.5.8.
mod common;

use tidb_base::mysql::{
    ModeANSIQuotes, ModeHighNotPrecedence, ModeIgnoreSpace, ModeNoBackslashEscapes,
    ModePipesAsConcat, ModeRealAsFloat, SQLMode, format_sql_mode_str, get_sql_mode,
};
use tidb_parser::ast::{CastFunctionType, Datum, Expr, Op, Stmt, mysql_type};
use tidb_parser::{ParseError, ParseOptions, parse};

fn opts(sql_mode: SQLMode) -> ParseOptions {
    ParseOptions {
        sql_mode,
        ..Default::default()
    }
}

fn restore_all(stmts: &[Stmt]) -> Result<String, String> {
    let mut out = String::new();
    for (i, s) in stmts.iter().enumerate() {
        if i != 0 {
            out.push_str("; ");
        }
        s.restore(&mut out)?;
    }
    Ok(out)
}

fn restore(sql: &str, sql_mode: SQLMode) -> String {
    let out = parse(sql, &opts(sql_mode)).unwrap_or_else(|e| panic!("{sql} ({sql_mode:?}): {e}"));
    restore_all(&out.stmts).unwrap_or_else(|why| panic!("{sql} ({sql_mode:?}) restore: {why}"))
}

fn first_select_expr(sql: &str, sql_mode: SQLMode) -> Expr {
    let mut out =
        parse(sql, &opts(sql_mode)).unwrap_or_else(|e| panic!("{sql} ({sql_mode:?}): {e}"));
    assert_eq!(out.stmts.len(), 1, "expected single SELECT: {sql}");
    let Stmt::Select(mut sel) = out.stmts.remove(0) else {
        panic!("expected single SELECT: {sql}");
    };
    sel.fields.fields.remove(0).expr.expect("expr field")
}

#[test]
fn real_as_float_corpus() {
    let cases = common::real_as_float_mode();
    assert_eq!(cases.len(), 7, "unexpected real_as_float corpus count");
    for c in cases {
        let mut o = opts(ModeRealAsFloat);
        o.window_functions = c.window;
        if !c.ok {
            assert!(
                parse(&c.src, &o).is_err(),
                "parser_test.go:{}: {:?} should fail",
                c.line,
                c.src
            );
            continue;
        }
        let out = parse(&c.src, &o)
            .unwrap_or_else(|e| panic!("parser_test.go:{}: {:?}: {e}", c.line, c.src));
        let got = restore_all(&out.stmts).unwrap();
        let want = c.restore.as_deref().unwrap();
        assert_eq!(got, want, "parser_test.go:{}: {:?}", c.line, c.src);
        let again = parse(&got, &o).unwrap();
        assert_eq!(restore_all(&again.stmts).unwrap(), got);
    }
}

#[test]
fn pipes_as_concat() {
    assert_eq!(
        restore("SELECT 'hello' || 'world'", SQLMode(0)),
        "SELECT _UTF8MB4'hello' OR _UTF8MB4'world'"
    );
    assert_eq!(
        restore("SELECT 'hello' || 'world'", ModePipesAsConcat),
        "SELECT CONCAT(_UTF8MB4'hello', _UTF8MB4'world')"
    );
    assert_eq!(
        restore("SELECT 'a' || 'b' || 'c'", ModePipesAsConcat),
        "SELECT CONCAT(CONCAT(_UTF8MB4'a', _UTF8MB4'b'), _UTF8MB4'c')"
    );
    assert_eq!(
        restore("SELECT 'a' || 'b' = 'ab'", ModePipesAsConcat),
        "SELECT CONCAT(_UTF8MB4'a', _UTF8MB4'b')=_UTF8MB4'ab'"
    );
    assert_eq!(
        restore("SELECT 1 || 2 OR 0", ModePipesAsConcat),
        "SELECT CONCAT(1, 2) OR 0"
    );
    assert_eq!(
        restore("SELECT 'a' OR 'b'", ModePipesAsConcat),
        "SELECT _UTF8MB4'a' OR _UTF8MB4'b'"
    );
}

#[test]
fn high_not_precedence() {
    // Default mode: NOT binds looser than BETWEEN / comparison.
    let def_between = first_select_expr("SELECT NOT 1 BETWEEN -5 AND 5", SQLMode(0));
    assert!(
        matches!(&def_between, Expr::Unary(u) if u.op == Op::Not && matches!(&u.v, Expr::Between(_))),
        "{def_between:?}"
    );
    assert_eq!(
        restore("SELECT NOT 1 BETWEEN -5 AND 5", SQLMode(0)),
        "SELECT NOT 1 BETWEEN -5 AND 5"
    );

    // HIGH_NOT_PRECEDENCE: NOT lexes as not2 and binds tightly like `!`.
    let high_between = first_select_expr("SELECT NOT 1 BETWEEN -5 AND 5", ModeHighNotPrecedence);
    assert!(
        matches!(&high_between, Expr::Between(b) if !b.not && matches!(&b.expr, Expr::Unary(u) if u.op == Op::Not2)),
        "{high_between:?}"
    );
    assert_eq!(
        restore("SELECT NOT 1 BETWEEN -5 AND 5", ModeHighNotPrecedence),
        "SELECT !1 BETWEEN -5 AND 5"
    );

    let where_between = parse(
        "SELECT * FROM t1 WHERE NOT a BETWEEN 2 AND 3",
        &opts(ModeHighNotPrecedence),
    )
    .unwrap();
    let [Stmt::Select(sel)] = where_between.stmts.as_slice() else {
        panic!("expected SELECT");
    };
    assert!(
        matches!(&sel.where_, Some(Expr::Between(b)) if !b.not && matches!(&b.expr, Expr::Unary(u) if u.op == Op::Not2)),
        "{:?}",
        sel.where_
    );

    assert_eq!(
        restore("SELECT NOT 0 = 0", ModeHighNotPrecedence),
        "SELECT !0=0"
    );
    assert_eq!(
        restore("SELECT NOT 1 IN (0, 2)", ModeHighNotPrecedence),
        "SELECT !1 IN (0,2)"
    );

    // NotSym productions must keep working when NOT lexes as not2.
    for (sql, want) in [
        (
            "SELECT a NOT BETWEEN 2 AND 3 FROM t",
            "SELECT `a` NOT BETWEEN 2 AND 3 FROM `t`",
        ),
        (
            "SELECT a NOT IN (1, 2) FROM t",
            "SELECT `a` NOT IN (1,2) FROM `t`",
        ),
        (
            "SELECT a IS NOT NULL FROM t",
            "SELECT `a` IS NOT NULL FROM `t`",
        ),
        (
            "SELECT a NOT LIKE 'x%' FROM t",
            "SELECT `a` NOT LIKE _UTF8MB4'x%' FROM `t`",
        ),
        (
            "SELECT a NOT REGEXP 'x' FROM t",
            "SELECT `a` NOT REGEXP _UTF8MB4'x' FROM `t`",
        ),
    ] {
        assert_eq!(restore(sql, ModeHighNotPrecedence), want, "{sql}");
    }
    assert!(
        parse(
            "CREATE TABLE IF NOT EXISTS t (a INT NOT NULL)",
            &opts(ModeHighNotPrecedence)
        )
        .is_ok()
    );

    // Dot-qualified `NOT` remains an identifier even under HIGH_NOT_PRECEDENCE.
    assert_eq!(
        restore("SELECT t.NOT FROM t", ModeHighNotPrecedence),
        "SELECT `t`.`NOT` FROM `t`"
    );
}

#[test]
fn ansi_quotes() {
    assert_eq!(
        restore(r#"SELECT "a" FROM t"#, SQLMode(0)),
        "SELECT _UTF8MB4'a' FROM `t`"
    );
    assert_eq!(
        restore(r#"SELECT "a" FROM "t""#, ModeANSIQuotes),
        "SELECT `a` FROM `t`"
    );
    assert_eq!(
        restore(r#"SELECT t."dot" = 10 FROM t"#, ModeANSIQuotes),
        "SELECT `t`.`dot`=10 FROM `t`"
    );
    assert_eq!(
        restore(r#"SELECT "SELECT" FROM "FROM""#, ModeANSIQuotes),
        "SELECT `SELECT` FROM `FROM`"
    );
    assert_eq!(
        restore(r#"SELECT "a""b", "a\"b" FROM t"#, ModeANSIQuotes),
        "SELECT `a\"b`,`a\"b` FROM `t`"
    );
    assert_eq!(
        restore(r#"SELECT "tq".1a, "tq".SELECT FROM "tq""#, ModeANSIQuotes),
        "SELECT `tq`.`1a`,`tq`.`SELECT` FROM `tq`"
    );
    // Adjacent double quotes under ANSI_QUOTES: column + alias; single quotes: string concat.
    assert_eq!(
        restore(r#"SELECT "a" "b" FROM t"#, ModeANSIQuotes),
        "SELECT `a` AS `b` FROM `t`"
    );
    assert_eq!(
        restore(r#"SELECT "a" "b""#, SQLMode(0)),
        "SELECT _UTF8MB4'ab'"
    );
    assert_eq!(
        restore(r#"SELECT 'a' 'b'"#, ModeANSIQuotes),
        "SELECT _UTF8MB4'ab'"
    );

    // DDL & alias cases from Go parser_test.go TestSQLModeANSIQuotes.
    assert!(parse(r#"CREATE TABLE "table" ("id" int)"#, &opts(ModeANSIQuotes)).is_ok());
    assert_eq!(
        restore(r#"SELECT * FROM t "tt""#, ModeANSIQuotes),
        "SELECT * FROM `t` AS `tt`"
    );

    // Introducer / national char / @ variables match Go lexer rules.
    assert!(matches!(
        parse(r#"SELECT _utf8mb4"abc""#, &opts(ModeANSIQuotes)),
        Err(ParseError::Syntax { .. })
    ));
    assert_eq!(
        restore(r#"SELECT N"abc""#, ModeANSIQuotes),
        "SELECT `N` AS `abc`"
    );
    assert_eq!(
        restore(r#"SET @"x" = 9; SELECT @"x", @@"sql_mode""#, ModeANSIQuotes),
        "SET @`x`=9; SELECT @`x`,@@`sql_mode`"
    );
}

#[test]
fn no_backslash_escapes() {
    // Go TestScanStringWithNoBackslashEscapesMode cases.
    let cases = [
        (r"' \n\tTest String'", r" \n\tTest String"),
        (r"'\x\B'", r"\x\B"),
        (r#"'\0\\''"\b\n\r\t\'"#, r#"\0\\'"\b\n\r\t\"#),
        (r"'\Z'", r"\Z"),
        (r"'\%\_'", r"\%\_"),
        (r"'hello'", "hello"),
        (r#"'"hello"'"#, r#""hello""#),
        (r#"'""hello""'"#, r#"""hello"""#),
        (r"'hel''lo'", "hel'lo"),
        (r#""hello""#, "hello"),
        (r#""'hello'""#, "'hello'"),
        (r#""''hello''""#, "''hello''"),
        (r#""hel""lo""#, r#"hel"lo"#),
        (
            "'한국의中文UTF8およびテキストトラック'",
            "한국의中文UTF8およびテキストトラック",
        ),
        (r"'\'", r"\"),
    ];
    for (lit, want) in cases {
        let sql = format!("SELECT {lit}");
        let Expr::Value(v) = first_select_expr(&sql, ModeNoBackslashEscapes) else {
            panic!("{sql}");
        };
        assert_eq!(v.datum, Datum::Str(want.to_owned()), "{lit}");
    }

    // `'\'hello'` closes the string after `\` and fails at trailing `'`.
    assert!(parse(r"SELECT '\'hello'", &opts(ModeNoBackslashEscapes)).is_err());
    // Default mode rejects `/*! '\' */`; NO_BACKSLASH_ESCAPES accepts it (Go TestSpecialComments).
    assert!(parse(r"SELECT /*! '\' */;", &opts(SQLMode(0))).is_err());
    assert_eq!(
        restore(r"SELECT /*! '\' */;", ModeNoBackslashEscapes),
        "SELECT _UTF8MB4'\\'"
    );
    // Trailing backslash before EOF does not panic or report out-of-bounds offset.
    let sql = r"SELECT '\";
    assert_eq!(
        parse(sql, &opts(SQLMode(0))).unwrap_err(),
        ParseError::Lex {
            pos: sql.len(),
            msg: "unterminated string".into(),
        }
    );

    // LIKE ... ESCAPE under NO_BACKSLASH_ESCAPES.
    assert!(matches!(
        parse(r"SELECT 'a' LIKE 'b' ESCAPE '\\'", &opts(ModeNoBackslashEscapes)),
        Err(ParseError::Semantic(d)) if d.msg == "Incorrect arguments to ESCAPE"
    ));
    let Expr::PatternLike(like_explicit) =
        first_select_expr(r"SELECT 'a' LIKE 'b' ESCAPE '\'", ModeNoBackslashEscapes)
    else {
        panic!("expected PatternLike");
    };
    assert_eq!(
        (like_explicit.escape, like_explicit.escape_explicit),
        (b'\\', true)
    );
    let Expr::PatternLike(like_default) =
        first_select_expr(r"SELECT 'a_b' LIKE 'a\_b'", ModeNoBackslashEscapes)
    else {
        panic!("expected PatternLike");
    };
    assert_eq!(
        (like_default.escape, like_default.escape_explicit),
        (b'\\', false)
    );

    // `@'...'` and `@@'...'` preserve backslashes under NO_BACKSLASH_ESCAPES.
    let Expr::Variable(var) = first_select_expr(r"SELECT @'a\nb'", ModeNoBackslashEscapes) else {
        panic!("expected Variable");
    };
    assert_eq!(var.name, r"a\nb");

    // Combined ANSI_QUOTES + NO_BACKSLASH_ESCAPES.
    assert_eq!(
        restore(
            r#"SELECT "a\nb", "a""b" FROM "t\1""#,
            ModeANSIQuotes | ModeNoBackslashEscapes,
        ),
        r#"SELECT `a\nb`,`a"b` FROM `t\1`"#
    );
    assert!(
        parse(
            r#"SELECT "a\"b""#,
            &opts(ModeANSIQuotes | ModeNoBackslashEscapes),
        )
        .is_err()
    );
}

#[test]
fn ignore_space() {
    // Without IGNORE_SPACE: COUNT (a) is a generic UDF call, not AggregateFuncExpr; CAST (..) fails.
    assert!(matches!(
        first_select_expr("SELECT COUNT (a) FROM t", SQLMode(0)),
        Expr::FuncCall(f) if f.fn_name.l() == "count"
    ));
    assert!(parse("SELECT CAST (1 AS CHAR)", &opts(SQLMode(0))).is_err());
    assert!(parse("CREATE TABLE count (a int)", &opts(SQLMode(0))).is_ok());

    // With IGNORE_SPACE: whitespace between builtin function name and '(' is ignored.
    assert!(matches!(
        first_select_expr("SELECT COUNT (a) FROM t", ModeIgnoreSpace),
        Expr::Aggregate(a) if a.f.eq_ignore_ascii_case("count")
    ));
    assert!(matches!(
        first_select_expr("SELECT COUNT\t\n\x0b\x0c\r (a) FROM t", ModeIgnoreSpace),
        Expr::Aggregate(a) if a.f.eq_ignore_ascii_case("count")
    ));
    assert_eq!(
        restore(
            "SELECT CAST (1 AS CHAR), SUBSTRING ('hello', 2), TRIM (' hi ')",
            ModeIgnoreSpace
        ),
        "SELECT CAST(1 AS CHAR),SUBSTRING(_UTF8MB4'hello', 2),TRIM(_UTF8MB4' hi ')"
    );
    assert_eq!(
        restore(
            "SELECT DATE_ADD ('2011-11-11', INTERVAL 10 SECOND), EXTRACT (SECOND FROM '2011-11-11'), POSITION ('a' IN 'ab')",
            ModeIgnoreSpace
        ),
        "SELECT DATE_ADD(_UTF8MB4'2011-11-11', INTERVAL 10 SECOND),EXTRACT(SECOND FROM _UTF8MB4'2011-11-11'),POSITION(_UTF8MB4'a' IN _UTF8MB4'ab')"
    );
    assert!(parse("CREATE TABLE count (a int)", &opts(ModeIgnoreSpace)).is_err());

    // Comments are NOT whitespace, and dot-qualified names remain identifiers.
    assert!(matches!(
        first_select_expr("SELECT COUNT /* c */ (a) FROM t", ModeIgnoreSpace),
        Expr::FuncCall(f) if f.fn_name.l() == "count"
    ));
    assert_eq!(
        restore("SELECT t.count FROM t", ModeIgnoreSpace),
        "SELECT `t`.`count` FROM `t`"
    );
    assert!(matches!(
        first_select_expr("SELECT t . count (1) FROM t", ModeIgnoreSpace),
        Expr::FuncCall(f) if f.schema.l() == "t" && f.fn_name.l() == "count"
    ));
}

#[test]
fn real_as_float_and_combinations() {
    let cast_default = first_select_expr("SELECT CAST(1 AS REAL)", SQLMode(0));
    let Expr::FuncCast(cd) = cast_default else {
        panic!("expected FuncCast");
    };
    assert_eq!(cd.tp.tp, mysql_type::DOUBLE);
    assert_eq!(
        restore("SELECT CAST(1 AS REAL), CONVERT(1, REAL)", SQLMode(0)),
        "SELECT CAST(1 AS DOUBLE),CONVERT(1, DOUBLE)"
    );

    let cast_float = first_select_expr("SELECT CAST(1 AS REAL)", ModeRealAsFloat);
    let Expr::FuncCast(cf) = cast_float else {
        panic!("expected FuncCast");
    };
    assert_eq!(cf.tp.tp, mysql_type::FLOAT);
    assert_eq!(cf.function_type, CastFunctionType::Cast);
    assert_eq!(
        restore(
            "SELECT CAST(1 AS REAL), CONVERT(1, REAL), CAST(1 AS DOUBLE)",
            ModeRealAsFloat
        ),
        "SELECT CAST(1 AS FLOAT),CONVERT(1, FLOAT),CAST(1 AS DOUBLE)"
    );

    // ANSI combination mode enables REAL_AS_FLOAT, PIPES_AS_CONCAT, ANSI_QUOTES, IGNORE_SPACE.
    let (ansi, err) = get_sql_mode(&format_sql_mode_str("ANSI"));
    assert!(err.is_ok());
    assert_eq!(
        restore(
            r#"SELECT 'a' || "col", COUNT (1), CAST(1 AS REAL) FROM "t""#,
            ansi,
        ),
        "SELECT CONCAT(_UTF8MB4'a', `col`),COUNT(1),CAST(1 AS FLOAT) FROM `t`"
    );
}
