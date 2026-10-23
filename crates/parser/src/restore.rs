//! Restore: port of the Go `Restore(ctx *format.RestoreCtx)` methods for the ported AST subset,
//! with `format.DefaultRestoreFlags` (single-quoted strings, upper-case keywords, back-quoted
//! names). Used to verify the hand-ported actions against the `Restore()` texts expected by
//! parser_test.go. A node built by a still-unported action makes restore fail with `Err`.
#![allow(missing_docs)]
use crate::ast::*;

pub type R = Result<(), String>;

/// `ctx.WriteName`.
fn name(out: &mut String, s: &str) {
    out.push('`');
    out.push_str(&s.replace('`', "``"));
    out.push('`');
}

/// `ctx.WriteString` (single quotes, no backslash escaping).
fn string(out: &mut String, s: &str) {
    out.push('\'');
    out.push_str(&s.replace('\'', "''"));
    out.push('\'');
}

/// `ctx.WriteKeyWord`.
fn keyword(out: &mut String, s: &str) {
    out.push_str(&s.to_uppercase());
}

fn list(out: &mut String, items: &[Expr], sep: &str) -> R {
    for (i, e) in items.iter().enumerate() {
        if i != 0 {
            out.push_str(sep);
        }
        e.restore(out)?;
    }
    Ok(())
}

fn go_float_e(f: f64) -> String {
    // strconv.FormatFloat(f, 'e', -1, 64): shortest repr, exponent with sign and >= 2 digits
    let s = format!("{f:e}");
    let (m, e) = s.split_once('e').unwrap();
    let (sign, digits) = if let Some(d) = e.strip_prefix('-') {
        ('-', d)
    } else {
        ('+', e)
    };
    format!("{m}e{sign}{digits:0>2}")
}

impl ValueExpr {
    /// test_driver `ValueExpr.Restore`; `without_charset` = `RestoreStringWithoutCharset`.
    pub fn restore_with(&self, out: &mut String, without_charset: bool) -> R {
        match &self.datum {
            Datum::Null => out.push_str("NULL"),
            Datum::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
            Datum::Int(i) => out.push_str(&i.to_string()),
            Datum::Uint(u) => out.push_str(&u.to_string()),
            Datum::Float(f) => out.push_str(&go_float_e(*f)),
            Datum::Decimal(d) => out.push_str(d),
            Datum::Str(s) => {
                if !self.charset.is_empty() && !without_charset {
                    out.push('_');
                    keyword(out, self.charset);
                }
                string(out, s);
            }
            Datum::Hex(b) | Datum::Bit(b) => {
                if !self.charset.is_empty()
                    && self.charset != crate::charset::DEFAULT_CHARSET
                    && !without_charset
                    && self.charset != crate::charset::CHARSET_BIN
                {
                    out.push('_');
                    keyword(out, self.charset);
                    out.push(' ');
                }
                if matches!(self.datum, Datum::Hex(_)) {
                    // HexLiteral carries UnsignedFlag: x'%x'
                    out.push_str("x'");
                    for byte in b {
                        out.push_str(&format!("{byte:02x}"));
                    }
                    out.push('\'');
                } else {
                    // BinaryLiteral.ToBitLiteralString(true)
                    if b.is_empty() {
                        out.push_str("b''");
                    } else {
                        let bits: String = b.iter().map(|x| format!("{x:08b}")).collect();
                        let t = bits.trim_start_matches('0');
                        out.push_str("b'");
                        out.push_str(if t.is_empty() { "0" } else { t });
                        out.push('\'');
                    }
                }
            }
        }
        Ok(())
    }
}

impl TimeUnitType {
    /// `TimeUnitType.String()`.
    pub fn as_str(self) -> &'static str {
        use TimeUnitType::*;
        match self {
            Microsecond => "MICROSECOND",
            Second => "SECOND",
            Minute => "MINUTE",
            Hour => "HOUR",
            Day => "DAY",
            Week => "WEEK",
            Month => "MONTH",
            Quarter => "QUARTER",
            Year => "YEAR",
            SecondMicrosecond => "SECOND_MICROSECOND",
            MinuteMicrosecond => "MINUTE_MICROSECOND",
            MinuteSecond => "MINUTE_SECOND",
            HourMicrosecond => "HOUR_MICROSECOND",
            HourSecond => "HOUR_SECOND",
            HourMinute => "HOUR_MINUTE",
            DayMicrosecond => "DAY_MICROSECOND",
            DaySecond => "DAY_SECOND",
            DayMinute => "DAY_MINUTE",
            DayHour => "DAY_HOUR",
            YearMonth => "YEAR_MONTH",
            Invalid => "",
        }
    }
}

impl GetFormatSelectorType {
    /// `GetFormatSelectorType.String()`.
    pub fn as_str(self) -> &'static str {
        match self {
            GetFormatSelectorType::Date => "DATE",
            GetFormatSelectorType::Time => "TIME",
            GetFormatSelectorType::Datetime => "DATETIME",
        }
    }
}

impl TrimDirectionType {
    /// `TrimDirectionType.String()`.
    pub fn as_str(self) -> &'static str {
        match self {
            TrimDirectionType::Both | TrimDirectionType::BothDefault => "BOTH",
            TrimDirectionType::Leading => "LEADING",
            TrimDirectionType::Trailing => "TRAILING",
        }
    }
}

