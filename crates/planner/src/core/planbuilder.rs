//! go-file: pkg/planner/core/planbuilder.go
//!
//! The VS1-10 skeleton of the plan builder: the `PlanBuilder` state the SELECT builder
//! (VS1-21) fills, its construction / reuse, the visit-info helpers and `Build`'s dispatch.
//! `Build` sends a SELECT to `buildSelect` (D09.1, VS1-21) and every other statement to its
//! block; until those are ported each arm is a boundary stub.
//!
//! - `b.ctx` / `b.is`: the planner gets `&mut PlanCtx` per call (`M-CTX`); the InfoSchema is
//!   D06.3 part 2 (VS1-17).
//! - `hint.QBHintHandler` / `QBHintBuildState` and `SessionVars.PlannerSelectBlockAsName`
//!   are D13.3 (VS1-24): `Init` keeps Go's shape without them.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_parser::ast::{Expr, Op, SelectStmt, Stmt};
use tidb_sessionctx::stmtctx::TableEntry;
use tidb_sessionctx::variable::session::SessionVars;

/// visitInfo is used for privilege check.
#[derive(Clone, Debug, Default)]
pub struct VisitInfo {
    /// privilege.
    pub privilege: u64,
    /// db.
    pub db: String,
    /// table.
    pub table: String,
    /// column.
    pub column: String,
    /// err (by identity in Go; the message here).
    pub err: Option<String>,
    /// alterWritable.
    pub alter_writable: bool,
    /// dynamicPrivs.
    pub dynamic_privs: Vec<String>,
    /// dynamicWithGrant.
    pub dynamic_with_grant: bool,
}

impl VisitInfo {
    /// Go compares `err` by identity (`==` on the interface); here by message.
    // go: visitInfo.Equals L2 (err compared by message: Rust errors have no identity)
    pub fn equals(&self, other: &VisitInfo) -> bool {
        self.privilege == other.privilege
            && self.db == other.db
            && self.table == other.table
            && self.column == other.column
            && self.err == other.err
            && self.alter_writable == other.alter_writable
            && self.dynamic_privs == other.dynamic_privs
            && self.dynamic_with_grant == other.dynamic_with_grant
    }
}

/// `appendVisitInfoIsRestrictedUser` asks the privilege manager (D04.1) whether a user is
/// RESTRICTED (SEM).
// glue: stub of appendVisitInfoIsRestrictedUser, blocked-by:D04.1 (privilege manager, SEM)
pub fn append_visit_info_is_restricted_user() -> Result<(), Error> {
    Err(blocked(
        "D04.1",
        "appendVisitInfoIsRestrictedUser (privilege manager)",
    ))
}

/// GetDBTableInfo gets the accessed dbs and tables info.
// go: GetDBTableInfo L1
pub fn get_db_table_info(visit_info: &[VisitInfo]) -> Vec<TableEntry> {
    let mut tables: Vec<TableEntry> = Vec::new();
    let exists_func = |tbls: &[TableEntry], tbl: &TableEntry| tbls.iter().any(|t| t == tbl);
    for v in visit_info {
        if v.db.is_empty() && v.table.is_empty() {
            // when v.db == "" and v.table == "", it means this visitInfo is for dynamic privilege,
            // so it is not related to any database or table.
            continue;
        }

        let tbl = TableEntry {
            db: v.db.clone(),
            table: v.table.clone(),
        };
        if !exists_func(&tables, &tbl) {
            tables.push(tbl);
        }
    }
    tables
}

/// PlanBuilder builds Plan from an ast.Node.
/// It just builds the ast node straightforwardly.
///
/// The fields of the VS1-10 skeleton; the SELECT builder (VS1-21) adds the rest.
#[derive(Debug, Default)]
pub struct PlanBuilder {
    /// visitInfo is used for privilege check.
    pub visit_info: Vec<VisitInfo>,
    /// optFlag.
    pub opt_flag: u64,
    /// isForUpdateRead should be true in either of the following situations
    /// 1. use `inside insert`, `update`, `delete` or `select for update` statement
    /// 2. isolation level is RC
    pub is_for_update_read: bool,
    /// qbOffset is the offsets of current processing select stmts.
    pub qb_offset: Vec<isize>,
    /// isSampling indicates whether the query is sampling.
    pub is_sampling: bool,
    /// noDecorrelate.
    pub no_decorrelate: bool,
    /// disableSubQueryPreprocessing indicates whether to pre-process uncorrelated sub-queries in rewriting stage.
    pub disable_sub_query_preprocessing: bool,
    /// allowBuildCastArray indicates whether allow cast(... as ... array).
    pub allow_build_cast_array: bool,
}

