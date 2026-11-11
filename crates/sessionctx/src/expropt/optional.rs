//! go-file: pkg/expression/expropt/optional.go
//!
//! Mapping: the `intest.AssertFunc` type checks of `Add` are compiled out of production Go
//! builds and are not ported (the concrete provider types come with part 2b).

use std::any::Any;
use std::sync::Arc;

use tidb_base::{Arg, Error};

use crate::exprctx::{
    EvalContext, OptPropsCnt, OptionalEvalPropKey, OptionalEvalPropKeySet,
    OptionalEvalPropProviderRef,
};

/// RequireOptionalEvalProps is the interface for the function that requires optional
/// evaluation properties or not.
pub trait RequireOptionalEvalProps {
    /// RequiredOptionalEvalProps returns the optional properties the function requires.
    // glue: interface method of expropt.RequireOptionalEvalProps
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet;
}

/// OptionalEvalPropProviders contains some evaluation property providers in EvalContext.
#[derive(Clone, Default)]
pub struct OptionalEvalPropProviders(pub [Option<OptionalEvalPropProviderRef>; OptPropsCnt]);

impl OptionalEvalPropProviders {
    /// Contains checks whether the provider by key exists.
    // go: OptionalEvalPropProviders.Contains L1
    pub fn contains(&self, key: OptionalEvalPropKey) -> bool {
        self.0[key.0 as usize].is_some()
    }

    /// Get gets the provider by key.
    // go: OptionalEvalPropProviders.Get L1
    pub fn get(&self, key: OptionalEvalPropKey) -> Option<OptionalEvalPropProviderRef> {
        if key.0 < 0 || key.0 as usize >= OptPropsCnt {
            return None;
        }
        if let Some(val) = &self.0[key.0 as usize] {
            debug_assert!(key == val.desc().key());
            return Some(Arc::clone(val));
        }
        None
    }

    /// Add adds an optional property
    // go: OptionalEvalPropProviders.Add L1
    pub fn add(&mut self, val: OptionalEvalPropProviderRef) {
        let key = val.desc().key();
        self.0[key.0 as usize] = Some(val);
    }

    /// PropKeySet returns the set for optional evaluation properties in EvalContext.
    // go: OptionalEvalPropProviders.PropKeySet L1
    pub fn prop_key_set(&self) -> OptionalEvalPropKeySet {
        let mut set = OptionalEvalPropKeySet::default();
        for p in self.0.iter().flatten() {
            set = set.add(p.desc().key());
        }
        set
    }
}

/// getPropProvider gets the provider of type `T` by key, or an error when the context does
/// not have it or it has another type.
///
/// Go's message of the second error formats the type with `%T`; `std::any::type_name` here
/// (that branch is an `intest.Assert(false)` case).
// go: getPropProvider L2 (%T type name differs)
pub fn get_prop_provider<T: Any + Send + Sync>(
    ctx: &dyn EvalContext,
    key: OptionalEvalPropKey,
) -> Result<Arc<T>, Error> {
    let Some(val) = ctx.get_optional_prop_provider(key) else {
        return Err(tidb_base::errors::errorf(
            "optional property: '%s' not exists in EvalContext",
            &[Arg::from(key.string())],
        ));
    };
    match val.as_any_arc().downcast::<T>() {
        Ok(p) => Ok(p),
        Err(_) => Err(tidb_base::errors::errorf(
            "cannot cast OptionalEvalPropProvider to %s for key '%s'",
            &[
                Arg::from(std::any::type_name::<T>()),
                Arg::from(key.string()),
            ],
        )),
    }
}
