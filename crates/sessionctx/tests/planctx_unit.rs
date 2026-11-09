//! Unit tests for the D00.1 part 2b pieces govec does not compare: the empty plan context,
//! BuildPBContext's accessors over interface values, and the debug trace's arena bookkeeping.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tidb_base::gostd::json::Value;
use tidb_kv::context::Context;
use tidb_kv::kv::{Client, ClientSendOption, OptionValue, Request, Response};
use tidb_sessionctx::debugtrace::{OptimizerDebugTraceRoot, TraceValue};
use tidb_sessionctx::exprctx::BuildContextRef;
use tidb_sessionctx::exprstatic::{new_expr_context, with_charset};
use tidb_sessionctx::planctx::{BuildPBContext, EmptyPlanContextExtended};

struct NoClient;
impl Client for NoClient {
    fn send(
        &self,
        _: &Context,
        _: &Request,
        _: Option<OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!()
    }
    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

#[test]
fn empty_plan_context_extended() {
    let e = EmptyPlanContextExtended;
    assert!(e.advise_txn_warmup().is_ok());
    e.set_readonly_user_var_map(HashSet::from(["a".to_owned()]));
    assert!(e.get_readonly_user_var_map().is_empty());
    e.reset();
}

#[test]
fn build_pb_context_detach() {
    let a: BuildContextRef = new_expr_context(vec![]);
    let b: BuildContextRef =
        new_expr_context(vec![with_charset("latin1".into(), "latin1_bin".into())]);
    let client: Arc<dyn Client> = Arc::new(NoClient);
    let ctx = BuildPBContext {
        expr_ctx: a.clone(),
        client: client.clone(),
        tiflash_fast_scan: true,
        tiflash_fine_grained_shuffle_batch_size: 7,
        group_concat_max_len: 1024,
        in_explain_stmt: true,
        warn_handler: None,
        extra_warnghandler: None,
    };
    assert!(Arc::ptr_eq(&ctx.get_expr_ctx(), &a));
    assert!(Arc::ptr_eq(&ctx.get_client(), &client));
    let d = ctx.detach(b.clone());
    // The copy has the new expression context and every other field unchanged.
    assert!(Arc::ptr_eq(&d.get_expr_ctx(), &b));
    assert!(Arc::ptr_eq(&ctx.get_expr_ctx(), &a));
    assert!(Arc::ptr_eq(&d.get_client(), &client));
    assert!(d.tiflash_fast_scan && d.in_explain_stmt);
    assert_eq!(d.tiflash_fine_grained_shuffle_batch_size, 7);
    assert_eq!(d.group_concat_max_len, 1024);
}

#[test]
fn debug_trace_contexts() {
    let mut root = OptimizerDebugTraceRoot::new();
    assert_eq!(root.current_ctx(), 0);
    // Leaving the top context stays there (its parent is itself, as in Go).
    root.leave_context();
    assert_eq!(root.current_ctx(), 0);
    root.enter_context("f");
    assert_eq!(root.current_ctx(), 1);
    root.append_step_to_current_context(TraceValue::Json(Value::Int(1)));
    root.leave_context();
    assert_eq!(root.current_ctx(), 0);
    assert_eq!(root.steps(0), &[TraceValue::Ctx(1)]);
    assert_eq!(root.marshal_json(), "[{\"f\":1}]\n");
    assert_eq!(root.ctx_marshal_json(1), "{\"f\":1}\n");
}

#[test]
fn debug_trace_record_odd_panics_like_go() {
    let mut root = OptimizerDebugTraceRoot::new();
    let err = catch_unwind(AssertUnwindSafe(|| {
        root.record_any_values_with_names(vec![TraceValue::Json(Value::String("a".into()))]);
    }))
    .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert_eq!(msg, "runtime error: index out of range [1] with length 1");
}
