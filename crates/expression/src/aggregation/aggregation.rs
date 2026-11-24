//! go-file: pkg/expression/aggregation/aggregation.go
//!
//! The aggregate function modes and three helpers on function names (D08.11 spec §2.1 part
//! A). The push-down checks of this file (`CheckAggPushDown`, `checkVectorAggPushDown`,
//! `CheckAggPushFlash`) need D08.3 and live in tidb-expr-pushdown (spec §4.8). The legacy
//! evaluators (`Aggregation`, `AggEvaluateContext`, `NewDistAggFunc`, `aggFunction`) have no
//! caller but the mock store and are not ported (tracker block X, spec §2.3).

use super::descriptor::AggFuncDesc;
use crate::ast_funcs as ast;

/// AggFunctionMode stands for the aggregation function's mode.
///
/// ```text
/// |-----------------|--------------|--------------|
/// | AggFunctionMode | input        | output       |
/// |-----------------|--------------|--------------|
/// | CompleteMode    | origin data  | final result |
/// | FinalMode       | partial data | final result |
/// | Partial1Mode    | origin data  | partial data |
/// | Partial2Mode    | partial data | partial data |
/// | DedupMode       | origin data  | origin data  |
/// |-----------------|--------------|--------------|
/// ```
// Go's constant names (M-GONAME).
#[allow(clippy::enum_variant_names)]
#[repr(isize)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AggFunctionMode {
    /// CompleteMode.
    #[default]
    CompleteMode = 0,
    /// FinalMode.
    FinalMode = 1,
    /// Partial1Mode.
    Partial1Mode = 2,
    /// Partial2Mode.
    Partial2Mode = 3,
    /// DedupMode.
    DedupMode = 4,
}

impl AggFunctionMode {
    /// ToString show the agg mode.
    ///
    /// Go returns "" for a value outside the five constants, which the enum cannot hold.
    // go: AggFunctionMode.ToString L1 [M-NAME]
    pub fn to_string(self) -> &'static str {
        match self {
            AggFunctionMode::CompleteMode => "complete",
            AggFunctionMode::FinalMode => "final",
            AggFunctionMode::Partial1Mode => "partial1",
            AggFunctionMode::Partial2Mode => "partial2",
            AggFunctionMode::DedupMode => "deduplicate",
        }
    }
}

/// NeedCount indicates whether the aggregate function should record count.
// go: NeedCount L1
pub fn need_count(name: &str) -> bool {
    name == ast::AggFuncCount || name == ast::AggFuncAvg
}

/// NeedValue indicates whether the aggregate function should record value.
// go: NeedValue L1
pub fn need_value(name: &str) -> bool {
    matches!(
        name,
        ast::AggFuncSum
            | ast::AggFuncAvg
            | ast::AggFuncFirstRow
            | ast::AggFuncMax
            | ast::AggFuncMin
            | ast::AggFuncGroupConcat
            | ast::AggFuncBitOr
            | ast::AggFuncBitAnd
            | ast::AggFuncBitXor
            | ast::AggFuncApproxPercentile
    )
}

/// IsAllFirstRow checks whether functions in `aggFuncs` are all FirstRow.
// go: IsAllFirstRow L1
pub fn is_all_first_row(agg_funcs: &[AggFuncDesc]) -> bool {
    for fun in agg_funcs {
        if fun.name != ast::AggFuncFirstRow {
            return false;
        }
    }
    true
}
