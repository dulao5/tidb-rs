//! go-file: pkg/executor/select.go
//!
//! `ResetContextOfStmt` and `isWeakConsistencyRead` (block D12.2), ported ahead of VS1-35
//! by VS1-14: the session calls them before `Compile` (spec `specs/D05.3-session/spec.md`
//! §2.4). The executors of the file come with VS1-35.
//!
//! Not ported here, each the business of another block (spec §2.4): the session memory and
//! disk trackers and the OOM action (`SessionVars.MemTracker`, A1), the ANALYZE tracker
//! (ANALYZE is delegated to Go TiDB), `EXECUTE` (`GetPreparedStmt`, D13.1), Top SQL and pprof
//! labels (D14.2), the index usage collector (D11.2), the CTE / union-scan maps (their
//! executors are not ported: nothing to clear), the plan cache switches (D13.1) and the
//! planner context reset (N2: a `PlanCtx` is made per optimization). Statements outside the
//! Rust AST (`Stmt::Unported`: INSERT, UPDATE, DELETE, EXPLAIN, ...) are a boundary stub
//! (D02.2): the new stack only runs SELECT (VS1 router).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql::{NoPriority, PriorityEnum, SQLMode, ServerStatusCursorExists};
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_memory::tracker::LabelForSQLText;
use tidb_parser::ast;
use tidb_sessionctx::errctx;
use tidb_sessionctx::stmtctx::{StatementContext, allocate_task_id};
use tidb_sessionctx::variable::session::SessionVars;

use crate::glue::SessionContext;

