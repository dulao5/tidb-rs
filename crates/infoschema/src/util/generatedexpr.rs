//! go-file: pkg/util/generatedexpr/generated_expr.go
//!
//! Parsing of generated-column and default expressions stored in the table metadata
//! (`pkg/util/generatedexpr`, a D00.3 leaf utility file, ported whole on first need by
//! D06.4: constitution §4.4).
//!
//! Go's `nameResolver` is an `ast.Visitor`; the Rust AST has no `Accept` yet, so
//! [`simple_resolve_name`] walks the expression kinds a generated column can hold (DDL
//! rejects subqueries, aggregates, window functions and variables there) in Go's `Accept`
//! order and stops at the first unknown column, as the visitor does.

use tidb_base::Error;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::terror::ClassParser;
use tidb_model::TableInfo;
use tidb_parser::ast::{Expr, Stmt};
use tidb_parser::{ParseOptions, parse};

/// ParseExpression parses an ExprNode from a string. When TiDB loads infoschema from TiKV,
/// `GeneratedExprString` of `ColumnInfo` is a string field, so we need to parse it into
/// ast.ExprNode. This function is for that.
///
/// The connection charset and collation Go passes (`charset.GetDefaultCharsetAndCollate`,
/// utf8mb4 / utf8mb4_bin) are the parser's defaults. A parse error is `ErrParse` as
/// `util.SyntaxError` makes it, with the Rust parser's error text (L2).
// go: ParseExpression L2 (the parse error text is the Rust parser's)
pub fn parse_expression(expr: &str) -> Result<Expr, Error> {
    let expr = format!("select {expr}");
    match parse(&expr, &ParseOptions::default()) {
        Ok(mut out) => {
            if let Some(Stmt::Select(sel)) = out.stmts.get_mut(0)
                && let Some(field) = sel.fields.fields.get_mut(0)
                && let Some(e) = field.expr.take()
            {
                return Ok(e);
            }
            // Go: stmts[0].(*ast.SelectStmt).Fields.Fields[0].Expr on a statement that is
            // not a single expression (e.g. `select *`) is a nil node or a panic.
            Err(syntax_error("not an expression"))
        }
        Err(e) => Err(syntax_error(&e.to_string())),
    }
}

/// `util.SyntaxError`: `parser.ErrParse.GenWithStackByArgs(SyntaxErrorPrefix, err.Error())`.
// go-part: ParseExpression (util.SyntaxError, pkg/util/misc.go)
fn syntax_error(msg: &str) -> Error {
    ClassParser
        .new_std(errno::ErrParse)
        .gen_with_stack_by_args(vec![
            Arg::from("You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use"),
            Arg::from(msg.to_owned()),
        ])
}

/// SimpleResolveName resolves all column names in the expression node.
// go: SimpleResolveName L2 (a walk of the Rust AST instead of ast.Node.Accept, module docs)
pub fn simple_resolve_name(node: Expr, tbl_info: &TableInfo) -> Result<Expr, Error> {
    let mut nr = NameResolver {
        table_info: tbl_info,
        err: None,
    };
    if !nr.accept(&node) {
        return Err(nr
            .err
            .unwrap_or_else(|| tidb_base::errors::new("unknown error")));
    }
    Ok(node)
}

/// nameResolver is the visitor to resolve table name and column name. it combines TableInfo
/// and ColumnInfo to a generation expression.
struct NameResolver<'a> {
    table_info: &'a TableInfo,
    err: Option<Error>,
}

impl NameResolver<'_> {
    /// The children of `n` in Go's `Accept` order, then [`NameResolver::leave`]; `false`
    /// stops the walk (Go's `ok == false`).
    // go: nameResolver.Enter L2 (Enter never skips children: the walk always descends)
    fn accept(&mut self, n: &Expr) -> bool {
        let all = |nr: &mut Self, es: &[Expr]| es.iter().all(|e| nr.accept(e));
        let ok = match n {
            Expr::Binary(x) => self.accept(&x.l) && self.accept(&x.r),
            Expr::Unary(x) => self.accept(&x.v),
            Expr::FuncCall(x) => all(self, &x.args),
            Expr::FuncCast(x) => self.accept(&x.expr),
            Expr::JSONSumCrc32(x) => self.accept(&x.expr),
            Expr::Paren(x) => self.accept(x),
            Expr::PatternIn(x) => {
                self.accept(&x.expr)
                    && all(self, &x.list)
                    && x.sel.as_ref().is_none_or(|s| self.accept(s))
            }
            Expr::PatternLike(x) => self.accept(&x.expr) && self.accept(&x.pattern),
            Expr::PatternRegexp(x) => self.accept(&x.expr) && self.accept(&x.pattern),
            Expr::Between(x) => {
                self.accept(&x.expr) && self.accept(&x.left) && self.accept(&x.right)
            }
            Expr::IsNull(x) => self.accept(&x.expr),
            Expr::IsTruth(x) => self.accept(&x.expr),
            Expr::Case(x) => {
                x.value.as_ref().is_none_or(|v| self.accept(v))
                    && x.when_clauses
                        .iter()
                        .all(|w| self.accept(&w.expr) && self.accept(&w.result))
                    && x.else_clause.as_ref().is_none_or(|e| self.accept(e))
            }
            Expr::Row(x) => all(self, &x.values),
            Expr::SetCollation(x) => self.accept(&x.expr),
            // Leaves, and kinds a generated column cannot hold (module docs).
            _ => true,
        };
        ok && self.leave(n)
    }

    /// Leave implements ast.Visitor interface.
    // go: nameResolver.Leave L1
    fn leave(&mut self, in_node: &Expr) -> bool {
        if let Expr::Column(v) = in_node {
            for col in self.table_info.columns.iter().flatten() {
                if col.name.l == v.name.l() {
                    return true;
                }
            }
            self.err = Some(tidb_base::errors::errorf(
                "can't find column %s in %s",
                &[
                    Arg::from(v.name.o.clone()),
                    Arg::from(self.table_info.name.o.clone()),
                ],
            ));
            return false;
        }
        true
    }
}
