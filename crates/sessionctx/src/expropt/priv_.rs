//! go-file: pkg/expression/expropt/priv.go
//!
//! Mapping: Go's function type `PrivilegeCheckerProvider` is a struct holding the closure.

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::PrivilegeType;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropPrivilegeChecker, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// PrivilegeChecker provides privilege check for expressions.
pub trait PrivilegeChecker: Send + Sync {
    /// RequestVerification verifies user privilege
    // glue: interface method of expropt.PrivilegeChecker
    fn request_verification(
        &self,
        db: &str,
        table: &str,
        column: &str,
        priv_: PrivilegeType,
    ) -> bool;
    /// RequestDynamicVerification verifies user privilege for a DYNAMIC privilege.
    // glue: interface method of expropt.PrivilegeChecker
    fn request_dynamic_verification(&self, priv_name: &str, grantable: bool) -> bool;
}

/// A shared `expropt.PrivilegeChecker` interface value.
pub type PrivilegeCheckerRef = Arc<dyn PrivilegeChecker>;

/// PrivilegeCheckerProvider is used to provide PrivilegeChecker.
#[derive(Clone)]
pub struct PrivilegeCheckerProvider(pub Arc<dyn Fn() -> PrivilegeCheckerRef + Send + Sync>);

impl OptionalEvalPropProvider for PrivilegeCheckerProvider {
    /// Desc returns the description for the property key.
    // go: PrivilegeCheckerProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropPrivilegeChecker.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// PrivilegeCheckerPropReader is used by expression to get PrivilegeChecker.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrivilegeCheckerPropReader;

impl RequireOptionalEvalProps for PrivilegeCheckerPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: PrivilegeCheckerPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropPrivilegeChecker.as_prop_key_set()
    }
}

impl PrivilegeCheckerPropReader {
    /// GetPrivilegeChecker returns a PrivilegeChecker.
    // go: PrivilegeCheckerPropReader.GetPrivilegeChecker L1
    pub fn get_privilege_checker(
        &self,
        ctx: &dyn EvalContext,
    ) -> Result<PrivilegeCheckerRef, Error> {
        let p = get_prop_provider::<PrivilegeCheckerProvider>(ctx, OptPropPrivilegeChecker)?;
        Ok((p.0)())
    }
}
