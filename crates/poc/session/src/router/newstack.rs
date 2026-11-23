//! The real [`NewStack`]: a `tidb_session::Session` and the glue between its results and the
//! MySQL protocol responses of the PoC's connection loop.
//!
//! The session runs `ExecuteStmt` (VS1-14 part 1), whose planner is still a boundary stub,
//! so every statement falls back (or, in `strict` mode, reports 1235). Prepared statements
//! go through the session's `PrepareStmt` and the prepare executor (prepared-statement
//! planning, VS1-15), which pass the metadata lock of their tables (registered while
//! `PREPARE` runs, per Go, #252) and stop at the plan builder (D09.1, VS1-21).
//! The session's InfoSchema is the ported domain's (D06.5 part 1), which the PoC domain's
//! sync loop reloads at the PoC's own reload timestamps (VS1 plan §4.1).
//!
//! A statement that ran is answered here, never handed back to the router as `Blocked`
//! (VS1 plan §4.3): its rows are encoded by the port of Go's result writing (`tidb-server`,
//! D03.1 part 1: `writeResultSet` / `writeChunks`, `DumpTextRow` / `DumpBinaryRow`, column
//! definitions) into packets the PoC's connection sends as they are; the parameters of
//! `COM_STMT_EXECUTE` are decoded by Go's `parseBinaryParams` and `ExecBinaryParam`.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_mysql_protocol::{
    ColumnDef, ColumnType, EncodedResultSet, OkInfo, Param, PreparedInfo, RawParams, Response,
};
use tidb_parser::ast;
use tidb_server::column::{Info, ResultEncoder, convert_column_info};
use tidb_server::conn::{ResultPackets, init_input_encoder, write_result_set};
use tidb_server::resultset::ResultSet;
use tidb_session::exec_result::ExecStmtResult;
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
    /// The ported domain the InfoSchema comes from (`None` without schema sync).
    domain: Option<Arc<tidb_domain::Domain>>,
    /// Go's `clientConn.rsEncoder`: the result encoder of the last result set (`None`
    /// before the first), which `writePrepare` also dumps column definitions with.
    rs_encoder: Option<ResultEncoder>,
}

impl NewSession {
    /// A new session in the MySQL dialect (the only protocol and front end, ADR 0010), over
    /// the ported store driver when given (without one, the session's store is the
    /// stand-in whose every read is a boundary of the store driver).
    pub fn new(
        domain: Option<Arc<tidb_domain::Domain>>,
        store: Option<Arc<tidb_store::driver::TikvStore>>,
    ) -> NewSession {
        if let Some(d) = &domain {
            // Go's bootstrap (`InitMDLVariable`); idempotent, so done with each session.
            tidb_session::session::init_mdl_variable();
            // Go's `init` of planner/core (`domain.NewInstancePlanCache`) and the domain's
            // `InitInstancePlanCache` at start; both idempotent, so done with each session.
            tidb_planner::core::plan_cache_instance::register_instance_plan_cache_factory();
            d.init_instance_plan_cache();
        }
        let mut session = match store {
            Some(store) => {
                let oracle = Arc::new(tidb_session::glue::StoreOracle(store.get_oracle()));
                Session::with_store(Dialect::MySQL, Box::new(DefaultGlobals), store, oracle)
            }
            None => Session::new(Dialect::MySQL, Box::new(DefaultGlobals)),
        };
        session.set_instance_plan_cache(domain.as_ref().and_then(|d| d.get_instance_plan_cache()));
        NewSession {
            session,
            domain,
            rs_encoder: None,
        }
    }
}

impl Default for NewSession {
    fn default() -> Self {
        NewSession::new(None, None)
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
        self.domain.as_ref()?;
        Some(
            self.session
                .sctx()
                .get_domain_info_schema()
                .schema_meta_version(),
        )
    }

    fn refresh_info_schema(&mut self) {
        if let Some(is) = self.domain.as_ref().and_then(|d| d.info_schema()) {
            self.session.set_domain_info_schema(is);
        }
    }

