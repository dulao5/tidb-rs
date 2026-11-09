//! go-file: pkg/expression/expropt/sqlexec.go
//!
//! Mapping: Go's function type `SQLExecutorPropProvider` is a struct holding the closure.
//! `SQLExecutor.ExecRestrictedSQL` returns `[]chunk.Row` and `[]*resolve.ResultField`; the
//! restricted SQL executor (`util/sqlexec`, D00.3) and `resolve.ResultField` (P3) are not ported,
//! so the interface has no methods yet (its one method comes with the executor).

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropSQLExecutor, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// SQLExecutor is the interface for SQL executing in expression.
/// We do not `sqlexec.SQLExecutor` to limit expression to use specified methods only.
// glue: stand-in: D00.3, expropt.SQLExecutor (ExecRestrictedSQL comes with util/sqlexec)
pub trait SQLExecutor: Send + Sync {}

/// A shared `expropt.SQLExecutor` interface value.
pub type SQLExecutorRef = Arc<dyn SQLExecutor>;

/// SQLExecutorPropProvider provides the SQLExecutor
#[derive(Clone)]
pub struct SQLExecutorPropProvider(
    pub Arc<dyn Fn() -> Result<SQLExecutorRef, Error> + Send + Sync>,
);

impl OptionalEvalPropProvider for SQLExecutorPropProvider {
    /// Desc returns the description for the property key.
    // go: SQLExecutorPropProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropSQLExecutor.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// SQLExecutorPropReader is used by expression to get sql executor
#[derive(Clone, Copy, Debug, Default)]
pub struct SQLExecutorPropReader;

impl RequireOptionalEvalProps for SQLExecutorPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: SQLExecutorPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropSQLExecutor.as_prop_key_set()
    }
}

impl SQLExecutorPropReader {
    /// GetSQLExecutor returns a SQLExecutor.
    // go: SQLExecutorPropReader.GetSQLExecutor L1
    pub fn get_sql_executor(&self, ctx: &dyn EvalContext) -> Result<SQLExecutorRef, Error> {
        let p = get_prop_provider::<SQLExecutorPropProvider>(ctx, OptPropSQLExecutor)?;
        (p.0)()
    }
}