impl FieldType {
    /// `FieldType.RestoreAsCastType`.
    pub fn restore_as_cast_type(&self, out: &mut String, explicit_charset: bool) -> R {
        use crate::charset::{CHARSET_BIN, COLLATION_BIN, DEFAULT_CHARSET};
        use mysql_type::*;
        if let Some(kind) = self.unported {
            return Err(format!("unported field type {kind}"));
        }
        match self.tp {
            VAR_STRING | STRING => {
                let mut skip_write_binary = false;
                if self.charset == CHARSET_BIN && self.collate == COLLATION_BIN {
                    out.push_str("BINARY");
                    skip_write_binary = true;
                } else {
                    out.push_str("CHAR");
                }
                if self.flen != UNSPECIFIED_LENGTH {
                    out.push_str(&format!("({})", self.flen));
                }
                if explicit_charset {
                    if !skip_write_binary && self.flag & mysql_flag::BINARY != 0 {
                        out.push_str(" BINARY");
                    }
                    if self.charset != CHARSET_BIN && self.charset != DEFAULT_CHARSET {
                        out.push_str(" CHARSET ");
                        keyword(out, &self.charset);
                    }
                }
            }
            DATE => out.push_str("DATE"),
            DATETIME => {
                out.push_str("DATETIME");
                if self.decimal > 0 {
                    out.push_str(&format!("({})", self.decimal));
                }
            }
            NEW_DECIMAL => {
                out.push_str("DECIMAL");
                if self.flen > 0 && self.decimal > 0 {
                    out.push_str(&format!("({}, {})", self.flen, self.decimal));
                } else if self.flen > 0 {
                    out.push_str(&format!("({})", self.flen));
                }
            }
            DURATION => {
                out.push_str("TIME");
                if self.decimal > 0 {
                    out.push_str(&format!("({})", self.decimal));
                }
            }
            LONGLONG => out.push_str(if self.flag & mysql_flag::UNSIGNED != 0 {
                "UNSIGNED"
            } else {
                "SIGNED"
            }),
            JSON => out.push_str("JSON"),
            DOUBLE => out.push_str("DOUBLE"),
            FLOAT => out.push_str("FLOAT"),
            YEAR => out.push_str("YEAR"),
            TIDB_VECTOR_FLOAT32 => out.push_str("VECTOR"),
            _ => {}
        }
        if self.array {
            out.push_str(" ARRAY");
        }
        Ok(())
    }
}

impl Expr {
    pub fn restore(&self, out: &mut String) -> R {
        match self {
            Expr::Column(c) => c.restore(out),
            Expr::Value(v) => v.restore_with(out, false),
            Expr::ParamMarker(_) => {
                out.push('?');
                Ok(())
            }
            Expr::Binary(b) => {
                b.l.restore(out)?;
                let (lit, kw) = b.op.literal();
                if kw {
                    out.push(' ');
                }
                out.push_str(lit);
                if kw {
                    out.push(' ');
                }
                b.r.restore(out)
            }
            Expr::Unary(u) => {
                out.push_str(u.op.literal().0);
                u.v.restore(out)
            }
            Expr::FuncCall(f) => f.restore(out),
            Expr::FuncCast(c) => {
                match c.function_type {
                    CastFunctionType::Cast => {
                        out.push_str("CAST(");
                        c.expr.restore(out)?;
                        out.push_str(" AS ");
                        c.tp.restore_as_cast_type(out, c.explicit_char_set)?;
                        out.push(')');
                    }
                    CastFunctionType::Convert => {
                        out.push_str("CONVERT(");
                        c.expr.restore(out)?;
                        out.push_str(", ");
                        c.tp.restore_as_cast_type(out, c.explicit_char_set)?;
                        out.push(')');
                    }
                    CastFunctionType::BinaryOperator => {
                        out.push_str("BINARY ");
                        c.expr.restore(out)?;
                    }
                }
                Ok(())
            }
            Expr::JSONSumCrc32(j) => {
                out.push_str("JSON_SUM_CRC32(");
                j.expr.restore(out)?;
                out.push_str(" AS ");
                j.tp.restore_as_cast_type(out, j.explicit_char_set)?;
                out.push(')');
                Ok(())
            }
            Expr::Aggregate(a) => a.restore(out),
            Expr::Window(w) => {
                keyword(out, &w.name);
                out.push('(');
                for (i, a) in w.args.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    } else if w.distinct {
                        out.push_str("DISTINCT ");
                    }
                    a.restore(out)?;
                }
                out.push(')');
                if w.from_last {
                    out.push_str(" FROM LAST");
                }
                if w.ignore_null {
                    out.push_str(" IGNORE NULLS");
                }
                out.push_str(" OVER ");
                w.spec.restore(out)
            }
            Expr::Paren(e) => {
                out.push('(');
                e.restore(out)?;
                out.push(')');
                Ok(())
            }
            Expr::PatternIn(p) => {
                p.expr.restore(out)?;
                out.push_str(if p.not { " NOT IN " } else { " IN " });
                if let Some(sel) = &p.sel {
                    return sel.restore(out);
                }
                out.push('(');
                list(out, &p.list, ",")?;
                out.push(')');
                Ok(())
            }
            Expr::PatternLike(p) => {
                p.expr.restore(out)?;
                out.push_str(match (p.is_like, p.not) {
                    (true, false) => " LIKE ",
                    (true, true) => " NOT LIKE ",
                    (false, false) => " ILIKE ",
                    (false, true) => " NOT ILIKE ",
                });
                p.pattern.restore(out)?;
                if p.escape_explicit && p.escape != b'\\' {
                    out.push_str(" ESCAPE ");
                    if p.escape == 0 {
                        string(out, "");
                    } else {
                        // Go: ctx.WriteString(string(n.Escape)) -- a byte converted as a rune
                        string(out, &char::from(p.escape).to_string());
                    }
                }
                Ok(())
            }
            Expr::PatternRegexp(p) => {
                p.expr.restore(out)?;
                out.push_str(if p.not { " NOT REGEXP " } else { " REGEXP " });
                p.pattern.restore(out)
            }
            Expr::Between(b) => {
                b.expr.restore(out)?;
                out.push_str(if b.not { " NOT BETWEEN " } else { " BETWEEN " });
                b.left.restore(out)?;
                out.push_str(" AND ");
                b.right.restore(out)
            }
            Expr::IsNull(n) => {
                n.expr.restore(out)?;
                out.push_str(if n.not { " IS NOT NULL" } else { " IS NULL" });
                Ok(())
            }
            Expr::IsTruth(t) => {
                t.expr.restore(out)?;
                out.push_str(if t.not { " IS NOT" } else { " IS" });
                out.push_str(if t.true_ > 0 { " TRUE" } else { " FALSE" });
                Ok(())
            }
            Expr::Case(c) => {
                out.push_str("CASE");
                if let Some(v) = &c.value {
                    out.push(' ');
                    v.restore(out)?;
                }
                for w in &c.when_clauses {
                    out.push_str(" WHEN ");
                    w.expr.restore(out)?;
                    out.push_str(" THEN ");
                    w.result.restore(out)?;
                }
                if let Some(e) = &c.else_clause {
                    out.push_str(" ELSE ");
                    e.restore(out)?;
                }
                out.push_str(" END");
                Ok(())
            }
            Expr::Subquery(s) => {
                out.push('(');
                s.query.restore(out)?;
                out.push(')');
                Ok(())
            }
            Expr::Exists(e) => {
                out.push_str(if e.not { "NOT EXISTS " } else { "EXISTS " });
                e.sel.restore(out)
            }
            Expr::CompareSubquery(c) => {
                c.l.restore(out)?;
                let (lit, kw) = c.op.literal();
                if kw {
                    out.push(' ');
                }
                out.push_str(lit);
                if kw {
                    out.push(' ');
                }
                out.push_str(if c.all { "ALL " } else { "ANY " });
                c.r.restore(out)
            }
            Expr::Variable(v) => {
                if v.is_system {
                    out.push_str("@@");
                    if v.explicit_scope {
                        out.push_str(if v.is_global {
                            "GLOBAL"
                        } else if v.is_instance {
                            "INSTANCE"
                        } else {
                            "SESSION"
                        });
                        out.push('.');
                    }
                } else {
                    out.push('@');
                }
                name(out, &v.name);
                if let Some(val) = &v.value {
                    out.push_str(":=");
                    val.restore(out)?;
                }
                Ok(())
            }
            Expr::Row(r) => {
                out.push_str("ROW(");
                list(out, &r.values, ",")?;
                out.push(')');
                Ok(())
            }
            Expr::SetCollation(s) => {
                s.expr.restore(out)?;
                out.push_str(" COLLATE ");
                out.push_str(&s.collate);
                Ok(())
            }
            Expr::Default(d) => {
                out.push_str("DEFAULT");
                if let Some(n) = &d.name {
                    out.push('(');
                    n.restore(out)?;
                    out.push(')');
                }
                Ok(())
            }
            Expr::Values(c) => {
                out.push_str("VALUES(");
                c.restore(out)?;
                out.push(')');
                Ok(())
            }
            Expr::MatchAgainst(m) => {
                use fulltext_modifier::*;
                out.push_str("MATCH (");
                for (i, c) in m.column_names.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    c.restore(out)?;
                }
                out.push_str(") AGAINST (");
                m.against.restore(out)?;
                if m.modifier & MODE_MASK == BOOLEAN_MODE {
                    out.push_str(" IN BOOLEAN MODE");
                    if m.modifier & WITH_QUERY_EXPANSION != 0 {
                        return Err("BOOLEAN MODE doesn't support QUERY EXPANSION".into());
                    }
                } else if m.modifier & WITH_QUERY_EXPANSION != 0 {
                    out.push_str(" WITH QUERY EXPANSION");
                }
                out.push(')');
                Ok(())
            }
            Expr::TableName(t) => t.restore(out),
            Expr::TimeUnit(u) => {
                out.push_str(u.as_str());
                Ok(())
            }
            Expr::GetFormatSelector(s) => {
                out.push_str(s.as_str());
                Ok(())
            }
            Expr::TrimDirection(d) => {
                out.push_str(d.as_str());
                Ok(())
            }
            Expr::Position(n) => {
                out.push_str(&n.to_string());
                Ok(())
            }
            Expr::Unported(n) => Err(format!("unported expr {}", n.kind)),
        }
    }
}

