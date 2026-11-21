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

use std::collections::HashMap;

use std::sync::Arc;
use tidb_base::Error;
use tidb_base::blocked::blocked;

use tidb_base::dbterror::plannererrors::ErrInternal;
use tidb_base::{go_args, kv, mysql};
use tidb_model::{StatePublic, TableInfo};
use tidb_parser::ast::{
    ColumnName, ExecuteStmt, Expr, IndexHint, Op, SelectLockInfo, SelectLockType, SelectStmt, Stmt,
};
use tidb_parser::model::CIStr;
use tidb_sessionctx::variable::names::TiDBIsolationReadEngines;
use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;

use crate::arena::PlanId;
use crate::core::common_plans::Execute;
use crate::core::plan_cache_utils::get_prepared_stmt;
use crate::core::preprocess::InfoSchemaHandle;
use crate::core::rule::logical_rules::FlagPruneColumns;
use crate::planctx::PlanCtx;
use crate::util::path::INDEX_LOOK_UP_PUSH_DOWN_BY_SYS_VAR;
use crate::util::{AccessPath, HandleCols};
use tidb_sessionctx::stmtctx::TableEntry;
use tidb_sessionctx::variable::session::SessionVars;

use crate::core::expression_rewriter::{ByAddr, CheckSelectColumnPrivOption, ClauseCode};

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
    /// err: the error `CheckPrivilege` returns when the privilege is missing (the full
    /// error: the client sees its code and SQLSTATE).
    pub err: Option<Error>,
    /// alterWritable.
    pub alter_writable: bool,
    /// dynamicPrivs.
    pub dynamic_privs: Vec<String>,
    /// dynamicWithGrant.
    pub dynamic_with_grant: bool,
}

