//! Lowering of the parser AST into an owned, `Send` statement IR.
//!
//! The parser AST holds `Rc`s and spans into the SQL text, so it cannot be kept in a prepared
//! statement cache or held across an `.await` in a session. Lowering copies what the planner
//! needs and rejects every construct outside the supported subset with
//! `ER_NOT_SUPPORTED_YET` (1235), so unsupported SQL can never be half-understood later.

use tidb_mysql_protocol::SqlError;
use tidb_parser::ast;

/// A supported statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Select(Box<Select>),
    Use(String),
    Set(Vec<SetAssignment>),
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    Commit,
    Rollback,
    /// `SHOW WARNINGS` (`errors = false`) or `SHOW ERRORS`.
    ShowWarnings {
        errors: bool,
    },
}

/// One assignment of a `SET` statement.
#[derive(Clone, Debug, PartialEq)]
pub enum SetAssignment {
    /// `SET NAMES charset [COLLATE collation]`, `SET NAMES DEFAULT` (charset `None`).
    Names {
        charset: Option<String>,
        collation: Option<String>,
    },
    /// `SET CHARACTER SET charset` / `SET CHARSET DEFAULT`.
    Charset { charset: Option<String> },
    /// A system variable (name lower-cased, without `@@` and scope prefixes).
    SysVar {
        name: String,
        global: bool,
        value: SetValue,
    },
    /// `SET @user_var = ...`.
    UserVar { name: String },
}

/// Value of a system variable assignment.
#[derive(Clone, Debug, PartialEq)]
pub enum SetValue {
    Literal(Literal),
    /// A bare identifier (`SET autocommit = ON` parses `ON` as a string; others are names).
    Ident(String),
    Default,
    /// Anything else (expressions, other variables): kept as unsupported.
    Other,
}

/// A literal value.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Null,
    /// `TRUE` / `FALSE`.
    Bool(bool),
    Int(i64),
    UInt(u64),
    /// A decimal literal in its canonical text form.
    Decimal(String),
    Float(f64),
    Str(String),
}

/// Comparison operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The operator with its operands swapped (`a < b` == `b > a`).
    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Lt => "lt",
            CmpOp::Le => "le",
            CmpOp::Gt => "gt",
            CmpOp::Ge => "ge",
        }
    }
}

/// A column reference as written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColumnName {
    pub schema: String,
    pub table: String,
    pub name: String,
}

impl ColumnName {
    /// Go `ast.ColumnName.OrigColName`: `[schema.][table.]name` as written.
    pub fn orig_col_name(&self) -> String {
        let mut s = self.name.clone();
        if self.table.is_empty() {
            return s;
        }
        s = format!("{}.{s}", self.table);
        if self.schema.is_empty() {
            return s;
        }
        format!("{}.{s}", self.schema)
    }
}

/// Aggregate functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggName {
    Count,
    Sum,
    Min,
    Max,
}

impl AggName {
    pub fn as_str(self) -> &'static str {
        match self {
            AggName::Count => "count",
            AggName::Sum => "sum",
            AggName::Min => "min",
            AggName::Max => "max",
        }
    }
}

/// Supported expressions.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Column(ColumnName),
    Literal(Literal),
    /// The n-th `?` of the statement (0-based, in text order).
    Param(usize),
    /// Unary minus.
    Neg(Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    /// An aggregate; `COUNT(*)` has the argument `Literal(Int(1))`, as in TiDB's AST.
    Agg { name: AggName, arg: Box<Expr> },
    /// `@@[scope.]name`.
    SysVar { name: String, global: bool },
    /// A function call without arguments (`DATABASE()`, `USER()`, ...); name lower-cased.
    Func { name: String },
    /// `ORDER BY 1` / `GROUP BY 1`.
    Position(i64),
}

/// One select-list item.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    /// `*`, `t.*` or `db.t.*`.
    Wildcard { schema: String, table: String },
    Expr {
        expr: Expr,
        /// `AS alias` (empty if none).
        alias: String,
        /// The field's source text (Go `SelectField.Text()`), used as the column name of
        /// expressions.
        text: String,
    },
}

/// A table in `FROM`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub alias: String,
}