impl FuncCallExpr {
    fn restore(&self, out: &mut String) -> R {
        let l = self.fn_name.l();
        // customRestore
        let special = match l.as_str() {
            DATE_LITERAL => Some("DATE "),
            TIME_LITERAL => Some("TIME "),
            TIMESTAMP_LITERAL => Some("TIMESTAMP "),
            _ => None,
        };
        if let Some(lit) = special {
            out.push_str(lit);
            return self
                .args
                .first()
                .ok_or("literal without argument")?
                .restore(out);
        }
        if l == JSON_MEMBER_OF {
            if self.args.len() != 2 {
                return Err(
                    "Incorrect parameter count in the call to native function 'json_memberof'"
                        .into(),
                );
            }
            self.args[0].restore(out)?;
            out.push_str(" MEMBER OF (");
            self.args[1].restore(out)?;
            out.push(')');
            return Ok(());
        }

        if !self.schema.o.is_empty() {
            name(out, &self.schema.o);
            out.push('.');
        }
        if self.tp == FuncCallExprType::Generic {
            name(out, &self.fn_name.o);
        } else {
            keyword(out, &self.fn_name.o);
        }
        out.push('(');
        let arg = |i: usize| {
            self.args
                .get(i)
                .ok_or_else(|| format!("{l}: missing argument {i}"))
        };
        match l.as_str() {
            "convert" => {
                arg(0)?.restore(out)?;
                out.push_str(" USING ");
                arg(1)?.restore(out)?;
            }
            "adddate" | "subdate" | "date_add" | "date_sub" => {
                arg(0)?.restore(out)?;
                out.push_str(", INTERVAL ");
                arg(1)?.restore(out)?;
                out.push(' ');
                arg(2)?.restore(out)?;
            }
            "extract" => {
                arg(0)?.restore(out)?;
                out.push_str(" FROM ");
                arg(1)?.restore(out)?;
            }
            "position" => {
                arg(0)?.restore(out)?;
                out.push_str(" IN ");
                arg(1)?.restore(out)?;
            }
            "trim" => {
                let n = self.args.len();
                if n == 3 {
                    arg(2)?.restore(out)?;
                    out.push(' ');
                }
                if n >= 2 {
                    let is_nil_value =
                        matches!(&self.args[1], Expr::Value(v) if v.datum == Datum::Null);
                    if !is_nil_value {
                        self.args[1].restore(out)?;
                        out.push(' ');
                    }
                    out.push_str("FROM ");
                }
                if (1..=3).contains(&n) {
                    arg(0)?.restore(out)?;
                }
            }
            WEIGHT_STRING => {
                arg(0)?.restore(out)?;
                if self.args.len() == 3 {
                    out.push_str(" AS ");
                    match &self.args[1] {
                        Expr::Value(v) => match &v.datum {
                            Datum::Str(s) => keyword(out, s),
                            _ => return Err("weight_string: non-string type".into()),
                        },
                        _ => return Err("weight_string: non-value type".into()),
                    }
                    out.push('(');
                    self.args[2].restore(out)?;
                    out.push(')');
                }
            }
            _ => list(out, &self.args, ", ")?,
        }
        out.push(')');
        Ok(())
    }
}

