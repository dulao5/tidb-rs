//! Go: pkg/expression/exprctx/context.go (no `go-file` header until part 2b ports it).
//!
//! Interfaces of the expression contexts (block D00.1). Part 2a declares only the handle
//! type the ranger context carries; the interface methods and the static implementations
//! come with part 2b (tasks T6) together with D08.1.

use std::sync::Arc;

/// BuildContext is used to build an expression.
///
/// Part 2a: the method set is added in part 2b (T6); the trait exists so that
/// `RangerContext.ExprCtx` has its Go type.
// glue: interface skeleton, methods follow in D00.1 part 2b
pub trait BuildContext: Send + Sync {}

/// A shared `exprctx.BuildContext` interface value.
pub type BuildContextRef = Arc<dyn BuildContext>;
