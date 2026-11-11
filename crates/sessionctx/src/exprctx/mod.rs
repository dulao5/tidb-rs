//! The expression context interfaces (Go `pkg/expression/exprctx`, block D00.1 part 2a).
//!
//! Mapping (N4):
//! - Go interfaces are object-safe traits (`Send + Sync`) shared as `Arc<dyn …>`.
//! - Go's struct embedding of an interface (`NullRejectCheckExprContext{ExprContext}`) is a
//!   wrapper struct holding the inner `Arc` and forwarding every method.
//! - `variable.UserVarsReader` and `variable.UserVars` (D05.2) live in `variable::uservars`
//!   (where Go defines them) and are re-exported here.

pub mod context;
pub mod optional;
pub mod param;

pub use self::context::*;
pub use self::optional::*;
pub use self::param::*;
pub use crate::variable::uservars::*;
