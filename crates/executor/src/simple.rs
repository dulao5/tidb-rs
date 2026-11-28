//! go-file: pkg/executor/simple.go
//!
//! Utility / administrative simple executors (`ADMIN FLUSH ... PLAN_CACHE`, `KILL`).

use std::sync::Arc;

use async_trait::async_trait;
use tidb_base::{Error, config, errors};
use tidb_chunk::chunk::Chunk;
use tidb_planner::core::common_plans::{SimpleKillStmt, SimpleStmt};
use tidb_sessionctx::context::InstancePlanCache;
use tidb_sessionctx::globalconn::parse_conn_id;
use tidb_sessionctx::session_mgr::SessionManager;
use tidb_sessionctx::stmtctx::StatementContext;
use tidb_sessionctx::variable::session::SessionVars;

use crate::exec::ExecCtx;
use crate::internal::exec::executor::{BaseExecutor, BaseExecutorV2, Executor};

/// `ast.StatementScope` of `ADMIN FLUSH <scope> PLAN_CACHE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementScope {
    /// `ast.StatementScopeSession` (also the default, no keyword).
    Session,
    /// `ast.StatementScopeGlobal`.
    Global,
    /// `ast.StatementScopeInstance`.
    Instance,
}

/// executeAdminFlushPlanCache: the checks and the warning are Go's; `LastUpdateTime4PC`,
/// the session-level cache's `DeleteAll` and the domain's `SetExpiredTimeStamp4PC` only act on
/// session-level caches, which tidb-rs does not have (A3 spec §1.2); for `INSTANCE` the
/// instance-level cache is cleared (A3 decision 8). `cache` is
/// `domain.GetDomain(ctx).GetInstancePlanCache()`.
// go: SimpleExec.executeAdminFlushPlanCache L2 (clears the instance plan cache for INSTANCE, A3 decision 8; no session-level cache)
pub fn execute_admin_flush_plan_cache(
    vars: &mut SessionVars,
    scope: StatementScope,
    cache: Option<&dyn InstancePlanCache>,
) -> Result<(), Error> {
    // s.Tp != ast.AdminFlushPlanCache: the caller routes only this statement here.
    if scope == StatementScope::Global {
        return Err(errors::new(
            "Do not support the 'admin flush global scope.'",
        ));
    }
    if !vars.enable_prepared_plan_cache {
        vars.stmt_ctx().append_warning(Error::plain(
            "The plan cache is disable. So there no need to flush the plan cache",
        ));
        return Ok(());
    }
    // now := ...; LastUpdateTime4PC = now; GetSessionPlanCache().DeleteAll(): there is no
    // session-level cache.
    if scope == StatementScope::Instance {
        // domain.SetExpiredTimeStamp4PC(now): read by session-level caches only.
        if let Some(cache) = cache {
            cache.delete_all();
        }
    }
    Ok(())
}

