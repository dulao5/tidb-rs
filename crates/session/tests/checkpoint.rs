//! VS1 plan §4.6: after a fallback the session looks as if the statement never ran here.
//! Every group of fields the statement path writes is changed the way Go's statement path
//! changes it; restoring the checkpoint gives back the same serialized snapshot, and the
//! snapshot sees each of the changes (so the equality is not vacuous).

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::mysql::ServerStatusInTrans;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_types::datum::new_int_datum;

struct Defaults;

impl GlobalVarAccessor for Defaults {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        Ok(tidb_sessionctx::variable::get_sys_var(name)
            .map(|v| v.value.clone())
            .unwrap_or_default())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

fn session() -> Session {
    let mut s = Session::new(Dialect::MySQL, Box::new(Defaults));
    // Some state left by earlier statements.
    s.vars_mut().last_found_rows = 3;
    s.vars()
        .stmt_ctx()
        .append_warning(errors::new("an earlier warning"));
    s
}

/// The changes of a statement that got as far as planning, one closure per group of
/// §4.6 item 1.
/// A named change to the session.
type Mutation = (&'static str, Box<dyn Fn(&mut Session)>);

fn mutations() -> Vec<Mutation> {
    vec![
        (
            "StmtCtx replaced (ResetContextOfStmt)",
            Box::new(|s: &mut Session| {
                let mut sc = new_stmt_ctx();
                sc.prev_affected_rows = -1;
                s.vars_mut().set_stmt_ctx(Arc::new(sc));
            }),
        ),
        (
            "warning on the old StmtCtx",
            Box::new(|s: &mut Session| {
                s.vars()
                    .stmt_ctx()
                    .append_warning(errors::new("planner warning"))
            }),
        ),
        (
            "TxnCtx (PrepareTxnCtx)",
            Box::new(|s: &mut Session| {
                let v = s.vars_mut();
                v.state.txn_ctx.start_ts = 42;
                v.set_status_flag(ServerStatusInTrans, true);
            }),
        ),
        (
            "LastFoundRows",
            Box::new(|s: &mut Session| s.vars_mut().last_found_rows = 10),
        ),
        (
            "PlanCacheParams",
            Box::new(|s: &mut Session| {
                s.vars_mut()
                    .state
                    .plan_cache_params
                    .append(&[new_int_datum(1)])
            }),
        ),
        (
            "FoundInPlanCache",
            Box::new(|s: &mut Session| s.vars_mut().found_in_plan_cache = true),
        ),
        (
            "LastQueryInfo",
            Box::new(|s: &mut Session| {
                s.vars_mut().state.last_query_info.start_ts = 7;
                s.vars_mut().last_query_info.start_ts = 7;
            }),
        ),
        (
            "plan id counter",
            Box::new(|s: &mut Session| {
                s.vars().alloc_new_plan_id();
            }),
        ),
        (
            "prepared statement id",
            Box::new(|s: &mut Session| s.vars_mut().prepared_stmt_id += 1),
        ),
    ]
}

#[test]
fn restore_undoes_every_group() {
    for (name, f) in mutations() {
        let mut s = session();
        let before = s.state_snapshot();
        let cp = s.checkpoint();
        f(&mut s);
        assert_ne!(
            s.state_snapshot(),
            before,
            "{name}: the snapshot must see it"
        );
        s.restore(cp);
        assert_eq!(s.state_snapshot(), before, "{name}: not restored");
    }
}

#[test]
fn restore_undoes_all_groups_together() {
    let mut s = session();
    let before = s.state_snapshot();
    let cp = s.checkpoint();
    for (_, f) in mutations() {
        f(&mut s);
    }
    s.restore(cp);
    assert_eq!(s.state_snapshot(), before);
    // Only the earlier warning is left (SHOW WARNINGS shows the PoC's, VS1 plan §4.6).
    assert_eq!(s.vars().stmt_ctx().warning_count(), 1);
}

/// The phase-0 session plans nothing: every statement path is the D05.3 boundary stub.
#[test]
fn statement_paths_are_blocked() {
    let mut s = session();
    let stmt = tidb_parser::parse("SELECT 1", &Default::default())
        .unwrap()
        .stmts
        .remove(0);
    let block = |e: Error| tidb_base::blocked::blocked_by(&e).map(str::to_owned);
    let got = block_on(Box::pin(async { s.execute_stmt(&stmt).await.err() }));
    assert_eq!(got.and_then(block), Some("D05.3".to_owned()));
    let got = block_on(Box::pin(async { s.prepare_stmt(&stmt).await.err() }));
    assert_eq!(got.and_then(block), Some("D05.3".to_owned()));
    assert_eq!(s.dialect(), Dialect::MySQL);
    assert!(s.is_autocommit() && !s.in_txn() && s.current_db().is_empty());
}

/// A minimal executor for the stubs' futures (they never wait).
fn block_on<T>(f: std::pin::Pin<Box<dyn std::future::Future<Output = T> + '_>>) -> T {
    use std::task::{Context, Poll, Waker};
    let mut f = f;
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("the stub futures complete at once"),
    }
}