impl AggregateFuncExpr {
    fn restore(&self, out: &mut String) -> R {
        keyword(out, &self.f);
        out.push('(');
        if self.distinct {
            out.push_str("DISTINCT ");
        }
        if self.f.eq_ignore_ascii_case(AGG_FUNC_GROUP_CONCAT) {
            let (sep, args) = self
                .args
                .split_last()
                .ok_or("group_concat without separator")?;
            list(out, args, ", ")?;
            if let Some(o) = &self.order {
                out.push(' ');
                o.restore(out)?;
            }
            out.push_str(" SEPARATOR ");
            // The separator is restored with RestoreStringWithoutCharset.
            match sep {
                Expr::Value(v) => v.restore_with(out, true)?,
                other => other.restore(out)?,
            }
        } else {
            list(out, &self.args, ", ")?;
        }
        out.push(')');
        Ok(())
    }
}

impl ColumnName {
    pub fn restore(&self, out: &mut String) -> R {
        if !self.schema.o.is_empty() {
            name(out, &self.schema.o);
            out.push('.');
        }
        if !self.table.o.is_empty() {
            name(out, &self.table.o);
            out.push('.');
        }
        name(out, &self.name.o);
        Ok(())
    }
}

impl Stmt {
    pub fn restore(&self, out: &mut String) -> R {
        match self {
            Stmt::Select(s) => s.restore(out),
            Stmt::SetOpr(s) => s.restore(out),
            Stmt::Begin(b) => b.restore(out),
            Stmt::Commit(c) => {
                out.push_str("COMMIT");
                c.completion_type.restore(out);
                Ok(())
            }
            Stmt::Rollback(r) => {
                out.push_str("ROLLBACK");
                if !r.savepoint_name.is_empty() {
                    out.push_str(" TO ");
                    out.push_str(&r.savepoint_name);
                }
                r.completion_type.restore(out);
                Ok(())
            }
            Stmt::Use(u) => {
                out.push_str("USE ");
                name(out, &u.db_name);
                Ok(())
            }
            Stmt::Set(s) => {
                out.push_str("SET ");
                for (i, v) in s.variables.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    }
                    v.restore(out)?;
                }
                Ok(())
            }
            Stmt::Show(s) => s.restore(out),
            Stmt::Unported(n) => Err(format!("unported stmt {}", n.kind)),
        }
    }
}

impl BeginStmt {
    fn restore(&self, out: &mut String) -> R {
        if self.mode.is_empty() {
            if self.read_only {
                out.push_str("START TRANSACTION READ ONLY");
                if let Some(a) = &self.as_of {
                    out.push(' ');
                    return a.restore(out);
                }
            } else if self.causal_consistency_only {
                out.push_str("START TRANSACTION WITH CAUSAL CONSISTENCY ONLY");
            } else {
                out.push_str("START TRANSACTION");
            }
        } else {
            out.push_str("BEGIN ");
            keyword(out, self.mode);
        }
        Ok(())
    }
}

impl CompletionType {
    fn restore(self, out: &mut String) {
        match self {
            CompletionType::Default => {}
            CompletionType::Chain => out.push_str(" AND CHAIN"),
            CompletionType::Release => out.push_str(" RELEASE"),
        }
    }
}

impl AsOfClause {
    fn restore(&self, out: &mut String) -> R {
        out.push_str("AS OF TIMESTAMP ");
        self.ts_expr.restore(out)
    }
}

/// `embeddingAPIKeySysVars`: values restored as `'******'`.
const EMBEDDING_API_KEY_SYS_VARS: &[&str] = &[
    "tidb_exp_embed_jina_ai_api_key",
    "tidb_exp_embed_openai_api_key",
    "tidb_exp_embed_cohere_api_key",
    "tidb_exp_embed_huggingface_api_key",
    "tidb_exp_embed_nvidia_nim_api_key",
    "tidb_exp_embed_gemini_api_key",
];

impl VariableAssignment {
    fn restore(&self, out: &mut String) -> R {
        if let Some(kind) = self.unported {
            return Err(format!("unported variable assignment {kind}"));
        }
        if self.is_system {
            out.push_str("@@");
            out.push_str(if self.is_global {
                "GLOBAL"
            } else if self.is_instance {
                "INSTANCE"
            } else {
                "SESSION"
            });
            out.push('.');
        } else if self.name != SET_NAMES && self.name != SET_CHARSET {
            out.push('@');
        }
        if self.name == SET_NAMES {
            out.push_str("NAMES ");
        } else if self.name == SET_CHARSET {
            out.push_str("CHARSET ");
        } else {
            name(out, &self.name);
            out.push('=');
        }
        let lower = self.name.to_lowercase();
        if self.is_system && EMBEDDING_API_KEY_SYS_VARS.contains(&lower.as_str()) {
            string(out, "******");
        } else if self.name == TIDB_CLOUD_STORAGE_URI {
            // Go writes RedactURL(value) here; URL redaction is not ported.
            return Err("unported RedactURL".into());
        } else {
            self.value.restore(out)?;
        }
        if let Some(ev) = &self.extend_value {
            out.push_str(" COLLATE ");
            ev.restore_with(out, false)?;
        }
        Ok(())
    }
}

