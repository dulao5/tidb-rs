//! go-file: pkg/expression/expropt/ddlowner.go
//!
//! Mapping: Go's function type `DDLOwnerInfoProvider` is a struct holding the closure.

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropDDLOwnerInfo, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// DDLOwnerInfoProvider is used to provide the DDL owner information.
/// It is a function to return whether the current node is ddl owner
#[derive(Clone)]
pub struct DDLOwnerInfoProvider(pub Arc<dyn Fn() -> bool + Send + Sync>);

impl OptionalEvalPropProvider for DDLOwnerInfoProvider {
    /// Desc returns the description for the property key.
    // go: DDLOwnerInfoProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropDDLOwnerInfo.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// DDLOwnerPropReader is used by expression to get the DDL owner information.
#[derive(Clone, Copy, Debug, Default)]
pub struct DDLOwnerPropReader;

impl RequireOptionalEvalProps for DDLOwnerPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: DDLOwnerPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropDDLOwnerInfo.as_prop_key_set()
    }
}

impl DDLOwnerPropReader {
    /// IsDDLOwner returns whether the current node is DDL owner
    // go: DDLOwnerPropReader.IsDDLOwner L1
    pub fn is_ddl_owner(&self, ctx: &dyn EvalContext) -> Result<bool, Error> {
        let p = get_prop_provider::<DDLOwnerInfoProvider>(ctx, OptPropDDLOwnerInfo)?;
        Ok((p.0)())
    }
}
