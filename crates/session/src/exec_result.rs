//! go-file: pkg/session/session.go
//!
//! `execStmtResult`: the record set `ExecuteStmt` returns (spec `specs/D05.3-session/spec.md`
//! §2.3).
//!
//! The statement (`ExecStmt`) stays in the session ([`RunningStmt`]) while its rows are read;
//! every method of the result takes the session. The record set itself is in a slot shared by
//! the result and the session:
//!
//! - a result kept but never closed: the next statement finishes it first
//!   (`Session::finish_unfinished_stmt`) and takes the record set from the slot, so the
//!   executors are closed and the statement context is released as with Go's `Close`;
//! - a result dropped (or a method's future cancelled while the record set was taken out):
//!   the record set is gone with it; the session does the session side of `Close` only
//!   (`finishStmt`, `CloseRecordSet`); the executors' `Close` does not run (spec §4).
//!
//! Each result carries its statement's sequence number: once the session finished that
//! statement, `next` / `fields` fail and `finish` / `close` do nothing, so an old result never
//! acts on a newer statement (Lucky, #235).

use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_executor::adapter::{ExecStmt, RecordSet};
use tidb_executor::glue::ResultField;
use tidb_executor::internal::exec::executor::Executor;

use crate::session::Session;
use crate::tidb::finish_stmt;

/// The slot of a statement's record set, shared by its result and the session. The lock is
/// only held to take or put the record set (never across `await`).
pub(crate) type RecordSetSlot = Arc<Mutex<Option<RecordSet>>>;

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
    /// The statement's sequence number in the session (its result carries the same).
    pub(crate) seq: u64,
    /// The record set, while the result holds it.
    pub(crate) rs: RecordSetSlot,
}