    fn execute<'a>(
        &'a mut self,
        stmt: ast::Stmt,
        sql: &'a str,
    ) -> BoxFut<'a, Result<Response, Error>> {
        Box::pin(async move {
            let rs = self.session.execute_stmt(stmt, sql).await?;
            self.write_result(rs, false).await
        })
    }

    fn prepare<'a>(&'a mut self, sql: &'a str) -> BoxFut<'a, Result<NewPrepared, Error>> {
        Box::pin(async move {
            // Go `TiDBContext.Prepare`: the statement, then its columns (ConvertColumnInfo).
            let info = self.session.prepare_stmt(sql).await?;
            let columns: Vec<Info> = info.fields.iter().map(convert_column_info).collect();
            // Go `writePrepare`: the definitions are dumped with the connection's current
            // result encoder (the default one before the first result set).
            let mut column_packets = Vec::with_capacity(columns.len());
            for c in &columns {
                let mut b = Vec::new();
                c.dump(&mut b, self.rs_encoder.as_mut());
                column_packets.push(b);
            }
            Ok(NewPrepared {
                stmt_id: info.stmt_id,
                info: PreparedInfo {
                    param_count: u16::try_from(info.param_count).unwrap_or(u16::MAX),
                    columns: columns.iter().map(column_def).collect(),
                    column_packets,
                },
            })
        })
    }

    fn execute_prepared<'a>(
        &'a mut self,
        stmt_id: u32,
        params: &'a [Param],
        raw: &'a RawParams,
    ) -> BoxFut<'a, Result<Response, Error>> {
        Box::pin(async move {
            // Go `executePreparedStmtAndWriteResult`: parseBinaryParams with the statement's
            // long data and parameter types, then ExecBinaryParam with the type context of
            // the session's current statement context.
            let n = params.len();
            let mut binary_params = vec![tidb_expression::param::BinaryParam::default(); n];
            if n > 0 {
                let enc = init_input_encoder(&mut self.session);
                let mut bound = raw.bound.clone();
                bound.resize(n, None);
                tidb_server::params::parse_binary_params(
                    &mut binary_params,
                    &bound,
                    &raw.null_bitmap,
                    &raw.param_types,
                    &raw.param_values,
                    Some(&enc),
                )?;
            }
            let type_ctx = self.session.vars().stmt_ctx().type_ctx();
            let args = tidb_expression::util::exec_binary_param(&type_ctx, &binary_params)?;
            // The session takes the constants' values (its EXECUTE is D02.2 / D13.1).
            let args = args.into_iter().map(|c| c.value).collect();
            let rs = self.session.execute_prepared_stmt(stmt_id, args).await?;
            self.write_result(rs, true).await
        })
    }

    fn close(&mut self, stmt_id: u32) {
        let _ = self.session.drop_prepared_stmt(stmt_id);
    }
}

impl NewSession {
    /// Go `handleQuery` / `executePreparedStmtAndWriteResult` after the statement ran: no
    /// result set is an OK packet (`writeOK`), a result set is written with
    /// `writeResultSet` and closed (`defer rs.Close()`: its error is only logged).
    // glue: the response of D03.1's handleStmt (the connection writes the packets)
    async fn write_result(
        &mut self,
        rs: Option<ExecStmtResult>,
        binary: bool,
    ) -> Result<Response, Error> {
        let Some(rs) = rs else {
            let sc = self.session.vars().stmt_ctx();
            return Ok(Response::Ok(OkInfo {
                affected_rows: sc.affected_rows(),
                last_insert_id: sc.last_insert_id(),
                warnings: sc.warning_count(),
                info: String::new(),
            }));
        };
        let mut trs = ResultSet::new(rs);
        let mut out = ResultPackets::default();
        let r = write_result_set(
            &mut self.session,
            &mut trs,
            binary,
            &mut self.rs_encoder,
            &mut out,
        )
        .await;
        if let Err(e) = trs.close(&mut self.session).await {
            log::warn!("close result set: {e}");
        }
        r?;
        Ok(Response::Encoded(EncodedResultSet {
            columns: out.infos.iter().map(column_def).collect(),
            column_packets: out.columns,
            rows: out.rows,
            warnings: self.session.vars().stmt_ctx().warning_count(),
        }))
    }
}

/// The PoC's description of a column definition the new stack sent, for the router's
/// metadata checks (VS1 plan §4.7: a fallback must send the definitions of the PREPARE).
// glue: column.Info → the PoC protocol's ColumnDef
fn column_def(c: &Info) -> ColumnDef {
    ColumnDef {
        schema: c.schema.clone(),
        table: c.table.clone(),
        org_table: c.org_table.clone(),
        name: c.name.clone(),
        org_name: c.org_name.clone(),
        charset: c.charset,
        column_length: c.column_length,
        column_type: ColumnType::from_u8(c.tp).unwrap_or(ColumnType::LongBlob),
        flags: c.flag,
        decimals: c.decimal,
    }
}