impl VisitInfo {
    /// Go compares `err` by identity (`==` on the interface); Rust errors have no identity,
    /// so two errors are equal when their code and message are.
    // go: visitInfo.Equals L2 (err compared by code and message: Rust errors have no identity)
    pub fn equals(&self, other: &VisitInfo) -> bool {
        self.privilege == other.privilege
            && self.db == other.db
            && self.table == other.table
            && self.column == other.column
            && match (&self.err, &other.err) {
                (None, None) => true,
                (Some(a), Some(b)) => a.code() == b.code() && a.error() == b.error(),
                _ => false,
            }
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

/// handleColHelper is used to collect the handle columns of the tables in a query block
/// (SELECT ... FOR UPDATE and the DML statements read them).
#[derive(Debug, Default)]
pub struct HandleColHelper {
    /// id2HandleMapStack (a nil map is `None`).
    pub id2_handle_map_stack: Vec<Option<HashMap<i64, Vec<HandleCols>>>>,
    /// stackTail.
    pub stack_tail: usize,
}

impl HandleColHelper {
    // go: handleColHelper.popMap L1
    pub fn pop_map(&mut self) -> Option<HashMap<i64, Vec<HandleCols>>> {
        let ret = self.id2_handle_map_stack[self.stack_tail - 1].take();
        self.stack_tail -= 1;
        self.id2_handle_map_stack.truncate(self.stack_tail);
        ret
    }

    // go: handleColHelper.pushMap L1
    pub fn push_map(&mut self, m: Option<HashMap<i64, Vec<HandleCols>>>) {
        self.id2_handle_map_stack.push(m);
        self.stack_tail += 1;
    }
}

/// PlanBuilder builds Plan from an ast.Node.
/// It just builds the ast node straightforwardly.
///
/// The fields of the SELECT builder (VS1-21); the fields of the other statements, of hints
/// (D13.3), CTEs (D09.5), subqueries (D09.6), windows and aggregations (D09.4) come with
/// those blocks (D09.1 spec §3).
#[derive(Debug, Default)]
pub struct PlanBuilder<'ast> {
    /// is: the InfoSchema of the statement (`Init`'s argument).
    pub is: Option<InfoSchemaHandle>,
    /// The domain's newest InfoSchema for the metadata lock (`domain.GetDomain(sctx)` in
    /// `tryLockMDLAndUpdateSchemaIfNecessary`): the caller hands it in, as to the
    /// preprocessor (`with_domain_info_schema`).
    pub domain_is: Option<InfoSchemaHandle>,
    /// The SQL text the statement's AST was parsed from: Go keeps a node's text in the node
    /// (`ast.Node.Text`), the Rust AST keeps byte spans into this text.
    pub sql_text: &'ast str,
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
    /// curClause: the clause being built (the expression rewriter's error texts, D09.2).
    pub cur_clause: ClauseCode,
    /// checkColPriv.
    pub check_col_priv: CheckSelectColumnPrivOption,
    /// colMapper stores the column that must be pre-resolved, keyed by the `ColumnNameExpr`
    /// node's identity (Go: the node pointer; D09.2 spec §3 item 4): the key borrows the
    /// node from the statement's AST (`'ast`).
    pub col_mapper: HashMap<ByAddr<'ast, ColumnName>, usize>,
    /// inStraightJoin represents whether the current "SELECT" statement has
    /// "STRAIGHT_JOIN" option.
    pub in_straight_join: bool,
    /// handleHelper records the handle column position for tables. Delete/Update/SelectLock/UnionScan may need this information.
    /// It collects the information by the following procedure:
    ///
    /// ```text
    ///   Since we build the plan tree from bottom to top, we maintain a stack to record the current handle information.
    ///   If it's a dataSource/tableDual node, we create a new map.
    ///   If it's an aggregation, we pop the map and push a nil map since no handle information left.
    ///   If it's a union, we pop all children's and push a nil map.
    ///   If it's a join, we pop its children's out then merge them and push the new map to stack.
    ///   If we meet a subquery, it's clearly that it's an independent problem so we just pop one map out when we finish building the subquery.
    /// ```
    pub handle_helper: HandleColHelper,
    /// allNames: the output names of the plans the query blocks being built read
    /// (`evalDefaultExpr` reads them, D09.15).
    pub all_names: Vec<NameSlice>,
}

/// PlanBuilderOpt is used to adjust the plan builder.
pub trait PlanBuilderOpt {
    /// Apply.
    // glue: interface method of PlanBuilderOpt
    fn apply(&self, builder: &mut PlanBuilder<'_>);
}

/// PlanBuilderOptNoExecution means the plan builder should not run any executor during Build().
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanBuilderOptNoExecution;

impl PlanBuilderOpt for PlanBuilderOptNoExecution {
    /// Apply implements the interface PlanBuilderOpt.
    // go: PlanBuilderOptNoExecution.Apply L1
    fn apply(&self, builder: &mut PlanBuilder<'_>) {
        builder.disable_sub_query_preprocessing = true;
    }
}

/// PlanBuilderOptAllowCastArray means the plan builder should allow build cast(... as ... array).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanBuilderOptAllowCastArray;

impl PlanBuilderOpt for PlanBuilderOptAllowCastArray {
    /// Apply implements the interface PlanBuilderOpt.
    // go: PlanBuilderOptAllowCastArray.Apply L1
    fn apply(&self, builder: &mut PlanBuilder<'_>) {
        builder.allow_build_cast_array = true;
    }
}

/// NewPlanBuilder creates a new PlanBuilder.
///
/// Go also allocates `outerCTEs`, `colMapper`, `handleHelper` and `correlatedAggMapper`;
/// those fields come with the SELECT builder (VS1-21).
// go: NewPlanBuilder L1
pub fn new_plan_builder<'ast>(opts: &[&dyn PlanBuilderOpt]) -> PlanBuilder<'ast> {
    let mut builder = PlanBuilder::default();
    for opt in opts {
        opt.apply(&mut builder);
    }
    builder
}

impl<'ast> PlanBuilder<'ast> {
    /// Init initialize a PlanBuilder.
    /// Return the original PlannerSelectBlockAsName as well, callers decide if
    /// PlannerSelectBlockAsName should be restored after using this builder.
    ///
    /// `PlannerSelectBlockAsName` and the hint handler are D13.3 (see the module doc): the
    /// saved block names are always empty for now.
    // go: PlanBuilder.Init L1 [M-CTX]
    pub fn init(&mut self, vars: &mut SessionVars, is: Option<InfoSchemaHandle>) -> Vec<String> {
        self.is = is;
        self.is_for_update_read = vars.is_pessimistic_read_consistency();
        self.no_decorrelate = vars.enable_no_decorrelate_in_select;
        Vec::new()
    }

