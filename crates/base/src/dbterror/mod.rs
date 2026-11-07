//! go-file: pkg/util/dbterror/terror.go
//!
//! Port of Go TiDB v8.5.8 `pkg/util/dbterror` and its subpackages `exeerrors`,
//! `plannererrors` (block D00.2): error classes whose `NewStd` uses TiDB's message table
//! (`errno.MySQLErrName`), and the error prototypes. The prototype files are generated
//! (tracker `govec/cmd/rsgen`); `ReorgRetryableErrCodes` / `ReorgRetryableErrMsgs` of
//! `ddl_terror.go` are not ported (DDL reorganisation only, see the block spec).

#![allow(non_upper_case_globals)]

mod ddl_terror;
pub mod exeerrors;
pub mod plannererrors;

pub use ddl_terror::*;

use crate::errno;
use crate::errors::{Error, IntoErrCode};
use crate::mysql::ErrMessage;
use crate::terror;

/// ErrClass represents a class of errors (Go: `struct{ terror.ErrClass }`, embedding).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrClass(pub terror::ErrClass);

// Error classes.
pub const ClassAutoid: ErrClass = ErrClass(terror::ClassAutoid);
pub const ClassDDL: ErrClass = ErrClass(terror::ClassDDL);
pub const ClassDomain: ErrClass = ErrClass(terror::ClassDomain);
pub const ClassExecutor: ErrClass = ErrClass(terror::ClassExecutor);
pub const ClassExpression: ErrClass = ErrClass(terror::ClassExpression);
pub const ClassAdmin: ErrClass = ErrClass(terror::ClassAdmin);
pub const ClassKV: ErrClass = ErrClass(terror::ClassKV);
pub const ClassMeta: ErrClass = ErrClass(terror::ClassMeta);
pub const ClassOptimizer: ErrClass = ErrClass(terror::ClassOptimizer);
pub const ClassPrivilege: ErrClass = ErrClass(terror::ClassPrivilege);
pub const ClassSchema: ErrClass = ErrClass(terror::ClassSchema);
pub const ClassServer: ErrClass = ErrClass(terror::ClassServer);
pub const ClassStructure: ErrClass = ErrClass(terror::ClassStructure);
pub const ClassVariable: ErrClass = ErrClass(terror::ClassVariable);
pub const ClassXEval: ErrClass = ErrClass(terror::ClassXEval);
pub const ClassTable: ErrClass = ErrClass(terror::ClassTable);
pub const ClassTypes: ErrClass = ErrClass(terror::ClassTypes);
pub const ClassJSON: ErrClass = ErrClass(terror::ClassJSON);
pub const ClassTiKV: ErrClass = ErrClass(terror::ClassTiKV);
pub const ClassSession: ErrClass = ErrClass(terror::ClassSession);
pub const ClassPlugin: ErrClass = ErrClass(terror::ClassPlugin);
pub const ClassUtil: ErrClass = ErrClass(terror::ClassUtil);

impl ErrClass {
    /// NewStd calls New using the standard message for the error code. A code without a
    /// message in `errno.MySQLErrName` panics (Go dereferences a nil `*ErrMessage`).
    // go: ErrClass.NewStd L1
    pub fn new_std(self, code: impl IntoErrCode) -> Error {
        let code = code.into_err_code();
        let msg = errno::MySQLErrName
            .get(&(code as u16))
            .unwrap_or_else(|| panic!("invalid memory address or nil pointer dereference"));
        self.0.new_std_err(code, msg)
    }

    /// Promoted method of the embedded `terror.ErrClass`.
    // glue: Go struct embedding (terror.ErrClass.NewStdErr)
    pub fn new_std_err(self, code: impl IntoErrCode, message: &ErrMessage) -> Error {
        self.0.new_std_err(code, message)
    }

    /// Promoted method of the embedded `terror.ErrClass`.
    // glue: Go struct embedding (terror.ErrClass.EqualClass)
    pub fn equal_class(self, err: Option<&Error>) -> bool {
        self.0.equal_class(err)
    }

    /// Promoted method of the embedded `terror.ErrClass`.
    // glue: Go struct embedding (terror.ErrClass.Synthesize)
    pub fn synthesize(
        self,
        code: impl IntoErrCode,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Error {
        self.0.synthesize(code, message)
    }
}
