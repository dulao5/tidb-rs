//! go-file: pkg/expression/expropt/advisory_lock.go
//!
//! Mapping: Go's struct embedding of the `AdvisoryLockContext` interface is a struct holding
//! the shared interface value; the reader returns the provider itself (Go returns the
//! `*AdvisoryLockPropProvider`, which implements the interface through the embedding).

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropAdvisoryLock, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// AdvisoryLockContext is the interface to operate advisory locks
pub trait AdvisoryLockContext: Send + Sync {
    /// GetAdvisoryLock acquires an advisory lock (aka GET_LOCK()).
    // glue: interface method of expropt.AdvisoryLockContext
    fn get_advisory_lock(&self, name: &str, timeout: i64) -> Result<(), Error>;
    /// IsUsedAdvisoryLock checks for existing locks (aka IS_USED_LOCK()).
    // glue: interface method of expropt.AdvisoryLockContext
    fn is_used_advisory_lock(&self, name: &str) -> u64;
    /// ReleaseAdvisoryLock releases an advisory lock (aka RELEASE_LOCK()).
    // glue: interface method of expropt.AdvisoryLockContext
    fn release_advisory_lock(&self, name: &str) -> bool;
    /// ReleaseAllAdvisoryLocks releases all advisory locks that this session holds.
    // glue: interface method of expropt.AdvisoryLockContext
    fn release_all_advisory_locks(&self) -> i64;
}

/// A shared `expropt.AdvisoryLockContext` interface value.
pub type AdvisoryLockContextRef = Arc<dyn AdvisoryLockContext>;

/// AdvisoryLockPropProvider is a provider to provide AdvisoryLockContext.
#[derive(Clone)]
pub struct AdvisoryLockPropProvider {
    advisory_lock_context: AdvisoryLockContextRef,
}

/// NewAdvisoryLockPropProvider creates a new AdvisoryLockPropProvider
// go: NewAdvisoryLockPropProvider L1
pub fn new_advisory_lock_prop_provider(ctx: AdvisoryLockContextRef) -> AdvisoryLockPropProvider {
    AdvisoryLockPropProvider {
        advisory_lock_context: ctx,
    }
}

impl AdvisoryLockContext for AdvisoryLockPropProvider {
    // glue: forwarded AdvisoryLockContext.GetAdvisoryLock (Go struct embedding)
    fn get_advisory_lock(&self, name: &str, timeout: i64) -> Result<(), Error> {
        self.advisory_lock_context.get_advisory_lock(name, timeout)
    }
    // glue: forwarded AdvisoryLockContext.IsUsedAdvisoryLock (Go struct embedding)
    fn is_used_advisory_lock(&self, name: &str) -> u64 {
        self.advisory_lock_context.is_used_advisory_lock(name)
    }
    // glue: forwarded AdvisoryLockContext.ReleaseAdvisoryLock (Go struct embedding)
    fn release_advisory_lock(&self, name: &str) -> bool {
        self.advisory_lock_context.release_advisory_lock(name)
    }
    // glue: forwarded AdvisoryLockContext.ReleaseAllAdvisoryLocks (Go struct embedding)
    fn release_all_advisory_locks(&self) -> i64 {
        self.advisory_lock_context.release_all_advisory_locks()
    }
}

impl OptionalEvalPropProvider for AdvisoryLockPropProvider {
    /// Desc returns the description for the property key.
    // go: AdvisoryLockPropProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropAdvisoryLock.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// AdvisoryLockPropReader is used by expression to operate advisory lock
#[derive(Clone, Copy, Debug, Default)]
pub struct AdvisoryLockPropReader;

impl RequireOptionalEvalProps for AdvisoryLockPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: AdvisoryLockPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropAdvisoryLock.as_prop_key_set()
    }
}

impl AdvisoryLockPropReader {
    /// AdvisoryLockCtx returns the AdvisoryLockContext from the context
    // go: AdvisoryLockPropReader.AdvisoryLockCtx L1
    pub fn advisory_lock_ctx(
        &self,
        ctx: &dyn EvalContext,
    ) -> Result<AdvisoryLockContextRef, Error> {
        let p = get_prop_provider::<AdvisoryLockPropProvider>(ctx, OptPropAdvisoryLock)?;
        Ok(p)
    }
}