impl ShowStmt {
    fn restore(&self, out: &mut String) -> R {
        use ShowStmtType as T;
        if let Some(kind) = self.unported {
            return Err(format!("unported show {kind}"));
        }
        let table = |out: &mut String| -> R {
            self.table
                .as_ref()
                .ok_or("SHOW without table")?
                .restore(out)
        };
        let db_name_opt = |out: &mut String| {
            if !self.db_name.is_empty() {
                out.push_str(" IN ");
                name(out, &self.db_name);
            }
        };
        let like_or_where = |out: &mut String| -> R {
            if let Some(p) = &self.pattern {
                out.push_str(" LIKE ");
                p.restore(out)
            } else if let Some(w) = &self.where_ {
                out.push_str(" WHERE ");
                w.restore(out)
            } else {
                Ok(())
            }
        };
        let full = |out: &mut String| {
            if self.full {
                out.push_str("FULL ");
            }
        };
        let global_scope = |out: &mut String| {
            out.push_str(if self.global_scope {
                "GLOBAL "
            } else {
                "SESSION "
            });
        };
        out.push_str("SHOW ");
        match self.tp {
            T::BinlogStatus => out.push_str("BINARY LOG STATUS"),
            T::CreateTable => {
                out.push_str("CREATE TABLE ");
                table(out)?;
            }
            T::CreateProcedure => {
                out.push_str("CREATE PROCEDURE ");
                self.procedure
                    .as_ref()
                    .ok_or("SHOW without procedure")?
                    .restore(out)?;
            }
            T::CreateView => {
                out.push_str("CREATE VIEW ");
                table(out)?;
            }
            T::CreateDatabase => {
                out.push_str("CREATE DATABASE ");
                if self.if_not_exists {
                    out.push_str("IF NOT EXISTS ");
                }
                name(out, &self.db_name);
            }
            T::CreateSequence => {
                out.push_str("CREATE SEQUENCE ");
                table(out)?;
            }
            T::CreatePlacementPolicy => {
                out.push_str("CREATE PLACEMENT POLICY ");
                name(out, &self.db_name);
            }
            T::CreateResourceGroup => {
                out.push_str("CREATE RESOURCE GROUP ");
                name(out, &self.resource_group_name);
            }
            T::MaskingPolicies => {
                out.push_str("MASKING POLICIES FOR ");
                table(out)?;
                if let Some(w) = &self.where_ {
                    out.push_str(" WHERE ");
                    w.restore(out)?;
                }
            }
            // `SHOW GRANTS` without FOR (the user / role forms are not ported).
            T::Grants => out.push_str("GRANTS"),
            T::MasterStatus => out.push_str("MASTER STATUS"),
            T::ProcessList => {
                full(out);
                out.push_str("PROCESSLIST");
            }
            T::StatsExtended
            | T::StatsMeta
            | T::StatsLocked
            | T::StatsHistograms
            | T::StatsTopN
            | T::StatsBuckets
            | T::StatsHealthy
            | T::HistogramsInFlight
            | T::ColumnStatsUsage => {
                out.push_str(match self.tp {
                    T::StatsExtended => "STATS_EXTENDED",
                    T::StatsMeta => "STATS_META",
                    T::StatsLocked => "STATS_LOCKED",
                    T::StatsHistograms => "STATS_HISTOGRAMS",
                    T::StatsTopN => "STATS_TOPN",
                    T::StatsBuckets => "STATS_BUCKETS",
                    T::StatsHealthy => "STATS_HEALTHY",
                    T::HistogramsInFlight => "HISTOGRAMS_IN_FLIGHT",
                    _ => "COLUMN_STATS_USAGE",
                });
                like_or_where(out)?;
            }
            T::Profiles => out.push_str("PROFILES"),
            T::Privileges => out.push_str("PRIVILEGES"),
            T::Builtins => out.push_str("BUILTINS"),
            T::ImportJobs => {
                if self.import_job_raw {
                    out.push_str("RAW ");
                }
                if let Some(id) = self.import_job_id {
                    out.push_str("IMPORT JOB ");
                    out.push_str(&id.to_string());
                } else {
                    out.push_str("IMPORT JOBS");
                    like_or_where(out)?;
                }
            }
            T::ImportGroups => {
                if !self.show_group_key.is_empty() {
                    out.push_str("IMPORT GROUP ");
                    string(out, &self.show_group_key);
                } else {
                    out.push_str("IMPORT GROUPS");
                    like_or_where(out)?;
                }
            }
            T::DistributionJobs => {
                if let Some(id) = self.distribution_job_id {
                    out.push_str("DISTRIBUTION JOB ");
                    out.push_str(&id.to_string());
                } else {
                    out.push_str("DISTRIBUTION JOBS");
                    like_or_where(out)?;
                }
            }
            // ShowTargetFilterable
            _ => {
                match self.tp {
                    T::Engines => out.push_str("ENGINES"),
                    T::Config => out.push_str("CONFIG"),
                    T::Databases => out.push_str("DATABASES"),
                    T::Charset => out.push_str("CHARSET"),
                    T::Tables => {
                        full(out);
                        out.push_str("TABLES");
                        db_name_opt(out);
                    }
                    T::OpenTables => {
                        out.push_str("OPEN TABLES");
                        db_name_opt(out);
                    }
                    T::TableStatus => {
                        out.push_str("TABLE STATUS");
                        db_name_opt(out);
                    }
                    T::Index => {
                        out.push_str("INDEX IN ");
                        table(out)?;
                    }
                    T::Columns => {
                        if self.extended {
                            out.push_str("EXTENDED ");
                        }
                        full(out);
                        out.push_str("COLUMNS");
                        if let Some(t) = &self.table {
                            out.push_str(" IN ");
                            t.restore(out)?;
                        }
                        db_name_opt(out);
                    }
                    T::Warnings => out.push_str("WARNINGS"),
                    T::Errors => out.push_str("ERRORS"),
                    T::Variables => {
                        global_scope(out);
                        out.push_str("VARIABLES");
                    }
                    T::Status => {
                        global_scope(out);
                        out.push_str("STATUS");
                    }
                    T::Collation => out.push_str("COLLATION"),
                    T::Triggers => {
                        out.push_str("TRIGGERS");
                        db_name_opt(out);
                    }
                    T::ProcedureStatus => out.push_str("PROCEDURE STATUS"),
                    T::FunctionStatus => out.push_str("FUNCTION STATUS"),
                    T::Events => {
                        out.push_str("EVENTS");
                        db_name_opt(out);
                    }
                    T::Plugins => out.push_str("PLUGINS"),
                    T::Bindings => {
                        global_scope(out);
                        out.push_str("BINDINGS");
                    }
                    T::BindingCacheStatus => out.push_str("BINDING_CACHE STATUS"),
                    T::AnalyzeStatus => out.push_str("ANALYZE STATUS"),
                    T::Distributions => {
                        out.push_str("TABLE ");
                        table(out)?;
                        out.push_str(" DISTRIBUTIONS");
                        return like_or_where(out);
                    }
                    T::Regions => {
                        out.push_str("TABLE ");
                        table(out)?;
                        if !self.index_name.o.is_empty() {
                            out.push_str(" INDEX ");
                            name(out, &self.index_name.o);
                        }
                        out.push_str(" REGIONS");
                        return like_or_where(out);
                    }
                    T::TableNextRowId => {
                        out.push_str("TABLE ");
                        table(out)?;
                        out.push_str(" NEXT_ROW_ID");
                        return Ok(());
                    }
                    T::Backups => out.push_str("BACKUPS"),
                    T::Restores => out.push_str("RESTORES"),
                    T::Placement => out.push_str("PLACEMENT"),
                    T::PlacementLabels => out.push_str("PLACEMENT LABELS"),
                    T::SessionStates => out.push_str("SESSION_STATES"),
                    T::ReplicaStatus => out.push_str("REPLICA STATUS"),
                    T::StorageClassTransitions => out.push_str("STORAGE_CLASS TRANSITIONS"),
                    _ => return Err("Unknown ShowStmt type".into()),
                }
                like_or_where(out)?;
            }
        }
        Ok(())
    }
}

