//! go-file: pkg/expression/aggregation/agg_to_pb.go
//!
//! Aggregate function descriptors to and from `tipb.Expr` (D08.11 spec §2.1 part B, §4.8).
//! `baseFuncDesc.GetTiPBExpr` is a method of a tidb-expression type in Go; here it is the free
//! function [`get_tipb_expr`] (N0 rule 6).

use protobuf::RepeatedField;
use tidb_base::Error;
use tidb_base::errors::{self as goerrors, RedactLogDisable};
use tidb_base::gofmt::Arg;
use tidb_base::kv::StoreType;
use tidb_codec::number::encode_uint;
use tidb_expression::BuildContext;
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, BaseFuncDesc};
use tidb_expression::ast_funcs as ast;
use tidb_kv::kv::ReqTypeSelect;
use tidb_types::etc::FieldType;
use tipb::{ExprType, ScalarFuncSig};

use crate::distsql_builtin::{field_type, pb_to_exprs, raw_enum};
use crate::expr_to_pb::{field_type_from_pb, sort_by_item_to_pb, to_pb_field_type_with_check};
use crate::infer_pushdown::PushDownContext;

/// GetTiPBExpr return the TiPB ExprType of desc.
// go: baseFuncDesc.GetTiPBExpr L1
pub fn get_tipb_expr(desc: &BaseFuncDesc, try_window_desc: bool) -> ExprType {
    let mut tp = ExprType::Null;
    match desc.name.as_str() {
        ast::AggFuncCount => tp = ExprType::Count,
        ast::AggFuncApproxCountDistinct => tp = ExprType::ApproxCountDistinct,
        ast::AggFuncFirstRow => tp = ExprType::First,
        ast::AggFuncGroupConcat => tp = ExprType::GroupConcat,
        ast::AggFuncMax => tp = ExprType::Max,
        ast::AggFuncMin => tp = ExprType::Min,
        ast::AggFuncSum => tp = ExprType::Sum,
        ast::AggFuncAvg => tp = ExprType::Avg,
        ast::AggFuncBitOr => tp = ExprType::AggBitOr,
        ast::AggFuncBitXor => tp = ExprType::AggBitXor,
        ast::AggFuncBitAnd => tp = ExprType::AggBitAnd,
        ast::AggFuncVarPop => tp = ExprType::VarPop,
        ast::AggFuncJsonArrayagg => tp = ExprType::JsonArrayAgg,
        ast::AggFuncJsonObjectAgg => tp = ExprType::JsonObjectAgg,
        ast::AggFuncStddevPop => tp = ExprType::StddevPop,
        ast::AggFuncVarSamp => tp = ExprType::VarSamp,
        ast::AggFuncStddevSamp => tp = ExprType::StddevSamp,
        _ => {}
    }

    if tp != ExprType::Null || !try_window_desc {
        return tp;
    }

    match desc.name.as_str() {
        ast::WindowFuncRowNumber => tp = ExprType::RowNumber,
        ast::WindowFuncRank => tp = ExprType::Rank,
        ast::WindowFuncDenseRank => tp = ExprType::DenseRank,
        ast::WindowFuncCumeDist => tp = ExprType::CumeDist,
        ast::WindowFuncPercentRank => tp = ExprType::PercentRank,
        ast::WindowFuncNtile => tp = ExprType::Ntile,
        ast::WindowFuncLead => tp = ExprType::Lead,
        ast::WindowFuncLag => tp = ExprType::Lag,
        ast::WindowFuncFirstValue => tp = ExprType::FirstValue,
        ast::WindowFuncLastValue => tp = ExprType::LastValue,
        ast::WindowFuncNthValue => tp = ExprType::NthValue,
        _ => {}
    }
    tp
}

