//! go-file: pkg/session/session.go
//!
//! `execStmtResult`: the record set `ExecuteStmt` returns (spec `specs/D05.3-session/spec.md`
//! §2.3).
//!
//! The statement (`ExecStmt`) stays in the session ([`RunningStmt`]) while its rows are read;
//! the caller holds only the record set, and every method takes the session. If the result is
//! dropped before `close` (the statement's future cancelled, KILL, a disconnect), the session
//! finishes the statement before the next one (`Session::finish_unfinished_stmt`).

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_executor::adapter::{ExecStmt, RecordSet};
use tidb_executor::glue::ResultField;
use tidb_executor::internal::exec::executor::Executor;

use crate::session::Session;
use crate::tidb::finish_stmt;

/// The statement of an open result (the `sql` and `once` of Go's `execStmtResult`, and the
/// errors the record set met, for a finish after a drop).
pub(crate) struct RunningStmt {
    pub(crate) stmt: ExecStmt,
    pub(crate) txn_start_ts: u64,
    /// Errors returned by `Next` (the record set's `lastErrs`, kept here too).
    pub(crate) last_errs: Vec<Error>,
    /// `once` of `Finish` ran.
    pub(crate) finished: bool,
    /// An `execStmtResult` (its `Finish` runs `finishStmt`); `false`: the bare `recordSet` of
    /// the point get short path.
    pub(crate) wrapped: bool,
}

impl RunningStmt {
    // glue: constructor
    pub(crate) fn new(stmt: ExecStmt, txn_start_ts: u64, wrapped: bool) -> RunningStmt {
        RunningStmt {
            stmt,
            txn_start_ts,
            last_errs: Vec::new(),
            finished: false,
            wrapped,
        }
    }
}

/// `errors.Join(errs...)`: `None` for none, the messages joined by newlines otherwise.
// go-std: errors.Join L1
pub(crate) fn join_errors(errs: &[Error]) -> Option<Error> {
    match errs {
        [] => None,
        [e] => Some(e.clone()),
        _ => {
            let msg: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
            Some(Error::plain(msg.join("\n")))
        }
    }
}

/// The record set of a statement with rows (Go `execStmtResult`, or the bare `recordSet` of
/// the point get short path).
pub struct ExecStmtResult {
    rs: RecordSet,
    closed: bool,
}

/// The session's running statement; the result was made by this session.
// glue: accessor
fn running(se: &mut Session) -> Result<&mut RunningStmt, Error> {
    se.running
        .as_mut()
        .ok_or_else(|| Error::plain("the record set is closed"))
}

impl ExecStmtResult {
    // glue: constructor
    pub(crate) fn new(rs: RecordSet) -> ExecStmtResult {
        ExecStmtResult { rs, closed: false }
    }

    /// `Fields()`.
    // glue: delegation to recordSet.Fields (embedded RecordSet)
    pub fn fields(&mut self, se: &Session) -> &[ResultField] {
        let run = se.running.as_ref().expect("an open result");
        self.rs.fields(&run.stmt, &se.core)
    }

    /// `Next(ctx, chk)`. The error is also kept for a finish after a drop.
    // glue: delegation to recordSet.Next (embedded RecordSet)
    pub async fn next(&mut self, se: &mut Session, req: &mut Chunk) -> Result<(), Error> {
        let Some(run) = se.running.as_mut() else {
            return Err(Error::plain("the record set is closed"));
        };
        let r = self.rs.next(&mut run.stmt, &mut se.core, req).await;
        if let Err(e) = &r
            && let Some(run) = se.running.as_mut()
        {
            run.last_errs.push(e.clone());
        }
        r
    }

    /// `NewChunk(nil)`.
    // glue: delegation to recordSet.NewChunk (embedded RecordSet)
    pub fn new_chunk(&self) -> Chunk {
        self.rs.new_chunk(None)
    }

    // go: execStmtResult.Finish L1
    pub async fn finish(&mut self, se: &mut Session) -> Result<(), Error> {
        let run = running(se)?;
        if !run.wrapped {
            // The bare recordSet of the point get short path: recordSet.Finish.
            return self.rs.finish(&mut se.core).await;
        }
        if run.finished {
            return Ok(());
        }
        run.finished = true;
        let err1 = self.rs.finish(&mut se.core).await;
        // finishStmt(ctx, rs.se, err, rs.sql): `err` is always nil there.
        let Session { core, running, .. } = se;
        let Some(run) = running.as_ref() else {
            return err1;
        };
        let err2 = finish_stmt(core, None, &run.stmt).await;
        err1?;
        err2
    }

    /// Close: `Finish`, then the record set's `Close` (`ExecStmt.CloseRecordSet`); the
    /// statement leaves the session.
    // go: execStmtResult.Close L2 (consumes the result; the statement leaves the session)
    pub async fn close(mut self, se: &mut Session) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        let err1 = self.finish(se).await;
        let err2 = match se.running.as_mut() {
            Some(run) => self.rs.close(&mut run.stmt, &mut se.core).await,
            None => Ok(()),
        };
        se.running = None;
        self.closed = true;
        err1?;
        err2
    }

    /// TryDetach: a cursor's record set (`COM_STMT_FETCH`, D12.12). The VS1 router keeps
    /// cursors on the PoC (plan §4.3 step 0).
    // go: execStmtResult.TryDetach L2 (the cursor tracker is D05.2 P1-4, the static record set D12.12)
    pub fn try_detach(&self, se: &Session) -> Result<Option<ExecStmtResult>, Error> {
        let vars = se.vars();
        // intest.Assert(ServerStatusCursorExists)
        let run = se.running.as_ref().expect("an open result");
        if !run.stmt.is_read_only()? {
            return Ok(None);
        }
        if !tidb_planner::core::common_plans::is_auto_commit_txn(vars) {
            return Ok(None);
        }
        match self.rs.try_detach()? {
            None => Ok(None),
            // cursorTracker.NewCursor + WrapRecordSetWithCursor + finishStmt: the record
            // set's TryDetach already returns blocked-by:D12.12.
            Some(_) => Err(tidb_base::blocked::blocked(
                "D12.12",
                "detached record sets (cursor fetch)",
            )),
        }
    }

    /// GetExecutor4Test exports the internal executor for test purpose.
    // go: execStmtResult.GetExecutor4Test L1
    pub fn get_executor4_test(&self) -> Option<&dyn Executor> {
        self.rs.get_executor4_test()
    }
}