fn unported_if<T>(o: &Option<T>, what: &str) -> R {
    if o.is_some() {
        Err(format!("unported {what}"))
    } else {
        Ok(())
    }
}

impl SelectStmt {
    pub fn restore(&self, out: &mut String) -> R {
        if self.with_before_braces {
            self.with
                .as_ref()
                .ok_or("WithBeforeBraces without With")?
                .restore(out)?;
        }
        if self.is_in_braces {
            out.push('(');
        }
        if !self.with_before_braces
            && let Some(w) = &self.with
        {
            w.restore(out)?;
        }
        self.restore_body(out)?;
        if self.is_in_braces {
            out.push(')');
        }
        Ok(())
    }

    fn restore_body(&self, out: &mut String) -> R {
        if self.kind != SelectStmtKind::Select {
            return Err("unported TABLE/VALUES".into());
        }
        out.push_str("SELECT ");
        let o = &self.select_stmt_opts;
        // SelectStmtBasic moves the hints from the options into SelectStmt.TableHints.
        if !o.table_hints.is_empty() || !self.table_hints.is_empty() {
            return Err("unported hints".into());
        }
        match o.priority {
            1 => out.push_str("LOW_PRIORITY "),
            2 => out.push_str("HIGH_PRIORITY "),
            3 => out.push_str("DELAYED "),
            _ => {}
        }
        if o.sql_small_result {
            out.push_str("SQL_SMALL_RESULT ");
        }
        if o.sql_big_result {
            out.push_str("SQL_BIG_RESULT ");
        }
        if o.sql_buffer_result {
            out.push_str("SQL_BUFFER_RESULT ");
        }
        if !o.sql_cache {
            out.push_str("SQL_NO_CACHE ");
        }
        if o.calc_found_rows {
            out.push_str("SQL_CALC_FOUND_ROWS ");
        }
        if self.distinct {
            out.push_str("DISTINCT ");
        } else if o.explicit_all {
            out.push_str("ALL ");
        }
        if o.straight_join {
            out.push_str("STRAIGHT_JOIN ");
        }
        for (i, f) in self.fields.fields.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            f.restore(out)?;
        }
        if let Some(from) = &self.from {
            out.push_str(" FROM ");
            from.table_refs.restore(out)?;
        }
        if self.from.is_none() && self.where_.is_some() {
            out.push_str(" FROM DUAL");
        }
        if let Some(w) = &self.where_ {
            out.push_str(" WHERE ");
            w.restore(out)?;
        }
        if let Some(g) = &self.group_by {
            out.push_str(" GROUP BY ");
            for (i, it) in g.items.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                it.restore(out)?;
            }
            if g.rollup {
                out.push_str(" WITH ROLLUP");
            }
        }
        if let Some(h) = &self.having {
            out.push_str(" HAVING ");
            h.expr.restore(out)?;
        }
        if !self.window_specs.is_empty() {
            out.push_str(" WINDOW ");
            for (i, w) in self.window_specs.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                w.restore(out)?;
            }
        }
        if let Some(ob) = &self.order_by {
            out.push(' ');
            ob.restore(out)?;
        }
        if let Some(l) = &self.limit {
            out.push(' ');
            l.restore(out)?;
        }
        if let Some(l) = &self.lock_info {
            l.restore(out)?;
        }
        unported_if(&self.select_into_opt, "into")?;
        Ok(())
    }
}

impl Limit {
    fn restore(&self, out: &mut String) -> R {
        out.push_str("LIMIT ");
        if let Some(off) = &self.offset {
            off.restore(out)?;
            out.push(',');
        }
        self.count.restore(out)
    }
}

impl SetOprType {
    /// `SetOprType.String()`.
    pub fn as_str(self) -> &'static str {
        match self {
            SetOprType::Union => "UNION",
            SetOprType::UnionAll => "UNION ALL",
            SetOprType::Except => "EXCEPT",
            SetOprType::ExceptAll => "EXCEPT ALL",
            SetOprType::Intersect => "INTERSECT",
            SetOprType::IntersectAll => "INTERSECT ALL",
        }
    }
}

/// `ctx.WriteKeyWord(" " + AfterSetOperator.String() + " ")` (Go would panic on a nil operator).
fn set_operator(out: &mut String, op: Option<SetOprType>) -> R {
    let op = op.ok_or("set operation operand without operator")?;
    out.push(' ');
    out.push_str(op.as_str());
    out.push(' ');
    Ok(())
}

impl SetOprSelectList {
    fn restore(&self, out: &mut String) -> R {
        if let Some(w) = &self.with {
            w.restore(out)?;
        }
        for (i, node) in self.selects.iter().enumerate() {
            match node {
                SetOprNode::Select(s) => {
                    if i != 0 {
                        set_operator(out, s.after_set_operator)?;
                    }
                    s.restore(out)?;
                }
                SetOprNode::List(l) => {
                    if i != 0 {
                        set_operator(out, l.after_set_operator)?;
                    }
                    out.push('(');
                    l.restore(out)?;
                    out.push(')');
                }
                SetOprNode::Unported(n) => return Err(format!("unported set operand {}", n.kind)),
            }
        }
        if let Some(o) = &self.order_by {
            out.push(' ');
            o.restore(out)?;
        }
        if let Some(l) = &self.limit {
            out.push(' ');
            l.restore(out)?;
        }
        Ok(())
    }
}

