//! go-file: pkg/planner/core/cost/factors_thresholds.go
//!
//! The constants of the cost and estimation code that FindBestTask and the statistics
//! derivation read (block D10.2). The aggregation factors (`AggFuncFactor`) come with
//! aggregation.

/// SelectionFactor is the default factor of the selectivity.
/// For example, If we have no idea about the selectivity of a Selection, we will assume
/// its selectivity is 0.8.
#[allow(non_upper_case_globals)]
pub const SelectionFactor: f64 = 0.8;

/// ToleranceFactor is the arbitrary tolerance factor of the floating point comparisons of
/// the row counts.
#[allow(non_upper_case_globals)]
pub const ToleranceFactor: f64 = 0.00001;