/// PlanBuilderOpt is used to adjust the plan builder.
pub trait PlanBuilderOpt {
    /// Apply.
    // glue: interface method of PlanBuilderOpt
    fn apply(&self, builder: &mut PlanBuilder);
}

/// PlanBuilderOptNoExecution means the plan builder should not run any executor during Build().
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanBuilderOptNoExecution;

impl PlanBuilderOpt for PlanBuilderOptNoExecution {
    /// Apply implements the interface PlanBuilderOpt.
    // go: PlanBuilderOptNoExecution.Apply L1
    fn apply(&self, builder: &mut PlanBuilder) {
        builder.disable_sub_query_preprocessing = true;
    }
}

/// PlanBuilderOptAllowCastArray means the plan builder should allow build cast(... as ... array).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanBuilderOptAllowCastArray;

impl PlanBuilderOpt for PlanBuilderOptAllowCastArray {
    /// Apply implements the interface PlanBuilderOpt.
    // go: PlanBuilderOptAllowCastArray.Apply L1
    fn apply(&self, builder: &mut PlanBuilder) {
        builder.allow_build_cast_array = true;
    }
}

/// NewPlanBuilder creates a new PlanBuilder.
///
/// Go also allocates `outerCTEs`, `colMapper`, `handleHelper` and `correlatedAggMapper`;
/// those fields come with the SELECT builder (VS1-21).
// go: NewPlanBuilder L1
pub fn new_plan_builder(opts: &[&dyn PlanBuilderOpt]) -> PlanBuilder {
    let mut builder = PlanBuilder::default();
    for opt in opts {
        opt.apply(&mut builder);
    }
    builder
}

impl PlanBuilder {
    /// Init initialize a PlanBuilder.
    /// Return the original PlannerSelectBlockAsName as well, callers decide if
    /// PlannerSelectBlockAsName should be restored after using this builder.
    ///
    /// `PlannerSelectBlockAsName` and the hint handler are D13.3 (see the module doc): the
    /// saved block names are always empty for now.
    // go: PlanBuilder.Init L1 [M-CTX]
    pub fn init(&mut self, vars: &mut SessionVars) -> Vec<String> {
        self.is_for_update_read = vars.is_pessimistic_read_consistency();
        self.no_decorrelate = vars.enable_no_decorrelate_in_select;
        Vec::new()
    }

    /// ResetForReuse reset the plan builder, put it into pool for reuse.
    /// After reset for reuse, the object should be equal to a object returned by NewPlanBuilder().
    // go: PlanBuilder.ResetForReuse L1
    pub fn reset_for_reuse(&mut self) -> &mut PlanBuilder {
        // Reset ALL the fields.
        *self = PlanBuilder::default();
        self.no_decorrelate = false;
        self
    }

    /// Build builds the ast node to a Plan.
    // go: PlanBuilder.Build L1 [M-CTX]
    pub fn build(&mut self, node: &Stmt) -> Result<(), Error> {
        // b.optFlag |= rule.FlagPruneColumns
        match node {
            Stmt::Select(_) | Stmt::SetOpr(_) => {
                // glue: blocked-by:D09.1 (VS1-21: buildSelect / buildSetOpr)
                Err(blocked("D09.1", "buildSelect"))
            }
            Stmt::Use(raw) => {
                if raw.db_name.is_empty() {
                    return Err(tidb_base::dbterror::plannererrors::ErrNoDB.clone());
                }
                // glue: blocked-by:D12.12 (the SIMPLE plan of USE / BEGIN / COMMIT / SET)
                Err(blocked("D12.12", "the Simple plan"))
            }
            _ => Err(blocked("D12.12", "the plan of this statement")),
        }
    }

    /// GetOptFlag gets the OptFlag of the PlanBuilder.
    // go: PlanBuilder.GetOptFlag L1
    pub fn get_opt_flag(&self) -> u64 {
        if self.is_sampling {
            // Disable logical optimization to avoid the optimizer
            // push down/eliminate operands like Selection, Limit or Sort.
            return 0;
        }
        self.opt_flag
    }

    /// GetVisitInfo gets the visitInfo of the PlanBuilder.
    // go: PlanBuilder.GetVisitInfo L1
    pub fn get_visit_info(&self) -> &[VisitInfo] {
        &self.visit_info
    }

    /// GetIsForUpdateRead gets if the PlanBuilder use forUpdateRead
    // go: PlanBuilder.GetIsForUpdateRead L1
    pub fn get_is_for_update_read(&self) -> bool {
        self.is_for_update_read
    }

