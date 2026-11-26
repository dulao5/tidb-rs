//! go-file: pkg/executor/aggfuncs/builder.go
//!
//! `Build` and the builders of parts A, B1, and B2 (spec D12.4 §3.1, §3.2, §3.3). `Build`
//! keeps Go's exact order of branches. `BuildWindowFunctions` comes with the window
//! functions (part W).
//!
//! Go's `Build` returns nil for an unknown name, the dedup mode or an unmatched type; Rust
//! returns `Ok(None)` (spec §5.1).

// Go identifiers for the evaluation type constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::sync::atomic::AtomicI32;

use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::row::Row;
use tidb_expression::Expr;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode};
use tidb_expression::ast_funcs as ast;
use tidb_expression::context::EvalContext;
use tidb_expression::expression::EvalState;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType, FieldType,
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
use super::func_count_distinct::{
    ApproxCountDistinctFinal, ApproxCountDistinctOriginal, ApproxCountDistinctPartial1,
    ApproxCountDistinctPartial2, BaseApproxCountDistinct, CountOriginalWithDistinct,
    CountOriginalWithDistinct4Decimal, CountOriginalWithDistinct4Duration,
    CountOriginalWithDistinct4Int, CountOriginalWithDistinct4Real,
    CountOriginalWithDistinct4String,
};
use super::func_first_row::{
    FirstRow4Decimal, FirstRow4Duration, FirstRow4Enum, FirstRow4Float32, FirstRow4Float64,
    FirstRow4Int, FirstRow4JSON, FirstRow4Set, FirstRow4String, FirstRow4Time,
    FirstRow4VectorFloat32,
};
use super::func_group_concat::{
    BaseGroupConcat4String, GroupConcat, GroupConcatDistinct, GroupConcatDistinctOrder,
    GroupConcatOrder,
};
use super::func_json_arrayagg::JsonArrayagg;
use super::func_json_objectagg::JsonObjectAgg;
use super::func_max_min::{
    BaseMaxMinAggFunc, MaxMin4Decimal, MaxMin4Duration, MaxMin4Enum, MaxMin4Float32,
    MaxMin4Float64, MaxMin4Int, MaxMin4JSON, MaxMin4Set, MaxMin4String, MaxMin4Time, MaxMin4Uint,
    MaxMin4VectorFloat32,
};
use super::func_percentile::{
    BasePercentile, PercentileOriginal4Decimal, PercentileOriginal4Duration,
    PercentileOriginal4Int, PercentileOriginal4Real, PercentileOriginal4Time,
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

/// Build is used to build a specific AggFunc implementation according to the input
/// aggFuncDesc.
// go: Build L2 (nil → Ok(None), spec §5.1)
pub fn build(ctx: &dyn AggFuncBuildContext, agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    match agg_func_desc.base.name.as_str() {
        ast::AggFuncCount => build_count(&*ctx.get_eval_ctx(), agg_func_desc, ordinal),
        ast::AggFuncSum => build_sum(ctx, agg_func_desc, ordinal),
        ast::AggFuncAvg => build_avg(ctx, agg_func_desc, ordinal),
        ast::AggFuncFirstRow => build_first_row(agg_func_desc, ordinal),
        ast::AggFuncMax => build_max_min(agg_func_desc, ordinal, true),
        ast::AggFuncMin => build_max_min(agg_func_desc, ordinal, false),
        ast::AggFuncGroupConcat => build_group_concat(ctx, agg_func_desc, ordinal),
        ast::AggFuncBitOr => build_bit_or(agg_func_desc, ordinal),
        ast::AggFuncBitXor => build_bit_xor(agg_func_desc, ordinal),
        ast::AggFuncBitAnd => build_bit_and(agg_func_desc, ordinal),
        ast::AggFuncVarPop => build_var_pop(agg_func_desc, ordinal),
        ast::AggFuncStddevPop => build_std_dev_pop(agg_func_desc, ordinal),
        ast::AggFuncJsonArrayagg => build_json_arrayagg(agg_func_desc, ordinal),
        ast::AggFuncJsonObjectAgg => build_json_object_agg(agg_func_desc, ordinal),
        ast::AggFuncApproxCountDistinct => build_approx_count_distinct(agg_func_desc, ordinal),
        ast::AggFuncApproxPercentile => build_approx_percentile(ctx, agg_func_desc, ordinal),
        ast::AggFuncVarSamp => build_var_samp(agg_func_desc, ordinal),
        ast::AggFuncStddevSamp => build_stddev_samp(agg_func_desc, ordinal),
        _ => Ok(None),
    }
}

// go: buildApproxCountDistinct L1
fn build_approx_count_distinct(agg_func_desc: &AggFuncDesc, ordinal: isize) -> Built {
    let base = BaseApproxCountDistinct::new(BaseAggFunc {
        args: agg_func_desc.base.args.clone(),
        ordinal,
        ret_tp: None,
    });

    // In partition table, union need to compute partial result into partial result.
    // We can detect and handle this case by checking whether return type is string.

    match deref_ret_tp(agg_func_desc).get_type() {
        mysql::TypeLonglong => match agg_func_desc.mode {
            AggFunctionMode::CompleteMode => return some(ApproxCountDistinctOriginal::new(base)),
            AggFunctionMode::Partial1Mode => {
                return some(ApproxCountDistinctPartial1::new(
                    ApproxCountDistinctOriginal::new(base),
                ));
            }
            AggFunctionMode::Partial2Mode => {
                return some(ApproxCountDistinctPartial2::new(
                    ApproxCountDistinctPartial1::new(ApproxCountDistinctOriginal::new(base)),
                ));
            }
            AggFunctionMode::FinalMode => {
                return some(ApproxCountDistinctFinal::new(
                    ApproxCountDistinctPartial2::new(ApproxCountDistinctPartial1::new(
                        ApproxCountDistinctOriginal::new(base),
                    )),
                ));
            }
            AggFunctionMode::DedupMode => {}
        },
        mysql::TypeString => match agg_func_desc.mode {
            AggFunctionMode::CompleteMode | AggFunctionMode::Partial1Mode => {
                return some(ApproxCountDistinctPartial1::new(
                    ApproxCountDistinctOriginal::new(base),
                ));
            }
            AggFunctionMode::Partial2Mode | AggFunctionMode::FinalMode => {
                return some(ApproxCountDistinctPartial2::new(
                    ApproxCountDistinctPartial1::new(ApproxCountDistinctOriginal::new(base)),
                ));
            }
            AggFunctionMode::DedupMode => {}
        },
        _ => {}
    }

    Ok(None)
}

// go: getEvalTypeForApproxPercentile L1
fn get_eval_type_for_approx_percentile(
    ctx: &dyn EvalContext,
    agg_func_desc: &AggFuncDesc,
) -> EvalType {
    let mut eval_type = agg_func_desc.base.args[0].get_type(ctx).eval_type();
    let arg_type = agg_func_desc.base.args[0].get_type(ctx).get_type();

    // Sometimes `mysql.EnumSetAsIntFlag` may be set to true, such as when join,
    // which is unexpected for `buildApproxPercentile` and `mysql.TypeEnum` and `mysql.TypeSet` will return unexpected `ETInt` here,
    // so here `evalType` are forcibly set to `ETString`.
    // For mysql.TypeBit, just same as other aggregate function.
    if arg_type == mysql::TypeEnum || arg_type == mysql::TypeSet || arg_type == mysql::TypeBit {
        eval_type = ETString;
    }

    eval_type
}

// go: buildApproxPercentile L1
fn build_approx_percentile(
    sctx: &dyn AggFuncBuildContext,
    agg_func_desc: &AggFuncDesc,
    ordinal: isize,
) -> Built {
    if agg_func_desc.mode == AggFunctionMode::DedupMode {
        return Ok(None);
    }

    // Checked while building descriptor
    let eval_ctx = sctx.get_eval_ctx();
    let percent =
        match agg_func_desc.base.args[1].eval_int(&*eval_ctx, &EvalState::new(), Row::default()) {
            Ok(v) => v.unwrap_or(0) as isize,
            Err(err) => {
                // Should not reach here
                log::error!("Error happened when buildApproxPercentile: {err}");
                return Ok(None);
            }
        };

    let base = BasePercentile::new(
        percent,
        BaseAggFunc {
            args: agg_func_desc.base.args.clone(),
            ordinal,
            ret_tp: None,
        },
    );

    let eval_type = get_eval_type_for_approx_percentile(&*eval_ctx, agg_func_desc);
    match agg_func_desc.mode {
        AggFunctionMode::CompleteMode
        | AggFunctionMode::Partial1Mode
        | AggFunctionMode::FinalMode => match eval_type {
            ETInt => return some(PercentileOriginal4Int::new(base)),
            ETReal => return some(PercentileOriginal4Real::new(base)),
            ETDecimal => return some(PercentileOriginal4Decimal::new(base)),
            ETDatetime | ETTimestamp => return some(PercentileOriginal4Time::new(base)),
            ETDuration => return some(PercentileOriginal4Duration::new(base)),
            _ => {
                // Return NULL in any case
                return some(base);
            }
        },
        AggFunctionMode::Partial2Mode | AggFunctionMode::DedupMode => {}
    }

    Ok(None)
}

// go: buildCount L1
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
        let base_count = BaseCount { base };
        if base_count.base.args.len() == 1 {
            // optimize with single column
            match agg_func_desc.base.args[0].get_type(ctx).eval_type() {
                ETInt => return some(CountOriginalWithDistinct4Int::new(base_count)),
                ETReal => return some(CountOriginalWithDistinct4Real::new(base_count)),
                ETDecimal => return some(CountOriginalWithDistinct4Decimal::new(base_count)),
                ETDuration => return some(CountOriginalWithDistinct4Duration::new(base_count)),
                ETString => return some(CountOriginalWithDistinct4String::new(base_count)),
                _ => {}
            }
        }
        return some(CountOriginalWithDistinct::new(base_count));
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

// go: buildGroupConcat L1
fn build_group_concat(
    ctx: &dyn AggFuncBuildContext,
    agg_func_desc: &AggFuncDesc,
    ordinal: isize,
) -> Built {
    match agg_func_desc.mode {
        AggFunctionMode::DedupMode => Ok(None),
        _ => {
            // The last arg is promised to be a not-null string constant, so the error can be ignored.
            let last = &agg_func_desc.base.args[agg_func_desc.base.args.len() - 1];
            assert!(
                matches!(last.as_ref(), Expr::Constant(_)),
                "runtime error: invalid memory address or nil pointer dereference"
            );
            let eval_ctx = ctx.get_eval_ctx();
            let sep = match last.eval_string(&*eval_ctx, &EvalState::new(), Row::default()) {
                Ok(sep) => sep.unwrap_or_default(),
                Err(err) => panic!("Error happened when buildGroupConcat: {err}"),
            };
            let max_len = ctx.get_group_concat_max_len();
            let base = BaseGroupConcat4String {
                base: BaseAggFunc {
                    args: agg_func_desc.base.args[..agg_func_desc.base.args.len() - 1].to_vec(),
                    ordinal,
                    ret_tp: None,
                },
                by_items: agg_func_desc
                    .order_by_items
                    .iter()
                    .map(|b| b.clone_item())
                    .collect(),
                sep,
                max_len,
                truncated: Arc::new(AtomicI32::new(0)),
            };
            if agg_func_desc.has_distinct {
                if !agg_func_desc.order_by_items.is_empty() {
                    let mut desc = Vec::with_capacity(base.by_items.len());
                    let mut ctors = Vec::with_capacity(base.by_items.len());
                    for by_item in &base.by_items {
                        desc.push(by_item.desc);
                        ctors.push(get_collator(
                            by_item.expr.get_type(&*eval_ctx).get_collate(),
                        ));
                    }
                    return some(GroupConcatDistinctOrder::new(base, ctors, desc));
                }
                return some(GroupConcatDistinct::new(base));
            }
            if !agg_func_desc.order_by_items.is_empty() {
                let mut desc = Vec::with_capacity(base.by_items.len());
                let mut ctors = Vec::with_capacity(base.by_items.len());
                for by_item in &base.by_items {
                    desc.push(by_item.desc);
                    ctors.push(get_collator(
                        by_item.expr.get_type(&*eval_ctx).get_collate(),
                    ));
                }
                return some(GroupConcatOrder::new(base, ctors, desc));
            }
            some(GroupConcat::new(base))
        }
    }
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