impl SetOprStmt {
    pub fn restore(&self, out: &mut String) -> R {
        if let Some(w) = &self.with {
            w.restore(out)?;
        }
        if self.is_in_braces {
            out.push('(');
        }
        self.select_list.restore(out)?;
        if let Some(o) = &self.order_by {
            out.push(' ');
            o.restore(out)?;
        }
        if let Some(l) = &self.limit {
            out.push(' ');
            l.restore(out)?;
        }
        if self.is_in_braces {
            out.push(')');
        }
        Ok(())
    }
}

impl WithClause {
    fn restore(&self, out: &mut String) -> R {
        if let Some(kind) = self.unported {
            return Err(format!("unported with clause {kind}"));
        }
        out.push_str("WITH ");
        if self.is_recursive {
            out.push_str("RECURSIVE ");
        }
        for (i, cte) in self.ctes.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            name(out, &cte.name.o);
            if !cte.col_name_list.is_empty() {
                out.push_str(" (");
                for (j, c) in cte.col_name_list.iter().enumerate() {
                    if j != 0 {
                        out.push_str(", ");
                    }
                    name(out, &c.o);
                }
                out.push(')');
            }
            out.push_str(" AS ");
            cte.query.restore(out)?;
        }
        out.push(' ');
        Ok(())
    }
}

impl WindowSpec {
    fn restore(&self, out: &mut String) -> R {
        if let Some(kind) = self.unported {
            return Err(format!("unported window spec {kind}"));
        }
        if !self.name.o.is_empty() {
            name(out, &self.name.o);
            if self.only_alias {
                return Ok(());
            }
            out.push_str(" AS ");
        }
        out.push('(');
        let mut sep = "";
        if !self.ref_.o.is_empty() {
            name(out, &self.ref_.o);
            sep = " ";
        }
        if let Some(pb) = &self.partition_by {
            out.push_str(sep);
            out.push_str("PARTITION BY ");
            for (i, it) in pb.items.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                it.restore(out)?;
            }
            sep = " ";
        }
        if let Some(ob) = &self.order_by {
            out.push_str(sep);
            ob.restore(out)?;
            sep = " ";
        }
        if let Some(f) = &self.frame {
            out.push_str(sep);
            match f.tp {
                FrameType::Rows => out.push_str("ROWS"),
                FrameType::Ranges => out.push_str("RANGE"),
                FrameType::Groups => return Err("Unsupported window function frame type".into()),
            }
            out.push_str(" BETWEEN ");
            f.extent.start.restore(out)?;
            out.push_str(" AND ");
            f.extent.end.restore(out)?;
        }
        out.push(')');
        Ok(())
    }
}

impl FrameBound {
    fn restore(&self, out: &mut String) -> R {
        if self.unbounded {
            out.push_str("UNBOUNDED");
        }
        match self.tp {
            BoundType::CurrentRow => out.push_str("CURRENT ROW"),
            BoundType::Preceding | BoundType::Following => {
                if self.unit != TimeUnitType::Invalid {
                    out.push_str("INTERVAL ");
                }
                if let Some(e) = &self.expr {
                    e.restore(out)?;
                }
                if self.unit != TimeUnitType::Invalid {
                    out.push(' ');
                    out.push_str(self.unit.as_str());
                }
                out.push_str(if self.tp == BoundType::Preceding {
                    " PRECEDING"
                } else {
                    " FOLLOWING"
                });
            }
        }
        Ok(())
    }
}

impl OrderByClause {
    fn restore(&self, out: &mut String) -> R {
        out.push_str("ORDER BY ");
        for (i, it) in self.items.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            it.restore(out)?;
        }
        Ok(())
    }
}

impl SelectField {
    fn restore(&self, out: &mut String) -> R {
        if let Some(w) = &self.wild_card {
            if !w.schema.o.is_empty() {
                name(out, &w.schema.o);
                out.push('.');
            }
            if !w.table.o.is_empty() {
                name(out, &w.table.o);
                out.push('.');
            }
            out.push('*');
        }
        if let Some(e) = &self.expr {
            e.restore(out)?;
        }
        if !self.as_name.o.is_empty() {
            out.push_str(" AS ");
            name(out, &self.as_name.o);
        }
        Ok(())
    }
}

impl ByItem {
    fn restore(&self, out: &mut String) -> R {
        self.expr.restore(out)?;
        if self.desc {
            out.push_str(" DESC");
        }
        Ok(())
    }
}

impl ResultSetNode {
    fn restore(&self, out: &mut String) -> R {
        match self {
            ResultSetNode::TableName(t) => t.restore(out),
            ResultSetNode::TableSource(t) => t.restore(out),
            ResultSetNode::Join(j) => j.restore(out),
            ResultSetNode::Select(s) => s.restore(out),
            ResultSetNode::SetOpr(s) => s.restore(out),
            ResultSetNode::Unported(n) => Err(format!("unported resultset {}", n.kind)),
        }
    }
}

impl TableSource {
    fn restore(&self, out: &mut String) -> R {
        match &self.source {
            ResultSetNode::TableName(tn) => tn.restore_with_alias(out, &self.as_name),
            ResultSetNode::TableSource(_) => Err("nested table source".into()),
            ResultSetNode::Join(j) => {
                j.restore(out)?;
                if !self.as_name.o.is_empty() {
                    out.push_str(" AS ");
                    name(out, &self.as_name.o);
                }
                Ok(())
            }
            ResultSetNode::Select(_) | ResultSetNode::SetOpr(_) => {
                out.push('(');
                self.source.restore(out)?;
                out.push(')');
                if !self.as_name.o.is_empty() {
                    out.push_str(" AS ");
                    name(out, &self.as_name.o);
                }
                Ok(())
            }
            ResultSetNode::Unported(_) => self.source.restore(out),
        }
    }
}

