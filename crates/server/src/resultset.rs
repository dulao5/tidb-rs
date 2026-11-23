//! go-file: pkg/server/internal/resultset/resultset.go
//!
//! The protocol's view of a statement's rows: the session's result
//! ([`tidb_session::exec_result::ExecStmtResult`], Go's `sqlexec.RecordSet`) plus the column
//! definitions derived from its fields. The record set lives in the session (D05.3 spec
//! §2.3), so every call takes the session; Go's `finishLock` (a cursor's `Finish` racing a
//! `Next`) has nothing to guard: the connection owns both.

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_parser::types::FieldType;
use tidb_session::Session;
use tidb_session::exec_result::ExecStmtResult;

use crate::column::{Info, convert_column_info};

/// `tidbResultSet`.
pub struct ResultSet {
    record_set: Option<ExecStmtResult>,
    columns: Option<Vec<Info>>,
    closed: bool,
}

impl ResultSet {
    /// New creates a new result set. Go also takes the prepared statement, whose point-get
    /// cache keeps the column definitions between executions: the plan cache's
    /// `PointGet.ColumnInfos` is D13.1 (prepared-statement planning), so nothing is cached.
    // go: New L2 (no prepared statement: the plan cache's column cache is D13.1)
    pub fn new(record_set: ExecStmtResult) -> ResultSet {
        ResultSet {
            record_set: Some(record_set),
            columns: None,
            closed: false,
        }
    }

    // glue: the record set until Close
    fn rs(&mut self) -> Result<&mut ExecStmtResult, Error> {
        self.record_set
            .as_mut()
            .ok_or_else(|| Error::plain("the result set is closed"))
    }

    // go: tidbResultSet.NewChunk L1
    pub fn new_chunk(&mut self) -> Result<Chunk, Error> {
        self.rs()?.new_chunk()
    }

    // go: tidbResultSet.Next L1
    pub async fn next(&mut self, se: &mut Session, req: &mut Chunk) -> Result<(), Error> {
        self.rs()?.next(se, req).await
    }

    // go: tidbResultSet.Finish L1
    pub async fn finish(&mut self, se: &mut Session) -> Result<(), Error> {
        match self.record_set.as_mut() {
            Some(rs) => rs.finish(se).await,
            None => Ok(()),
        }
    }

    /// Close closes the result set; Go logs the record set's error (`terror.Call`), which
    /// is returned here for the caller to log.
    // go: tidbResultSet.Close L1
    pub async fn close(&mut self, se: &mut Session) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        match self.record_set.take() {
            Some(rs) => rs.close(se).await,
            None => Ok(()),
        }
    }

    // go: tidbResultSet.IsClosed L1
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Columns gets result columns.
    // go: tidbResultSet.Columns L2 (no prepared statement's column cache: D13.1)
    pub fn columns(&mut self, se: &Session) -> Result<&[Info], Error> {
        if self.columns.is_none() {
            let fields = self.rs()?.fields(se)?;
            self.columns = Some(fields.iter().map(convert_column_info).collect());
        }
        Ok(self.columns.as_deref().unwrap_or_default())
    }

    /// The cached column definitions, moved out (empty if `columns` was never called).
    // glue: hands the definitions sent to the router (`ResultPackets.infos`) without a copy
    pub fn take_columns(&mut self) -> Vec<Info> {
        self.columns.take().unwrap_or_default()
    }

    // go: tidbResultSet.FieldTypes L1
    pub fn field_types(&mut self, se: &Session) -> Result<Vec<FieldType>, Error> {
        Ok(self
            .rs()?
            .fields(se)?
            .into_iter()
            .map(|f| f.column_type)
            .collect())
    }

    /// TryDetach: a cursor's record set. The VS1 router keeps cursors on the PoC; the
    /// session's `TryDetach` is `blocked-by:D12.12` (cursor fetch) when it would detach.
    // go: tidbResultSet.TryDetach L2 (the detached result set comes with cursor fetch, D12.12)
    pub fn try_detach(&mut self, se: &Session) -> Result<bool, Error> {
        Ok(self.rs()?.try_detach(se)?.is_some())
    }
}
