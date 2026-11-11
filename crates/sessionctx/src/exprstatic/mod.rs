//! The static expression contexts (Go `pkg/expression/exprstatic`, block D00.1 part 2a):
//! an `EvalContext` and an `ExprContext` that hold their own values instead of reading a
//! session, so they can be used outside the session (plan cache, background tasks).

pub mod evalctx;
pub mod exprctx;

pub use self::evalctx::*;
pub use self::exprctx::*;
