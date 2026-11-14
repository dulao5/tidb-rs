//! go-file: pkg/planner/core/preprocess.go
//!
//! The VS1-10 part of the preprocessor: name resolution and the checks a SELECT (or set
//! operation) goes through. The DDL / DML / binding / repair / sequence checks of the file
//! are the rest of D09.0.
//!
//! Mapping:
//! - Go's `preprocessor` is an `ast.Visitor` and the AST nodes' `Accept` methods drive it.
//!   The Rust AST (tidb-parser, the hand-written SELECT subset) has no visitor yet (D02.2),
//!   so [`Walker`] calls `Enter` / `Leave` in the order of Go's `Accept` methods for the
//!   nodes it has: `Enter` returning skip-children leaves the node at once, a `Leave`
//!   returning not-ok stops the walk (glue, `walk_*`).
//! - `p.sctx` is the session's `&mut SessionVars` (`M-CTX`). `sctx.GetPlanCtx()
//!   .SetReadonlyUserVarMap` has no plan context to go to before the session (VS1-14): the
//!   read-only variables are returned in `PreprocessorReturn::readonly_user_vars`.
//! - `PreprocessorReturn.InfoSchema` and `table.Table` come with the `InfoSchema` interface
//!   of D06.3 part 2 (VS1-17): resolving a base table is `blocked-by:D06.3` until then,
//!   and a missing InfoSchema (`sessiontxn.GetTxnManager(...).GetTxnInfoSchema()`) is
//!   `blocked-by:D05.4`. The stale-read processor (`staleread`, D05.4) and the MDL lock
//!   (`tryLockMDLAndUpdateSchemaIfNecessary`, D05.4 / D06.5) are boundary stubs too.
//! - The AST's `WithClause` is shared (`Arc`, as Go shares the `*WithClause`) and has no
//!   `ConsumerCount`: the counts `UpdateCTEConsumerCount` keeps are a side table keyed by the
//!   CTE (L2; the CTE builder, D09.5, reads them).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use tidb_base::blocked::blocked;
use tidb_base::dbterror::ErrDerivedMustHaveAlias;
use tidb_base::dbterror::plannererrors::{
    ErrInternal, ErrNoDB, ErrNonUniqTable, ErrWrongArguments, ErrWrongUsage,
};
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{self, ModeOracle, SQLMode};
use tidb_base::terror::ClassParser;
use tidb_base::{Error, errno};
use tidb_expression::errors::{
    ErrFunctionNotExists, ErrFunctionsNoopImpl, ErrIncorrectParameterCount,
};
use tidb_parser::ast::{
    self, CommonTableExpression, Expr, FuncCastExpr, GroupByClause, Join, ResultSetNode,
    SelectLockType, SelectStmt, SetOprNode, SetOprSelectList, SetOprStmt, Stmt, TableName,
    TableSource, WithClause, funcs as ast_funcs,
};
use tidb_parser::types::{ETDatetime, ETDecimal};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::varsutil::{OffInt, OnInt};
use tidb_types::errors::{ErrMBiggerThanD, ErrTooBigPrecision, ErrTooBigScale};
use tidb_types::fsp::MaxFsp;

use super::resolve::{ContextRef, NodeW};

/// `parser.ErrSyntax` (`pkg/parser/yy_parser.go`: `terror.ClassParser.NewStd(mysql.ErrSyntax)`).
#[allow(non_upper_case_globals)]
static ErrSyntax: LazyLock<Error> = LazyLock::new(|| ClassParser.new_std(errno::ErrSyntax));