/// An `ORDER BY` / `GROUP BY` item.
#[derive(Clone, Debug, PartialEq)]
pub struct ByItem {
    pub expr: Expr,
    pub desc: bool,
}

/// `LIMIT [offset,] count`; each a literal or `?`.
#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub count: Expr,
    pub offset: Option<Expr>,
}

/// A single-table `SELECT` (or one without `FROM`).
#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub fields: Vec<Field>,
    pub from: Option<TableRef>,
    pub where_: Option<Expr>,
    pub group_by: Vec<ByItem>,
    pub order_by: Vec<ByItem>,
    pub limit: Option<Limit>,
}

/// A lowered statement and its number of `?` placeholders.
#[derive(Clone, Debug, PartialEq)]
pub struct Lowered {
    pub stmt: Statement,
    pub param_count: usize,
}

fn unsupported(what: impl std::fmt::Display) -> SqlError {
    SqlError::not_supported(what)
}

/// Parse `sql` and lower its single statement. Several statements, empty input and parse errors
/// are reported like TiDB (1064 for syntax errors).
pub fn parse_and_lower(sql: &str) -> Result<Lowered, SqlError> {
    let out = tidb_parser::parse(sql, &tidb_parser::ParseOptions::default())
        .map_err(|e| parse_error(sql, &e))?;
    let mut stmts = out.stmts;
    match stmts.len() {
        0 => Err(SqlError::new(1065, "Query was empty")),
        1 => lower_statement(&stmts.remove(0), sql),
        _ => Err(unsupported("multiple statements in one query")),
    }
}

/// TiDB's syntax error (1064) for a parse error.
fn parse_error(sql: &str, e: &tidb_parser::ParseError) -> SqlError {
    use tidb_parser::ParseError;
    let near = |pos: usize| -> String {
        let rest = sql.get(pos..).unwrap_or("");
        let near: String = rest.chars().take(80).collect();
        near
    };
    let msg = match e {
        ParseError::Syntax { pos, .. } | ParseError::Lex { pos, .. } => {
            let line = sql[..(*pos).min(sql.len())].matches('\n').count() + 1;
            format!(
                "You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line {line} column {pos} near \"{}\" ",
                near(*pos)
            )
        }
        ParseError::Semantic(d) => d.msg.clone(),
    };
    SqlError::new(1064, msg)
}

/// Lower one parsed statement. `sql` is the text it was parsed from (field texts are slices
/// of it).
pub fn lower_statement(stmt: &ast::Stmt, sql: &str) -> Result<Lowered, SqlError> {
    let mut cx = Lowerer {
        sql,
        params: Vec::new(),
    };
    let stmt = match stmt {
        ast::Stmt::Select(s) => Statement::Select(Box::new(cx.select(s)?)),
        ast::Stmt::Use(u) => Statement::Use(u.db_name.clone()),
        ast::Stmt::Set(s) => Statement::Set(cx.set(s)?),
        ast::Stmt::Begin(_) => Statement::Begin,
        ast::Stmt::Commit(_) => Statement::Commit,
        ast::Stmt::Rollback(r) => {
            if !r.savepoint_name.is_empty() {
                return Err(unsupported("ROLLBACK TO SAVEPOINT"));
            }
            Statement::Rollback
        }
        ast::Stmt::Show(s) => match s.tp {
            ast::ShowStmtType::Warnings | ast::ShowStmtType::Errors
                if s.unported.is_none() && !s.count_warnings_or_errors =>
            {
                Statement::ShowWarnings {
                    errors: s.tp == ast::ShowStmtType::Errors,
                }
            }
            _ => return Err(unsupported("SHOW statements other than SHOW WARNINGS/ERRORS")),
        },
        ast::Stmt::SetOpr(_) => return Err(unsupported("UNION/EXCEPT/INTERSECT")),
        ast::Stmt::Unported(n) => {
            return Err(unsupported(format!(
                "{} statements (read-only prototype)",
                statement_kind(n.kind)
            )));
        }
    };
    // `?` are numbered in text order, as Go does after parsing.
    let mut offsets = cx.params.clone();
    offsets.sort_unstable();
    offsets.dedup();
    let stmt = renumber_params(stmt, &offsets);
    Ok(Lowered {
        stmt,
        param_count: offsets.len(),
    })
}

