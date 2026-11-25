//! go-file: pkg/executor/aggfuncs/builder.go
//!
//! `Build` and the builders of part A (spec D12.4 §3.1). `Build` keeps Go's order of
//! branches; the names that lead to the parts B1 and B2 (GROUP_CONCAT, VAR_POP, STDDEV_POP,
//! JSON_ARRAYAGG, JSON_OBJECTAGG, APPROX_COUNT_DISTINCT, APPROX_PERCENTILE, VAR_SAMP,
//! STDDEV_SAMP), and the DISTINCT branch of `buildCount` (`countOriginalWithDistinct*`,
//! part B1), return a temporary `blocked` until those parts are ported (spec §4, §9 item
//! 3): the router then falls back to the prototype. `BuildWindowFunctions` comes with the
//! window functions (part W).
//!
//! Go's `Build` returns nil for an unknown name, the dedup mode or an unmatched type; Rust
//! returns `Ok(None)` (spec §5.1), and `Err` only for the temporary stub.

// Go identifiers for the evaluation type constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::mysql;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode};
use tidb_expression::ast_funcs as ast;
use tidb_expression::context::EvalContext;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType,
};
use tidb_types::collate::get_collator;

use super::aggfuncs::{AggFunc, BaseAggFunc};
use super::func_avg::{
    AvgOriginal4Decimal, AvgOriginal4DistinctDecimal, AvgOriginal4DistinctFloat64,
    AvgOriginal4Float64, AvgOriginal4Float64HighPrecision, AvgPartial4Decimal, AvgPartial4Float64,
    BaseAvgDecimal, BaseAvgFloat64,
};
use super::func_bitfuncs::{BaseBitAggFunc, BitAndUint64, BitOrUint64, BitXorUint64};
use super::func_count::{
    BaseCount, CountOriginal4Decimal, CountOriginal4Duration, CountOriginal4Int,
    CountOriginal4JSON, CountOriginal4Real, CountOriginal4String, CountOriginal4Time,
    CountOriginal4VectorFloat32, CountPartial,
};
use super::func_first_row::{
    FirstRow4Decimal, FirstRow4Duration, FirstRow4Enum, FirstRow4Float32, FirstRow4Float64,
    FirstRow4Int, FirstRow4JSON, FirstRow4Set, FirstRow4String, FirstRow4Time,
    FirstRow4VectorFloat32,
};
use super::func_json_arrayagg::JsonArrayagg;
use super::func_json_objectagg::JsonObjectAgg;
use super::func_max_min::{
    BaseMaxMinAggFunc, MaxMin4Decimal, MaxMin4Duration, MaxMin4Enum, MaxMin4Float32,
    MaxMin4Float64, MaxMin4Int, MaxMin4JSON, MaxMin4Set, MaxMin4String, MaxMin4Time, MaxMin4Uint,
    MaxMin4VectorFloat32,
};
use super::func_stddevpop::{StdDevPop4DistinctFloat64, StdDevPop4Float64};
use super::func_stddevsamp::{StddevSamp4DistinctFloat64, StddevSamp4Float64};
use super::func_sum::{
    BaseSum4Float64, Sum4Decimal, Sum4DistinctDecimal, Sum4DistinctFloat64, Sum4Float64,
    Sum4Float64HighPrecision,
};
use super::func_varpop::{BaseVarPopAggFunc, VarPop4DistinctFloat64, VarPop4Float64};
use super::func_varsamp::{VarSamp4DistinctFloat64, VarSamp4Float64};

/// AggFuncBuildContext is used to build aggregation functions.
pub use tidb_expression::context::AggFuncBuildContext;

type Built = Result<Option<Box<dyn AggFunc>>, Error>;

// glue: a built function (Go returns the pointer as an AggFunc)
fn some(f: impl AggFunc + 'static) -> Built {
    Ok(Some(Box::new(f)))
}

/// Go's `aggFuncDesc.RetTp`: `None` when Go's would be nil.
// glue: Go's nil *FieldType of the descriptor (D08.11 spec §4.1)
fn ret_tp(desc: &AggFuncDesc) -> Option<Arc<FieldType>> {
    (!desc.base.ret_tp_is_nil()).then(|| Arc::clone(&desc.base.ret_tp))
}

