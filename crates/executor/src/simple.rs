//! go-file: pkg/executor/simple.go
//!
//! `ADMIN FLUSH ... PLAN_CACHE` (`SimpleExec.executeAdminFlushPlanCache`). The other simple
//! statements are answered by the prototype until their blocks are ported. With the
//! instance-level plan cache, Go's statement clears only the session's (unused) session-level
//! cache; tidb-rs has no session-level cache and clears the instance-level one for
//! `INSTANCE` (the plan-cache exception A3, decision 8, a Go defect corrected).

use tidb_base::{Error, errors};
use tidb_sessionctx::context::InstancePlanCache;
use tidb_sessionctx::variable::session::SessionVars;

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
