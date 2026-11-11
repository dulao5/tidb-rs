//! go-file: pkg/expression/exprctx/optional.go
//!
//! Mapping: `intest.Assert` checks are compiled out of production Go builds; the Rust code
//! follows the production behaviour (an out-of-range key is ignored), as govec records it.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::any::Any;
use std::sync::Arc;

/// OptionalEvalPropKey is the key for optional evaluation properties in EvalContext.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionalEvalPropKey(pub i64);

impl OptionalEvalPropKey {
    /// AsPropKeySet returns the set only contains the property key.
    // go: OptionalEvalPropKey.AsPropKeySet L1
    pub fn as_prop_key_set(self) -> OptionalEvalPropKeySet {
        // Go: `1 << k` on a uint64: a negative shift panics, shifts of 64 or more give 0.
        if self.0 < 0 {
            panic!("runtime error: negative shift amount");
        }
        OptionalEvalPropKeySet(1u64.checked_shl(self.0 as u32).unwrap_or(0))
    }

    /// Desc returns the description for the property key.
    // go: OptionalEvalPropKey.Desc L1
    pub fn desc(self) -> &'static OptionalEvalPropDesc {
        &optionalPropertyDescList[self.0 as usize]
    }

    /// String implements fmt.Stringer interface.
    // go: OptionalEvalPropKey.String L1
    pub fn string(self) -> String {
        if self.0 < optPropsCnt.0 {
            return self.desc().str.to_owned();
        }
        format!("UnknownOptionalEvalPropKey({})", self.0)
    }
}

impl std::fmt::Display for OptionalEvalPropKey {
    // glue: fmt.Stringer, used by `%s`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string())
    }
}

/// OptPropCurrentUser indicates the current user property
pub const OptPropCurrentUser: OptionalEvalPropKey = OptionalEvalPropKey(0);
/// OptPropSessionVars indicates the session variables
pub const OptPropSessionVars: OptionalEvalPropKey = OptionalEvalPropKey(1);
/// OptPropInfoSchema indicates the information schema
pub const OptPropInfoSchema: OptionalEvalPropKey = OptionalEvalPropKey(2);
/// OptPropKVStore indicates the kv store
pub const OptPropKVStore: OptionalEvalPropKey = OptionalEvalPropKey(3);
/// OptPropSQLExecutor indicates the sql executor
pub const OptPropSQLExecutor: OptionalEvalPropKey = OptionalEvalPropKey(4);
/// OptPropSequenceOperator indicates the sequence operator for sequence functions
pub const OptPropSequenceOperator: OptionalEvalPropKey = OptionalEvalPropKey(5);
/// OptPropAdvisoryLock indicates the advisory lock operations
pub const OptPropAdvisoryLock: OptionalEvalPropKey = OptionalEvalPropKey(6);
/// OptPropDDLOwnerInfo indicates the DDL owner info
pub const OptPropDDLOwnerInfo: OptionalEvalPropKey = OptionalEvalPropKey(7);
/// OptPropPrivilegeChecker indicates the privilege checker
pub const OptPropPrivilegeChecker: OptionalEvalPropKey = OptionalEvalPropKey(8);
/// optPropsCnt is the count of optional properties. DO NOT use it as a property key.
const optPropsCnt: OptionalEvalPropKey = OptionalEvalPropKey(9);

/// allOptPropsMask is the mask of all optional properties.
const allOptPropsMask: u64 = (1 << optPropsCnt.0) - 1;

/// OptPropsCnt is the count of optional properties.
pub const OptPropsCnt: usize = optPropsCnt.0 as usize;

/// The checks of Go's `init` (the key set is wide enough, the description list has one entry
/// per key, in key order), evaluated at compile time.
// go: init L1
const _: () = {
    assert!(
        OptPropsCnt as u64 <= 64,
        "The count optional properties should less than the bits of OptionalEvalProps"
    );
    assert!(optionalPropertyDescList.len() == OptPropsCnt);
    let mut i = 0;
    while i < OptPropsCnt {
        assert!(optionalPropertyDescList[i].key.0 == i as i64);
        i += 1;
    }
};