/// `aggFuncDesc.RetTp` dereferenced, as Go's `RetTp.EvalType()` / `RetTp.GetType()` do.
///
/// # Panics
///
/// With Go's nil-pointer panic when the descriptor's type is nil, which the type inference
/// of every aggregate function name of part A excludes.
// glue: Go dereferences RetTp (M-PANIC: nil pointer dereference)
fn deref_ret_tp(desc: &AggFuncDesc) -> &FieldType {
    assert!(
        !desc.base.ret_tp_is_nil(),
        "runtime error: invalid memory address or nil pointer dereference"
    );
    &desc.base.ret_tp
}

/// The builders of the parts B1 and B2 (and `countOriginalWithDistinct*` of
/// `buildCount`), not ported yet (spec §4, §9 item 3).
// glue: stub of buildGroupConcat, blocked-by:D12.4 (the aggregate functions of the parts B1
// and B2 are not ported yet: GROUP_CONCAT, VAR_POP / VAR_SAMP, STDDEV_POP / STDDEV_SAMP,
// JSON_ARRAYAGG, JSON_OBJECTAGG, APPROX_COUNT_DISTINCT, APPROX_PERCENTILE and COUNT(DISTINCT);
// the router falls back to the prototype)
fn build_not_ported(what: &str) -> Built {
    let what = format!("aggregate function {what} (implemented in a later part of the block)");
    Err(blocked("D12.4", &what))
}

/// Build is used to build a specific AggFunc implementation according to the input
/// aggFuncDesc.
// go: Build L2 (nil → Ok(None); the names of the parts B1 and B2 return blocked, spec §4)
pub fn build(ctx: &dyn AggFuncBuildContext, agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    match agg_func_desc.base.name.as_str() {
        ast::AggFuncCount => build_count(&*ctx.get_eval_ctx(), agg_func_desc, ordinal),
        ast::AggFuncSum => build_sum(ctx, agg_func_desc, ordinal),
        ast::AggFuncAvg => build_avg(ctx, agg_func_desc, ordinal),
        ast::AggFuncFirstRow => build_first_row(agg_func_desc, ordinal),
        ast::AggFuncMax => build_max_min(agg_func_desc, ordinal, true),
        ast::AggFuncMin => build_max_min(agg_func_desc, ordinal, false),
        ast::AggFuncGroupConcat => build_not_ported(ast::AggFuncGroupConcat),
        ast::AggFuncBitOr => build_bit_or(agg_func_desc, ordinal),
        ast::AggFuncBitXor => build_bit_xor(agg_func_desc, ordinal),
        ast::AggFuncBitAnd => build_bit_and(agg_func_desc, ordinal),
        ast::AggFuncVarPop => build_var_pop(agg_func_desc, ordinal),
        ast::AggFuncStddevPop => build_std_dev_pop(agg_func_desc, ordinal),
        ast::AggFuncJsonArrayagg => build_json_arrayagg(agg_func_desc, ordinal),
        ast::AggFuncJsonObjectAgg => build_json_object_agg(agg_func_desc, ordinal),
        ast::AggFuncApproxCountDistinct => build_not_ported(ast::AggFuncApproxCountDistinct),
        ast::AggFuncApproxPercentile => build_not_ported(ast::AggFuncApproxPercentile),
        ast::AggFuncVarSamp => build_var_samp(agg_func_desc, ordinal),
        ast::AggFuncStddevSamp => build_stddev_samp(agg_func_desc, ordinal),
        _ => Ok(None),
    }
}

// go: buildCount L2 (the DISTINCT branch, part B1, returns blocked: spec §4)
fn build_count(ctx: &dyn EvalContext, agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    // If mode is DedupMode, we return nil for not implemented.
    if agg_func_desc.mode == AggFunctionMode::DedupMode {
        return Ok(None); // not implemented yet.
    }

    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: ret_tp(agg_func_desc),
    };

    // If HasDistinct and mode is CompleteMode or Partial1Mode, we should
    // use countOriginalWithDistinct.
    if agg_func_desc.has_distinct
        && (agg_func_desc.mode == AggFunctionMode::CompleteMode
            || agg_func_desc.mode == AggFunctionMode::Partial1Mode)
    {
        // countOriginalWithDistinct4Int / 4Real / 4Decimal / 4Duration / 4String for one
        // argument of those evaluation types, countOriginalWithDistinct otherwise.
        return build_not_ported("count(distinct)");
    }

    let base = BaseCount { base };
    match agg_func_desc.mode {
        AggFunctionMode::CompleteMode | AggFunctionMode::Partial1Mode => {
            match agg_func_desc.base.args[0].get_type(ctx).eval_type() {
                ETInt => return some(CountOriginal4Int { base }),
                ETReal => return some(CountOriginal4Real { base }),
                ETDecimal => return some(CountOriginal4Decimal { base }),
                ETTimestamp | ETDatetime => return some(CountOriginal4Time { base }),
                ETDuration => return some(CountOriginal4Duration { base }),
                ETJson => return some(CountOriginal4JSON { base }),
                ETVectorFloat32 => return some(CountOriginal4VectorFloat32 { base }),
                ETString => return some(CountOriginal4String { base }),
                _ => {}
            }
        }
        AggFunctionMode::Partial2Mode | AggFunctionMode::FinalMode => {
            return some(CountPartial { base });
        }
        AggFunctionMode::DedupMode => {}
    }

    Ok(None)
}

