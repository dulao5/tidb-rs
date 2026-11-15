//! go-file: pkg/planner/core/plan_cacheable_checker.go
//!
//! The cacheability check of a prepared statement (block D13.1, the plan cache; the part that
//! prepared-statement planning needs, VS1-15). The check decides whether a statement's plan
//! may be cached and, when not, the reason that becomes the `skip prepared plan-cache: ...`
//! warning; nothing is cached on the new stack (the cache itself waits for the plan-cache
//! exception A3).
//!
//! Mapping:
//! - Go's `cacheableChecker` is an `ast.Visitor`; the Rust AST has no visitor yet (D02.2,
//!   the parser's AST walk), so [`Walker`] calls `enter` / `leave` in the order of Go's
//!   `Accept` methods for the nodes the checker looks at (the order decides which reason is
//!   reported first).
//! - Statements, expressions and table hints the Rust AST does not have (`Unported` nodes,
//!   the opaque `TableOptimizerHint`) cannot be checked: the result is `blocked-by:D02.2`
//!   (the parser's AST) or `blocked-by:D13.3` (hint parsing), never a guess.
//! - `infoschema.InfoSchema` is the narrow [`InfoSchemaLookup`] (see plan_cache_utils).
//! - The non-prepared plan cache checks (`NonPreparedPlanCacheableWithCtx` and its checker)
//!   are not ported: the new stack never uses the non-prepared plan cache.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::function_traits::UnCacheableFunctions;
use tidb_model::{TableInfo, TempTableNone};
use tidb_parser::ast::{
    self, Expr, Join, ResultSetNode, SelectStmt, SetOprNode, SetOprSelectList, SetOprStmt, Stmt,
    TableName,
};
use tidb_parser::model::CIStr as ModelCIStr;
use tidb_sessionctx::variable::session::SessionVars;

use crate::PhysicalPlanRef;
use crate::base::PhysicalKind;
use crate::core::plan_cache_utils::InfoSchemaLookup;
use crate::util::fixcontrol;

/// `h.HintIgnorePlanCache`.
#[allow(non_upper_case_globals)] // M-GONAME
pub const HintIgnorePlanCache: &str = "ignore_plan_cache";

/// IsASTCacheable checks whether the input ast(query) is cacheable.
/// Handle "ignore_plan_cache()" hint
/// If there are multiple hints, only one will take effect
///
/// `sctx` is the session's variables (Go's `base.PlanContext`, `M-CTX`); Go's tests pass a
/// nil context, which only `getMaxParamLimit` tolerates: the Rust callers always have one.
// go: IsASTCacheable L1 [M-CTX]
pub fn is_ast_cacheable(
    vars: &SessionVars,
    node: &Stmt,
    is: Option<&dyn InfoSchemaLookup>,
) -> Result<(bool, String), Error> {
    match node {
        Stmt::Select(_) | Stmt::SetOpr(_) => {}
        Stmt::Unported(n) if is_unported_dml(n.kind) => {
            // UPDATE / INSERT / DELETE are cacheable statements in Go; the Rust AST does not
            // have them (the read-only node never prepares them on the new stack).
            return Err(blocked("D02.2", "DML statements in the Rust AST"));
        }
        _ => {
            return Ok((
                false,
                "not a SELECT/UPDATE/INSERT/DELETE/SET statement".to_owned(),
            ));
        }
    }
    let mut checker = CacheableChecker {
        sctx: vars,
        cacheable: true,
        schema: is,
        reason: String::new(),
        sum_in_list_len: 0,
        max_num_param: get_max_param_limit(Some(vars)),
        err: None,
    };
    Walker(&mut checker).stmt(node);
    if let Some(e) = checker.err {
        return Err(e);
    }
    Ok((checker.cacheable, checker.reason))
}

