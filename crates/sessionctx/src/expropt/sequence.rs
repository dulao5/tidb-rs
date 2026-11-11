//! go-file: pkg/expression/expropt/sequence.go
//!
//! Mapping: Go's function type `SequenceOperatorProvider` is a struct holding the closure.

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropSequenceOperator, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// SequenceOperator is the interface for to operate sequence.
pub trait SequenceOperator: Send + Sync {
    /// GetSequenceID gets the sequence id.
    // glue: interface method of expropt.SequenceOperator
    fn get_sequence_id(&self) -> i64;
    /// GetSequenceNextVal gets the next value of the sequence.
    // glue: interface method of expropt.SequenceOperator
    fn get_sequence_next_val(&self) -> Result<i64, Error>;
    /// SetSequenceVal sets the sequence value.
    /// The returned bool indicates the newVal is already under the base.
    // glue: interface method of expropt.SequenceOperator
    fn set_sequence_val(&self, new_val: i64) -> Result<(i64, bool), Error>;
}

/// A shared `expropt.SequenceOperator` interface value.
pub type SequenceOperatorRef = Arc<dyn SequenceOperator>;

/// The function a `SequenceOperatorProvider` calls (`db`, `name`).
pub type SequenceOperatorFn =
    dyn Fn(&str, &str) -> Result<SequenceOperatorRef, Error> + Send + Sync;

/// SequenceOperatorProvider is the function to provide SequenceOperator.
#[derive(Clone)]
pub struct SequenceOperatorProvider(pub Arc<SequenceOperatorFn>);

impl OptionalEvalPropProvider for SequenceOperatorProvider {
    /// Desc returns the description for the property key.
    // go: SequenceOperatorProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropSequenceOperator.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// SequenceOperatorPropReader is used by expression to get SequenceOperator.
#[derive(Clone, Copy, Debug, Default)]
pub struct SequenceOperatorPropReader;

impl RequireOptionalEvalProps for SequenceOperatorPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: SequenceOperatorPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropSequenceOperator.as_prop_key_set()
    }
}

impl SequenceOperatorPropReader {
    /// GetSequenceOperator returns a SequenceOperator.
    // go: SequenceOperatorPropReader.GetSequenceOperator L1
    pub fn get_sequence_operator(
        &self,
        ctx: &dyn EvalContext,
        db: &str,
        name: &str,
    ) -> Result<SequenceOperatorRef, Error> {
        let p = get_prop_provider::<SequenceOperatorProvider>(ctx, OptPropSequenceOperator)?;
        (p.0)(db, name)
    }
}
