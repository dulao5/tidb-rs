//! Unit tests for the D00.1 part 2a functions the govec domains exempt or cannot express:
//! `RangerContext.Detach` (copies interface values) and the success path of
//! `getPropProvider` (Go's type assertion; the concrete providers come with part 2b).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::contextutil::{
    ignore_warn_appender, new_plan_cache_tracker, new_range_fallback_handler,
};
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{
    BuildContextRef, OptPropCurrentUser, OptPropKVStore, OptionalEvalPropDesc,
    OptionalEvalPropProvider, optionalPropertyDescList,
};
use tidb_sessionctx::expropt::get_prop_provider;
use tidb_sessionctx::exprstatic::{new_eval_context, new_expr_context, with_optional_property};
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_types::context::DefaultStmtNoWarningContext;

struct Provider(usize, &'static str);

impl OptionalEvalPropProvider for Provider {
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        &optionalPropertyDescList[self.0]
    }
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

struct Other;

impl OptionalEvalPropProvider for Other {
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        &optionalPropertyDescList[0]
    }
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

#[test]
fn get_prop_provider_downcasts() {
    let ctx = new_eval_context(vec![with_optional_property(vec![
        Arc::new(Provider(0, "user")),
        Arc::new(Provider(3, "store")),
    ])]);
    let p = get_prop_provider::<Provider>(&*ctx, OptPropCurrentUser).unwrap();
    assert_eq!(p.1, "user");
    let p = get_prop_provider::<Provider>(&*ctx, OptPropKVStore).unwrap();
    assert_eq!(p.1, "store");
    // A provider of another type under the key: the cast error.
    let err = get_prop_provider::<Other>(&*ctx, OptPropCurrentUser)
        .err()
        .unwrap();
    let msg = String::from_utf8(err.error()).unwrap();
    assert!(
        msg.starts_with("cannot cast OptionalEvalPropProvider to "),
        "{msg}"
    );
    assert!(msg.ends_with(" for key 'OptPropCurrentUser'"), "{msg}");
}

#[test]
fn ranger_context_detach_replaces_the_expr_ctx_and_copies_fix_control() {
    let tracker = Arc::new(new_plan_cache_tracker(None));
    let session_expr: BuildContextRef = new_expr_context(Vec::new());
    let static_expr: BuildContextRef = new_expr_context(Vec::new());
    let ctx = RangerContext {
        type_ctx: DefaultStmtNoWarningContext.clone(),
        err_ctx: errctx::StrictNoWarningContext.clone(),
        expr_ctx: Arc::clone(&session_expr),
        range_fallback_handler: Arc::new(new_range_fallback_handler(
            Arc::clone(&tracker),
            ignore_warn_appender(),
        )),
        plan_cache_tracker: Arc::clone(&tracker),
        optimizer_fix_control: HashMap::from([(44262, "ON".to_owned())]),
        use_cache: true,
        in_prepared_plan_building: true,
        regard_null_as_point: true,
        opt_prefix_index_single_scan: true,
    };
    let d = ctx.detach(Arc::clone(&static_expr));
    assert!(Arc::ptr_eq(&d.expr_ctx, &static_expr));
    assert!(Arc::ptr_eq(&ctx.expr_ctx, &session_expr));
    assert_eq!(d.optimizer_fix_control, ctx.optimizer_fix_control);
    assert!(Arc::ptr_eq(&d.plan_cache_tracker, &ctx.plan_cache_tracker));
    assert!(d.use_cache && d.in_prepared_plan_building && d.regard_null_as_point);
    assert!(d.opt_prefix_index_single_scan);
}
