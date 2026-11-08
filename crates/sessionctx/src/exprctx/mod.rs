//! The expression context interfaces (Go `pkg/expression/exprctx`, block D00.1 part 2a).
//!
//! Mapping (N4):
//! - Go interfaces are object-safe traits (`Send + Sync`) shared as `Arc<dyn …>`.
//! - Go's struct embedding of an interface (`NullRejectCheckExprContext{ExprContext}`) is a
//!   wrapper struct holding the inner `Arc` and forwarding every method.
//! - `variable.UserVarsReader` belongs to D05.2 (user variables, not ported yet); the trait and
//!   an empty `UserVars` stand-in live in [`uservars`] until D05.2 replaces them.

pub mod context;
pub mod optional;
pub mod param;
pub mod uservars;

pub use self::context::*;
pub use self::optional::*;
pub use self::param::*;
pub use self::uservars::*;