// go: buildSum L1
fn build_sum(ctx: &dyn AggFuncBuildContext, agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: ret_tp(agg_func_desc),
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => match deref_ret_tp(agg_func_desc).eval_type() {
            ETDecimal => {
                if agg_func_desc.has_distinct {
                    return some(Sum4DistinctDecimal::new(base));
                }
                some(Sum4Decimal { base })
            }
            _ => {
                if agg_func_desc.has_distinct {
                    return some(Sum4DistinctFloat64::new(base));
                }
                if ctx.get_windowing_use_high_precision() {
                    return some(Sum4Float64HighPrecision {
                        base: BaseSum4Float64 { base },
                    });
                }
                some(Sum4Float64 {
                    base: BaseSum4Float64 { base },
                })
            }
        },
    }
}

// go: buildAvg L1
fn build_avg(ctx: &dyn AggFuncBuildContext, agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: ret_tp(agg_func_desc),
    };
    match agg_func_desc.mode {
        // Build avg functions which consume the original data and remove the
        // duplicated input of the same group.
        AggFunctionMode::DedupMode => return Ok(None), // not implemented yet.

        // Build avg functions which consume the original data and update their
        // partial results.
        AggFunctionMode::CompleteMode | AggFunctionMode::Partial1Mode => {
            match deref_ret_tp(agg_func_desc).eval_type() {
                ETDecimal => {
                    if agg_func_desc.has_distinct {
                        return some(AvgOriginal4DistinctDecimal::new(base));
                    }
                    return some(AvgOriginal4Decimal {
                        base: BaseAvgDecimal { base },
                    });
                }
                _ => {
                    if agg_func_desc.has_distinct {
                        return some(AvgOriginal4DistinctFloat64::new(base));
                    }
                    if ctx.get_windowing_use_high_precision() {
                        return some(AvgOriginal4Float64HighPrecision {
                            base: BaseAvgFloat64 { base },
                        });
                    }
                    return some(AvgOriginal4Float64 {
                        base: AvgOriginal4Float64HighPrecision {
                            base: BaseAvgFloat64 { base },
                        },
                    });
                }
            }
        }

        // Build avg functions which consume the partial result of other avg
        // functions and update their partial results.
        AggFunctionMode::Partial2Mode | AggFunctionMode::FinalMode => {
            match deref_ret_tp(agg_func_desc).get_type() {
                mysql::TypeNewDecimal => {
                    return some(AvgPartial4Decimal {
                        base: BaseAvgDecimal { base },
                    });
                }
                mysql::TypeDouble => {
                    return some(AvgPartial4Float64 {
                        base: BaseAvgFloat64 { base },
                    });
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

// go: buildFirstRow L1
fn build_first_row(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: ret_tp(agg_func_desc),
    };
    let field_type = deref_ret_tp(agg_func_desc);
    let mut eval_type = field_type.eval_type();
    if field_type.get_type() == mysql::TypeBit {
        eval_type = ETString;
    }
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => {}
        _ => {
            match field_type.get_type() {
                mysql::TypeEnum => return some(FirstRow4Enum { base }),
                mysql::TypeSet => return some(FirstRow4Set { base }),
                _ => {}
            }

            match eval_type {
                ETInt => return some(FirstRow4Int { base }),
                ETReal => match field_type.get_type() {
                    mysql::TypeFloat => return some(FirstRow4Float32 { base }),
                    mysql::TypeDouble => return some(FirstRow4Float64 { base }),
                    _ => {}
                },
                ETDecimal => return some(FirstRow4Decimal { base }),
                ETDatetime | ETTimestamp => return some(FirstRow4Time { base }),
                ETDuration => return some(FirstRow4Duration { base }),
                ETString => return some(FirstRow4String { base }),
                ETJson => return some(FirstRow4JSON { base }),
                ETVectorFloat32 => return some(FirstRow4VectorFloat32 { base }),
                _ => {}
            }
        }
    }
    Ok(None)
}

// go: buildMaxMin L1
fn build_max_min(agg_func_desc: &AggFuncDesc, ordinal: isize, is_max: bool) -> Built {
    let field_type = deref_ret_tp(agg_func_desc);
    let base = BaseMaxMinAggFunc {
        base: BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: ret_tp(agg_func_desc),
        },
        is_max,
        collator: get_collator(field_type.get_collate()),
    };
    let mut eval_type = field_type.eval_type();
    if field_type.get_type() == mysql::TypeBit {
        eval_type = ETString;
    }
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => {}
        _ => {
            match field_type.get_type() {
                mysql::TypeEnum => return some(MaxMin4Enum { base }),
                mysql::TypeSet => return some(MaxMin4Set { base }),
                _ => {}
            }

            match eval_type {
                ETInt => {
                    if mysql::has_unsigned_flag(field_type.get_flag()) {
                        return some(MaxMin4Uint { base });
                    }
                    return some(MaxMin4Int { base });
                }
                ETReal => match field_type.get_type() {
                    mysql::TypeFloat => return some(MaxMin4Float32 { base }),
                    mysql::TypeDouble => return some(MaxMin4Float64 { base }),
                    _ => {}
                },
                ETDecimal => return some(MaxMin4Decimal { base }),
                ETString => {
                    return some(MaxMin4String {
                        base,
                        ret_tp: ret_tp(agg_func_desc),
                    });
                }
                ETDatetime | ETTimestamp => return some(MaxMin4Time { base }),
                ETDuration => return some(MaxMin4Duration { base }),
                ETJson => return some(MaxMin4JSON { base }),
                ETVectorFloat32 => return some(MaxMin4VectorFloat32 { base }),
                _ => {}
            }
        }
    }
    Ok(None)
}