/// ResetContextOfStmt resets the StmtContext and session variables.
/// Before every execution, we must clear statement context.
///
/// `text` is `s.Text()` (the parser AST does not keep the text). Go recovers a panic here;
/// panics are caught at the task boundary (M-PANIC). The old statement context is not
/// written (`SetVarHintRestore = nil` on it): it may be shared with the statement
/// checkpoint of VS1 plan §4.6 (N2), and the new context starts without restores anyway.
// go: ResetContextOfStmt L2 (see the module doc for the parts of other blocks; the old context is not written)
pub fn reset_context_of_stmt(
    ctx: &mut dyn SessionContext,
    s: &ast::Stmt,
    text: &str,
) -> Result<(), Error> {
    let killer = ctx.sql_killer();
    let vars = ctx.get_session_vars_mut();
    let restore: Vec<(String, String)> = vars
        .stmt_ctx()
        .set_var_hint_restore
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (name, val) in restore {
        if vars.set_system_var(&name, &val).is_err() {
            log::warn!(
                "Failed to restore the variable after SET_VAR hint, variable name={name}, expected value={val}"
            );
        }
    }
    let mut sc_arc: Arc<StatementContext> =
        if vars.state.txn_ctx.could_retry || vars.has_status_flag(ServerStatusCursorExists) {
            // Must construct new statement context object, the retry history need context for
            // every statement.
            // TODO: Maybe one day we can get rid of transaction retry, then this logic can be
            // deleted.
            Arc::new(tidb_sessionctx::stmtctx::new_stmt_ctx())
        } else {
            vars.init_statement_context()
        };
    let sc = Arc::get_mut(&mut sc_arc).expect("a new statement context");
    sc.set_time_zone(vars.location());
    sc.task_id = allocate_task_id();
    // CTEStorageMap / TblInfo2UnionScan: not ported (their executors), nothing to clear.
    sc.lock_table_ids.clear();
    sc.table_stats.clear();
    sc.related_table_ids.clear();
    sc.is_staleness = false;
    sc.enable_optimize_trace = false;
    // sc.OptimizeTracer = nil; sc.OptimizerCETrace = nil: the optimizer traces (D09.13).
    sc.is_sync_stats_failed = false;
    sc.is_explain_analyze_dml = false;
    sc.resource_group_name = vars.resource_group_name.clone();
    // Firstly we assume that UseDynamicPruneMode can be enabled according session variable,
    // then we will check other conditions in PlanBuilder.buildDataSource
    sc.use_dynamic_prune_mode = vars.is_dynamic_partition_prune_enabled();

    sc.stats_load_timeout = std::time::Duration::ZERO;
    // sc.StatsLoad.NeededItems / ResultCh: the sync stats load (D11.2).

    sc.sysdate_is_now = vars.sysdate_is_now;

    // vars.MemTracker / vars.DiskTracker: the session trackers are A1's (not in SessionVars).
    killer.reset();
    killer.conn_id.store(vars.connection_id, Ordering::SeqCst);

    // isAnalyze: ANALYZE and EXECUTE are not in the Rust AST (delegated / D13.1).
    sc.init_mem_tracker(LabelForSQLText, -1);
    // The OOM action (variable.OOMAction) and sc.MemTracker.AttachTo(vars.MemTracker): A1.
    sc.init_disk_tracker(LabelForSQLText, -1);
    // EnableTmpStorageOnOOM: the disk tracker is attached to the session's (A1).

    // EXECUTE (GetPreparedStmt, the SQL digest of the prepared statement): D13.1.
    // execute missed stmtID uses empty sql
    sc.original_sql = text.to_owned();
    // EXPLAIN / EXPLAIN FOR are not in the Rust AST (D02.2).
    sc.explain_format = String::new();

    // TODO: Many same bool variables here.
    // We should set only two variables (
    // IgnoreErr and StrictSQLMode) to avoid setting the same bool variables and
    // pushing them down to TiKV as flags.

    sc.in_restricted_sql = vars.in_restricted_sql;
    let sql_mode = SQLMode(vars.sql_mode);

    let mut err_levels = sc.err_levels();
    err_levels[errctx::ErrGroupDividedByZero.0 as usize] = errctx::LevelWarn;
    match s {
        ast::Stmt::Select(stmt) => {
            sc.in_select_stmt = true;

            // Return warning for truncate error in selection.
            sc.set_type_flags(
                sc.type_flags()
                    .with_truncate_as_warning(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
            let opts = &stmt.select_stmt_opts;
            sc.priority = PriorityEnum(opts.priority as isize);
            sc.not_fill_cache = !opts.sql_cache;
            sc.weak_consistency = is_weak_consistency_read(vars, s)?;
        }
        ast::Stmt::SetOpr(_) => {
            sc.in_select_stmt = true;
            sc.set_type_flags(
                sc.type_flags()
                    .with_truncate_as_warning(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
        }
        ast::Stmt::Show(stmt) => {
            sc.set_type_flags(
                sc.type_flags()
                    .with_ignore_truncate_err(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
            if matches!(
                stmt.tp,
                ast::ShowStmtType::Warnings
                    | ast::ShowStmtType::Errors
                    | ast::ShowStmtType::SessionStates
            ) {
                sc.in_show_warning = true;
                sc.set_warnings(vars.stmt_ctx().get_warnings());
            }
        }
        // UPDATE, DELETE, INSERT, CREATE / ALTER TABLE, LOAD DATA, IMPORT INTO, SPLIT
        // REGION, SET SESSION_STATES and the other statements Go distinguishes here are not
        // in the Rust AST.
        // glue: stub, blocked-by:D02.2 (the statements outside the Rust AST)
        ast::Stmt::Unported(_) => {
            return Err(blocked(
                "D02.2",
                "ResetContextOfStmt of a statement outside the Rust AST",
            ));
        }
        _ => {
            sc.set_type_flags(
                sc.type_flags()
                    .with_ignore_truncate_err(true)
                    .with_ignore_zero_in_date(true)
                    .with_ignore_invalid_date_err(sql_mode.has_allow_invalid_dates_mode()),
            );
        }
    }

    if err_levels != sc.err_levels() {
        sc.set_err_levels(err_levels);
    }

    let global_config = tidb_base::config::get_global_config();
    // WithAllowNegativeToUnsigned with false value indicates values less than 0 should be
    // clipped to 0 for unsigned integer types. This is the case for `insert`, `update`,
    // `alter table`, `create table` and `load data infile` statements, when not in strict
    // SQL mode. see https://dev.mysql.com/doc/refman/5.7/en/out-of-range-and-overflow.html
    // (inImportInto: IMPORT INTO is not in the Rust AST, false.)
    let allow_negative = !sc.in_insert_stmt
        && !sc.in_load_data_stmt
        && !sc.in_update_stmt
        && !sc.in_create_or_alter_stmt;
    sc.set_type_flags(
        sc.type_flags()
            .with_skip_utf8_check(vars.skip_utf8_check)
            .with_skip_sacii_check(vars.skip_ascii_check)
            .with_skip_utf8mb4_check(!global_config.instance.check_mb4_value_in_utf8.load())
            .with_allow_negative_to_unsigned(allow_negative),
    );

    vars.state.plan_cache_params.reset();
    let priority = PriorityEnum(
        tidb_sessionctx::variable::tidb_vars::ForcePriority.load(Ordering::SeqCst) as isize,
    );
    if priority != NoPriority {
        sc.priority = priority;
    }
    let old = vars.stmt_ctx();
    if old.last_insert_id() > 0 {
        sc.prev_last_insert_id = old.last_insert_id();
    } else {
        sc.prev_last_insert_id = old.prev_last_insert_id;
    }
    sc.prev_affected_rows = 0;
    if old.in_update_stmt
        || old.in_delete_stmt
        || old.in_insert_stmt
        || old.in_set_session_states_stmt
    {
        sc.prev_affected_rows = old.affected_rows() as i64;
    } else if old.in_select_stmt {
        sc.prev_affected_rows = -1;
    }
    if global_config.instance.enable_collect_execution_info.load() {
        // In ExplainFor case, RuntimeStatsColl should not be reset for reuse,
        // because ExplainFor need to display the last statement information.
        // (The old collector is not reused: it may be shared with the checkpoint, M-POOL.)
        sc.runtime_stats_coll = Some(RuntimeStatsColl::new(None));
        // also enable index usage collector: D11.2.
    } else {
        // turn off the index usage collector: D11.2.
    }

    // sc.SetForcePlanCache(fixcontrol Fix49736) / sc.SetAlwaysWarnSkipCache: D13.1.
    let (err_count, warn_count) = old.num_error_warnings();
    vars.sys_error_count = err_count;
    vars.sys_warning_count = warn_count;
    vars.exchange_chunk_status();
    vars.set_stmt_ctx(sc_arc);
    vars.prev_found_in_plan_cache = vars.found_in_plan_cache;
    vars.found_in_plan_cache = false;
    vars.prev_found_in_binding = vars.found_in_binding;
    vars.found_in_binding = false;
    vars.duration_wait_ts = 0;
    // vars.CurrInsertBatchExtraCols / CurrInsertValues: the write path.
    // ctx.GetPlanCtx().Reset(): N2, a PlanCtx is made per optimization.

    Ok(())
}

/// `plannercore.IsReadOnly(node, vars)` is a boundary stub (`blocked-by:D02.2`), reached only
/// with `tidb_read_consistency = weak` (Go's `&&` order is kept); its error is returned
/// (M-ERRVAL).
// go: isWeakConsistencyRead L1 [M-ERRVAL]
fn is_weak_consistency_read(session_vars: &SessionVars, node: &ast::Stmt) -> Result<bool, Error> {
    Ok(session_vars.connection_id > 0
        && session_vars.read_consistency == "weak"
        && tidb_planner::core::common_plans::is_auto_commit_txn(session_vars)
        && tidb_planner::optimize::is_read_only(node)?)
}
