//! Port of Go TiDB v8.5.8 `pkg/sessionctx/variable`: block D05.1 (system variable
//! definitions and validation, N8 §3.1). The session variables (session.go, slow_log.go)
//! are block D05.2; `session` here is the stub of the part D05.1 uses.

pub mod error;
pub(crate) mod hooks;
pub mod names;
pub mod removed;
pub mod sequence_state;
pub mod session;
pub(crate) mod setvar_affect;
pub mod statusvar;
pub mod sysvar_data;
pub mod tidb_vars;
pub(crate) mod timeparse;
pub(crate) mod util;
#[allow(clippy::module_inception)]
pub mod variable;
pub mod varsutil;

pub use variable::*;