impl RunningStmt {
    // glue: constructor
    pub(crate) fn new(
        stmt: ExecStmt,
        txn_start_ts: u64,
        wrapped: bool,
        seq: u64,
        rs: RecordSetSlot,
    ) -> RunningStmt {
        RunningStmt {
            stmt,
            txn_start_ts,
            last_errs: Vec::new(),
            finished: false,
            wrapped,
            seq,
            rs,
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

/// The error of a result whose statement the session already finished.
// glue: spec §2.3
fn closed_error() -> Error {
    Error::plain("the record set is closed")
}

/// The record set of a statement with rows (Go `execStmtResult`, or the bare `recordSet` of
/// the point get short path).
pub struct ExecStmtResult {
    slot: RecordSetSlot,
    closed: bool,
    seq: u64,
}

impl ExecStmtResult {
    // glue: constructor
    pub(crate) fn new(slot: RecordSetSlot, seq: u64) -> ExecStmtResult {
        ExecStmtResult {
            slot,
            closed: false,
            seq,
        }
    }

    /// Whether the session's running statement is this result's.
    // glue: spec §2.3
    fn is_current(&self, se: &Session) -> bool {
        se.running.as_ref().is_some_and(|r| r.seq == self.seq)
    }

    /// Takes the record set out of the slot (put back with [`Self::put`]).
    // glue: spec §2.3
    fn take(&self) -> Option<RecordSet> {
        self.slot.lock().take()
    }

    // glue: spec §2.3
    fn put(&self, rs: RecordSet) {
        *self.slot.lock() = Some(rs);
    }

    /// `Fields()` (copied: the record set is behind the slot); an error once the statement
    /// is finished.
    // glue: delegation to recordSet.Fields (embedded RecordSet)
    pub fn fields(&mut self, se: &Session) -> Result<Vec<ResultField>, Error> {
        let run = se
            .running
            .as_ref()
            .filter(|r| r.seq == self.seq)
            .ok_or_else(closed_error)?;
        let mut slot = self.slot.lock();
        let rs = slot.as_mut().ok_or_else(closed_error)?;
        Ok(rs.fields(&run.stmt, &se.core).to_vec())
    }

    /// `Next(ctx, chk)`. The error is also kept for a finish after a drop.
    // glue: delegation to recordSet.Next (embedded RecordSet)
    pub async fn next(&mut self, se: &mut Session, req: &mut Chunk) -> Result<(), Error> {
        if !self.is_current(se) {
            return Err(closed_error());
        }
        let Some(mut rs) = self.take() else {
            return Err(closed_error());
        };
        let Session { core, running, .. } = se;
        let run = running.as_mut().expect("checked above");
        let r = rs.next(&mut run.stmt, core, req).await;
        self.put(rs);
        if let Err(e) = &r {
            run.last_errs.push(e.clone());
        }
        r
    }

    /// `NewChunk(nil)`.
    // glue: delegation to recordSet.NewChunk (embedded RecordSet)
    pub fn new_chunk(&self) -> Result<Chunk, Error> {
        let slot = self.slot.lock();
        let rs = slot.as_ref().ok_or_else(closed_error)?;
        Ok(rs.new_chunk(None))
    }

    // go: execStmtResult.Finish L1
    pub async fn finish(&mut self, se: &mut Session) -> Result<(), Error> {
        if !self.is_current(se) {
            // The session finished the statement already.
            return Ok(());
        }
        let Some(mut rs) = self.take() else {
            return Ok(());
        };
        let r = finish_running(se, &mut rs).await;
        self.put(rs);
        r
    }

    /// Close: `Finish`, then the record set's `Close` (`ExecStmt.CloseRecordSet`); the
    /// statement leaves the session.
    // go: execStmtResult.Close L2 (consumes the result; the statement leaves the session)
    pub async fn close(mut self, se: &mut Session) -> Result<(), Error> {
        if self.closed || !self.is_current(se) {
            // Closed, or the session finished the statement already (and maybe runs another
            // one now): nothing to do.
            return Ok(());
        }
        let Some(mut rs) = self.take() else {
            return Ok(());
        };
        self.closed = true;
        close_running(se, &mut rs).await
    }

    /// TryDetach: a cursor's record set (`COM_STMT_FETCH`, D12.12). The VS1 router keeps
    /// cursors on the PoC (plan §4.3 step 0).
    // go: execStmtResult.TryDetach L2 (the cursor tracker is D05.2 P1-4, the static record set D12.12)
    pub fn try_detach(&self, se: &Session) -> Result<Option<ExecStmtResult>, Error> {
        let vars = se.vars();
        // intest.Assert(ServerStatusCursorExists)
        let run = se
            .running
            .as_ref()
            .filter(|r| r.seq == self.seq)
            .ok_or_else(closed_error)?;
        if !run.stmt.is_read_only(vars)? {
            return Ok(None);
        }
        if !tidb_planner::core::common_plans::is_auto_commit_txn(vars) {
            return Ok(None);
        }
        let slot = self.slot.lock();
        let rs = slot.as_ref().ok_or_else(closed_error)?;
        match rs.try_detach()? {
            None => Ok(None),
            // cursorTracker.NewCursor + WrapRecordSetWithCursor + finishStmt: the record
            // set's TryDetach already returns blocked-by:D12.12.
            // log-skip: execStmtResult.TryDetach:2462 detached cursor record sets are blocked-by:D12.12
            Some(_) => Err(tidb_base::blocked::blocked(
                "D12.12",
                "detached record sets (cursor fetch)",
            )),
        }
    }

    /// GetExecutor4Test exports the internal executor for test purpose (the record set is
    /// behind the slot: `f` sees it).
    // go: execStmtResult.GetExecutor4Test L2 (a closure over the executor: the record set is behind the slot)
    pub fn get_executor4_test<R>(&self, f: impl FnOnce(Option<&dyn Executor>) -> R) -> R {
        let slot = self.slot.lock();
        f(slot.as_ref().and_then(|rs| rs.get_executor4_test()))
    }
}

/// `execStmtResult.Finish` of the session's running statement with its record set.
// go-part: execStmtResult.Finish
async fn finish_running(se: &mut Session, rs: &mut RecordSet) -> Result<(), Error> {
    let Session { core, running, .. } = se;
    let Some(run) = running.as_mut() else {
        return Ok(());
    };
    if !run.wrapped {
        // The bare recordSet of the point get short path: recordSet.Finish.
        return rs.finish(core).await;
    }
    if run.finished {
        return Ok(());
    }
    run.finished = true;
    let err1 = rs.finish(core).await;
    // finishStmt(ctx, rs.se, err, rs.sql): `err` is always nil there.
    let err2 = finish_stmt(core, None, &run.stmt).await;
    err1?;
    err2
}

/// `execStmtResult.Close` of the session's running statement with its record set: `Finish`,
/// then `recordSet.Close`; the statement leaves the session.
// go-part: execStmtResult.Close
pub(crate) async fn close_running(se: &mut Session, rs: &mut RecordSet) -> Result<(), Error> {
    let err1 = finish_running(se, rs).await;
    let Session { core, running, .. } = se;
    let err2 = match running.as_mut() {
        Some(run) => rs.close(&mut run.stmt, core).await,
        None => Ok(()),
    };
    core.sctx.vars.stmt_ctx().detach_without_resetting_killer();
    se.running = None;
    err1?;
    err2
}
