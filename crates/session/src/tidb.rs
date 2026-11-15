//! go-file: pkg/session/tidb.go
//!
//! The end of a statement: `finishStmt` (statement commit / rollback, the autocommit
//! commit), `checkStmtLimit`, `GetHistory` (VS1-14 part 1).
//!
//! `Parse` is not ported: the session takes AST (ADR 0010 §4.2, spec §1.3); the test kit
//! helpers `GetRows4Test` / `ResultSetToStringSlice` neither.

use tidb_base::Error;
use tidb_executor::adapter::ExecStmt;
use tidb_parser::ast;
use tidb_sessionctx::variable::session::SessionVars;

use crate::session::SessionCore;

/// StmtHistory holds all histories of statements in a txn.
///
/// The history only records the statements of a retryable (optimistic, writing) transaction
/// (`TxnCtx.CouldRetry`), never set on the read-only node: it is always empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StmtHistory {
    history: Vec<String>,
}

impl StmtHistory {
    /// Count returns the count of the history.
    // glue: Go method StmtHistory.Count (one line)
    pub fn count(&self) -> usize {
        self.history.len()
    }
}

/// GetHistory get all stmtHistory in current txn. Exported only for test.
// go: GetHistory L2 (the history of retryable writes: always empty, see StmtHistory)
pub fn get_history(ctx: &SessionCore) -> StmtHistory {
    let _ = ctx;
    StmtHistory::default()
}

/// `IsReadOnly` of the statement: Go's `planner.IsReadOnly` is a boundary stub
/// (`blocked-by:D02.2`). Its error becomes the statement's error (`meetsErr`), so an
/// autocommit transaction is rolled back and the session ends idle.
// glue: ExecStmt.IsReadOnly (blocked-by:D09.13 / D02.2) as finishStmt's error
fn is_read_only(sql: &ExecStmt, meets_err: &mut Option<Error>) -> bool {
    match sql.is_read_only() {
        Ok(read_only) => read_only,
        Err(e) => {
            meets_err.get_or_insert(e);
            true
        }
    }
}

/// finishStmt: the end of a statement.
// go: finishStmt L2 (no failpoint; the statement buffer and the history of retryable writes are the write path)
pub async fn finish_stmt(
    se: &mut SessionCore,
    mut meets_err: Option<Error>,
    sql: &ExecStmt,
) -> Result<(), Error> {
    let read_only = is_read_only(sql, &mut meets_err);
    if !read_only {
        // shouldCheckConnectionAliveBeforeCommit, the history (TxnCtx.CouldRetry), and the
        // statement commit / rollback of the statement buffer (StmtCommit / StmtRollback):
        // the write path. A read-only transaction has nothing in its buffer.
    }
    let err = auto_commit_after_stmt(se, meets_err, sql).await;
    if se.sctx.txn.pending() {
        // After run statement finish, txn state is still pending means the
        // statement never need a Txn(), such as:
        //
        // set @@tidb_general_log = 1
        // set @@autocommit = 0
        // select 1
        //
        // Reset txn state to invalid to dispose the pending start ts.
        se.sctx.txn.change_to_invalid();
    }
    err?;
    check_stmt_limit(se, true).await
}

/// autoCommitAfterStmt: commit (or roll back) the autocommit transaction.
///
/// The deadlock rollback is for pessimistic locks (the write path).
// go: autoCommitAfterStmt L2 (the pessimistic deadlock rollback is the write path; PrevStmt is D14.2)
pub async fn auto_commit_after_stmt(
    se: &mut SessionCore,
    meets_err: Option<Error>,
    sql: &ExecStmt,
) -> Result<(), Error> {
    let is_internal = se
        .sctx
        .lazy_txn()
        .get_option(tidb_kv::option::RequestSourceInternal)
        .and_then(|v| v.downcast_ref::<bool>().copied())
        .unwrap_or(false);
    if let Some(err) = meets_err {
        if !se.sctx.vars.in_txn() {
            log::info!("rollbackTxn called due to ddl/autocommit failure");
            se.rollback_txn();
            record_abort_txn_duration(&se.sctx.vars, is_internal);
        }
        // else if txn.Valid() && txn.IsPessimistic() && ErrDeadlock: pessimistic locks.
        return Err(err);
    }

    if !se.sctx.vars.in_txn() {
        if let Err(err) = se.commit_txn().await {
            if let Some(ast::Stmt::Commit(_)) = sql.stmt_node.as_deref() {
                // errors.Annotatef(err, "previous statement: %s", PrevStmt): PrevStmt is
                // D14.2's (SessionVars.PrevStmt); M-ERR has no annotations.
            }
            return Err(err);
        }
        return Ok(());
    }
    Ok(())
}

/// checkStmtLimit: the number of statements in a transaction.
///
/// `BatchCommit` (`tidb_batch_commit`, the write path) is false.
// go: checkStmtLimit L2 (BatchCommit is the write path)
pub async fn check_stmt_limit(se: &mut SessionCore, is_finish: bool) -> Result<(), Error> {
    // If the user insert, insert, insert ... but never commit, TiDB would OOM.
    // So we limit the statement count in a transaction here.
    let history = get_history(se);
    let mut stmt_count = history.count();
    if !is_finish {
        // history stmt count + current stmt, since current stmt is not finish, it has not add
        // to history.
        stmt_count += 1;
    }
    let limit = tidb_base::config::get_global_config()
        .performance
        .stmt_count_limit;
    if stmt_count > limit {
        // !sessVars.BatchCommit
        se.rollback_txn();
        return Err(tidb_base::errors::errorf(
            "statement count %d exceeds the transaction limitation, transaction has been rollback, autocommit = %t",
            &[
                tidb_base::gofmt::Arg::from(stmt_count as i64),
                tidb_base::gofmt::Arg::from(se.sctx.vars.is_autocommit()),
            ],
        ));
    }
    Ok(())
}

/// recordAbortTxnDuration: the transaction duration histograms are D14.1's.
// go: recordAbortTxnDuration L2 (the histograms are D14.1)
fn record_abort_txn_duration(sess_vars: &SessionVars, is_internal: bool) {
    // duration := time.Since(sessVars.TxnCtx.CreateTime): CreateTime is not kept (D05.4
    // spec §4); TransactionDuration{Pessimistic,Optimistic}Abort{Internal,General}: D14.1.
    let _ = (sess_vars.state.txn_ctx.is_pessimistic, is_internal);
}
