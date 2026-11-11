//! The optional evaluation property providers (Go `pkg/expression/expropt`, block D00.1).
//!
//! `sessionvars.go` (the session variables provider) is not ported yet: under N2 the
//! evaluation context is shared with executor workers (`Send + Sync`) while `SessionVars` is
//! owned by the session task, so how a builtin reaches the session variables is decided with
//! the session (D05.3); see the D00.1 spec §4.

pub mod advisory_lock;
pub mod current_user;
pub mod ddlowner;
pub mod infoschema;
pub mod kvstore;
pub mod optional;
pub mod priv_;
pub mod sequence;
pub mod sqlexec;

pub use self::advisory_lock::*;
pub use self::current_user::*;
pub use self::ddlowner::*;
pub use self::infoschema::*;
pub use self::kvstore::*;
pub use self::optional::*;
pub use self::priv_::*;
pub use self::sequence::*;
pub use self::sqlexec::*;