fn statement_kind(kind: &str) -> &str {
    if kind.is_empty() { "these" } else { kind }
}

struct Lowerer<'a> {
    sql: &'a str,
    /// Byte offsets of the `?` seen so far (temporarily stored in `Expr::Param`).
    params: Vec<usize>,
}

impl Lowerer<'_> {
    fn select(&mut self, s: &ast::SelectStmt) -> Result<Select, SqlError> {
        if s.kind != ast::SelectStmtKind::Select {
            return Err(unsupported("TABLE/VALUES statements"));
        }
        if s.with.is_some() {
            return Err(unsupported("WITH (common table expressions)"));
        }
        if s.having.is_some() {
            return Err(unsupported("HAVING"));
        }
        if !s.window_specs.is_empty() {
            return Err(unsupported("WINDOW"));
        }
        if s.lock_info.as_ref().is_some_and(|l| {
            l.unported.is_some() || l.lock_type != ast::SelectLockType::None
        }) {
            return Err(unsupported("locking reads (FOR UPDATE/SHARE)"));
        }
        if s.select_into_opt.is_some() {
            return Err(unsupported("SELECT ... INTO"));
        }
        if !s.table_hints.is_empty() || !s.select_stmt_opts.table_hints.is_empty() {
            return Err(unsupported("optimizer hints"));
        }
        if s.select_stmt_opts.calc_found_rows {
            return Err(unsupported("SQL_CALC_FOUND_ROWS"));
        }
        let distinct = s.distinct || s.select_stmt_opts.distinct;
        let mut fields = Vec::with_capacity(s.fields.fields.len());
        for f in &s.fields.fields {
            fields.push(self.field(f)?);
        }
        let from = match &s.from {
            None => None,
            Some(refs) => Some(self.table_refs(refs)?),
        };
        let where_ = s.where_.as_ref().map(|e| self.expr(e)).transpose()?;
        let mut group_by = Vec::new();
        if let Some(g) = &s.group_by {
            if g.rollup {
                return Err(unsupported("GROUP BY ... WITH ROLLUP"));
            }
            for item in &g.items {
                // `NullOrder` is false iff ASC/DESC was written (Go's parser).
                if !item.null_order {
                    return Err(unsupported("GROUP BY ... ASC/DESC"));
                }
                group_by.push(self.by_item(item)?);
            }
        }
        let mut order_by = Vec::new();
        if let Some(o) = &s.order_by {
            for item in &o.items {
                order_by.push(self.by_item(item)?);
            }
        }
        let limit = match &s.limit {
            None => None,
            Some(l) => Some(Limit {
                count: self.expr(&l.count)?,
                offset: l.offset.as_ref().map(|e| self.expr(e)).transpose()?,
            }),
        };
        Ok(Select {
            distinct,
            fields,
            from,
            where_,
            group_by,
            order_by,
            limit,
        })
    }

    fn field(&mut self, f: &ast::SelectField) -> Result<Field, SqlError> {
        if let Some(w) = &f.wild_card {
            return Ok(Field::Wildcard {
                schema: w.schema.o.clone(),
                table: w.table.o.clone(),
            });
        }
        let Some(e) = &f.expr else {
            return Err(unsupported("this select field"));
        };
        let text = f
            .text
            .and_then(|sp| self.sql.get(sp.start()..sp.end()))
            .unwrap_or("")
            .trim()
            .to_owned();
        Ok(Field::Expr {
            expr: self.expr(e)?,
            alias: f.as_name.o.clone(),
            text,
        })
    }

    fn table_refs(&mut self, refs: &ast::TableRefsClause) -> Result<TableRef, SqlError> {
        let join = &refs.table_refs;
        if join.right.is_some() {
            return Err(unsupported("joins"));
        }
        let Some(left) = &join.left else {
            return Err(unsupported("this FROM clause"));
        };
        let (source, alias) = match left {
            ast::ResultSetNode::TableSource(ts) => (&ts.source, ts.as_name.o.clone()),
            ast::ResultSetNode::Join(_) => return Err(unsupported("joins")),
            _ => return Err(unsupported("this FROM clause")),
        };
        let tn = match source {
            ast::ResultSetNode::TableName(tn) => tn,
            ast::ResultSetNode::Select(_) | ast::ResultSetNode::SetOpr(_) => {
                return Err(unsupported("derived tables (subqueries in FROM)"));
            }
            ast::ResultSetNode::Join(_) => return Err(unsupported("joins")),
            _ => return Err(unsupported("this FROM clause")),
        };
        if !tn.partition_names.is_empty() {
            return Err(unsupported("PARTITION selection"));
        }
        if tn.as_of.is_some() {
            return Err(unsupported("AS OF TIMESTAMP (stale reads)"));
        }
        if tn.table_sample.is_some() {
            return Err(unsupported("TABLESAMPLE"));
        }
        if tn.index_hints.iter().any(|h| h.unported.is_some()) {
            return Err(unsupported("these index hints"));
        }
        // Index hints are accepted and ignored: only the row handle is used.
        Ok(TableRef {
            schema: tn.schema.o.clone(),
            name: tn.name.o.clone(),
            alias,
        })
    }

    fn by_item(&mut self, item: &ast::ByItem) -> Result<ByItem, SqlError> {
        Ok(ByItem {
            expr: self.expr(&item.expr)?,
            desc: item.desc,
        })
    }

    fn expr(&mut self, e: &ast::Expr) -> Result<Expr, SqlError> {
        use ast::Expr as A;
        Ok(match e {
            A::Column(c) => Expr::Column(ColumnName {
                schema: c.schema.o.clone(),
                table: c.table.o.clone(),
                name: c.name.o.clone(),
            }),
            A::Value(v) => Expr::Literal(literal(&v.datum)?),
            A::ParamMarker(p) => {
                self.params.push(p.offset);
                Expr::Param(p.offset)
            }
            A::Paren(inner) => self.expr(inner)?,
            A::Unary(u) => match u.op {
                ast::Op::Minus => Expr::Neg(Box::new(self.expr(&u.v)?)),
                ast::Op::Plus => self.expr(&u.v)?,
                _ => return Err(unsupported("this unary operator")),
            },
            A::Binary(b) => {
                let op = match b.op {
                    ast::Op::EQ => CmpOp::Eq,
                    ast::Op::LT => CmpOp::Lt,
                    ast::Op::LE => CmpOp::Le,
                    ast::Op::GT => CmpOp::Gt,
                    ast::Op::GE => CmpOp::Ge,
                    ast::Op::LogicAnd => {
                        return Ok(Expr::And(
                            Box::new(self.expr(&b.l)?),
                            Box::new(self.expr(&b.r)?),
                        ));
                    }
                    ast::Op::LogicOr => return Err(unsupported("OR")),
                    other => {
                        return Err(unsupported(format!(
                            "operator {}",
                            other.literal().0
                        )));
                    }
                };
                Expr::Cmp(op, Box::new(self.expr(&b.l)?), Box::new(self.expr(&b.r)?))
            }
            A::Between(b) => {
                if b.not {
                    return Err(unsupported("NOT BETWEEN"));
                }
                Expr::Between {
                    expr: Box::new(self.expr(&b.expr)?),
                    low: Box::new(self.expr(&b.left)?),
                    high: Box::new(self.expr(&b.right)?),
                }
            }
            A::Aggregate(a) => {
                let name = match a.f.to_ascii_lowercase().as_str() {
                    "count" => AggName::Count,
                    "sum" => AggName::Sum,
                    "min" => AggName::Min,
                    "max" => AggName::Max,
                    other => return Err(unsupported(format!("aggregate function {other}"))),
                };
                if a.distinct {
                    return Err(unsupported("DISTINCT in aggregate functions"));
                }
                if a.order.is_some() || a.args.len() != 1 {
                    return Err(unsupported("this aggregate function form"));
                }
                Expr::Agg {
                    name,
                    arg: Box::new(self.expr(&a.args[0])?),
                }
            }
            A::Variable(v) => {
                if !v.is_system {
                    return Err(unsupported("user variables"));
                }
                if v.value.is_some() || v.is_instance {
                    return Err(unsupported("this variable expression"));
                }
                Expr::SysVar {
                    name: v.name.to_ascii_lowercase(),
                    global: v.is_global,
                }
            }
            A::FuncCall(f) => {
                if !f.args.is_empty() || !f.schema.o.is_empty() {
                    return Err(unsupported(format!("function {}", f.fn_name.o)));
                }
                Expr::Func {
                    name: f.fn_name.o.to_ascii_lowercase(),
                }
            }
            A::Position(p) => Expr::Position(*p),
            A::PatternIn(_) => return Err(unsupported("IN")),
            A::PatternLike(_) => return Err(unsupported("LIKE")),
            A::IsNull(_) => return Err(unsupported("IS NULL")),
            A::Subquery(_) | A::Exists(_) | A::CompareSubquery(_) => {
                return Err(unsupported("subqueries"));
            }
            _ => return Err(unsupported("this expression")),
        })
    }

    fn set(&mut self, s: &ast::SetStmt) -> Result<Vec<SetAssignment>, SqlError> {
        let mut out = Vec::new();
        for v in &s.variables {
            if v.unported.is_some() {
                return Err(unsupported("this SET statement"));
            }
            let str_value = |e: &ast::Expr| match e {
                ast::Expr::Value(v) => match &v.datum {
                    ast::Datum::Str(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            };
            if v.name == ast::SET_NAMES {
                out.push(SetAssignment::Names {
                    charset: str_value(&v.value),
                    collation: v.extend_value.as_ref().and_then(|e| match &e.datum {
                        ast::Datum::Str(s) => Some(s.clone()),
                        _ => None,
                    }),
                });
                continue;
            }
            if v.name == ast::SET_CHARSET {
                out.push(SetAssignment::Charset {
                    charset: str_value(&v.value),
                });
                continue;
            }
            if !v.is_system {
                out.push(SetAssignment::UserVar {
                    name: v.name.clone(),
                });
                continue;
            }
            if v.is_instance {
                return Err(unsupported("SET INSTANCE"));
            }
            let value = match &v.value {
                ast::Expr::Value(val) => literal(&val.datum)
                    .map(SetValue::Literal)
                    .unwrap_or(SetValue::Other),
                ast::Expr::Column(c) if c.table.o.is_empty() => SetValue::Ident(c.name.o.clone()),
                ast::Expr::Default(_) => SetValue::Default,
                ast::Expr::Unary(u) if u.op == ast::Op::Minus => match &u.v {
                    ast::Expr::Value(val) => match literal(&val.datum) {
                        Ok(Literal::Int(i)) => SetValue::Literal(Literal::Int(-i)),
                        _ => SetValue::Other,
                    },
                    _ => SetValue::Other,
                },
                _ => SetValue::Other,
            };
            out.push(SetAssignment::SysVar {
                name: v.name.to_ascii_lowercase(),
                global: v.is_global,
                value,
            });
        }
        Ok(out)
    }
}

fn literal(d: &ast::Datum) -> Result<Literal, SqlError> {
    Ok(match d {
        ast::Datum::Null => Literal::Null,
        ast::Datum::Bool(b) => Literal::Bool(*b),
        ast::Datum::Int(i) => Literal::Int(*i),
        ast::Datum::Uint(u) => Literal::UInt(*u),
        ast::Datum::Float(f) => Literal::Float(*f),
        ast::Datum::Decimal(s) => Literal::Decimal(s.clone()),
        ast::Datum::Str(s) => Literal::Str(s.clone()),
        ast::Datum::Hex(_) | ast::Datum::Bit(_) => {
            return Err(unsupported("hexadecimal and bit literals"));
        }
    })
}

/// Replace the byte offsets temporarily stored in `Expr::Param` by their rank.
fn renumber_params(stmt: Statement, offsets: &[usize]) -> Statement {
    if offsets.is_empty() {
        return stmt;
    }
    let rank = |off: usize| offsets.binary_search(&off).unwrap_or(0);
    match stmt {
        Statement::Select(mut s) => {
            let fix = |e: &mut Expr| map_params(e, &rank);
            for f in &mut s.fields {
                if let Field::Expr { expr, .. } = f {
                    fix(expr);
                }
            }
            if let Some(w) = &mut s.where_ {
                fix(w);
            }
            for b in s.group_by.iter_mut().chain(s.order_by.iter_mut()) {
                fix(&mut b.expr);
            }
            if let Some(l) = &mut s.limit {
                fix(&mut l.count);
                if let Some(o) = &mut l.offset {
                    fix(o);
                }
            }
            Statement::Select(s)
        }
        other => other,
    }
}

fn map_params(e: &mut Expr, rank: &impl Fn(usize) -> usize) {
    match e {
        Expr::Param(p) => *p = rank(*p),
        Expr::Neg(x) => map_params(x, rank),
        Expr::Cmp(_, l, r) | Expr::And(l, r) => {
            map_params(l, rank);
            map_params(r, rank);
        }
        Expr::Between { expr, low, high } => {
            map_params(expr, rank);
            map_params(low, rank);
            map_params(high, rank);
        }
        Expr::Agg { arg, .. } => map_params(arg, rank),
        Expr::Column(_)
        | Expr::Literal(_)
        | Expr::SysVar { .. }
        | Expr::Func { .. }
        | Expr::Position(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(sql: &str) -> Lowered {
        parse_and_lower(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn select(sql: &str) -> Select {
        match lower(sql).stmt {
            Statement::Select(s) => *s,
            other => panic!("{other:?}"),
        }
    }

    fn err(sql: &str) -> SqlError {
        parse_and_lower(sql).expect_err(sql)
    }

    #[test]
    fn sysbench_point_select() {
        let l = lower("SELECT c FROM sbtest1 WHERE id=?");
        assert_eq!(l.param_count, 1);
        let Statement::Select(s) = l.stmt else {
            panic!()
        };
        assert_eq!(
            s.fields,
            vec![Field::Expr {
                expr: Expr::Column(ColumnName {
                    name: "c".into(),
                    ..Default::default()
                }),
                alias: String::new(),
                text: "c".into(),
            }]
        );
        assert_eq!(
            s.from,
            Some(TableRef {
                name: "sbtest1".into(),
                ..Default::default()
            })
        );
        assert_eq!(
            s.where_,
            Some(Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Column(ColumnName {
                    name: "id".into(),
                    ..Default::default()
                })),
                Box::new(Expr::Param(0))
            ))
        );
    }

    #[test]
    fn params_are_numbered_in_text_order() {
        let l = lower("SELECT SUM(k) FROM t WHERE id BETWEEN ? AND ? LIMIT ?");
        assert_eq!(l.param_count, 3);
        let Statement::Select(s) = l.stmt else {
            panic!()
        };
        let Some(Expr::Between { low, high, .. }) = &s.where_ else {
            panic!()
        };
        assert_eq!(**low, Expr::Param(0));
        assert_eq!(**high, Expr::Param(1));
        assert_eq!(s.limit.unwrap().count, Expr::Param(2));
    }

    #[test]
    fn field_texts_and_aliases() {
        let s = select("SELECT  SUM( k ) , c AS x, t.* FROM sbtest1 t");
        let Field::Expr { text, alias, .. } = &s.fields[0] else {
            panic!()
        };
        assert_eq!(text, "SUM( k )");
        assert!(alias.is_empty());
        let Field::Expr { alias, .. } = &s.fields[1] else {
            panic!()
        };
        assert_eq!(alias, "x");
        assert_eq!(
            s.fields[2],
            Field::Wildcard {
                schema: String::new(),
                table: "t".into()
            }
        );
        assert_eq!(s.from.unwrap().alias, "t");
        let s = select("SELECT COUNT(*) FROM t");
        let Field::Expr { expr, text, .. } = &s.fields[0] else {
            panic!()
        };
        assert_eq!(text, "COUNT(*)");
        assert_eq!(
            *expr,
            Expr::Agg {
                name: AggName::Count,
                arg: Box::new(Expr::Literal(Literal::Int(1)))
            }
        );
    }

    #[test]
    fn distinct_order_limit() {
        let s = select("SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN 1 AND 5 ORDER BY c DESC LIMIT 2, 3");
        assert!(s.distinct);
        assert!(s.order_by[0].desc);
        let l = s.limit.unwrap();
        assert_eq!(l.count, Expr::Literal(Literal::UInt(3)));
        assert_eq!(l.offset, Some(Expr::Literal(Literal::UInt(2))));
        let s = select("SELECT -5, - ?, 1.50, 'a', NULL, TRUE");
        let exprs: Vec<_> = s
            .fields
            .iter()
            .map(|f| match f {
                Field::Expr { expr, .. } => expr.clone(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            exprs[0],
            Expr::Neg(Box::new(Expr::Literal(Literal::Int(5))))
        );
        assert_eq!(exprs[1], Expr::Neg(Box::new(Expr::Param(0))));
        assert_eq!(exprs[2], Expr::Literal(Literal::Decimal("1.50".into())));
        assert_eq!(exprs[3], Expr::Literal(Literal::Str("a".into())));
        assert_eq!(exprs[4], Expr::Literal(Literal::Null));
        assert_eq!(exprs[5], Expr::Literal(Literal::Bool(true)));
    }

    #[test]
    fn session_statements() {
        assert_eq!(lower("USE `sbtest`").stmt, Statement::Use("sbtest".into()));
        assert_eq!(lower("BEGIN").stmt, Statement::Begin);
        assert_eq!(lower("START TRANSACTION READ ONLY").stmt, Statement::Begin);
        assert_eq!(lower("COMMIT").stmt, Statement::Commit);
        assert_eq!(lower("rollback").stmt, Statement::Rollback);
        assert_eq!(
            lower("SHOW WARNINGS").stmt,
            Statement::ShowWarnings { errors: false }
        );
        assert_eq!(
            lower("SET NAMES utf8mb4").stmt,
            Statement::Set(vec![SetAssignment::Names {
                charset: Some("utf8mb4".into()),
                collation: None
            }])
        );
        assert_eq!(
            lower("SET autocommit = ON").stmt,
            Statement::Set(vec![SetAssignment::SysVar {
                name: "autocommit".into(),
                global: false,
                value: SetValue::Literal(Literal::Str("ON".into()))
            }])
        );
        assert_eq!(
            lower("SET @@SESSION.AutoCommit = 0").stmt,
            Statement::Set(vec![SetAssignment::SysVar {
                name: "autocommit".into(),
                global: false,
                value: SetValue::Literal(Literal::Int(0))
            }])
        );
        assert_eq!(
            lower("SET @a = 1").stmt,
            Statement::Set(vec![SetAssignment::UserVar { name: "a".into() }])
        );
    }

    #[test]
    fn rejections() {
        for sql in [
            "SELECT c FROM t1 JOIN t2",
            "SELECT c FROM t1, t2",
            "SELECT c FROM (SELECT c FROM t) x",
            "SELECT c FROM t WHERE id = 1 OR id = 2",
            "SELECT c FROM t WHERE id IN (1, 2)",
            "SELECT c FROM t WHERE c LIKE 'a%'",
            "SELECT c FROM t WHERE id NOT BETWEEN 1 AND 2",
            "SELECT c FROM t WHERE id = (SELECT 1)",
            "SELECT AVG(k) FROM t",
            "SELECT COUNT(DISTINCT k) FROM t",
            "SELECT c FROM t FOR UPDATE",
            "SELECT c FROM t UNION SELECT c FROM t",
            "SELECT k FROM t GROUP BY k HAVING k > 1",
            "SELECT c FROM t AS OF TIMESTAMP NOW()",
            "SELECT CONCAT(c, 'x') FROM t",
            "SELECT 1; SELECT 2",
            "INSERT INTO t VALUES (1)",
            "SHOW TABLES",
            "SELECT /*+ USE_INDEX(t, k) */ c FROM t",
            "SELECT c FROM t WHERE id != 5",
        ] {
            assert_eq!(err(sql).code, 1235, "{sql}");
        }
        assert_eq!(err("SELEC 1").code, 1064);
        assert_eq!(err("").code, 1065);
    }
}