/// Whether an unported statement of the grammar is one of Go's cacheable DML kinds.
// glue: the kind names of the parser's unported productions (D02.2)
fn is_unported_dml(kind: &str) -> bool {
    ["Insert", "Replace", "Update", "Delete"]
        .iter()
        .any(|p| kind.starts_with(p))
}

/// cacheableChecker checks whether a query can be cached:
struct CacheableChecker<'a> {
    sctx: &'a SessionVars,
    cacheable: bool,
    schema: Option<&'a dyn InfoSchemaLookup>,
    /// reason why cannot use plan-cache
    reason: String,

    /// the accumulated number of elements in all in-lists
    sum_in_list_len: usize,
    max_num_param: usize,
    /// A boundary stub reached during the walk (stops it).
    err: Option<Error>,
}

/// The nodes the plan-cache visitors distinguish (Go type-switches on `ast.Node`).
#[derive(Clone, Copy)]
pub(crate) enum CNode<'n> {
    Select(&'n SelectStmt),
    PatternIn(&'n ast::PatternInExpr),
    Variable,
    Subquery,
    FuncCall(&'n ast::FuncCallExpr),
    OrderBy(&'n [ast::ByItem]),
    GroupBy(&'n [ast::ByItem]),
    Limit(&'n ast::Limit),
    FrameBound(&'n ast::FrameBound),
    TableName(&'n TableName),
    ParamMarker(&'n ast::ParamMarkerExpr),
    Other,
}

/// Go's `ast.Visitor` for the plan-cache visitors over the Rust AST (see the module doc).
// glue: ast.Visitor (D02.2)
pub(crate) trait Visit {
    /// `Enter`: returns skipChildren.
    // glue: ast.Visitor
    fn enter(&mut self, n: CNode<'_>) -> bool;
    /// `Leave`: returns ok.
    // glue: ast.Visitor
    fn leave(&mut self, n: CNode<'_>) -> bool;
    /// A boundary stub reached during the walk (a node the Rust AST does not have).
    // glue: ast.Visitor
    fn fail(&mut self, e: Error);
}

impl Visit for CacheableChecker<'_> {
    // glue: ast.Visitor
    fn enter(&mut self, n: CNode<'_>) -> bool {
        CacheableChecker::enter(self, n)
    }
    // glue: ast.Visitor
    fn leave(&mut self, _: CNode<'_>) -> bool {
        CacheableChecker::leave(self)
    }
    // glue: ast.Visitor (a boundary stub reached during the walk)
    fn fail(&mut self, e: Error) {
        self.err = Some(e);
        self.cacheable = false;
    }
}

impl CacheableChecker<'_> {
    /// Enter implements Visitor interface. Returns skipChildren.
    // go: cacheableChecker.Enter L2 (the hint names are not parsed in the Rust AST, D13.3; DELETE / UPDATE / INSERT are not in it)
    fn enter(&mut self, input: CNode<'_>) -> bool {
        match input {
            CNode::Select(node) => {
                if !node.table_hints.is_empty() {
                    // for _, hints := range node.TableHints { if hints.HintName.L ==
                    // h.HintIgnorePlanCache { ... "ignore plan cache by hint" } }: the hint
                    // names are not parsed in the Rust AST.
                    // glue: blocked-by:D13.3 (the optimizer hints of a SELECT)
                    self.err = Some(blocked("D13.3", "optimizer hints (ignore_plan_cache)"));
                    self.cacheable = false;
                    return true;
                }
            }
            CNode::PatternIn(node) => {
                self.sum_in_list_len += node.list.len();
                if self.sum_in_list_len > self.max_num_param {
                    // to save memory
                    self.cacheable = false;
                    self.reason = "too many values in in-list".to_owned();
                    return true;
                }
            }
            CNode::Variable => {
                self.cacheable = false;
                self.reason = "query has user-defined variables is un-cacheable".to_owned();
                return true;
            }
            CNode::Subquery => {
                if !self.sctx.enable_plan_cache_for_subquery {
                    self.cacheable = false;
                    self.reason = "query has sub-queries is un-cacheable".to_owned();
                    return true;
                }
                return false;
            }
            CNode::FuncCall(node) => {
                if UnCacheableFunctions.contains(&node.fn_name.l().as_str()) {
                    self.cacheable = false;
                    self.reason = format!("query has '{}' is un-cacheable", node.fn_name.l());
                    return true;
                }
            }
            CNode::OrderBy(items) => {
                for item in items {
                    if matches!(item.expr, Expr::ParamMarker(_)) {
                        self.cacheable = false;
                        self.reason = "query has 'order by ?' is un-cacheable".to_owned();
                        return true;
                    }
                }
            }
            CNode::GroupBy(items) => {
                for item in items {
                    if matches!(item.expr, Expr::ParamMarker(_)) {
                        self.cacheable = false;
                        self.reason = "query has 'group by ?' is un-cacheable".to_owned();
                        return true;
                    }
                }
            }
            CNode::Limit(node) => {
                // node.Count != nil: a LIMIT always has a count in the Rust AST.
                if matches!(node.count, Expr::ParamMarker(_))
                    && !self.sctx.enable_plan_cache_for_param_limit
                {
                    self.cacheable = false;
                    self.reason = "query has 'limit ?' is un-cacheable".to_owned();
                    return true;
                }
                if let Some(offset) = &node.offset
                    && matches!(offset, Expr::ParamMarker(_))
                    && !self.sctx.enable_plan_cache_for_param_limit
                {
                    self.cacheable = false;
                    self.reason = "query has 'limit ?, 10' is un-cacheable".to_owned();
                    return true;
                }
            }
            CNode::FrameBound(node) => {
                if matches!(node.expr, Some(Expr::ParamMarker(_))) {
                    self.cacheable = false;
                    self.reason =
                        "query has ? in window function frames is un-cacheable".to_owned();
                    return true;
                }
            }
            CNode::TableName(node) => {
                if let Some(schema) = self.schema {
                    match check_table_cacheable(self.sctx, schema, node, false) {
                        Ok((cacheable, reason)) => {
                            self.cacheable = cacheable;
                            self.reason = reason;
                        }
                        Err(e) => {
                            self.err = Some(e);
                            self.cacheable = false;
                        }
                    }
                    if !self.cacheable {
                        return true;
                    }
                }
            }
            CNode::ParamMarker(_) | CNode::Other => {}
        }
        false
    }

    /// Leave implements Visitor interface.
    // go: cacheableChecker.Leave L1
    fn leave(&mut self) -> bool {
        self.cacheable
    }
}

/// Calls `enter` / `leave` in the order of Go's `Accept` methods. Every method returns Go's
/// `ok`: false stops the walk.
pub(crate) struct Walker<'c, V: Visit>(pub(crate) &'c mut V);

impl<V: Visit> Walker<'_, V> {
    /// `n.Accept(v)` for a node: `enter`, the children unless skipped, `leave`.
    // glue: AST walk (Go Accept)
    fn node(&mut self, n: CNode<'_>, children: impl FnOnce(&mut Self) -> bool) -> bool {
        if self.0.enter(n) {
            return self.0.leave(n);
        }
        if !children(self) {
            return false;
        }
        self.0.leave(n)
    }

    /// A node of the Rust AST that stands for something it does not have.
    // glue: AST walk (Unported nodes)
    fn unported(&mut self) -> bool {
        self.0.fail(blocked("D02.2", "a node not in the Rust AST"));
        false
    }

    // glue: AST walk (Go Accept)
    pub(crate) fn stmt(&mut self, s: &Stmt) -> bool {
        match s {
            Stmt::Select(sel) => self.select(sel),
            Stmt::SetOpr(set_opr) => self.set_opr(set_opr),
            _ => self.node(CNode::Other, |_| true),
        }
    }

    // glue: AST walk (Go Accept)
    fn with(&mut self, with: &ast::WithClause) -> bool {
        self.node(CNode::Other, |w| {
            with.ctes
                .iter()
                .all(|cte| w.node(CNode::Other, |w| w.expr(&cte.query)))
        })
    }

    // glue: AST walk (Go Accept)
    fn by_items(&mut self, items: &[ast::ByItem]) -> bool {
        items
            .iter()
            .all(|item| self.node(CNode::Other, |w| w.expr(&item.expr)))
    }

    /// `OrderByClause.Accept`.
    // glue: AST walk (Go Accept)
    fn order_by(&mut self, o: &ast::OrderByClause) -> bool {
        self.node(CNode::OrderBy(&o.items), |w| w.by_items(&o.items))
    }

    /// `Limit.Accept`.
    // glue: AST walk (Go Accept)
    fn limit(&mut self, l: &ast::Limit) -> bool {
        self.node(CNode::Limit(l), |w| {
            w.expr(&l.count) && l.offset.as_ref().is_none_or(|o| w.expr(o))
        })
    }

    /// `WindowSpec.Accept`: PartitionBy, OrderBy, Frame.
    // glue: AST walk (Go Accept)
    fn window_spec(&mut self, spec: &ast::WindowSpec) -> bool {
        if spec.unported.is_some() {
            return self.unported();
        }
        self.node(CNode::Other, |w| {
            if let Some(p) = &spec.partition_by
                && !w.node(CNode::Other, |w| w.by_items(&p.items))
            {
                return false;
            }
            if let Some(o) = &spec.order_by
                && !w.order_by(o)
            {
                return false;
            }
            if let Some(f) = &spec.frame {
                return w.node(CNode::Other, |w| {
                    w.frame_bound(&f.extent.start) && w.frame_bound(&f.extent.end)
                });
            }
            true
        })
    }

    /// `FrameBound.Accept`.
    // glue: AST walk (Go Accept)
    fn frame_bound(&mut self, b: &ast::FrameBound) -> bool {
        self.node(CNode::FrameBound(b), |w| {
            b.expr.as_ref().is_none_or(|e| w.expr(e))
        })
    }

    /// `SelectStmt.Accept`.
    // glue: AST walk (Go Accept)
    fn select(&mut self, n: &SelectStmt) -> bool {
        self.node(CNode::Select(n), |w| {
            if let Some(with) = &n.with
                && !w.with(with)
            {
                return false;
            }
            // TableHints: a non-empty list stopped the walk in enter.
            let fields_ok = w.node(CNode::Other, |w| {
                n.fields
                    .fields
                    .iter()
                    .all(|f| w.node(CNode::Other, |w| f.expr.as_ref().is_none_or(|e| w.expr(e))))
            });
            if !fields_ok {
                return false;
            }
            if let Some(from) = &n.from
                && !w.node(CNode::Other, |w| w.join(&from.table_refs))
            {
                return false;
            }
            if let Some(e) = &n.where_
                && !w.expr(e)
            {
                return false;
            }
            if let Some(g) = &n.group_by
                && !w.node(CNode::GroupBy(&g.items), |w| w.by_items(&g.items))
            {
                return false;
            }
            if let Some(h) = &n.having
                && !w.node(CNode::Other, |w| w.expr(&h.expr))
            {
                return false;
            }
            for list in &n.lists {
                if !w.node(CNode::Other, |w| list.values.iter().all(|v| w.expr(v))) {
                    return false;
                }
            }
            for spec in &n.window_specs {
                if !w.window_spec(spec) {
                    return false;
                }
            }
            if let Some(o) = &n.order_by
                && !w.order_by(o)
            {
                return false;
            }
            if let Some(l) = &n.limit
                && !w.limit(l)
            {
                return false;
            }
            if let Some(lock) = &n.lock_info {
                if lock.unported.is_some() {
                    return w.unported();
                }
                for t in &lock.tables {
                    if !w.table_name(t) {
                        return false;
                    }
                }
            }
            true
        })
    }

    /// `SetOprStmt.Accept`.
    // glue: AST walk (Go Accept)
    fn set_opr(&mut self, n: &SetOprStmt) -> bool {
        self.node(CNode::Other, |w| {
            if let Some(with) = &n.with
                && !w.with(with)
            {
                return false;
            }
            if !w.set_opr_list(&n.select_list) {
                return false;
            }
            if let Some(o) = &n.order_by
                && !w.order_by(o)
            {
                return false;
            }
            n.limit.as_ref().is_none_or(|l| w.limit(l))
        })
    }

    /// `SetOprSelectList.Accept`.
    // glue: AST walk (Go Accept)
    fn set_opr_list(&mut self, n: &SetOprSelectList) -> bool {
        self.node(CNode::Other, |w| {
            if let Some(with) = &n.with
                && !w.with(with)
            {
                return false;
            }
            for sel in &n.selects {
                let ok = match sel {
                    SetOprNode::Select(s) => w.select(s),
                    SetOprNode::List(l) => w.set_opr_list(l),
                    SetOprNode::Unported(_) => w.unported(),
                };
                if !ok {
                    return false;
                }
            }
            if let Some(o) = &n.order_by
                && !w.order_by(o)
            {
                return false;
            }
            n.limit.as_ref().is_none_or(|l| w.limit(l))
        })
    }

    /// `ResultSetNode.Accept`.
    // glue: AST walk (Go Accept)
    fn result_set(&mut self, n: &ResultSetNode) -> bool {
        match n {
            ResultSetNode::TableName(t) => self.table_name(t),
            ResultSetNode::TableSource(ts) => self.node(CNode::Other, |w| w.result_set(&ts.source)),
            ResultSetNode::Join(j) => self.join(j),
            ResultSetNode::Select(s) => self.select(s),
            ResultSetNode::SetOpr(s) => self.set_opr(s),
            ResultSetNode::Unported(_) => self.unported(),
        }
    }

    /// `Join.Accept`.
    // glue: AST walk (Go Accept)
    fn join(&mut self, n: &Join) -> bool {
        self.node(CNode::Other, |w| {
            if let Some(l) = &n.left
                && !w.result_set(l)
            {
                return false;
            }
            if let Some(r) = &n.right
                && !w.result_set(r)
            {
                return false;
            }
            if let Some(on) = &n.on
                && !w.node(CNode::Other, |w| w.expr(&on.expr))
            {
                return false;
            }
            n.using.iter().all(|_| w.node(CNode::Other, |_| true))
        })
    }

    /// `TableName.Accept`.
    // glue: AST walk (Go Accept)
    fn table_name(&mut self, n: &TableName) -> bool {
        self.node(CNode::TableName(n), |w| {
            if let Some(as_of) = &n.as_of {
                // Go ignores the AsOfClause's ok.
                let _ = w.node(CNode::Other, |w| w.expr(&as_of.ts_expr));
            }
            true
        })
    }

    /// `ExprNode.Accept`: the children in the order of the Go nodes' fields.
    // glue: AST walk (Go Accept)
    fn expr(&mut self, e: &Expr) -> bool {
        match e {
            Expr::FuncCall(f) => {
                self.node(CNode::FuncCall(f), |w| f.args.iter().all(|a| w.expr(a)))
            }
            Expr::FuncCast(c) => self.node(CNode::Other, |w| w.expr(&c.expr)),
            Expr::Variable(v) => self.node(CNode::Variable, |w| {
                v.value.as_ref().is_none_or(|x| w.expr(x))
            }),
            Expr::Subquery(s) => self.node(CNode::Subquery, |w| w.result_set(&s.query)),
            Expr::Exists(x) => self.node(CNode::Subquery, |w| w.expr(&x.sel)),
            Expr::Binary(b) => self.node(CNode::Other, |w| w.expr(&b.l) && w.expr(&b.r)),
            Expr::Unary(u) => self.node(CNode::Other, |w| w.expr(&u.v)),
            Expr::Paren(p) => self.node(CNode::Other, |w| w.expr(p)),
            Expr::IsNull(x) => self.node(CNode::Other, |w| w.expr(&x.expr)),
            Expr::IsTruth(x) => self.node(CNode::Other, |w| w.expr(&x.expr)),
            Expr::Between(x) => self.node(CNode::Other, |w| {
                w.expr(&x.expr) && w.expr(&x.left) && w.expr(&x.right)
            }),
            Expr::CompareSubquery(x) => self.node(CNode::Other, |w| w.expr(&x.l) && w.expr(&x.r)),
            Expr::PatternIn(x) => self.node(CNode::PatternIn(x), |w| {
                w.expr(&x.expr)
                    && x.list.iter().all(|v| w.expr(v))
                    && x.sel.as_ref().is_none_or(|s| w.expr(s))
            }),
            Expr::Row(r) => self.node(CNode::Other, |w| r.values.iter().all(|v| w.expr(v))),
            Expr::Aggregate(a) => self.node(CNode::Other, |w| {
                a.args.iter().all(|v| w.expr(v)) && a.order.as_ref().is_none_or(|o| w.order_by(o))
            }),
            Expr::Case(c) => self.node(CNode::Other, |w| {
                c.value.as_ref().is_none_or(|v| w.expr(v))
                    && c.when_clauses
                        .iter()
                        .all(|wc| w.node(CNode::Other, |w| w.expr(&wc.expr) && w.expr(&wc.result)))
                    && c.else_clause.as_ref().is_none_or(|v| w.expr(v))
            }),
            Expr::Column(_) | Expr::Values(_) => {
                self.node(CNode::Other, |w| w.node(CNode::Other, |_| true))
            }
            Expr::PatternLike(x) => {
                self.node(CNode::Other, |w| w.expr(&x.expr) && w.expr(&x.pattern))
            }
            Expr::PatternRegexp(x) => {
                self.node(CNode::Other, |w| w.expr(&x.expr) && w.expr(&x.pattern))
            }
            Expr::SetCollation(x) => self.node(CNode::Other, |w| w.expr(&x.expr)),
            Expr::JSONSumCrc32(x) => self.node(CNode::Other, |w| w.expr(&x.expr)),
            Expr::Window(x) => self.node(CNode::Other, |w| {
                x.args.iter().all(|a| w.expr(a)) && w.window_spec(&x.spec)
            }),
            Expr::Default(x) => {
                let has_name = x.name.is_some();
                self.node(CNode::Other, |w| {
                    !has_name || w.node(CNode::Other, |_| true)
                })
            }
            Expr::MatchAgainst(x) => self.node(CNode::Other, |w| {
                (0..x.column_names.len()).all(|_| w.node(CNode::Other, |_| true))
                    && w.expr(&x.against)
            }),
            Expr::TableName(t) => self.node(CNode::Other, |w| w.table_name(t)),
            Expr::Unported(_) => self.unported(),
            Expr::ParamMarker(m) => self.node(CNode::ParamMarker(m), |_| true),
            Expr::Value(_)
            | Expr::TimeUnit(_)
            | Expr::GetFormatSelector(_)
            | Expr::TrimDirection(_)
            | Expr::Position(_) => self.node(CNode::Other, |_| true),
        }
    }
}

/// isPlanCacheable returns whether this plan is cacheable and the reason if not.
///
/// The new stack builds physical plans only (no `Insert` / `Update` / `Delete`: the
/// read-only node), so `p` is always Go's `base.PhysicalPlan` arm.
// go: isPlanCacheable L1 [M-CTX]
pub fn is_plan_cacheable(
    sctx: &SessionVars,
    p: &PhysicalPlanRef,
    param_num: usize,
    limit_param_num: usize,
    has_sub_query: bool,
) -> Result<(bool, String), Error> {
    let pp = p;
    if limit_param_num != 0 && !sctx.enable_plan_cache_for_param_limit {
        return Ok((
            false,
            "the switch 'tidb_enable_plan_cache_for_param_limit' is off".to_owned(),
        ));
    }
    if has_sub_query && !sctx.enable_plan_cache_for_subquery {
        return Ok((
            false,
            "the switch 'tidb_enable_plan_cache_for_subquery' is off".to_owned(),
        ));
    }
    if sctx.plan_cache_max_plan_size > 0 && pp.memory_usage() as u64 > sctx.plan_cache_max_plan_size
    {
        // to save memory
        return Ok((
            false,
            "plan is too large(decided by the variable @@tidb_plan_cache_max_plan_size)".to_owned(),
        ));
    }
    is_physical_plan_cacheable(sctx, pp, param_num, limit_param_num, false)
}

/// isPhysicalPlanCacheable returns whether this physical plan is cacheable and return the
/// reason if not.
///
/// The operators of the new stack (`PhysicalKind`) are the point get and the VS1 operators;
/// the readers and scans are not ported yet (D10.1, VS1-27), so a plan holding one is a
/// boundary stub (their fields decide the result). Go's arms for operators the new stack
/// does not build (IndexReader, IndexLookUp, IndexMerge, IndexScan, Shuffle, MemTable,
/// Apply) cannot be reached.
// go: isPhysicalPlanCacheable L2 (TableReader / TableScan fields: blocked-by:D10.1; the other arms are operators the new stack does not build)
#[allow(clippy::only_used_in_recursion)] // Go reads sctx in the IndexMerge arm (no index merge on the new stack)
pub fn is_physical_plan_cacheable(
    sctx: &SessionVars,
    p: &PhysicalPlanRef,
    param_num: usize,
    limit_param_num: usize,
    under_index_merge: bool,
) -> Result<(bool, String), Error> {
    match p.kind() {
        PhysicalKind::TableDual => {
            if param_num > 0 {
                return Ok((false, "get a TableDual plan".to_owned()));
            }
        }
        PhysicalKind::TableReader | PhysicalKind::TableScan => {
            // TableReader: StoreType == TiFlash, tablePlan; TableScan: isFullScan under an
            // index merge.
            // glue: blocked-by:D10.1 (PhysicalTableReader / PhysicalTableScan, VS1-27)
            return Err(blocked("D10.1", "isPhysicalPlanCacheable of a reader"));
        }
        PhysicalKind::PointGet
        | PhysicalKind::Selection
        | PhysicalKind::Projection
        | PhysicalKind::Limit
        | PhysicalKind::Sort
        | PhysicalKind::TopN => {}
    }

    for c in p.children() {
        let (cacheable, reason) =
            is_physical_plan_cacheable(sctx, c, param_num, limit_param_num, under_index_merge)?;
        if !cacheable {
            return Ok((cacheable, reason));
        }
    }
    Ok((true, String::new()))
}

/// getMaxParamLimit returns the maximum number of parameters for a query that can be cached
/// in the Plan Cache.
// go: getMaxParamLimit L1 [M-CTX]
pub fn get_max_param_limit(sctx: Option<&SessionVars>) -> usize {
    let mut v: usize = 200;
    let Some(vars) = sctx else {
        return v;
    };
    let n = fixcontrol::get_int_with_default(
        &vars.state.optimizer_fix_control,
        fixcontrol::Fix44823,
        v as i64,
    );
    if n == 0 {
        v = i32::MAX as usize; // no limitation
    } else if n > 0 {
        v = n as usize;
    }
    v
}

/// enablePlanCacheForGeneratedCols: disable this by default since it's not well tested.
// go: enablePlanCacheForGeneratedCols L1 [M-CTX]
pub fn enable_plan_cache_for_generated_cols(sctx: Option<&SessionVars>) -> bool {
    let default_val = true;
    let Some(vars) = sctx else {
        return default_val;
    };
    fixcontrol::get_bool_with_default(
        &vars.state.optimizer_fix_control,
        fixcontrol::Fix45798,
        default_val,
    )
}

/// checkTableCacheable checks whether a query accessing this table is cacheable.
///
/// The log line of a failed lookup and the test-only `PlanCacheKeyTestIssue46760` failure are
/// not ported (logging; a Go test hook).
// go: checkTableCacheable L1 [M-CTX]
pub fn check_table_cacheable(
    sctx: &SessionVars,
    schema: &dyn InfoSchemaLookup,
    node: &TableName,
    is_non_prep: bool,
) -> Result<(bool, String), Error> {
    let mut table_schema = ModelCIStr::new(&node.schema.o);
    if table_schema.l.is_empty() {
        table_schema = ModelCIStr::new(&sctx.current_db);
    }
    let name = ModelCIStr::new(&node.name.o);
    let tb: Arc<TableInfo> = match schema.table_by_name(&table_schema, &name) {
        Ok(tb) => tb,
        Err(err) => {
            if tidb_base::blocked::blocked_by(&err).is_some() {
                return Err(err);
            }
            return Ok((
                false,
                format!(
                    "find table {}.{} failed: {}",
                    table_schema.o,
                    node.name.o,
                    String::from_utf8_lossy(&err.error())
                ),
            ));
        }
    };

    if tb.get_partition_info().is_some() {
        if !sctx.is_dynamic_partition_prune_enabled() {
            return Ok((
                false,
                "query accesses partitioned tables is un-cacheable if tidb_partition_pruning_mode = 'static'".to_owned(),
            ));
        }
        if fixcontrol::get_bool_with_default(
            &sctx.state.optimizer_fix_control,
            fixcontrol::Fix33031,
            false,
        ) {
            return Ok((
                false,
                "Fix33031 fix-control set and partitioned table".to_owned(),
            ));
        }
    }

    if !enable_plan_cache_for_generated_cols(Some(sctx)) {
        for col in tb.cols().into_iter().flatten() {
            if col.is_generated() {
                return Ok((
                    false,
                    "query accesses generated columns is un-cacheable".to_owned(),
                ));
            }
        }
    }
    if tb.temp_table_type != TempTableNone {
        return Ok((
            false,
            "query accesses temporary tables is un-cacheable".to_owned(),
        ));
    }

    if is_non_prep {
        // non-prep plan cache is stricter: the non-prepared plan cache is not ported (see
        // the module doc).
        // glue: blocked-by:D13.1 (the non-prepared plan cache, not used by the new stack)
        return Err(blocked("D13.1", "the non-prepared plan cache"));
    }

    Ok((true, String::new()))
}

/// CacheableWithCtx checks whether the input ast(query) is cacheable.
// go: CacheableWithCtx L1 [M-CTX]
pub fn cacheable_with_ctx(
    sctx: &SessionVars,
    node: &Stmt,
    is: Option<&dyn InfoSchemaLookup>,
) -> Result<(bool, String), Error> {
    is_ast_cacheable(sctx, node, is)
}

/// `Cacheable`: Go's test helper with a nil context (only `getMaxParamLimit` tolerates it);
/// the Rust callers always have a session, see [`cacheable_with_ctx`].
// glue: stub of Cacheable, blocked-by:D13.1 (a test helper with a nil context: not used)
pub fn cacheable() -> Result<bool, Error> {
    Err(blocked("D13.1", "Cacheable (test helper)"))
}

/// The non-prepared plan cache checks (`tidb_enable_non_prepared_plan_cache`): the new
/// stack never uses the non-prepared plan cache (spec §3).
// glue: stub of NonPreparedPlanCacheableWithCtx, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of extractTableNames, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
// glue: stub of getColType, blocked-by:D13.1 (the non-prepared plan cache is not used by the new stack)
pub fn non_prepared_plan_cacheable_with_ctx() -> Result<(bool, String), Error> {
    Err(blocked("D13.1", "the non-prepared plan cache"))
}