// go: buildBitOr L1
fn build_bit_or(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    };
    some(BitOrUint64 {
        base: BaseBitAggFunc { base },
    })
}

// go: buildBitXor L1
fn build_bit_xor(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    };
    some(BitXorUint64 {
        base: BaseBitAggFunc { base },
    })
}

// go: buildBitAnd L1
fn build_bit_and(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    };
    some(BitAndUint64 {
        base: BaseBitAggFunc { base },
    })
}

// go: buildVarPop L1
fn build_var_pop(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseVarPopAggFunc {
        base: BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: None,
        },
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => {
            if agg_func_desc.has_distinct {
                return some(VarPop4DistinctFloat64::new(base));
            }
            some(VarPop4Float64 { base })
        }
    }
}

// go: buildStdDevPop L1
fn build_std_dev_pop(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseVarPopAggFunc {
        base: BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: None,
        },
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => {
            if agg_func_desc.has_distinct {
                return some(StdDevPop4DistinctFloat64 {
                    base: VarPop4DistinctFloat64::new(base),
                });
            }
            some(StdDevPop4Float64 {
                base: VarPop4Float64 { base },
            })
        }
    }
}

// go: buildVarSamp L1
fn build_var_samp(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseVarPopAggFunc {
        base: BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: None,
        },
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => {
            if agg_func_desc.has_distinct {
                return some(VarSamp4DistinctFloat64 {
                    base: VarPop4DistinctFloat64::new(base),
                });
            }
            some(VarSamp4Float64 {
                base: VarPop4Float64 { base },
            })
        }
    }
}

// go: buildStddevSamp L1
fn build_stddev_samp(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseVarPopAggFunc {
        base: BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: None,
        },
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => {
            if agg_func_desc.has_distinct {
                return some(StddevSamp4DistinctFloat64 {
                    base: VarPop4DistinctFloat64::new(base),
                });
            }
            some(StddevSamp4Float64 {
                base: VarPop4Float64 { base },
            })
        }
    }
}

// go: buildJSONArrayagg L1
fn build_json_arrayagg(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => some(JsonArrayagg::new(base)),
    }
}

// go: buildJSONObjectAgg L1
fn build_json_object_agg(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    };
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => some(JsonObjectAgg::new(base)),
    }
}
