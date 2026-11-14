//! Steps 1 and 2 of the routing order (VS1 plan §4.3): the statement kind and the AST
//! pre-check. A cheap syntactic filter only: it sends obvious non-VS1 statements to the PoC
//! without entering the new planner, and carries no semantics. Anything it lets through can
//! still come back from the planner as `Blocked`.

use tidb_parser::ast::{
    Expr, ResultSetNode, SelectLockType, SelectStmt, SelectStmtKind, Stmt, TableName,
};

/// Step 1: only `SELECT` statements are candidates.
pub(super) fn select_of(stmt: &Stmt) -> Option<&SelectStmt> {
    match stmt {
        Stmt::Select(sel) => Some(sel),
        _ => None,
    }
}

/// Step 2: why `sel` is not a VS1 shape, or `Ok`. `allow_params`: `?` is allowed
/// (prepared statements); in `COM_QUERY` it is left to the PoC, which reports Go's 1064.
pub(super) fn ast_precheck(sel: &SelectStmt, allow_params: bool) -> Result<(), &'static str> {
    if sel.kind != SelectStmtKind::Select {
        return Err("TABLE / VALUES statement");
    }
    if sel.with.is_some() {
        return Err("CTE");
    }
    if sel.after_set_operator.is_some() {
        return Err("set operation");
    }
    if sel.distinct || sel.select_stmt_opts.distinct {
        return Err("DISTINCT");
    }
    if sel.group_by.is_some() || sel.having.is_some() {
        return Err("GROUP BY / HAVING");
    }
    if !sel.window_specs.is_empty() {
        return Err("window");
    }
    if let Some(lock) = &sel.lock_info
        && (lock.lock_type != SelectLockType::None || lock.unported.is_some())
    {
        return Err("locking clause");
    }
    if sel.select_into_opt.is_some() {
        return Err("SELECT INTO");
    }
    if let Some(from) = &sel.from {
        single_table(&from.table_refs)?;
    }
    let v = Walker { allow_params };
    for f in &sel.fields.fields {
        if let Some(e) = &f.expr {
            v.expr(e)?;
        }
    }
    if let Some(e) = &sel.where_ {
        v.expr(e)?;
    }
    if let Some(o) = &sel.order_by {
        for item in &o.items {
            v.expr(&item.expr)?;
        }
    }
    if let Some(l) = &sel.limit {
        v.expr(&l.count)?;
        if let Some(o) = &l.offset {
            v.expr(o)?;
        }
    }
    Ok(())
}

/// FROM is a single base table (no join, no derived table).
fn single_table(j: &tidb_parser::ast::Join) -> Result<(), &'static str> {
    if j.right.is_some() {
        return Err("join");
    }
    let table = match &j.left {
        Some(ResultSetNode::TableName(t)) => t,
        Some(ResultSetNode::TableSource(ts)) => match &ts.source {
            ResultSetNode::TableName(t) => t,
            ResultSetNode::Join(inner) => return single_table(inner),
            ResultSetNode::Select(_) | ResultSetNode::SetOpr(_) => return Err("derived table"),
            ResultSetNode::TableSource(_) | ResultSetNode::Unported(_) => {
                return Err("unported syntax");
            }
        },
        Some(ResultSetNode::Join(inner)) => return single_table(inner),
        Some(ResultSetNode::Select(_)) | Some(ResultSetNode::SetOpr(_)) => {
            return Err("derived table");
        }
        Some(ResultSetNode::Unported(_)) | None => return Err("unported syntax"),
    };
    table_name(table)
}

fn table_name(t: &TableName) -> Result<(), &'static str> {
    if t.as_of.is_some() {
        return Err("AS OF");
    }
    Ok(())
}

struct Walker {
    allow_params: bool,
}

impl Walker {
    fn exprs<'a>(&self, es: impl IntoIterator<Item = &'a Expr>) -> Result<(), &'static str> {
        es.into_iter().try_for_each(|e| self.expr(e))
    }

    /// No subquery, aggregate, window function or unported node; functions are in the new
    /// stack's registry.
    ///
    /// TODO(VS1-22): operators (`a + b`, `a = b`) are left to the planner; the expression
    /// rewriter maps them to functions and returns `Blocked` for unported ones.
    fn expr(&self, e: &Expr) -> Result<(), &'static str> {
        match e {
            Expr::Subquery(_) | Expr::Exists(_) | Expr::CompareSubquery(_) => Err("subquery"),
            Expr::Aggregate(_) => Err("aggregate"),
            Expr::Window(_) => Err("window"),
            Expr::Unported(_) => Err("unported syntax"),
            Expr::ParamMarker(_) if !self.allow_params => Err("parameter marker"),
            Expr::FuncCall(f) => {
                let name = f.fn_name.l();
                if tidb_expression::builtin_registry::function_class(&name).is_none() {
                    return Err("function not ported");
                }
                self.exprs(&f.args)
            }
            Expr::PatternIn(p) => {
                if p.sel.is_some() {
                    return Err("subquery");
                }
                self.expr(&p.expr)?;
                self.exprs(&p.list)
            }
            Expr::Binary(b) => {
                self.expr(&b.l)?;
                self.expr(&b.r)
            }
            Expr::Unary(u) => self.expr(&u.v),
            Expr::FuncCast(c) => self.expr(&c.expr),
            Expr::JSONSumCrc32(c) => self.expr(&c.expr),
            Expr::Paren(e) => self.expr(e),
            Expr::PatternLike(p) => {
                self.expr(&p.expr)?;
                self.expr(&p.pattern)
            }
            Expr::PatternRegexp(p) => {
                self.expr(&p.expr)?;
                self.expr(&p.pattern)
            }
            Expr::Between(b) => {
                self.expr(&b.expr)?;
                self.expr(&b.left)?;
                self.expr(&b.right)
            }
            Expr::IsNull(i) => self.expr(&i.expr),
            Expr::IsTruth(i) => self.expr(&i.expr),
            Expr::Case(c) => {
                if let Some(v) = &c.value {
                    self.expr(v)?;
                }
                for w in &c.when_clauses {
                    self.expr(&w.expr)?;
                    self.expr(&w.result)?;
                }
                match &c.else_clause {
                    Some(e) => self.expr(e),
                    None => Ok(()),
                }
            }
            Expr::Row(r) => self.exprs(&r.values),
            Expr::SetCollation(s) => self.expr(&s.expr),
            // `@@var` / `@var`: connectors read system variables right after connecting
            // (`SELECT @@max_allowed_packet, ...`), and not every variable is synced into the
            // new session; the PoC answers them. TODO(VS1-22): with the expression rewriter's
            // variable handling and the full sysvar sync.
            Expr::Variable(_) => Err("variable"),
            Expr::MatchAgainst(m) => self.expr(&m.against),
            Expr::Column(_)
            | Expr::Value(_)
            | Expr::ParamMarker(_)
            | Expr::Default(_)
            | Expr::Values(_)
            | Expr::TableName(_)
            | Expr::TimeUnit(_)
            | Expr::GetFormatSelector(_)
            | Expr::TrimDirection(_)
            | Expr::Position(_) => Ok(()),
        }
    }
}