/// AggFuncToPBExpr converts aggregate function to pb.
// go: AggFuncToPBExpr L1 [M-ERR]
pub fn agg_func_to_pb_expr(
    ctx: &PushDownContext<'_>,
    agg_func: &AggFuncDesc,
    store_type: StoreType,
) -> Result<tipb::Expr, Error> {
    let pc = ctx.pb_converter();
    let tp = get_tipb_expr(agg_func, false);
    let client = ctx.client();
    if !client.is_request_type_supported(ReqTypeSelect, tp as i64) {
        return Err(goerrors::new("select request is not supported by client"));
    }

    let cannot_convert = || {
        let mut msg = agg_func.string_with_ctx(ctx.eval_ctx(), RedactLogDisable);
        msg.extend_from_slice(b" can't be converted to PB.");
        goerrors::new(msg)
    };
    let mut children = Vec::with_capacity(agg_func.args.len());
    for arg in &agg_func.args {
        let Some(pb_arg) = pc.expr_to_pb(arg) else {
            return Err(cannot_convert());
        };
        children.push(pb_arg);
    }
    let ret_type = to_pb_field_type_with_check(&agg_func.ret_tp, store_type)?;

    // &tipb.Expr{...}: the gogoproto non-nullable fields (tp, sig, has_distinct) are always
    // marshalled.
    let mut e = tipb::Expr::default();
    e.set_tp(tp);
    e.set_sig(ScalarFuncSig::Unspecified);
    e.set_field_type(ret_type);
    e.set_has_distinct(agg_func.has_distinct);
    e.set_agg_func_mode(agg_function_mode_to_pb(agg_func.mode));
    if tp == ExprType::GroupConcat {
        let mut order_by = Vec::with_capacity(agg_func.order_by_items.len());
        for arg in &agg_func.order_by_items {
            let Some(pb_arg) = sort_by_item_to_pb(
                ctx.eval_ctx(),
                ctx.eval_state(),
                &**client,
                &arg.expr,
                arg.desc,
            ) else {
                return Err(cannot_convert());
            };
            order_by.push(pb_arg);
        }
        // encode GroupConcatMaxLen
        let max_len = ctx.get_group_concat_max_len();
        let mut val = Vec::new();
        encode_uint(&mut val, max_len);
        e.set_val(val);
        e.set_children(RepeatedField::from_vec(children));
        e.set_order_by(RepeatedField::from_vec(order_by));
        return Ok(e);
    }
    e.set_children(RepeatedField::from_vec(children));
    Ok(e)
}

/// AggFunctionModeToPB converts aggregate function mode to PB.
///
/// Go returns a new `*tipb.AggFunctionMode`, never nil.
// go: AggFunctionModeToPB L1
pub fn agg_function_mode_to_pb(mode: AggFunctionMode) -> tipb::AggFunctionMode {
    match mode {
        AggFunctionMode::CompleteMode => tipb::AggFunctionMode::CompleteMode,
        AggFunctionMode::FinalMode => tipb::AggFunctionMode::FinalMode,
        AggFunctionMode::Partial1Mode => tipb::AggFunctionMode::Partial1Mode,
        AggFunctionMode::Partial2Mode => tipb::AggFunctionMode::Partial2Mode,
        AggFunctionMode::DedupMode => tipb::AggFunctionMode::DedupMode,
    }
}

/// PBAggFuncModeToAggFuncMode converts pb to aggregate function mode.
///
/// `None` is Go's nil pointer; a value the Rust tipb enum does not know is not kept in the
/// field either, and Go's switch keeps the default for it too.
// go: PBAggFuncModeToAggFuncMode L1
pub fn pb_agg_func_mode_to_agg_func_mode(
    pb_mode: Option<tipb::AggFunctionMode>,
) -> AggFunctionMode {
    // Default mode of the aggregate function is PartialMode.
    let mut mode = AggFunctionMode::Partial1Mode;
    if let Some(pb_mode) = pb_mode {
        match pb_mode {
            tipb::AggFunctionMode::CompleteMode => mode = AggFunctionMode::CompleteMode,
            tipb::AggFunctionMode::FinalMode => mode = AggFunctionMode::FinalMode,
            tipb::AggFunctionMode::Partial1Mode => mode = AggFunctionMode::Partial1Mode,
            tipb::AggFunctionMode::Partial2Mode => mode = AggFunctionMode::Partial2Mode,
            tipb::AggFunctionMode::DedupMode => mode = AggFunctionMode::DedupMode,
        }
    }
    mode
}