/// PreprocessOpt presents optional parameters to `Preprocess` method.
pub type PreprocessOpt = Box<dyn FnOnce(&mut Preprocessor<'_>)>;

/// InPrepare is a PreprocessOpt that indicates preprocess is executing under prepare statement.
// go: InPrepare L1
pub fn in_prepare(p: &mut Preprocessor<'_>) {
    p.flag |= IN_PREPARE;
}

/// InTxnRetry is a PreprocessOpt that indicates preprocess is executing under transaction retry.
// go: InTxnRetry L1
pub fn in_txn_retry(p: &mut Preprocessor<'_>) {
    p.flag |= IN_TXN_RETRY;
}

/// InitTxnContextProvider is a PreprocessOpt that indicates preprocess should init transaction's context
// go: InitTxnContextProvider L1
pub fn init_txn_context_provider(p: &mut Preprocessor<'_>) {
    p.flag |= INIT_TXN_CONTEXT_PROVIDER;
}

/// WithPreprocessorReturn returns a PreprocessOpt to initialize the PreprocessorReturn.
///
/// Go hands in a pointer the caller reads afterwards; the result is returned by
/// [`preprocess`] instead, so the option sets the initial value.
// go: WithPreprocessorReturn L1
pub fn with_preprocessor_return(ret: PreprocessorReturn) -> PreprocessOpt {
    Box::new(move |p| p.ret = Some(ret))
}

/// TryAddExtraLimit trys to add an extra limit for SELECT or UNION statement when sql_select_limit is set.
///
/// Go returns a shallow copy of the statement with `Limit` set; the statement is changed in
/// place here (the callers replace their node with the result). The EXPLAIN and SHOW arms
/// need those statements in the AST (D02.2) and leave them as they are.
// go: TryAddExtraLimit L1
pub fn try_add_extra_limit(vars: &SessionVars, node: &mut Stmt) {
    if vars.select_limit == u64::MAX || vars.in_restricted_sql {
        return;
    }
    let limit = || {
        Box::new(ast::Limit {
            count: Expr::Value(Box::new(ast::ValueExpr::new(
                ast::Datum::Uint(vars.select_limit),
                "",
                "",
            ))),
            offset: None,
        })
    };
    match node {
        Stmt::Select(sel) => {
            if sel.limit.is_some() || sel.select_into_opt.is_some() {
                return;
            }
            sel.limit = Some(limit());
        }
        Stmt::SetOpr(set_opr) => {
            if set_opr.limit.is_some() {
                return;
            }
            set_opr.limit = Some(limit());
        }
        // ExplainStmt / ShowStmt: see the doc comment.
        _ => {}
    }
}

/// Preprocess resolves table names of the node, and checks some statements' validation.
/// preprocessReturn used to extract the infoschema for the tableName and the timestamp from the asof clause.
///
/// Returns the `PreprocessorReturn` (Go fills the caller's) and the first error.
// go: Preprocess L1 [M-CTX]
pub fn preprocess(
    vars: &mut SessionVars,
    node: &mut NodeW<'_>,
    preprocess_opt: Vec<PreprocessOpt>,
) -> (PreprocessorReturn, Result<(), Error>) {
    let mut v = Preprocessor {
        vars,
        flag: 0,
        stmt_tp: TYPE_INVALID,
        table_alias_in_join: Vec::new(),
        preprocess_with: PreprocessWith::default(),
        vars_mutable: HashSet::new(),
        vars_readonly: HashSet::new(),
        ret: None,
        err: None,
        resolve_ctx: node.get_resolve_context(),
    };
    for opt_fn in preprocess_opt {
        opt_fn(&mut v);
    }
    // PreprocessorReturn must be non-nil before preprocessing
    if v.ret.is_none() {
        v.ret = Some(PreprocessorReturn::default());
    }
    Walker(&mut v).stmt(node.node);
    // InfoSchema must be non-nil after preprocessing
    if let Err(e) = v.ensure_info_schema()
        && v.err.is_none()
    {
        v.err = Some(e);
    }
    let mut ret = v.ret.take().expect("set above");
    ret.readonly_user_vars = v.vars_readonly.clone();
    if !v.vars_readonly.is_empty() {
        v.vars
            .stmt_ctx()
            .plan_cache_tracker
            .set_skip_plan_cache("read-only variables are used");
    }
    (ret, v.err.map_or(Ok(()), Err))
}

type PreprocessorFlag = u64;

/// inPrepare is set when visiting in prepare statement.
const IN_PREPARE: PreprocessorFlag = 1;
/// inTxnRetry is set when visiting in transaction retry.
const IN_TXN_RETRY: PreprocessorFlag = 1 << 1;
/// inCreateOrDropTable is set when visiting create/drop table/view/sequence,
/// rename table, alter table add foreign key, and BR restore.
const IN_CREATE_OR_DROP_TABLE: PreprocessorFlag = 1 << 2;
/// parentIsJoin is set when visiting node's parent is join.
const PARENT_IS_JOIN: PreprocessorFlag = 1 << 3;
/// inRepairTable is set when visiting a repair table statement.
const IN_REPAIR_TABLE: PreprocessorFlag = 1 << 4;
/// inSequenceFunction is set when visiting a sequence function.
/// This flag indicates the tableName in these function should be checked as sequence object.
const IN_SEQUENCE_FUNCTION: PreprocessorFlag = 1 << 5;
/// initTxnContextProvider is set when we should init txn context in preprocess
const INIT_TXN_CONTEXT_PROVIDER: PreprocessorFlag = 1 << 6;
/// inImportInto is set when visiting an import into statement.
const IN_IMPORT_INTO: PreprocessorFlag = 1 << 7;
/// inAnalyze is set when visiting an analyze statement.
const IN_ANALYZE: PreprocessorFlag = 1 << 8;

/// PreprocessorReturn is used to retain information obtained in the preprocessor.
///
/// `InfoSchema` and `SnapshotTSEvaluator` come with D06.3 part 2 / D05.4 (see the module
/// doc); `readonly_user_vars` is what Go hands to `SetReadonlyUserVarMap`.
#[derive(Clone, Debug, Default)]
pub struct PreprocessorReturn {
    /// initedLastSnapshotTS.
    pub inited_last_snapshot_ts: bool,
    /// IsStaleness.
    pub is_staleness: bool,
    /// LastSnapshotTS is the last evaluated snapshotTS if any
    /// otherwise it defaults to zero
    pub last_snapshot_ts: u64,
    /// Whether an InfoSchema was provided (Go: `InfoSchema != nil`).
    pub has_info_schema: bool,
    /// The read-only user variables (Go: `sctx.GetPlanCtx().SetReadonlyUserVarMap`).
    pub readonly_user_vars: HashSet<String>,
}

/// preprocessWith is used to record info from WITH statements like CTE name.
#[derive(Debug, Default)]
pub struct PreprocessWith {
    cte_can_used: Vec<String>,
    cte_before_offset: Vec<usize>,
    /// A stack is implemented using a two-dimensional array.
    /// Each layer stores the cteList of the current query block.
    /// ** Only the cteStack of SelectStmt and SetOprStmt will be set. **
    cte_stack: Vec<Arc<WithClause>>,
    /// `CommonTableExpression.ConsumerCount`, by CTE (see the module doc).
    pub consumer_count: HashMap<usize, isize>,
}

impl PreprocessWith {
    // go: preprocessWith.UpdateCTEConsumerCount L2 (ConsumerCount is a side table, module doc)
    pub fn update_cte_consumer_count(&mut self, table_name: &str) {
        // must search from the back to the front (from the inner layer to the outer layer)
        for with in self.cte_stack.iter().rev() {
            for cte in &with.ctes {
                if cte.name.l() == table_name {
                    *self
                        .consumer_count
                        .entry(cte as *const CommonTableExpression as usize)
                        .or_insert(0) += 1;
                    return;
                }
            }
        }
    }
}

/// preprocessor is an ast.Visitor that preprocess
/// ast Nodes parsed from parser.
pub struct Preprocessor<'a> {
    vars: &'a mut SessionVars,
    flag: PreprocessorFlag,
    stmt_tp: u8,

    /// tableAliasInJoin is a stack that keeps the table alias names for joins.
    /// len(tableAliasInJoin) may bigger than 1 because the left/right child of join may be subquery that contains `JOIN`
    table_alias_in_join: Vec<HashSet<String>>,
    preprocess_with: PreprocessWith,

    vars_mutable: HashSet<String>,
    vars_readonly: HashSet<String>,

    // values that may be returned
    ret: Option<PreprocessorReturn>,
    err: Option<Error>,

    resolve_ctx: ContextRef,
}