/// Shared `KILL` statement execution used by both [`SimpleExec`] (new stack) and `--new-stack=off`.
// go: SimpleExec.executeKillStmt L2 (delegates cross-node kill via SessionManager::kill_remote_conn and guards unleased cluster server_id)
pub async fn execute_kill_stmt(
    stmt_ctx: Option<&StatementContext>,
    connection_id: u64,
    session_mgr: Option<&dyn SessionManager>,
    is_from_remote: bool,
    source_addr: &str,
    kill_stmt: &SimpleKillStmt,
) -> Result<(), Error> {
    let invalid_op = "Invalid operation. Please use 'KILL TIDB [CONNECTION | QUERY] [connectionID | CONNECTION_ID()]' instead";
    if kill_stmt.expr_invalid {
        return Err(errors::new(invalid_op));
    }
    if let Some(fn_name) = &kill_stmt.expr_func_name {
        if fn_name == "connection_id" {
            if let Some(sm) = session_mgr {
                sm.kill(connection_id, kill_stmt.query, false, false);
            }
            return Ok(());
        }
        return Err(errors::new(invalid_op));
    }

    let cfg = config::get_global_config();
    if !cfg.enable_global_kill {
        if kill_stmt.tidb_extension || cfg.compatible_kill_query {
            if let Some(sm) = session_mgr {
                sm.kill(kill_stmt.connection_id, kill_stmt.query, false, false);
            }
        } else if let Some(sc) = stmt_ctx {
            sc.append_warning(Error::plain(invalid_op));
        }
        return Ok(());
    }

    let Some(sm) = session_mgr else {
        return Ok(());
    };
    if is_from_remote {
        tidb_base::log_info!(
            "Killing connection in current instance redirected from remote TiDB",
            "conn" => kill_stmt.connection_id,
            "query" => kill_stmt.query,
            "sourceAddr" => source_addr,
        );
        sm.kill(kill_stmt.connection_id, kill_stmt.query, false, false);
        return Ok(());
    }

    let (gcid, is_truncated) = match parse_conn_id(kill_stmt.connection_id) {
        Ok(parsed) => parsed,
        Err(err) => {
            if let Some(sc) = stmt_ctx {
                sc.append_warning(Error::plain(format!("Parse ConnectionID failed: {err}")));
            }
            return Ok(());
        }
    };
    if is_truncated {
        let message = "Kill failed: Received a 32bits truncated ConnectionID, expect 64bits. Please execute 'KILL [CONNECTION | QUERY] ConnectionID' to send a Kill without truncating ConnectionID.";
        tidb_base::log_warn!(
            "Kill failed: Received a 32bits truncated ConnectionID, expect 64bits. Please execute 'KILL [CONNECTION | QUERY] ConnectionID' to send a Kill without truncating ConnectionID.",
            "conn" => kill_stmt.connection_id,
        );
        if let Some(sc) = stmt_ctx {
            sc.append_warning(Error::plain(message));
        }
        return Ok(());
    }

    let local_server_id = sm.server_id();
    if gcid.server_id != local_server_id {
        let res = if gcid.server_id == 0 {
            Err(errors::new(
                "Unexpected ZERO ServerID. Please file a bug to the TiDB Team",
            ))
        } else {
            sm.kill_remote_conn(gcid.server_id, gcid.to_conn_id(), kill_stmt.query)
                .await
        };
        if let Err(err) = res
            && let Some(sc) = stmt_ctx
        {
            sc.append_warning(Error::plain(format!(
                "KILL remote connection failed: {err}"
            )));
        }
    } else if !sm.has_valid_server_id() {
        return Err(errors::new(
            "Kill failed: node does not hold a valid /tidb/server_id lease",
        ));
    } else {
        sm.kill(kill_stmt.connection_id, kill_stmt.query, false, false);
    }

    Ok(())
}

/// `SimpleExec` executes non-relational utility statements (`KILL`, etc.).
pub struct SimpleExec {
    pub(crate) base: BaseExecutor,
    pub(crate) stmt: SimpleStmt,
    pub(crate) is_from_remote: bool,
    pub(crate) done: bool,
    pub(crate) session_mgr: Option<Arc<dyn SessionManager>>,
    pub(crate) connection_id: u64,
    pub(crate) source_addr: String,
}

impl SimpleExec {
    /// Constructs a [`SimpleExec`].
    // glue: constructor from ExecutorBuilder::build_simple
    pub fn new(
        base: BaseExecutor,
        stmt: SimpleStmt,
        is_from_remote: bool,
        session_mgr: Option<Arc<dyn SessionManager>>,
        connection_id: u64,
        source_addr: String,
    ) -> SimpleExec {
        SimpleExec {
            base,
            stmt,
            is_from_remote,
            done: false,
            session_mgr,
            connection_id,
            source_addr,
        }
    }
}

#[async_trait]
impl Executor for SimpleExec {
    // glue: M-SELF
    fn base(&self) -> &BaseExecutorV2 {
        &self.base.base
    }

    // glue: M-SELF
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base.base
    }

    /// `SimpleExec.Next`: runs the simple statement on the first call, then returns empty.
    // go: SimpleExec.Next L2 (supports KILL; remaining SimpleExec arms handled on their paths)
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.done {
            return Ok(());
        }
        self.done = true;
        match &self.stmt {
            SimpleStmt::Kill(kill_stmt) => {
                let stmt_ctx = self.base.ctx().map(Arc::as_ref);
                execute_kill_stmt(
                    stmt_ctx,
                    self.connection_id,
                    self.session_mgr.as_deref(),
                    self.is_from_remote,
                    &self.source_addr,
                    kill_stmt,
                )
                .await
            }
        }
    }
}