    /// ResetForReuse reset the plan builder, put it into pool for reuse.
    /// After reset for reuse, the object should be equal to a object returned by NewPlanBuilder().
    // go: PlanBuilder.ResetForReuse L1
    pub fn reset_for_reuse(&mut self) -> &mut PlanBuilder<'ast> {
        // Reset ALL the fields.
        *self = PlanBuilder::default();
        self.no_decorrelate = false;
        self
    }

    /// Build builds the ast node to a Plan.
    ///
    /// The logical plans are in the planning context's arena: a SELECT returns the root's
    /// [`PlanId`]. The other statements (and `SELECT ... INTO`, set operations) are boundary
    /// stubs of their blocks. Go sets `b.resolveCtx` here; only the lock clause and the
    /// write path read it, which are boundary stubs, so the builder does not keep it.
    // go: PlanBuilder.Build L2 (only SELECT is built; the other statements are boundary stubs)
    pub fn build(&mut self, ctx: &mut PlanCtx<'_>, node: &'ast Stmt) -> Result<PlanId, Error> {
        self.opt_flag |= FlagPruneColumns;
        match node {
            Stmt::Select(x) => {
                if x.select_into_opt.is_some() {
                    // glue: blocked-by:D09.16 (SELECT ... INTO: buildSelectInto)
                    return Err(blocked("D09.16", "SELECT ... INTO (buildSelectInto)"));
                }
                self.build_select(ctx, x)
            }
            Stmt::SetOpr(_) => {
                // glue: blocked-by:D09.5 (set operations: buildSetOpr)
                Err(blocked("D09.5", "a set operation (buildSetOpr)"))
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

    /// buildExecute: the `Execute` plan of an `EXECUTE` statement. `Build` returns no plan
    /// yet (its arms are other blocks), so `OptimizeExecStmt` calls this arm directly.
    ///
    /// The text protocol's `USING @var, ...` needs the expression rewriter (`b.rewrite`, D09.2);
    /// the binary protocol's arguments are the values of constants (see [`Execute`]).
    // go: PlanBuilder.buildExecute L2 (USING variables need the expression rewriter; the binary arguments are values)
    pub fn build_execute(&mut self, vars: &SessionVars, v: &ExecuteStmt) -> Result<Execute, Error> {
        let mut params: Vec<Datum> = Vec::with_capacity(v.using_vars.len());
        if !v.using_vars.is_empty() {
            // newExpr, _, err := b.rewrite(ctx, expr, nil, nil, true) for each variable.
            // glue: blocked-by:D09.2 (the expression rewriter, b.rewrite of EXECUTE ... USING)
            return Err(blocked("D09.2", "b.rewrite of EXECUTE ... USING"));
        }

        let prep_stmt = get_prepared_stmt(v, vars)?;
        if let Some(args) = &v.binary_args {
            // v.BinaryArgs.([]expression.Expression): Go's type assertion panics on another
            // type; the session always sets the values.
            let Some(args) = args.downcast_ref::<Vec<Datum>>() else {
                return Err(tidb_base::errors::new(
                    "invalid EXECUTE binary arguments".to_owned(),
                ));
            };
            params = args.clone();
        }
        Ok(Execute {
            name: v.name.clone(),
            params,
            prep_stmt,
            plan: None,
            output_names: NameSlice::default(),
        })
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

    // go: PlanBuilder.detectSelectWindow L1
    pub fn detect_select_window(&self, sel: &SelectStmt) -> bool {
        for f in &sel.fields.fields {
            if f.expr.as_ref().is_some_and(has_window_flag) {
                return true;
            }
        }
        if let Some(o) = &sel.order_by {
            for item in &o.items {
                if has_window_flag(&item.expr) {
                    return true;
                }
            }
        }
        false
    }
}

/// `ast.HasWindowFlag`: whether a window function is in `e`, outside subqueries (the parser
/// sets `FlagHasWindowFunc` bottom-up; the Rust AST has no flags, D02.2).
// glue: ast.HasWindowFlag over the Rust AST (parser flags, D02.2)
pub fn has_window_flag(e: &Expr) -> bool {
    match e {
        Expr::Window(_) => true,
        Expr::Aggregate(a) => a.args.iter().any(has_window_flag),
        Expr::Binary(b) => has_window_flag(&b.l) || has_window_flag(&b.r),
        Expr::Unary(u) => has_window_flag(&u.v),
        Expr::Paren(p) => has_window_flag(p),
        Expr::FuncCall(f) => f.args.iter().any(has_window_flag),
        Expr::FuncCast(c) => has_window_flag(&c.expr),
        Expr::IsNull(x) => has_window_flag(&x.expr),
        Expr::IsTruth(x) => has_window_flag(&x.expr),
        Expr::Between(x) => {
            has_window_flag(&x.expr) || has_window_flag(&x.left) || has_window_flag(&x.right)
        }
        Expr::PatternIn(x) => has_window_flag(&x.expr) || x.list.iter().any(has_window_flag),
        Expr::PatternLike(x) => has_window_flag(&x.expr) || has_window_flag(&x.pattern),
        Expr::PatternRegexp(x) => has_window_flag(&x.expr) || has_window_flag(&x.pattern),
        Expr::Row(r) => r.values.iter().any(has_window_flag),
        Expr::SetCollation(x) => has_window_flag(&x.expr),
        Expr::Case(c) => {
            c.value.as_ref().is_some_and(has_window_flag)
                || c.when_clauses
                    .iter()
                    .any(|w| has_window_flag(&w.expr) || has_window_flag(&w.result))
                || c.else_clause.as_ref().is_some_and(has_window_flag)
        }
        _ => false,
    }
}

/// `ast.ReadCommitted`.
#[allow(non_upper_case_globals)]
const ReadCommitted: &str = "READ-COMMITTED";

// go: fillContentForTablePath L1
fn fill_content_for_table_path(table_path: &mut AccessPath, tbl_info: &TableInfo) {
    if tbl_info.is_common_handle {
        table_path.is_common_handle_path = true;
        for index in tbl_info.indices.iter().flatten() {
            if index.primary {
                table_path.index = Some(Arc::clone(index));
                break;
            }
        }
    } else {
        table_path.is_int_handle_path = true;
    }
}

// go: genTiFlashPath L1
fn gen_tiflash_path(tbl_info: &TableInfo) -> AccessPath {
    let mut tiflash_path = AccessPath {
        store_type: kv::TiFlash,
        ..AccessPath::default()
    };
    fill_content_for_table_path(&mut tiflash_path, tbl_info);
    tiflash_path
}

/// isForUpdateReadSelectLock checks if the lock type need to use forUpdateRead
// go: isForUpdateReadSelectLock L1
pub fn is_for_update_read_select_lock(lock: Option<&SelectLockInfo>) -> bool {
    let Some(lock) = lock else {
        return false;
    };
    lock.lock_type == SelectLockType::ForUpdate
        || lock.lock_type == SelectLockType::ForUpdateNoWait
        || lock.lock_type == SelectLockType::ForUpdateWaitN
}

// go: getPathByIndexName L1
fn get_path_by_index_name<'p>(
    paths: &'p [AccessPath],
    idx_name: &str,
    tbl_info: &TableInfo,
) -> Option<&'p AccessPath> {
    let mut index_prefix_path = None;
    let mut prefix_matches = 0;
    for path in paths {
        // Only accept tikv's primary key table path.
        if path.is_tikv_table_path() && is_primary_index(idx_name) && tbl_info.has_clustered_index()
        {
            return Some(path);
        }
        // If it's not a tikv table path and the index is nil, it could not be any index path.
        let Some(index) = &path.index else {
            continue;
        };
        if index.name.l == idx_name {
            return Some(path);
        }
        if index.name.l.starts_with(idx_name) {
            index_prefix_path = Some(path);
            prefix_matches += 1;
        }
    }

    // Return only unique prefix matches
    if prefix_matches == 1 {
        return index_prefix_path;
    }
    None
}

// go: isPrimaryIndex L1
fn is_primary_index(index_name: &str) -> bool {
    index_name == "primary"
}

/// `checkAutoForceIndexLookUpPushDown`: under the default policy (`hint-only`) the index
/// lookup is not pushed down automatically. The other policies check the session and the
/// table (`checkIndexLookUpPushDownSupported`, the access path handling of D10.2).
// go: checkAutoForceIndexLookUpPushDown L2 (the force policies are blocked-by:D10.2)
fn check_auto_force_index_look_up_push_down(vars: &SessionVars) -> Result<bool, Error> {
    match vars.index_look_up_push_down_policy.as_str() {
        "force" | "affinity-force" => {
            // glue: blocked-by:D10.2 (checkIndexLookUpPushDownSupported, the forced index lookup push down)
            Err(blocked(
                "D10.2",
                "tidb_index_lookup_pushdown_policy other than hint-only (checkIndexLookUpPushDownSupported)",
            ))
        }
        _ => Ok(false),
    }
}

/// getPossibleAccessPaths: the table path, the TiFlash path when the table has an available
/// replica, and a path for each public index the optimizer may use (D09.1 spec §4).
///
/// The planner has the table's meta (`tbl.Meta()`); a cluster table (`tbl.Type()
/// .IsClusterTable()`) is a memory table, which the builder stops at before. Boundary stubs
/// (D09.1 spec §5): index hints (`USE / FORCE / IGNORE INDEX`, and the comment-style hints
/// of `tableHints`, D13.3) are `blocked-by:D13.3`; hypothetical indexes and TiFlash replicas
/// (EXPLAIN only) and reading the newest index states from the domain (`check`: a
/// connection reading for update or under READ COMMITTED) are `blocked-by:D10.2`.
// go: getPossibleAccessPaths L2 (index hints, hypothetical indexes and the newest index states are boundary stubs)
pub fn get_possible_access_paths(
    vars: &mut SessionVars,
    index_hints: &[IndexHint],
    tbl_info: &TableInfo,
    db_name: &CIStr,
    check: bool,
    has_flag_partition_processor: bool,
) -> Result<Vec<AccessPath>, Error> {
    let mut public_paths: Vec<AccessPath> =
        Vec::with_capacity(tbl_info.indices.iter().flatten().count() + 2);
    // tbl.Type().IsClusterTable(): cluster tables are memory tables (see the doc).
    let tp = kv::TiKV;
    let mut table_path = AccessPath {
        store_type: tp,
        ..AccessPath::default()
    };
    fill_content_for_table_path(&mut table_path, tbl_info);
    public_paths.push(table_path.clone_path());

    match &tbl_info.tiflash_replica {
        None => vars.raise_warning_when_mpp_enforced(&format!(
            "MPP mode may be blocked because there aren't tiflash replicas of table `{}`.",
            tbl_info.name.o
        )),
        Some(r) if !r.available => vars.raise_warning_when_mpp_enforced(&format!(
            "MPP mode may be blocked because tiflash replicas of table `{}` not ready.",
            tbl_info.name.o
        )),
        Some(_) => public_paths.push(gen_tiflash_path(tbl_info)),
    }

    // consider hypo TiFlash replicas
    if vars.stmt_ctx().in_explain_stmt && vars.hypo_tiflash_replicas.is_some() {
        // glue: blocked-by:D10.2 (hypothetical TiFlash replicas of EXPLAIN)
        return Err(blocked("D10.2", "hypothetical TiFlash replicas (EXPLAIN)"));
    }
    let _ = db_name;

    let optimizer_use_invisible_indexes = vars.optimizer_use_invisible_indexes;

    let mut check = check || vars.is_isolation(ReadCommitted);
    check = check && vars.connection_id > 0;

    // When NO_INDEX_LOOKUP_PUSHDOWN hint is specified, we should set `forceNoIndexLookUpPushDown = true` to avoid
    // using index look up push down even if other hint or system variable `tidb_index_lookup_pushdown_policy`
    // tries to enable it.
    // (tableHints.NoIndexLookUpPushDown: no hints here, D13.3.)
    let force_no_index_look_up_push_down = false;

    for index in tbl_info.indices.iter().flatten() {
        if index.state == StatePublic {
            // Filter out invisible index, because they are not visible for optimizer
            if !optimizer_use_invisible_indexes && index.invisible {
                continue;
            }
            if tbl_info.is_common_handle && index.primary {
                continue;
            }
            if check {
                // latestIndexes, check, err = domainmisc.GetLatestIndexInfo(ctx, tblInfo.ID, 0)
                // glue: blocked-by:D10.2 (domainmisc.GetLatestIndexInfo: the newest index states for reads for update / READ COMMITTED)
                return Err(blocked(
                    "D10.2",
                    "the newest index states of the domain (GetLatestIndexInfo) for a read for update or under READ COMMITTED",
                ));
            }
            if index.vector_info.is_some() {
                // Because the value of `TiFlashReplica.Available` changes as the user modify replica, it is not ideal if the state of index changes accordingly.
                // So the current way to use the vector indexes is to require the TiFlash Replica to be available.
                if !tbl_info
                    .tiflash_replica
                    .as_ref()
                    .is_some_and(|r| r.available)
                {
                    continue;
                }
                let mut path = gen_tiflash_path(tbl_info);
                path.store_type = kv::TiFlash;
                path.index = Some(Arc::clone(index));
                public_paths.push(path);
                continue;
            }
            let mut path = AccessPath {
                index: Some(Arc::clone(index)),
                ..AccessPath::default()
            };
            if !force_no_index_look_up_push_down && check_auto_force_index_look_up_push_down(vars)?
            {
                path.index_look_up_push_down_by = INDEX_LOOK_UP_PUSH_DOWN_BY_SYS_VAR;
            }
            public_paths.push(path);
        }
    }

    // consider hypo-indexes
    if vars.stmt_ctx().in_explain_stmt && vars.hypo_indexes.is_some() {
        // glue: blocked-by:D10.2 (hypothetical indexes of EXPLAIN)
        return Err(blocked("D10.2", "hypothetical indexes (EXPLAIN)"));
    }
    if vars.stmt_ctx().stmt_hints.hinted_hypo_indexes.is_some() {
        // glue: blocked-by:D13.3 (statement-level hypothetical indexes from hints)
        return Err(blocked("D13.3", "HYPO_INDEX hints"));
    }

    // Extract comment-style index hint like /*+ INDEX(t, idx1, idx2) */ (tableHints, D13.3:
    // none here) and the index hints of the table name.
    if !index_hints.is_empty() {
        // glue: blocked-by:D13.3 (index hints: USE / FORCE / IGNORE INDEX)
        return Err(blocked("D13.3", "index hints (USE / FORCE / IGNORE INDEX)"));
    }
    let (has_scan_hint, has_use_or_force) = (false, false);
    let ignored: Vec<AccessPath> = Vec::new();
    let mut available: Vec<AccessPath> = Vec::new();

    if !has_scan_hint || !has_use_or_force {
        available = public_paths;
    }

    available = remove_ignored_paths(available, &ignored, tbl_info);

    // global index must not use partition pruning optimization, as LogicalPartitionAll not suitable for global index.
    // ignore global index if flagPartitionProcessor exists.
    if has_flag_partition_processor {
        available = remove_global_index_paths(available);
    }

    // If we have got "FORCE" or "USE" index hint but got no available index,
    // we have to use table scan.
    if available.is_empty() {
        available.push(table_path.clone_path());
    }

    // If all available paths are Multi-Valued Index, Partial index or other index that need to check its usability in later phase,
    // it's possible that all these path are inapplicable,
    // so that the table paths are still added here to avoid failing to find any physical plan.
    let mut all_undetermined_path = true;
    for available_path in &available {
        if !available_path.is_undetermined() {
            all_undetermined_path = false;
            break;
        }
    }
    if all_undetermined_path {
        available.push(table_path);
    }

    Ok(available)
}

// go: filterPathByIsolationRead L1
pub fn filter_path_by_isolation_read(
    vars: &mut SessionVars,
    mut paths: Vec<AccessPath>,
    tbl_name: &CIStr,
    db_name: &CIStr,
) -> Result<Vec<AccessPath>, Error> {
    // TODO: filter paths with isolation read locations.
    if is_sys_db(&db_name.l) {
        return Ok(paths);
    }
    let isolation_read_engines = vars.get_isolation_read_engines().clone();
    let mut available_engine: Vec<kv::StoreType> = Vec::new();
    let mut available_engine_str = String::new();
    let mut i = paths.len();
    while i > 0 {
        i -= 1;
        // availableEngineStr is for warning message.
        if !available_engine.contains(&paths[i].store_type) {
            available_engine.push(paths[i].store_type);
            if !available_engine_str.is_empty() {
                available_engine_str.push_str(", ");
            }
            available_engine_str.push_str(paths[i].store_type.name());
        }
        if !isolation_read_engines.contains(&paths[i].store_type) && paths[i].store_type != kv::TiDB
        {
            paths.remove(i);
        }
    }
    let mut err = None;
    let (engine_vals, _) = vars.get_system_var(TiDBIsolationReadEngines);
    if paths.is_empty() {
        let mut help_msg = String::new();
        if engine_vals == "tiflash" {
            help_msg = ". Please check tiflash replica".to_owned();
            if vars
                .stmt_ctx()
                .tiflash_engine_removed_due_to_strict_sql_mode
            {
                help_msg += " or check if the query is not readonly and sql mode is strict";
            }
        }
        err = Some(ErrInternal.gen_with_stack_by_args(go_args![format!(
            "No access path for table '{}' is found with '{}' = '{}', valid values can be '{}'{}.",
            tbl_name.o, TiDBIsolationReadEngines, engine_vals, available_engine_str, help_msg
        )]));
    }
    if !isolation_read_engines.contains(&kv::TiFlash) {
        if vars
            .stmt_ctx()
            .tiflash_engine_removed_due_to_strict_sql_mode
        {
            vars.raise_warning_when_mpp_enforced(
                "MPP mode may be blocked because the query is not readonly and sql mode is strict.",
            );
        } else {
            vars.raise_warning_when_mpp_enforced(&format!(
                "MPP mode may be blocked because '{}'(value: '{}') not match, need 'tiflash'.",
                TiDBIsolationReadEngines, engine_vals
            ));
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok(paths),
    }
}

// go: removeIgnoredPaths L1
fn remove_ignored_paths(
    paths: Vec<AccessPath>,
    ignored_paths: &[AccessPath],
    tbl_info: &TableInfo,
) -> Vec<AccessPath> {
    if ignored_paths.is_empty() {
        return paths;
    }
    let mut remained_paths = Vec::with_capacity(paths.len());
    for path in paths {
        if path.is_tikv_table_path()
            || path.is_tiflash_simple_table_path()
            || get_path_by_index_name(
                ignored_paths,
                &path.index.as_ref().expect("an index path").name.l,
                tbl_info,
            )
            .is_none()
        {
            remained_paths.push(path);
        }
    }
    remained_paths
}

// go: removeGlobalIndexPaths L1
fn remove_global_index_paths(paths: Vec<AccessPath>) -> Vec<AccessPath> {
    paths
        .into_iter()
        .filter(|path| !path.index.as_ref().is_some_and(|i| i.global))
        .collect()
}

/// `util.IsSysDB` (`pkg/util/misc.go`).
// go-dep: pkg/util IsSysDB
fn is_sys_db(db_lower_name: &str) -> bool {
    db_lower_name == mysql::SystemDB || db_lower_name == mysql::SysDB
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