/// A node the walker hands to `Enter` / `Leave` (Go: `ast.Node`).
enum PNode<'n> {
    Select(&'n SelectStmt),
    SetOpr(&'n SetOprStmt),
    SetOprList(&'n SetOprSelectList),
    Join(&'n Join),
    TableSource(&'n TableSource),
    TableName(&'n mut TableName),
    FuncCast(&'n FuncCastExpr),
    FuncCall(&'n ast::FuncCallExpr),
    ParamMarker,
    GroupBy(&'n GroupByClause),
    Cte(&'n CommonTableExpression),
    Subquery,
    Variable(&'n ast::VariableExpr),
    Other,
}

impl Preprocessor<'_> {
    // go: preprocessor.Enter L1
    fn enter(&mut self, input: PNode<'_>) -> bool {
        match input {
            PNode::Select(node) => {
                self.stmt_tp = TYPE_SELECT;
                if let Some(with) = &node.with {
                    self.preprocess_with.cte_stack.push(Arc::clone(with));
                }
                self.check_select_noop_funcs(node);
                // pushLockSelectCtx: the lock clause belongs to buildSelectLock (outside VS1-10).
            }
            PNode::SetOpr(node) => {
                if let Some(with) = &node.with {
                    self.preprocess_with.cte_stack.push(Arc::clone(with));
                }
            }
            PNode::SetOprList(node) => {
                if let Some(with) = &node.with {
                    self.preprocess_with.cte_stack.push(Arc::clone(with));
                }
                self.check_set_opr_select_list(node);
            }
            PNode::Join(node) => self.check_non_uniq_table_alias(node),
            PNode::FuncCast(node) => self.check_func_cast_expr(node),
            PNode::FuncCall(node) => {
                let l = node.fn_name.l();
                if l == ast_funcs::NextVal || l == ast_funcs::LastVal || l == ast_funcs::SetVal {
                    self.flag |= IN_SEQUENCE_FUNCTION;
                }
                // not support procedure right now.
                if !node.schema.l().is_empty() {
                    self.err = Some(ErrFunctionNotExists.gen_with_stack_by_args(vec![
                        Arg::from("FUNCTION"),
                        Arg::go_string(format!("{}.{}", node.schema.l(), l).into_bytes()),
                    ]));
                }
            }
            PNode::TableSource(node) => {
                let is_mode_oracle = SQLMode(self.vars.sql_mode) & ModeOracle != SQLMode(0);
                if matches!(node.source, ResultSetNode::Select(_))
                    && !is_mode_oracle
                    && node.as_name.l().is_empty()
                {
                    self.err = Some(ErrDerivedMustHaveAlias.gen_with_stack_by_args(vec![]));
                }
                if let ResultSetNode::TableName(v) = &node.source
                    && v.table_sample.is_some()
                {
                    // The AST's TableSample is not ported (D02.2): the sampling method is
                    // unknown, so the REGIONS check cannot run.
                    // glue: blocked-by:D02.2 (ast.TableSample)
                    self.err = Some(blocked("D02.2", "TABLESAMPLE (ast.TableSample)"));
                }
            }
            PNode::GroupBy(node) => self.check_group_by(node),
            PNode::Cte(node) => {
                let with = &mut self.preprocess_with;
                let before_offset = with.cte_can_used.len();
                with.cte_before_offset.push(before_offset);
                if node.is_recursive {
                    with.cte_can_used.push(node.name.l());
                }
            }
            PNode::Subquery => {
                let with = &mut self.preprocess_with;
                let before_offset = with.cte_can_used.len();
                with.cte_before_offset.push(before_offset);
            }
            PNode::Variable(node) => {
                // Use lowercase for consistency with how user variables are stored (session.go) and
                // looked up in convertReadonlyVarToConst (builtin_other.go), which uses the lowercased
                // name from the GetVar expression.
                let name_lower = tidb_base::gostd::to_lower(&node.name);
                if node.value.is_some() {
                    self.vars_readonly.remove(&name_lower);
                    self.vars_mutable.insert(name_lower);
                } else if self.stmt_tp == TYPE_SELECT {
                    // Only check the variable in select statement.
                    if !self.vars_mutable.contains(&name_lower) {
                        self.vars_readonly.insert(name_lower);
                    }
                }
            }
            PNode::TableName(_) | PNode::ParamMarker | PNode::Other => {
                self.flag &= !PARENT_IS_JOIN;
            }
        }
        self.err.is_some()
    }

    // go: preprocessor.Leave L1
    fn leave(&mut self, input: PNode<'_>) -> bool {
        match input {
            PNode::ParamMarker => {
                if self.flag & IN_PREPARE == 0 {
                    self.err =
                        Some(ErrSyntax.gen_with_stack("syntax error, unexpected '?'", vec![]));
                    return false;
                }
            }
            PNode::TableName(x) => {
                // Skip resolving TableName nodes that come from locking clauses: the lock clause
                // is buildSelectLock's (outside VS1-10), so none is skipped here.
                self.handle_table_name(x);
            }
            PNode::Join(_) => {
                self.table_alias_in_join.pop();
            }
            PNode::FuncCall(x) => {
                let l = x.fn_name.l();
                // The arguments for builtin NAME_CONST should be constants
                // See https://dev.mysql.com/doc/refman/5.7/en/miscellaneous-functions.html#function_name-const for details
                if l == ast_funcs::NameConst {
                    if x.args.len() != 2 {
                        self.err = Some(
                            ErrIncorrectParameterCount
                                .gen_with_stack_by_args(vec![Arg::go_string(l.into_bytes())]),
                        );
                    } else {
                        let is_value_expr1 = matches!(x.args[0], Expr::Value(_));
                        let is_value_expr2 = matches!(x.args[1], Expr::Value(_) | Expr::Unary(_));
                        if !is_value_expr1 || !is_value_expr2 {
                            self.err = Some(
                                ErrWrongArguments
                                    .gen_with_stack_by_args(vec![Arg::from("NAME_CONST")]),
                            );
                        }
                    }
                    return self.err.is_none();
                }
                // no need sleep when retry transaction and avoid unexpect sleep caused by retry:
                // Go rewrites the argument of SLEEP to 0 under inTxnRetry; the walker only
                // reads the AST (the rewrite comes with the session's retry, D05.3).
                if l == ast_funcs::NextVal || l == ast_funcs::LastVal || l == ast_funcs::SetVal {
                    self.flag &= !IN_SEQUENCE_FUNCTION;
                }
            }
            PNode::Cte(_) | PNode::Subquery => {
                let with = &mut self.preprocess_with;
                let len_with_cte_before_offset = with.cte_before_offset.len();
                if len_with_cte_before_offset < 1 {
                    self.err = Some(ErrInternal.gen_with_stack(
                        "len(cteBeforeOffset) is less than one.Maybe it was deleted in somewhere.Should match in Enter and Leave",
                        vec![],
                    ));
                    return self.err.is_none();
                }
                let before_offset = with.cte_before_offset[len_with_cte_before_offset - 1];
                with.cte_before_offset
                    .truncate(len_with_cte_before_offset - 1);
                with.cte_can_used.truncate(before_offset);
                if let PNode::Cte(cte_node) = input {
                    with.cte_can_used.push(cte_node.name.l());
                }
            }
            PNode::Select(x) if x.with.is_some() => {
                self.preprocess_with.cte_stack.pop();
            }
            PNode::SetOpr(x) if x.with.is_some() => {
                self.preprocess_with.cte_stack.pop();
            }
            PNode::SetOprList(x) if x.with.is_some() => {
                self.preprocess_with.cte_stack.pop();
            }
            _ => {}
        }
        self.err.is_none()
    }

    // go: preprocessor.checkSetOprSelectList L1
    fn check_set_opr_select_list(&mut self, stmt: &SetOprSelectList) {
        let n = stmt.selects.len();
        for sel in &stmt.selects[..n.saturating_sub(1)] {
            match sel {
                SetOprNode::Select(s) => {
                    if s.select_into_opt.is_some() {
                        self.err =
                            Some(ErrWrongUsage.gen_with_stack_by_args(vec![
                                Arg::from("UNION"),
                                Arg::from("INTO"),
                            ]));
                        return;
                    }
                    if s.is_in_braces {
                        continue;
                    }
                    if s.limit.is_some() {
                        self.err =
                            Some(ErrWrongUsage.gen_with_stack_by_args(vec![
                                Arg::from("UNION"),
                                Arg::from("LIMIT"),
                            ]));
                        return;
                    }
                    if s.order_by.is_some() {
                        self.err = Some(ErrWrongUsage.gen_with_stack_by_args(vec![
                            Arg::from("UNION"),
                            Arg::from("ORDER BY"),
                        ]));
                        return;
                    }
                }
                SetOprNode::List(s) => self.check_set_opr_select_list(s),
                SetOprNode::Unported(_) => {}
            }
        }
    }

    // go: preprocessor.checkNonUniqTableAlias L1
    fn check_non_uniq_table_alias(&mut self, stmt: &Join) {
        if self.flag & PARENT_IS_JOIN == 0 {
            self.table_alias_in_join.push(HashSet::new());
        }
        let is_oracle_mode = SQLMode(self.vars.sql_mode) & ModeOracle != SQLMode(0);
        let table_aliases = self.table_alias_in_join.last_mut().expect("pushed");
        if !is_oracle_mode {
            if let Err(err) = is_table_alias_duplicate(stmt.left.as_ref(), table_aliases) {
                self.err = Some(err);
                return;
            }
            if let Err(err) = is_table_alias_duplicate(stmt.right.as_ref(), table_aliases) {
                self.err = Some(err);
                return;
            }
        }
        self.flag |= PARENT_IS_JOIN;
    }

    // go: preprocessor.checkSelectNoopFuncs L1
    fn check_select_noop_funcs(&mut self, stmt: &SelectStmt) {
        let noop_funcs_mode = self.vars.noop_funcs_mode;
        let for_share = stmt.lock_info.as_ref().is_some_and(|l| {
            l.lock_type == SelectLockType::ForShare || l.lock_type == SelectLockType::ForShareNoWait
        });
        if noop_funcs_mode == OnInt {
            // Set `ForShareLockEnabledByNoop` properly before returning.
            // When `tidb_enable_shared_lock_promotion` is enabled, the `for share` statements would be
            // executed as `for update` statements despite setting of noop functions.
            if for_share && !self.vars.shared_lock_promotion {
                self.vars.stmt_ctx_mut().for_share_lock_enabled_by_noop = true;
            }
            return;
        }
        if stmt.select_stmt_opts.calc_found_rows {
            let err =
                ErrFunctionsNoopImpl.gen_with_stack_by_args(vec![Arg::from("SQL_CALC_FOUND_ROWS")]);
            if noop_funcs_mode == OffInt {
                self.err = Some(err);
                return;
            }
            // NoopFuncsMode is Warn, append an error
            self.vars.stmt_ctx().append_warning(err);
        }

        // When `tidb_enable_shared_lock_promotion` is enabled, the `for share` statements would be
        // executed as `for update` statements.
        if for_share && !self.vars.shared_lock_promotion {
            let err =
                ErrFunctionsNoopImpl.gen_with_stack_by_args(vec![Arg::from("LOCK IN SHARE MODE")]);
            if noop_funcs_mode == OffInt {
                self.err = Some(err);
                return;
            }
            // NoopFuncsMode is Warn, append an error
            self.vars.stmt_ctx().append_warning(err);
            self.vars.stmt_ctx_mut().for_share_lock_enabled_by_noop = true;
        }
    }

    // go: preprocessor.checkGroupBy L1
    fn check_group_by(&mut self, stmt: &GroupByClause) {
        let noop_funcs_mode = self.vars.noop_funcs_mode;
        for item in &stmt.items {
            if !item.null_order && noop_funcs_mode != OnInt {
                let err = ErrFunctionsNoopImpl
                    .gen_with_stack_by_args(vec![Arg::from("GROUP BY expr ASC|DESC")]);
                if noop_funcs_mode == OffInt {
                    self.err = Some(err);
                    return;
                }
                // NoopFuncsMode is Warn, append an error
                self.vars.stmt_ctx().append_warning(err);
            }
        }
    }

    // go: preprocessor.stmtType L1
    fn stmt_type(&self) -> &'static str {
        match self.stmt_tp {
            TYPE_DELETE => "DELETE",
            TYPE_UPDATE => "UPDATE",
            TYPE_INSERT => "INSERT",
            TYPE_DROP => "DROP",
            TYPE_CREATE => "CREATE",
            TYPE_ALTER => "ALTER",
            TYPE_RENAME => "DROP, ALTER",
            TYPE_REPAIR => "SELECT, INSERT",
            TYPE_SHOW => "SHOW",
            TYPE_IMPORT_INTO => "IMPORT INTO",
            _ => "SELECT", // matches Select and uncaught cases.
        }
    }

    // go: preprocessor.handleTableName L1
    fn handle_table_name(&mut self, tn: &mut TableName) {
        if tn.schema.l().is_empty() {
            for cte in &self.preprocess_with.cte_can_used {
                if *cte == tn.name.l() {
                    let name = tn.name.l();
                    self.preprocess_with.update_cte_consumer_count(&name);
                    return;
                }
            }

            let current_db = self.vars.current_db.clone();
            if current_db.is_empty() {
                self.err = Some(ErrNoDB.clone());
                return;
            }

            tn.schema = ast::CIStr::new(current_db);
        }

        if self.flag & IN_CREATE_OR_DROP_TABLE > 0 {
            // The table may not exist in create table or drop table statement (DDL, D09.0
            // outside VS1-10; the repair check needs domainutil.RepairInfo, D06.5).
            return;
        }
        // repairStmt: admin repair table A create table B ... (outside VS1-10).
        if self.flag & IN_REPAIR_TABLE > 0 {
            return;
        }

        if self.stmt_tp == TYPE_SELECT {
            // staleReadProcessor.OnSelectTable (sessiontxn/staleread, D05.4): a plain table
            // name without AS OF changes nothing when the session has no stale-read state,
            // which the router requires (VS1 plan §4.3 step 0).
            if tn.as_of.is_some() {
                self.err = Some(blocked("D05.4", "AS OF TIMESTAMP (staleread)"));
                return;
            }
            if let Err(e) = self.update_state_from_stale_read_processor() {
                self.err = Some(e);
                return;
            }
        }

        if let Err(err) = self.table_by_name(tn) {
            self.err = Some(err);
            return;
        }
        if !self.skip_lock_mdl()
            && let Err(err) = try_lock_mdl_and_update_schema_if_necessary()
        {
            self.err = Some(err);
            return;
        }
        // SchemaByTable, the sequence check (inSequenceFunction) and resolveCtx.AddTableName
        // follow once tableByName returns a table (D06.3, VS1-17).
        let _ = (&self.resolve_ctx, IN_SEQUENCE_FUNCTION);
    }

    // go: preprocessor.tableByName L1
    fn table_by_name(&mut self, tn: &TableName) -> Result<(), Error> {
        let mut current_db = self.vars.current_db.clone();
        if !tn.schema.o.is_empty() {
            current_db = tn.schema.l();
        }
        if current_db.is_empty() {
            return Err(ErrNoDB.clone());
        }
        self.ensure_info_schema()?;
        // is.TableByName(...) and the privilege check of a missing table need the InfoSchema
        // interface (D06.3 part 2, VS1-17) and the privilege manager (D04.1).
        // glue: blocked-by:D06.3 (VS1-17: InfoSchema.TableByName)
        let _ = self.stmt_type();
        Err(blocked("D06.3", "InfoSchema.TableByName"))
    }

    // go: preprocessor.checkFuncCastExpr L1
    fn check_func_cast_expr(&mut self, node: &FuncCastExpr) {
        let tp = &node.tp;
        let eval_type = tidb_types::field_type::new_field_type(tp.tp).eval_type();
        let (flen, decimal) = (tp.flen as isize, tp.decimal as isize);
        if eval_type == ETDecimal {
            if flen >= decimal
                && flen <= mysql::MaxDecimalWidth
                && decimal <= mysql::MaxDecimalScale
            {
                // valid
                return;
            }

            let mut buf = String::new();
            if let Err(err) = node.expr.restore(&mut buf) {
                self.err = Some(Error::plain(err.to_string()));
                return;
            }
            if flen < decimal {
                self.err = Some(
                    ErrMBiggerThanD.gen_with_stack_by_args(vec![Arg::go_string(buf.into_bytes())]),
                );
                return;
            }
            if flen > mysql::MaxDecimalWidth {
                self.err = Some(ErrTooBigPrecision.gen_with_stack_by_args(vec![
                    Arg::Int(flen as i64, "int"),
                    Arg::go_string(buf.into_bytes()),
                    Arg::Int(mysql::MaxDecimalWidth as i64, "int"),
                ]));
                return;
            }
            if decimal > mysql::MaxDecimalScale {
                self.err = Some(ErrTooBigScale.gen_with_stack_by_args(vec![
                    Arg::Int(decimal as i64, "int"),
                    Arg::go_string(buf.into_bytes()),
                    Arg::Int(mysql::MaxDecimalScale as i64, "int"),
                ]));
                return;
            }
        }
        if eval_type == ETDatetime && decimal > MaxFsp {
            self.err = Some(ErrTooBigPrecision.gen_with_stack_by_args(vec![
                Arg::Int(decimal as i64, "int"),
                Arg::from("CAST"),
                Arg::Int(MaxFsp as i64, "int"),
            ]));
        }
    }

    // go: preprocessor.ensureInfoSchema L1
    fn ensure_info_schema(&mut self) -> Result<(), Error> {
        if self.ret.as_ref().is_some_and(|r| r.has_info_schema) {
            return Ok(());
        }
        // sessiontxn.GetTxnManager(p.sctx).GetTxnInfoSchema() (D05.4, VS1-14).
        // glue: blocked-by:D05.4 (sessiontxn GetTxnInfoSchema)
        Err(blocked("D05.4", "GetTxnInfoSchema"))
    }

    /// The stale-read processor (`staleread.Processor`, D05.4) keeps no state for a session
    /// that the router sends to the new stack (no AS OF, no `tidb_read_staleness`,
    /// `tx_read_ts` or `tidb_snapshot`): `IsStaleness` stays false and nothing is
    /// evaluated. A statement that needs it never gets here (`handleTableName`).
    // go: preprocessor.updateStateFromStaleReadProcessor L1
    fn update_state_from_stale_read_processor(&mut self) -> Result<(), Error> {
        // glue: blocked-by:D05.4 (staleread.Processor; see the doc comment)
        Ok(())
    }

    // go: preprocessor.skipLockMDL L1
    fn skip_lock_mdl(&self) -> bool {
        // skip lock mdl for IMPORT INTO statement,
        // because it's a batch process and will do both DML and DDL.
        // skip lock mdl for ANALYZE statement.
        self.flag & IN_IMPORT_INTO > 0 || self.flag & IN_ANALYZE > 0
    }
}

// go: isTableAliasDuplicate L1
fn is_table_alias_duplicate(
    node: Option<&ResultSetNode>,
    table_aliases: &mut HashSet<String>,
) -> Result<(), Error> {
    if let Some(ResultSetNode::TableSource(ts)) = node {
        let mut tab_name_o = ts.as_name.o.clone();
        let mut tab_name_l = ts.as_name.l();
        if tab_name_l.is_empty()
            && let ResultSetNode::TableName(table_node) = &ts.source
        {
            if !table_node.schema.l().is_empty() {
                tab_name_o = format!("{}.{}", table_node.schema.l(), table_node.name.l());
                tab_name_l = tidb_base::gostd::to_lower(&tab_name_o);
            } else {
                tab_name_o = table_node.name.o.clone();
                tab_name_l = table_node.name.l();
            }
        }
        let exists = table_aliases.contains(&tab_name_l);
        if !tab_name_l.is_empty() && exists {
            return Err(ErrNonUniqTable
                .gen_with_stack_by_args(vec![Arg::go_string(tab_name_o.into_bytes())]));
        }
        table_aliases.insert(tab_name_l);
    }
    Ok(())
}

/// EraseLastSemicolon removes last semicolon of sql.
///
/// The Rust AST keeps a statement's text as a read-only span (no `SetText`, D02.2); the
/// callers are the binding statements (D13.2).
// glue: stub of EraseLastSemicolon, blocked-by:D02.2 (ast.Node.SetText)
pub fn erase_last_semicolon() -> Result<(), Error> {
    Err(blocked("D02.2", "EraseLastSemicolon (ast.Node.SetText)"))
}

/// EraseLastSemicolonInSQL removes last semicolon of the SQL.
// go: EraseLastSemicolonInSQL L1
pub fn erase_last_semicolon_in_sql(sql: &str) -> &str {
    if !sql.is_empty() && sql.as_bytes()[sql.len() - 1] == b';' {
        return &sql[..sql.len() - 1];
    }
    sql
}

/// TypeInvalid for unexpected types.
pub const TYPE_INVALID: u8 = 0;
/// TypeSelect for SelectStmt.
pub const TYPE_SELECT: u8 = 1;
/// TypeSetOpr for SetOprStmt.
pub const TYPE_SET_OPR: u8 = 2;
/// TypeDelete for DeleteStmt.
pub const TYPE_DELETE: u8 = 3;
/// TypeUpdate for UpdateStmt.
pub const TYPE_UPDATE: u8 = 4;
/// TypeInsert for InsertStmt.
pub const TYPE_INSERT: u8 = 5;
/// TypeDrop for DropStmt
pub const TYPE_DROP: u8 = 6;
/// TypeCreate for CreateStmt
pub const TYPE_CREATE: u8 = 7;
/// TypeAlter for AlterStmt
pub const TYPE_ALTER: u8 = 8;
/// TypeRename for RenameStmt
pub const TYPE_RENAME: u8 = 9;
/// TypeRepair for RepairStmt
pub const TYPE_REPAIR: u8 = 10;
/// TypeShow for ShowStmt
pub const TYPE_SHOW: u8 = 11;
/// TypeExecute for ExecuteStmt
pub const TYPE_EXECUTE: u8 = 12;
/// TypeImportInto for ImportIntoStmt
pub const TYPE_IMPORT_INTO: u8 = 13;

/// The DELETE / UPDATE / INSERT statements are not in the Rust AST yet (D02.2): they are
/// `Stmt::Unported` and answer `TypeInvalid` until they are.
// go: bindableStmtType L1
pub fn bindable_stmt_type(node: &Stmt) -> u8 {
    match node {
        Stmt::Select(_) => TYPE_SELECT,
        Stmt::SetOpr(_) => TYPE_SET_OPR,
        _ => TYPE_INVALID,
    }
}

/// `aliasChecker.Enter`: only DELETE statements are rewritten (`DeleteStmt` is not in the
/// Rust AST, D02.2).
// glue: stub of aliasChecker.Enter, blocked-by:D02.2 (ast.DeleteStmt)
pub fn alias_checker_enter() -> Result<(), Error> {
    Err(blocked("D02.2", "aliasChecker (ast.DeleteStmt)"))
}

// go: aliasChecker.Leave L1
pub fn alias_checker_leave() -> bool {
    true
}

// go: getTableRefsAlias L1
pub fn get_table_refs_alias(table_refs: &ResultSetNode) -> Option<&ast::CIStr> {
    match table_refs {
        ResultSetNode::Join(v) => {
            if let Some(left) = &v.left {
                return get_table_refs_alias(left);
            }
        }
        ResultSetNode::TableSource(v) => return Some(&v.as_name),
        _ => {}
    }
    None
}

/// `preprocessor.resolveExecuteStmt`: the prepared statement cache is D13.1 (VS1-15).
// glue: stub of preprocessor.resolveExecuteStmt, blocked-by:D13.1 (VS1-15: PlanCacheStmt)
pub fn resolve_execute_stmt() -> Result<(), Error> {
    Err(blocked("D13.1", "EXECUTE (PlanCacheStmt)"))
}

/// `preprocessor.checkNotInRepair`: `domainutil.RepairInfo` (D06.5).
// glue: stub of preprocessor.checkNotInRepair, blocked-by:D06.5 (domainutil.RepairInfo)
pub fn check_not_in_repair() -> Result<(), Error> {
    Err(blocked("D06.5", "repair mode (domainutil.RepairInfo)"))
}

/// `preprocessor.hasAutoConvertWarning` reads a column definition (`ast.ColumnDef`, DDL; not
/// in the Rust AST, D02.2).
// glue: stub of preprocessor.hasAutoConvertWarning, blocked-by:D02.2 (ast.ColumnDef)
pub fn has_auto_convert_warning() -> Result<(), Error> {
    Err(blocked("D02.2", "hasAutoConvertWarning (ast.ColumnDef)"))
}

/// `tryLockMDLAndUpdateSchemaIfNecessary`: the metadata lock of the table a statement reads
/// (`tidb_enable_metadata_lock`) needs the transaction context (D05.4) and `table.Table`
/// (D06.3 part 2).
// glue: stub of tryLockMDLAndUpdateSchemaIfNecessary, blocked-by:D05.4 (MDL: the txn context)
pub fn try_lock_mdl_and_update_schema_if_necessary() -> Result<(), Error> {
    Err(blocked(
        "D05.4",
        "metadata lock (tryLockMDLAndUpdateSchemaIfNecessary)",
    ))
}

/// Calls `Enter` / `Leave` of the preprocessor in the order of Go's `Accept` methods (see
/// the module doc). Every method returns Go's `ok`: false stops the walk.
struct Walker<'p, 'a>(&'p mut Preprocessor<'a>);

impl Walker<'_, '_> {
    /// `node.Accept(v)` for a statement; statements other than SELECT and set operations
    /// are visited as leaves (their `Enter` arms are outside VS1-10).
    // glue: AST walk (Go Accept)
    fn stmt(&mut self, s: &mut Stmt) -> bool {
        match s {
            Stmt::Select(sel) => self.select(sel),
            Stmt::SetOpr(set_opr) => self.set_opr(set_opr),
            _ => {
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                self.0.leave(PNode::Other)
            }
        }
    }

    /// `WithClause.Accept` (the clause itself is a plain node for the preprocessor).
    // glue: AST walk (Go Accept)
    fn with(&mut self, with: &mut Arc<WithClause>) -> bool {
        if self.0.enter(PNode::Other) {
            return self.0.leave(PNode::Other);
        }
        let Some(with) = Arc::get_mut(with) else {
            // A WITH clause shared with an enclosing set operation (CTE, D09.5).
            self.0.err = Some(blocked("D09.5", "shared WITH clause"));
            return false;
        };
        for cte in &mut with.ctes {
            if self.0.enter(PNode::Cte(cte)) {
                if !self.0.leave(PNode::Cte(cte)) {
                    return false;
                }
                continue;
            }
            if !self.expr(&mut cte.query) {
                return false;
            }
            if !self.0.leave(PNode::Cte(cte)) {
                return false;
            }
        }
        self.0.leave(PNode::Other)
    }

    /// A node the preprocessor does not look at (Go's `default` arm of `Enter`, which clears
    /// `parentIsJoin`) with `children` inside.
    // glue: AST walk (Go Accept)
    fn other(&mut self, children: impl FnOnce(&mut Self) -> bool) -> bool {
        if self.0.enter(PNode::Other) {
            return self.0.leave(PNode::Other);
        }
        if !children(self) {
            return false;
        }
        self.0.leave(PNode::Other)
    }

    /// `OrderByClause.Accept` / `GroupByClause` items: each `ByItem` node with its expression.
    // glue: AST walk (Go Accept)
    fn by_items(&mut self, items: &mut [ast::ByItem]) -> bool {
        for item in items {
            if !self.other(|w| w.expr(&mut item.expr)) {
                return false;
            }
        }
        true
    }

    /// `Limit.Accept`.
    // glue: AST walk (Go Accept)
    fn limit(&mut self, l: &mut ast::Limit) -> bool {
        self.other(|w| {
            if !w.expr(&mut l.count) {
                return false;
            }
            if let Some(off) = &mut l.offset {
                return w.expr(off);
            }
            true
        })
    }

    /// `SelectStmt.Accept`.
    // glue: AST walk (Go Accept)
    fn select(&mut self, n: &mut SelectStmt) -> bool {
        if self.0.enter(PNode::Select(n)) {
            return self.0.leave(PNode::Select(n));
        }
        if let Some(with) = &mut n.with
            && !self.with(with)
        {
            return false;
        }
        // TableHints: not ported (TableOptimizerHint, D13.3); leaves for the preprocessor.
        for _ in &n.table_hints {
            if !self.other(|_| true) {
                return false;
            }
        }
        let fields = &mut n.fields.fields;
        if !self.other(|w| {
            for f in fields.iter_mut() {
                if !w.other(|w| f.expr.as_mut().is_none_or(|e| w.expr(e))) {
                    return false;
                }
            }
            true
        }) {
            return false;
        }
        if let Some(from) = &mut n.from
            && !self.other(|w| w.join(&mut from.table_refs))
        {
            return false;
        }
        if let Some(w) = &mut n.where_
            && !self.expr(w)
        {
            return false;
        }
        if let Some(g) = &mut n.group_by {
            if !self.0.enter(PNode::GroupBy(g)) && !self.by_items(&mut g.items) {
                return false;
            }
            if !self.0.leave(PNode::GroupBy(g)) {
                return false;
            }
        }
        if let Some(h) = &mut n.having
            && !self.other(|w| w.expr(&mut h.expr))
        {
            return false;
        }
        for list in &mut n.lists {
            if !self.other(|w| list.values.iter_mut().all(|v| w.expr(v))) {
                return false;
            }
        }
        for _ in &n.window_specs {
            // WindowSpec: window functions are D09.4 (outside VS1); a leaf here.
            if !self.other(|_| true) {
                return false;
            }
        }
        if let Some(o) = &mut n.order_by
            && !self.other(|w| w.by_items(&mut o.items))
        {
            return false;
        }
        if let Some(l) = &mut n.limit
            && !self.limit(l)
        {
            return false;
        }
        if let Some(lock) = &mut n.lock_info {
            for t in &mut lock.tables {
                if !self.table_name(t) {
                    return false;
                }
            }
        }
        self.0.leave(PNode::Select(n))
    }

    /// `SetOprStmt.Accept`.
    // glue: AST walk (Go Accept)
    fn set_opr(&mut self, n: &mut SetOprStmt) -> bool {
        if self.0.enter(PNode::SetOpr(n)) {
            return self.0.leave(PNode::SetOpr(n));
        }
        if let Some(with) = &mut n.with
            && !self.with(with)
        {
            return false;
        }
        if !self.set_opr_list(&mut n.select_list) {
            return false;
        }
        if let Some(o) = &mut n.order_by
            && !self.other(|w| w.by_items(&mut o.items))
        {
            return false;
        }
        if let Some(l) = &mut n.limit
            && !self.limit(l)
        {
            return false;
        }
        self.0.leave(PNode::SetOpr(n))
    }

    /// `SetOprSelectList.Accept`.
    // glue: AST walk (Go Accept)
    fn set_opr_list(&mut self, n: &mut SetOprSelectList) -> bool {
        if self.0.enter(PNode::SetOprList(n)) {
            return self.0.leave(PNode::SetOprList(n));
        }
        if let Some(with) = &mut n.with
            && !self.with(with)
        {
            return false;
        }
        for sel in &mut n.selects {
            let ok = match sel {
                SetOprNode::Select(s) => self.select(s),
                SetOprNode::List(l) => self.set_opr_list(l),
                SetOprNode::Unported(_) => true,
            };
            if !ok {
                return false;
            }
        }
        if let Some(o) = &mut n.order_by
            && !self.other(|w| w.by_items(&mut o.items))
        {
            return false;
        }
        if let Some(l) = &mut n.limit
            && !self.limit(l)
        {
            return false;
        }
        self.0.leave(PNode::SetOprList(n))
    }

    /// `ResultSetNode.Accept`.
    // glue: AST walk (Go Accept)
    fn result_set(&mut self, n: &mut ResultSetNode) -> bool {
        match n {
            ResultSetNode::TableName(t) => self.table_name(t),
            ResultSetNode::TableSource(ts) => {
                if self.0.enter(PNode::TableSource(ts)) {
                    return self.0.leave(PNode::TableSource(ts));
                }
                if !self.result_set(&mut ts.source) {
                    return false;
                }
                self.0.leave(PNode::TableSource(ts))
            }
            ResultSetNode::Join(j) => self.join(j),
            ResultSetNode::Select(s) => self.select(s),
            ResultSetNode::SetOpr(s) => self.set_opr(s),
            ResultSetNode::Unported(_) => true,
        }
    }

    /// `Join.Accept`.
    // glue: AST walk (Go Accept)
    fn join(&mut self, n: &mut Join) -> bool {
        if self.0.enter(PNode::Join(n)) {
            return self.0.leave(PNode::Join(n));
        }
        if let Some(l) = &mut n.left
            && !self.result_set(l)
        {
            return false;
        }
        if let Some(r) = &mut n.right
            && !self.result_set(r)
        {
            return false;
        }
        if let Some(on) = &mut n.on
            && !self.other(|w| w.expr(&mut on.expr))
        {
            return false;
        }
        for _ in &n.using {
            if !self.other(|_| true) {
                return false;
            }
        }
        self.0.leave(PNode::Join(n))
    }

    /// `TableName.Accept`.
    // glue: AST walk (Go Accept)
    fn table_name(&mut self, n: &mut TableName) -> bool {
        if self.0.enter(PNode::TableName(n)) {
            return self.0.leave(PNode::TableName(n));
        }
        if let Some(as_of) = &mut n.as_of {
            let _ = self.expr(&mut as_of.ts_expr);
        }
        self.0.leave(PNode::TableName(n))
    }

    /// `ExprNode.Accept`: the children in the order of the Go nodes' fields.
    // glue: AST walk (Go Accept)
    fn expr(&mut self, e: &mut Expr) -> bool {
        macro_rules! leaf {
            ($node:expr) => {{
                if self.0.enter($node) {
                    return self.0.leave($node);
                }
                self.0.leave($node)
            }};
        }
        macro_rules! children {
            ($($child:expr),*) => {{
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                $(if !self.expr($child) { return false; })*
                self.0.leave(PNode::Other)
            }};
        }
        match e {
            Expr::ParamMarker(_) => leaf!(PNode::ParamMarker),
            Expr::FuncCast(c) => {
                if self.0.enter(PNode::FuncCast(c)) {
                    return self.0.leave(PNode::FuncCast(c));
                }
                if !self.expr(&mut c.expr) {
                    return false;
                }
                self.0.leave(PNode::FuncCast(c))
            }
            Expr::FuncCall(f) => {
                if self.0.enter(PNode::FuncCall(f)) {
                    return self.0.leave(PNode::FuncCall(f));
                }
                for a in &mut f.args {
                    if !self.expr(a) {
                        return false;
                    }
                }
                self.0.leave(PNode::FuncCall(f))
            }
            Expr::Variable(v) => {
                if self.0.enter(PNode::Variable(v)) {
                    return self.0.leave(PNode::Variable(v));
                }
                if let Some(val) = &mut v.value
                    && !self.expr(val)
                {
                    return false;
                }
                self.0.leave(PNode::Variable(v))
            }
            Expr::Subquery(s) => {
                if self.0.enter(PNode::Subquery) {
                    return self.0.leave(PNode::Subquery);
                }
                if !self.result_set(&mut s.query) {
                    return false;
                }
                self.0.leave(PNode::Subquery)
            }
            Expr::Binary(b) => children!(&mut b.l, &mut b.r),
            Expr::Unary(u) => children!(&mut u.v),
            Expr::Paren(p) => children!(p),
            Expr::IsNull(x) => children!(&mut x.expr),
            Expr::IsTruth(x) => children!(&mut x.expr),
            Expr::Between(x) => children!(&mut x.expr, &mut x.left, &mut x.right),
            Expr::Exists(x) => children!(&mut x.sel),
            Expr::CompareSubquery(x) => children!(&mut x.l, &mut x.r),
            Expr::PatternIn(x) => {
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                if !self.expr(&mut x.expr) {
                    return false;
                }
                for v in &mut x.list {
                    if !self.expr(v) {
                        return false;
                    }
                }
                if let Some(s) = &mut x.sel
                    && !self.expr(s)
                {
                    return false;
                }
                self.0.leave(PNode::Other)
            }
            Expr::Row(r) => {
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                for v in &mut r.values {
                    if !self.expr(v) {
                        return false;
                    }
                }
                self.0.leave(PNode::Other)
            }
            Expr::Aggregate(a) => {
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                for v in &mut a.args {
                    if !self.expr(v) {
                        return false;
                    }
                }
                self.0.leave(PNode::Other)
            }
            Expr::Case(c) => {
                if self.0.enter(PNode::Other) {
                    return self.0.leave(PNode::Other);
                }
                if let Some(v) = &mut c.value
                    && !self.expr(v)
                {
                    return false;
                }
                for w in &mut c.when_clauses {
                    if !self.expr(&mut w.expr) || !self.expr(&mut w.result) {
                        return false;
                    }
                }
                if let Some(v) = &mut c.else_clause
                    && !self.expr(v)
                {
                    return false;
                }
                self.0.leave(PNode::Other)
            }
            _ => leaf!(PNode::Other),
        }
    }
}
