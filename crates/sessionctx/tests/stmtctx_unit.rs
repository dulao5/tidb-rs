//! Unit tests for the D00.1 part 1 functions the govec `stmtctx` domain exempts: process-wide
//! counters, opaque `any` values and the wall clock (see govec/domains/stmtctx_pending.go).

use std::any::Any;
use std::sync::Arc;

use tidb_base::contextutil::gen_context_id;
use tidb_sessionctx::stmtctx::{self, allocate_task_id};

#[test]
fn ids_are_unique_and_increasing() {
    let a = gen_context_id();
    let b = gen_context_id();
    assert!(b > a);
    let sc1 = stmtctx::new_stmt_ctx();
    let mut sc2 = stmtctx::new_stmt_ctx();
    assert!(sc2.ctx_id() > sc1.ctx_id());
    // Reset gives a new context ID (Go: `ctxID: contextutil.GenContextID()`).
    let before = sc2.ctx_id();
    assert!(sc2.reset());
    assert!(sc2.ctx_id() > before);
    let t1 = allocate_task_id();
    let t2 = allocate_task_id();
    assert_eq!(t2, t1 + 1);
}

#[test]
fn opaque_values() {
    let mut sc = stmtctx::new_stmt_ctx();
    assert!(sc.get_plan().is_none() && sc.get_flat_plan().is_none());
    sc.set_plan(Some(Arc::new(7i64)));
    sc.set_flat_plan(Some(Arc::new("flat")));
    assert_eq!(sc.get_plan().unwrap().downcast_ref::<i64>(), Some(&7));
    assert_eq!(
        sc.get_flat_plan().unwrap().downcast_ref::<&str>(),
        Some(&"flat")
    );
    // The cached contexts are created once (sync.Once).
    let mut calls = 0;
    let a = sc.get_or_init_ranger_ctx_from_cache(|| {
        calls += 1;
        Arc::new(1u8) as Arc<dyn Any + Send + Sync>
    });
    let b = sc.get_or_init_ranger_ctx_from_cache(|| {
        calls += 1;
        Arc::new(2u8) as Arc<dyn Any + Send + Sync>
    });
    assert_eq!(calls, 1);
    assert!(Arc::ptr_eq(&a, &b));
    let c = sc.get_or_init_build_pb_ctx_from_cache(|| Arc::new(3u8));
    assert_eq!(c.downcast_ref::<u8>(), Some(&3));
}

#[test]
fn lock_wait_start_time_is_set_once() {
    let sc = stmtctx::new_stmt_ctx();
    let t1 = sc.get_lock_wait_start_time();
    assert!(t1 > 0);
    assert_eq!(sc.get_lock_wait_start_time(), t1);
}

#[test]
fn statement_context_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<stmtctx::StatementContext>();
}

/// D00.1 part 1 review (tidb-rs #65): copies of the type context, the error context and the
/// warning appender taken before the handler is replaced and the context is reset still write
/// to the new handler (Go hands out the statement context itself as the appender).
#[test]
fn old_copies_write_to_the_new_handler_after_reset() {
    use tidb_base::contextutil::{WarnAppender, WarnHandlerExtRef, new_static_warn_handler};

    let mut sc = stmtctx::new_stmt_ctx();
    let (tc, ec, app) = (sc.type_ctx(), sc.err_ctx(), sc.warn_appender());
    let old: WarnHandlerExtRef = sc.warn_handler();

    let h2: WarnHandlerExtRef = Arc::new(new_static_warn_handler(0));
    sc.set_warn_handler(Arc::clone(&h2));
    assert!(sc.reset());

    tc.append_warning(tidb_base::errors::new("via type ctx"));
    ec.append_warning(tidb_base::errors::new("via err ctx"));
    app.append_warning(tidb_base::errors::new("via appender"));

    assert_eq!(h2.warning_count(), 3);
    assert_eq!(sc.warning_count(), 3);
    assert_eq!(old.warning_count(), 0);
    let msgs: Vec<_> = h2
        .copy_warnings(Vec::new())
        .iter()
        .map(|w| String::from_utf8(w.err.error()).unwrap())
        .collect();
    assert_eq!(msgs, ["via type ctx", "via err ctx", "via appender"]);
}
