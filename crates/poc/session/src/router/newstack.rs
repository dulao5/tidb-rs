//! The real [`NewStack`]: a `tidb_session::Session` and the glue between its results and the
//! MySQL protocol responses of the PoC's connection loop.
//!
//! In VS1 phase 0 the session's statement paths are `blocked-by:D05.3` stubs, so every
//! statement falls back (or, in `strict` mode, reports 1235). The pieces that turn a
//! successful result into a response are stubs of their own until their work items land:
//!
//! - TODO(VS1-16): result sets and column definitions (`writeResultSet`, `column` / `dump`,
//!   D03.1) and binary parameters (`parseBinaryParams`, D03.1);
//! - TODO(VS1-17): the new stack's InfoSchema, loaded from the same schema diffs as the PoC's.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_mysql_protocol::{Param, Response};
use tidb_parser::ast;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::variable::session::GlobalVarAccessor;

use super::{BoxFut, NewPrepared, NewStack};

/// The global values of system variables for the new session: the compiled defaults, as the
/// PoC uses them (the PoC reads no `mysql.global_variables`).
///
/// TODO(VS1-17): the domain's sysvar cache (D06.5), so the new session sees `SET GLOBAL`s
/// made on Go TiDB.
pub struct DefaultGlobals;

impl GlobalVarAccessor for DefaultGlobals {
    // glue: stand-in for the domain's sysvar cache (D06.5)
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        match tidb_sessionctx::variable::get_sys_var(name) {
            Some(sv) => Ok(sv.value.clone()),
            None => Err(tidb_base::errors::new(format!(
                "unknown system variable '{name}'"
            ))),
        }
    }
    // glue: stand-in (the read-only node never writes global variables)
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Err(blocked("D06.5", "SET GLOBAL in the new session"))
    }
    // glue: stand-in (mysql.tidb is read by the domain, D06.5)
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Err(blocked("D06.5", "mysql.tidb in the new session"))
    }
    // glue: stand-in (the read-only node never writes mysql.tidb)
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Err(blocked("D06.5", "mysql.tidb in the new session"))
    }
}

/// The ported stack of one connection.
pub struct NewSession {
    session: Session,
}

impl NewSession {
    /// A new session in the MySQL dialect (the only protocol and front end, ADR 0010).
    pub fn new() -> NewSession {
        NewSession {
            session: Session::new(Dialect::MySQL, Box::new(DefaultGlobals)),
        }
    }
}

impl Default for NewSession {
    fn default() -> Self {
        NewSession::new()
    }
}

impl NewStack for NewSession {
    fn session(&self) -> &Session {
        &self.session
    }

    fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    fn info_schema_version(&self) -> Option<i64> {
        None
    }

    fn execute<'a>(&'a mut self, stmt: &'a ast::Stmt) -> BoxFut<'a, Result<Response, Error>> {
        Box::pin(async move {
            let _rs = self.session.execute_stmt(stmt).await?;
            Err(blocked("D03.1", "result sets of the new stack"))
        })
    }

    fn prepare<'a>(&'a mut self, stmt: &'a ast::Stmt) -> BoxFut<'a, Result<NewPrepared, Error>> {
        Box::pin(async move {
            let info = self.session.prepare_stmt(stmt).await?;
            // The statement exists in the new session now; without its column definitions
            // it cannot be used, so it is dropped again.
            let _ = self.session.drop_prepared_stmt(info.stmt_id);
            Err(blocked("D03.1", "column definitions of the new stack"))
        })
    }

    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        params: &'a [Param],
    ) -> BoxFut<'a, Result<Response, Error>> {
        Box::pin(async move {
            if !params.is_empty() {
                return Err(blocked("D03.1", "binary parameters of the new stack"));
            }
            let _rs = self
                .session
                .execute_prepared_stmt(stmt_id, Vec::new())
                .await?;
            Err(blocked("D03.1", "result sets of the new stack"))
        })
    }

    fn close(&mut self, stmt_id: u32) {
        let _ = self.session.drop_prepared_stmt(stmt_id);
    }
}