impl TableName {
    fn restore_name(&self, out: &mut String) {
        if !self.schema.o.is_empty() {
            name(out, &self.schema.o);
            out.push('.');
        }
        name(out, &self.name.o);
    }

    fn restore_partitions(&self, out: &mut String) {
        if !self.partition_names.is_empty() {
            out.push_str(" PARTITION(");
            for (i, p) in self.partition_names.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                name(out, &p.o);
            }
            out.push(')');
        }
    }

    fn restore_index_hints(&self, out: &mut String) -> R {
        for h in &self.index_hints {
            out.push(' ');
            h.restore(out)?;
        }
        Ok(())
    }

    /// `TableName.Restore`.
    pub fn restore(&self, out: &mut String) -> R {
        self.restore_name(out);
        self.restore_partitions(out);
        self.restore_index_hints(out)?;
        if let Some(a) = &self.as_of {
            out.push(' ');
            a.restore(out)?;
        }
        unported_if(&self.table_sample, "tablesample")
    }

    /// The `*TableName` branch of `TableSource.Restore`.
    pub fn restore_with_alias(&self, out: &mut String, as_name: &CIStr) -> R {
        self.restore_name(out);
        self.restore_partitions(out);
        if !as_name.o.is_empty() {
            out.push_str(" AS ");
            name(out, &as_name.o);
        }
        if let Some(a) = &self.as_of {
            out.push(' ');
            a.restore(out)?;
        }
        self.restore_index_hints(out)?;
        unported_if(&self.table_sample, "tablesample")
    }
}

impl IndexHint {
    fn restore(&self, out: &mut String) -> R {
        if let Some(kind) = self.unported {
            return Err(format!("unported index hint {kind}"));
        }
        out.push_str(match self.hint_type {
            IndexHintType::Use => "USE INDEX",
            IndexHintType::Ignore => "IGNORE INDEX",
            IndexHintType::Force => "FORCE INDEX",
            IndexHintType::OrderIndex => "ORDER INDEX",
            IndexHintType::NoOrderIndex => "NO ORDER INDEX",
        });
        out.push_str(match self.hint_scope {
            IndexHintScope::ForScan => "",
            IndexHintScope::ForJoin => " FOR JOIN",
            IndexHintScope::ForOrderBy => " FOR ORDER BY",
            IndexHintScope::ForGroupBy => " FOR GROUP BY",
        });
        out.push_str(" (");
        for (i, n) in self.index_names.iter().enumerate() {
            if i != 0 {
                out.push_str(", ");
            }
            name(out, &n.o);
        }
        out.push(')');
        Ok(())
    }
}

impl SelectLockInfo {
    /// The `LockInfo` part of `SelectStmt.Restore` (leading space included).
    fn restore(&self, out: &mut String) -> R {
        use SelectLockType as L;
        if let Some(kind) = self.unported {
            return Err(format!("unported lock {kind}"));
        }
        out.push(' ');
        let of_tables = |out: &mut String| -> R {
            if !self.tables.is_empty() {
                out.push_str(" OF ");
                for (i, t) in self.tables.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    }
                    t.restore(out)?;
                }
            }
            Ok(())
        };
        match self.lock_type {
            L::None => {}
            L::ForUpdateNoWait | L::ForShareNoWait => {
                out.push_str(if self.lock_type == L::ForUpdateNoWait {
                    "FOR UPDATE"
                } else {
                    "FOR SHARE"
                });
                of_tables(out)?;
                out.push_str(" NOWAIT");
            }
            L::ForUpdateWaitN => {
                out.push_str("FOR UPDATE");
                of_tables(out)?;
                out.push_str(" WAIT");
                out.push_str(&format!(" {}", self.wait_sec));
            }
            L::ForUpdateSkipLocked | L::ForShareSkipLocked => {
                out.push_str(if self.lock_type == L::ForUpdateSkipLocked {
                    "FOR UPDATE"
                } else {
                    "FOR SHARE"
                });
                of_tables(out)?;
                out.push_str(" SKIP LOCKED");
            }
            L::ForUpdate | L::ForShare => {
                // ctx.WriteKeyWord(n.LockInfo.LockType.String())
                out.push_str(if self.lock_type == L::ForUpdate {
                    "FOR UPDATE"
                } else {
                    "FOR SHARE"
                });
                of_tables(out)?;
            }
        }
        Ok(())
    }
}

impl Join {
    fn restore(&self, out: &mut String) -> R {
        let left = self.left.as_ref().ok_or("join without left")?;
        let left_is_join = matches!(left, ResultSetNode::Join(_));
        let mut use_comma_join = false;
        if let ResultSetNode::Join(lj) = left
            && lj.right.is_none()
            && let Some(ResultSetNode::TableSource(ts)) = &lj.left
            && matches!(
                ts.source,
                ResultSetNode::Select(_) | ResultSetNode::SetOpr(_)
            )
        {
            use_comma_join = true;
        }
        if left_is_join && !use_comma_join {
            out.push('(');
        }
        left.restore(out)?;
        if left_is_join && !use_comma_join {
            out.push(')');
        }
        let Some(right) = &self.right else {
            return Ok(());
        };
        if self.natural_join {
            out.push_str(" NATURAL");
        }
        match self.tp {
            JoinType::LeftJoin => out.push_str(" LEFT"),
            JoinType::RightJoin => out.push_str(" RIGHT"),
            JoinType::FullJoin => out.push_str(" FULL OUTER"),
            JoinType::CrossJoin => {}
        }
        if self.straight_join {
            out.push_str(" STRAIGHT_JOIN ");
        } else if use_comma_join {
            out.push_str(", ");
        } else {
            out.push_str(" JOIN ");
        }
        let right_is_join = matches!(right, ResultSetNode::Join(_));
        if right_is_join {
            out.push('(');
        }
        right.restore(out)?;
        if right_is_join {
            out.push(')');
        }
        if let Some(on) = &self.on {
            out.push_str(" ON ");
            on.expr.restore(out)?;
        }
        if !self.using.is_empty() {
            out.push_str(" USING (");
            for (i, c) in self.using.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                c.restore(out)?;
            }
            out.push(')');
        }
        Ok(())
    }
}
