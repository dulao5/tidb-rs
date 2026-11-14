//! go-file: pkg/planner/util/costusage/cost_misc.go
//!
//! `CostVer2.trace` is Go's `*CostTrace`: copies of a `CostVer2` share it and nothing writes
//! a trace after building it, so it is an `Arc`. `map[string]float64` is a `HashMap`; the
//! sums over it do not depend on the iteration order (`M-MAPORDER`).

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::strconv::format_float;

use super::optimizetrace::PlanCostOption;

// optimizetrace and costusage is isolated from `util` because `core/base` depended on them
// for interface definition. (Go's package comment.)

/// CostFlagRecalculate indicates the optimizer to ignore cached cost and recalculate it again.
pub const CostFlagRecalculate: u64 = 1;

/// CostFlagUseTrueCardinality indicates the optimizer to use true cardinality to calculate the cost.
pub const CostFlagUseTrueCardinality: u64 = 1 << 1;

/// CostFlagTrace indicates whether to trace the cost calculation.
pub const CostFlagTrace: u64 = 1 << 2;

// Go's `init` copies CostFlagTrace into `optimizetrace.CostFlagTrace`; the Rust
// `optimizetrace::CostFlagTrace` is this constant.
// go: init L2 (a package variable set at init is the constant itself)
const _: () = assert!(super::optimizetrace::CostFlagTrace == CostFlagTrace);

/// CostVer2 is a structure  of cost basic of version2
#[derive(Clone, Debug, Default)]
pub struct CostVer2 {
    cost: f64,
    trace: Option<Arc<CostTrace>>,
}

impl CostVer2 {
    /// GetCost returns the cost value of the costVer2
    // go: CostVer2.GetCost L1
    pub fn get_cost(&self) -> f64 {
        self.cost.max(0.0)
    }

    /// GetTrace returns the trace of current costVer2
    // go: CostVer2.GetTrace L1
    pub fn get_trace(&self) -> Option<&Arc<CostTrace>> {
        self.trace.as_ref()
    }
}

/// CostTrace record the basic factor and formula in cost est.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostTrace {
    /// map[factorName]cost, used to calibrate the cost model
    factor_costs: HashMap<String, f64>,
    /// It used to trace the cost calculation.
    formula: String,
}

impl CostTrace {
    /// GetFormula return the formula of current costTrace.
    // go: CostTrace.GetFormula L1
    pub fn get_formula(&self) -> &str {
        &self.formula
    }

    /// GetFactorCosts return the factors of current costTrace.
    // go: CostTrace.GetFactorCosts L1
    pub fn get_factor_costs(&self) -> &HashMap<String, f64> {
        &self.factor_costs
    }
}

/// NewZeroCostVer2 return a new zero costVer2.
// go: NewZeroCostVer2 L1
pub fn new_zero_cost_ver2(trace: bool) -> CostVer2 {
    let mut ret = CostVer2::default();
    if trace {
        ret.trace = Some(Arc::new(CostTrace::default()));
    }
    ret
}

/// HasCostFlag indicates whether the costFlag has the flag.
// go: HasCostFlag L1
pub fn has_cost_flag(cost_flag: u64, flag: u64) -> bool {
    (cost_flag & flag) > 0
}

/// TraceCost indicates whether to trace cost.
// go: TraceCost L1
pub fn trace_cost(option: Option<&PlanCostOption>) -> bool {
    if let Some(option) = option
        && has_cost_flag(option.cost_flag, CostFlagTrace)
    {
        return true;
    }
    false
}

/// NewCostVer2 is the constructor of CostVer2.
// go: NewCostVer2 L1
pub fn new_cost_ver2(
    option: Option<&PlanCostOption>,
    factor: &CostVer2Factor,
    cost: f64,
    lazy_formula: impl FnOnce() -> String,
) -> CostVer2 {
    let mut ret = CostVer2 { cost, trace: None };
    if trace_cost(option) {
        let mut trace = CostTrace::default();
        trace.factor_costs.insert(factor.name.clone(), cost);
        trace.formula = lazy_formula();
        ret.trace = Some(Arc::new(trace));
    }
    ret
}

/// CostVer2Factor is a record of internal cost factor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostVer2Factor {
    /// Name.
    pub name: String,
    /// Value.
    pub value: f64,
}

impl CostVer2Factor {
    /// String return the current CostVer2Factor's format string.
    // go: CostVer2Factor.String L1 [M-NAME]
    pub fn to_string(&self) -> Vec<u8> {
        sprintf(
            "%s(%v)",
            &[
                Arg::go_string(self.name.clone().into_bytes()),
                Arg::Float(self.value, 64),
            ],
        )
    }
}

/// SumCostVer2 sum the cost up of all the passed args.
// go: SumCostVer2 L1
pub fn sum_cost_ver2(costs: &[CostVer2]) -> CostVer2 {
    let mut ret = CostVer2::default();
    if costs.is_empty() {
        return ret;
    }
    let mut trace: Option<CostTrace> = None;
    for c in costs {
        ret.cost += c.cost;
        if let Some(ct) = &c.trace {
            let t = trace.get_or_insert_with(CostTrace::default); // init
            for (factor, factor_cost) in &ct.factor_costs {
                *t.factor_costs.entry(factor.clone()).or_insert(0.0) += factor_cost;
            }
            if !ct.formula.is_empty() {
                // this empty formula is created NewZeroCostVer2 and no update happened,
                if !t.formula.is_empty() {
                    t.formula.push_str(" + ");
                }
                t.formula.push('(');
                t.formula.push_str(&ct.formula);
                t.formula.push(')');
            }
        }
    }
    ret.trace = trace.map(Arc::new);
    ret
}

/// DivCostVer2 is div utility func of CostVer2.
// go: DivCostVer2 L1
pub fn div_cost_ver2(cost: &CostVer2, denominator: f64) -> CostVer2 {
    let mut ret = CostVer2 {
        cost: cost.cost / denominator,
        trace: None,
    };
    if let Some(ct) = &cost.trace {
        let mut t = CostTrace::default();
        for (f, c) in &ct.factor_costs {
            t.factor_costs.insert(f.clone(), c / denominator);
        }
        t.formula = format!(
            "({})/{}",
            ct.formula,
            format_float(denominator, b'f', 2, 64)
        );
        ret.trace = Some(Arc::new(t));
    }
    ret
}

/// MulCostVer2 is mul utility func of CostVer2.
// go: MulCostVer2 L1
pub fn mul_cost_ver2(cost: &CostVer2, scale: f64) -> CostVer2 {
    let mut ret = CostVer2 {
        cost: cost.cost * scale,
        trace: None,
    };
    if let Some(ct) = &cost.trace {
        let mut t = CostTrace::default();
        for (f, c) in &ct.factor_costs {
            t.factor_costs.insert(f.clone(), c * scale);
        }
        t.formula = format!("({})*{}", ct.formula, format_float(scale, b'f', 2, 64));
        ret.trace = Some(Arc::new(t));
    }
    ret
}

/// ZeroCostVer2 is a pre-defined zero CostVer2.
pub static ZeroCostVer2: LazyLock<CostVer2> = LazyLock::new(|| new_zero_cost_ver2(false));
