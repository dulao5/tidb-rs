//! go-file: pkg/sessionctx/variable/session.go
//!
//! `variable.PlanCacheParamList`: the parameter values of a prepared (or non-prepared plan
//! cache) statement (block D05.2).

use tidb_types::datum::{Datum, datums_to_str_no_err_smart};

/// PlanCacheParamList stores the parameters for plan cache.
/// Use attached methods to access or modify parameter values instead of accessing them directly.
#[derive(Clone, Debug, Default)]
pub struct PlanCacheParamList {
    param_values: Vec<Datum>,
    for_non_prep_cache: bool,
}

/// NewPlanCacheParamList creates a new PlanCacheParams.
// go: NewPlanCacheParamList L1
pub fn new_plan_cache_param_list() -> PlanCacheParamList {
    let mut p = PlanCacheParamList {
        param_values: Vec::with_capacity(8),
        for_non_prep_cache: false,
    };
    p.reset();
    p
}

impl PlanCacheParamList {
    /// Reset resets the PlanCacheParams.
    // go: PlanCacheParamList.Reset L1
    pub fn reset(&mut self) {
        self.param_values.clear();
        self.for_non_prep_cache = false;
    }

    /// String implements the fmt.Stringer interface.
    // Note: Go string bytes; `None` for a nil receiver is the caller's `Option`.
    // go: PlanCacheParamList.String L1
    pub fn string(&self) -> Vec<u8> {
        if self.param_values.is_empty() || self.for_non_prep_cache {
            // hide non-prep parameter values by default
            return Vec::new();
        }
        let mut s = b" [arguments: ".to_vec();
        s.extend_from_slice(&datums_to_str_no_err_smart(&self.param_values));
        s.push(b']');
        s
    }

    /// Append appends a parameter value to the PlanCacheParams.
    // go: PlanCacheParamList.Append L1
    pub fn append(&mut self, vs: &[Datum]) {
        self.param_values.extend_from_slice(vs);
    }

    /// SetForNonPrepCache sets the flag forNonPrepCache.
    // go: PlanCacheParamList.SetForNonPrepCache L1
    pub fn set_for_non_prep_cache(&mut self, flag: bool) {
        self.for_non_prep_cache = flag;
    }

    /// GetParamValue returns the value of the parameter at the specified index.
    /// Panics when `idx` is out of range, as Go's index expression does.
    // go: PlanCacheParamList.GetParamValue L1
    pub fn get_param_value(&self, idx: i64) -> Datum {
        match usize::try_from(idx)
            .ok()
            .and_then(|i| self.param_values.get(i))
        {
            Some(d) => d.clone(),
            None if idx < 0 => panic!("runtime error: index out of range [{idx}]"),
            None => panic!(
                "runtime error: index out of range [{idx}] with length {}",
                self.param_values.len()
            ),
        }
    }

    /// AllParamValues returns all parameter values.
    // go: PlanCacheParamList.AllParamValues L1
    pub fn all_param_values(&self) -> &[Datum] {
        &self.param_values
    }

    /// Whether the values are hidden from `String` (`forNonPrepCache`; Go's `LazyStmtText.Update`
    /// reads the field inside the package).
    // glue: read access to PlanCacheParamList.forNonPrepCache
    pub fn for_non_prep_cache(&self) -> bool {
        self.for_non_prep_cache
    }
}
