//! go-file: pkg/expression/expropt/kvstore.go
//!
//! Mapping: Go's function type `KVStorePropProvider` is a struct holding the closure;
//! `kv.Storage` is `Arc<dyn tidb_kv::kv::Storage>`.

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;
use tidb_kv::kv::Storage;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropKVStore, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// KVStorePropProvider is used to provide kv.Storage for context
#[derive(Clone)]
pub struct KVStorePropProvider(pub Arc<dyn Fn() -> Arc<dyn Storage> + Send + Sync>);

impl OptionalEvalPropProvider for KVStorePropProvider {
    /// Desc returns the description for the property key.
    // go: KVStorePropProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropKVStore.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// KVStorePropReader is used by expression to get kv.Storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct KVStorePropReader;

impl RequireOptionalEvalProps for KVStorePropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: KVStorePropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropKVStore.as_prop_key_set()
    }
}

impl KVStorePropReader {
    /// GetKVStore returns a SequenceOperator.
    // go: KVStorePropReader.GetKVStore L1
    pub fn get_kv_store(&self, ctx: &dyn EvalContext) -> Result<Arc<dyn Storage>, Error> {
        let p = get_prop_provider::<KVStorePropProvider>(ctx, OptPropKVStore)?;
        Ok((p.0)())
    }
}
