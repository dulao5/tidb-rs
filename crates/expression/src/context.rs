//! go-file: pkg/expression/context.go
//!
//! Package-level aliases of the `exprctx` interfaces and one-line accessors. The
//! `assertionEvalContext` (an `intest`-only wrapper that checks a built-in function only
//! reads the optional properties it declared) needs `builtinFunc` and comes with part 2.

use std::sync::Arc;

use tidb_base::contextutil::SQLWarn;
use tidb_base::gostd::time::Location;
use tidb_base::mysql::SQLMode;
use tidb_sessionctx::errctx;
use tidb_types::context::Context as TypeContext;

pub use tidb_sessionctx::exprctx::{
    BuildContext, EvalContext, ExprContext as AggFuncBuildContext, OptionalEvalPropDesc,
    OptionalEvalPropKey, OptionalEvalPropKeySet, OptionalEvalPropProvider, ParamValues,
};

// go: sqlMode L1
#[inline]
pub fn sql_mode(ctx: &dyn EvalContext) -> SQLMode {
    ctx.sql_mode()
}

// go: typeCtx L1
#[inline]
pub fn type_ctx(ctx: &dyn EvalContext) -> TypeContext {
    ctx.type_ctx()
}

// go: errCtx L1
#[inline]
pub fn err_ctx(ctx: &dyn EvalContext) -> errctx::Context {
    ctx.err_ctx()
}

// go: location L1
#[inline]
pub fn location(ctx: &dyn EvalContext) -> Arc<Location> {
    ctx.location()
}

// go: warningCount L1
#[inline]
pub fn warning_count(ctx: &dyn EvalContext) -> i64 {
    ctx.warning_count()
}

// go: truncateWarnings L1
#[inline]
pub fn truncate_warnings(ctx: &dyn EvalContext, start: i64) -> Vec<SQLWarn> {
    ctx.truncate_warnings(start)
}

// go: checkEvalCtx L1
#[allow(dead_code)] // called by wrapEvalAssert (part 2)
pub(crate) fn check_eval_ctx(ctx: &dyn EvalContext) {
    let tc = ctx.type_ctx();
    debug_assert!(
        Arc::ptr_eq(&ctx.location(), &tc.location()),
        "location is not equal, ctxLoc: {:?}, tcLoc: {:?}",
        ctx.location(),
        tc.location()
    );
}