/// OptionalEvalPropDesc is the description for optional evaluation properties in EvalContext.
#[derive(Debug, PartialEq, Eq)]
pub struct OptionalEvalPropDesc {
    key: OptionalEvalPropKey,
    str: &'static str,
}

impl OptionalEvalPropDesc {
    /// Key returns the property key.
    // go: OptionalEvalPropDesc.Key L1
    pub fn key(&self) -> OptionalEvalPropKey {
        self.key
    }
}

/// OptionalEvalPropProvider is the interface to provide optional properties in EvalContext.
pub trait OptionalEvalPropProvider: Any + Send + Sync {
    /// Desc returns the description of the property.
    // glue: interface method of exprctx.OptionalEvalPropProvider
    fn desc(&self) -> &'static OptionalEvalPropDesc;
    /// The provider as `Any`, for Go's type assertion `val.(T)`.
    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

/// A shared `exprctx.OptionalEvalPropProvider` interface value.
pub type OptionalEvalPropProviderRef = Arc<dyn OptionalEvalPropProvider>;

/// optionalPropertyDescList contains all optional property descriptions in EvalContext.
///
/// Go spells the description of `OptPropSequenceOperator` as "OptPropDDLOwnerInfo"; it is
/// kept (`String()` output).
pub static optionalPropertyDescList: [OptionalEvalPropDesc; OptPropsCnt] = [
    OptionalEvalPropDesc {
        key: OptPropCurrentUser,
        str: "OptPropCurrentUser",
    },
    OptionalEvalPropDesc {
        key: OptPropSessionVars,
        str: "OptPropSessionVars",
    },
    OptionalEvalPropDesc {
        key: OptPropInfoSchema,
        str: "OptPropInfoSchema",
    },
    OptionalEvalPropDesc {
        key: OptPropKVStore,
        str: "OptPropKVStore",
    },
    OptionalEvalPropDesc {
        key: OptPropSQLExecutor,
        str: "OptPropSQLExecutor",
    },
    OptionalEvalPropDesc {
        key: OptPropSequenceOperator,
        str: "OptPropDDLOwnerInfo",
    },
    OptionalEvalPropDesc {
        key: OptPropAdvisoryLock,
        str: "OptPropAdvisoryLock",
    },
    OptionalEvalPropDesc {
        key: OptPropDDLOwnerInfo,
        str: "OptPropDDLOwnerInfo",
    },
    OptionalEvalPropDesc {
        key: OptPropPrivilegeChecker,
        str: "OptPropPrivilegeChecker",
    },
];

/// OptionalEvalPropKeySet is a bit map for optional evaluation properties in EvalContext to
/// indicate whether some properties are set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OptionalEvalPropKeySet(pub u64);

// Go method names (`Add`, `Remove`) kept, not the std operator traits.
#[allow(clippy::should_implement_trait)]
impl OptionalEvalPropKeySet {
    /// Add adds the property key to the set
    // go: OptionalEvalPropKeySet.Add L1
    pub fn add(self, key: OptionalEvalPropKey) -> OptionalEvalPropKeySet {
        if key >= optPropsCnt {
            return self;
        }
        OptionalEvalPropKeySet(self.0 | key.as_prop_key_set().0)
    }

    /// Remove removes the property key from the set
    // go: OptionalEvalPropKeySet.Remove L1
    pub fn remove(self, key: OptionalEvalPropKey) -> OptionalEvalPropKeySet {
        if key >= optPropsCnt {
            return self;
        }
        OptionalEvalPropKeySet(self.0 & !key.as_prop_key_set().0)
    }

    /// Contains checks whether the set contains the property
    // go: OptionalEvalPropKeySet.Contains L1
    pub fn contains(self, key: OptionalEvalPropKey) -> bool {
        if key >= optPropsCnt {
            return false;
        }
        self.0 & key.as_prop_key_set().0 != 0
    }

    /// IsEmpty checks whether the bit map is empty.
    // go: OptionalEvalPropKeySet.IsEmpty L1
    pub fn is_empty(self) -> bool {
        self.0 & allOptPropsMask == 0
    }

    /// IsFull checks whether all optional properties are contained in the bit map.
    // go: OptionalEvalPropKeySet.IsFull L1
    pub fn is_full(self) -> bool {
        self.0 & allOptPropsMask == allOptPropsMask
    }
}