    /// GetHintState gets the HintState from the PlanBuilder (`hint.QBHintBuildState`, D13.3).
    // glue: stub of PlanBuilder.GetHintState, blocked-by:D13.3 (hint.QBHintBuildState)
    pub fn get_hint_state(&self) -> Result<(), Error> {
        Err(blocked("D13.3", "GetHintState (hint.QBHintBuildState)"))
    }

    // go: PlanBuilder.getSelectOffset L1
    pub fn get_select_offset(&self) -> isize {
        if let Some(o) = self.qb_offset.last() {
            return *o;
        }
        -1
    }

    // go: PlanBuilder.pushSelectOffset L1
    pub fn push_select_offset(&mut self, offset: isize) {
        self.qb_offset.push(offset);
    }

    // go: PlanBuilder.popSelectOffset L1
    pub fn pop_select_offset(&mut self) {
        self.qb_offset.pop();
    }

    /// detectSelectAgg detects an aggregate function or GROUP BY clause.
    // go: PlanBuilder.detectSelectAgg L1
    pub fn detect_select_agg(&self, sel: &SelectStmt) -> bool {
        if sel.group_by.is_some() {
            return true;
        }
        for f in &sel.fields.fields {
            if f.wild_card.is_some() {
                continue;
            }
            if f.expr.as_ref().is_some_and(has_agg_flag) {
                return true;
            }
        }
        if let Some(h) = &sel.having
            && has_agg_flag(&h.expr)
        {
            return true;
        }
        if let Some(o) = &sel.order_by {
            for item in &o.items {
                if has_agg_flag(&item.expr) {
                    return true;
                }
            }
        }
        false
    }

    /// `detectAggInExprNode`: the `AggregateFuncExtractor` visitor of `util.go` (D09.4);
    /// the flag check below answers whether any aggregate is inside.
    // go: PlanBuilder.detectAggInExprNode L2 (the AST has no visitor: a recursive check of the aggregate flag)
    pub fn detect_agg_in_expr_node(&self, exprs: &[Expr]) -> bool {
        exprs.iter().any(has_agg_flag)
    }
}

/// `ast.HasAggFlag`: whether an aggregate function is in `e`, outside subqueries (the parser
/// sets `FlagHasAggregateFunc` bottom-up; the Rust AST has no flags, D02.2).
// glue: ast.HasAggFlag over the Rust AST (parser flags, D02.2)
fn has_agg_flag(e: &Expr) -> bool {
    match e {
        Expr::Aggregate(_) => true,
        Expr::Binary(b) => has_agg_flag(&b.l) || has_agg_flag(&b.r),
        Expr::Unary(u) => has_agg_flag(&u.v),
        Expr::Paren(p) => has_agg_flag(p),
        Expr::FuncCall(f) => f.args.iter().any(has_agg_flag),
        Expr::FuncCast(c) => has_agg_flag(&c.expr),
        Expr::IsNull(x) => has_agg_flag(&x.expr),
        Expr::IsTruth(x) => has_agg_flag(&x.expr),
        Expr::Between(x) => {
            has_agg_flag(&x.expr) || has_agg_flag(&x.left) || has_agg_flag(&x.right)
        }
        Expr::PatternIn(x) => has_agg_flag(&x.expr) || x.list.iter().any(has_agg_flag),
        Expr::Row(r) => r.values.iter().any(has_agg_flag),
        Expr::Case(c) => {
            c.value.as_ref().is_some_and(has_agg_flag)
                || c.when_clauses
                    .iter()
                    .any(|w| has_agg_flag(&w.expr) || has_agg_flag(&w.result))
                || c.else_clause.as_ref().is_some_and(has_agg_flag)
        }
        _ => false,
    }
}

/// splitWhere split a where expression to a list of AND conditions.
// go: splitWhere L1
pub fn split_where(where_: Option<&Expr>) -> Vec<&Expr> {
    let mut conditions = Vec::new();
    match where_ {
        None => {}
        Some(Expr::Binary(x)) => {
            if x.op == Op::LogicAnd {
                conditions.extend(split_where(Some(&x.l)));
                conditions.extend(split_where(Some(&x.r)));
            } else {
                conditions.push(where_.expect("some"));
            }
        }
        Some(Expr::Paren(x)) => conditions.extend(split_where(Some(x))),
        Some(w) => conditions.push(w),
    }
    conditions
}

/// `checkHintedSQL` parses the hints of a binding (`hint.ParseHintsSet`, D13.3).
// glue: stub of checkHintedSQL, blocked-by:D13.3 (hint.ParseHintsSet)
pub fn check_hinted_sql() -> Result<(), Error> {
    Err(blocked("D13.3", "checkHintedSQL (hint.ParseHintsSet)"))
}