/// PBExprToAggFuncDesc converts pb to aggregate function.
// go: PBExprToAggFuncDesc L1 [M-ERR]
pub fn pb_expr_to_agg_func_desc(
    ctx: &dyn BuildContext,
    agg_func: &tipb::Expr,
    field_tps: &[FieldType],
) -> Result<AggFuncDesc, Error> {
    let tp = raw_enum(
        agg_func,
        agg_func.has_tp().then(|| agg_func.get_tp() as i32),
        1,
    )
    .unwrap_or(0);
    // A value the Go tipb module does not know (3021-3023 of the newer Rust crate) matches
    // no arm, as in Go.
    let name = match <ExprType as protobuf::ProtobufEnum>::from_i32(tp) {
        Some(ExprType::Count) => ast::AggFuncCount,
        Some(ExprType::ApproxCountDistinct) => ast::AggFuncApproxCountDistinct,
        Some(ExprType::First) => ast::AggFuncFirstRow,
        Some(ExprType::GroupConcat) => ast::AggFuncGroupConcat,
        Some(ExprType::Max) => ast::AggFuncMax,
        Some(ExprType::Min) => ast::AggFuncMin,
        Some(ExprType::Sum) => ast::AggFuncSum,
        Some(ExprType::Avg) => ast::AggFuncAvg,
        Some(ExprType::AggBitOr) => ast::AggFuncBitOr,
        Some(ExprType::AggBitXor) => ast::AggFuncBitXor,
        Some(ExprType::AggBitAnd) => ast::AggFuncBitAnd,
        _ => {
            return Err(goerrors::errorf(
                "unknown aggregation function type: %v",
                &[Arg::from(expr_type_string(tp))],
            ));
        }
    };

    let args = pb_to_exprs(ctx, agg_func.get_children(), field_tps)?;
    let mut base = BaseFuncDesc::from_parts(
        name.to_owned(),
        args,
        Some(std::sync::Arc::new(field_type_from_pb(field_type(
            agg_func,
        )))),
    );
    base.wrap_cast_for_agg_args(ctx)?;
    let mode = pb_agg_func_mode_to_agg_func_mode(
        agg_func
            .has_agg_func_mode()
            .then(|| agg_func.get_agg_func_mode()),
    );
    let mut desc = AggFuncDesc::from_base(base, false);
    desc.mode = mode;
    Ok(desc)
}

/// `ExprType.String()`: `proto.EnumName(ExprType_name, int32(x))`, the name of a known value,
/// else the number.
// go-dep: github.com/pingcap/tipb/go-tipb@v0.0.0-20260605083900-f9f651ef5fbc.ExprType.String L1
fn expr_type_string(x: i32) -> String {
    match GO_EXPR_TYPE_NAMES.binary_search_by_key(&x, |&(n, _)| n) {
        Ok(i) => GO_EXPR_TYPE_NAMES[i].1.to_owned(),
        Err(_) => x.to_string(),
    }
}

/// `ExprType_name` of `github.com/pingcap/tipb/go-tipb` at v0.0.0-20260605083900-f9f651ef5fbc
/// (TiDB v8.5.8's go.mod), sorted by value.
const GO_EXPR_TYPE_NAMES: &[(i32, &str)] = &[
    (0, "Null"),
    (1, "Int64"),
    (2, "Uint64"),
    (3, "Float32"),
    (4, "Float64"),
    (5, "String"),
    (6, "Bytes"),
    (101, "MysqlBit"),
    (102, "MysqlDecimal"),
    (103, "MysqlDuration"),
    (104, "MysqlEnum"),
    (105, "MysqlHex"),
    (106, "MysqlSet"),
    (107, "MysqlTime"),
    (108, "MysqlJson"),
    (121, "TiDBVectorFloat32"),
    (151, "ValueList"),
    (201, "ColumnRef"),
    (3001, "Count"),
    (3002, "Sum"),
    (3003, "Avg"),
    (3004, "Min"),
    (3005, "Max"),
    (3006, "First"),
    (3007, "GroupConcat"),
    (3008, "Agg_BitAnd"),
    (3009, "Agg_BitOr"),
    (3010, "Agg_BitXor"),
    (3011, "Std"),
    (3012, "Stddev"),
    (3013, "StddevPop"),
    (3014, "StddevSamp"),
    (3015, "VarPop"),
    (3016, "VarSamp"),
    (3017, "Variance"),
    (3018, "JsonArrayAgg"),
    (3019, "JsonObjectAgg"),
    (3020, "ApproxCountDistinct"),
    (4001, "RowNumber"),
    (4002, "Rank"),
    (4003, "DenseRank"),
    (4004, "CumeDist"),
    (4005, "PercentRank"),
    (4006, "Ntile"),
    (4007, "Lead"),
    (4008, "Lag"),
    (4009, "FirstValue"),
    (4010, "LastValue"),
    (4011, "NthValue"),
    (10000, "ScalarFunc"),
];
