//! go-file: pkg/expression/aggregation/aggregation.go
//!
//! The push-down checks of aggregate functions (D08.11 spec §2.1 part B). The rest of the
//! file that is ported (modes, `NeedCount`, `NeedValue`, `IsAllFirstRow`) is in
//! `tidb_expression::aggregation::aggregation`.

use tidb_base::kv::{StoreType, TiFlash, TiKV};
use tidb_base::mysql::{TypeDuration, TypeJSON, TypeTiDBVectorFloat32};
use tidb_expression::EvalContext;
use tidb_expression::aggregation::AggFuncDesc;
use tidb_expression::ast_funcs as ast;

use crate::infer_pushdown::is_push_down_enabled;

/// CheckAggPushDown checks whether an agg function can be pushed to storage.
// go: CheckAggPushDown L1
pub fn check_agg_push_down(
    ctx: &dyn EvalContext,
    agg_func: &AggFuncDesc,
    store_type: StoreType,
) -> bool {
    if !agg_func.order_by_items.is_empty() && agg_func.name != ast::AggFuncGroupConcat {
        return false;
    }
    if agg_func.name == ast::AggFuncApproxPercentile {
        return false;
    }
    if !check_vector_agg_push_down(ctx, agg_func) {
        return false;
    }
    let mut ret = true;
    if store_type == TiFlash {
        ret = check_agg_push_flash(ctx, agg_func);
    } else if store_type == TiKV {
        // TiKV does not support group_concat now
        ret = agg_func.name != ast::AggFuncGroupConcat;
    }
    if ret {
        ret = is_push_down_enabled(&agg_func.name.to_lowercase(), store_type);
    }
    ret
}

/// checkVectorAggPushDown returns false if this aggregate function is not supported to push
/// down.
/// - The aggregate function is not calculated over a Vector column (returns true)
/// - The aggregate function is calculated over a Vector column and the function is supported
///   (returns true)
/// - The aggregate function is calculated over a Vector column and the function is not
///   supported (returns false)
// go: checkVectorAggPushDown L1
fn check_vector_agg_push_down(ctx: &dyn EvalContext, agg_func: &AggFuncDesc) -> bool {
    match agg_func.name.as_str() {
        ast::AggFuncCount | ast::AggFuncMin | ast::AggFuncMax | ast::AggFuncFirstRow => {
            return true;
        }
        _ => {
            if agg_func.args[0].get_type(ctx).get_type() == TypeTiDBVectorFloat32 {
                return false;
            }
        }
    }
    true
}

/// CheckAggPushFlash checks whether an agg function can be pushed to flash storage.
// go: CheckAggPushFlash L1
pub fn check_agg_push_flash(ctx: &dyn EvalContext, agg_func: &AggFuncDesc) -> bool {
    for arg in &agg_func.args {
        if arg.get_type(ctx).get_type() == TypeDuration {
            return false;
        }
    }
    match agg_func.name.as_str() {
        ast::AggFuncCount
        | ast::AggFuncMin
        | ast::AggFuncMax
        | ast::AggFuncFirstRow
        | ast::AggFuncApproxCountDistinct => return true,
        ast::AggFuncSum | ast::AggFuncAvg | ast::AggFuncGroupConcat => {
            // Now tiflash doesn't support CastJsonAsReal and CastJsonAsString.
            return agg_func.args[0].get_type(ctx).get_type() != TypeJSON;
        }
        _ => {}
    }
    false
}
