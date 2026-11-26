//! go-file: pkg/expression/aggregation/window_func.go
//!
//! The tipb conversion and TiFlash push-down check of window function descriptors (D08.11
//! spec §2.1 part C, §4.8). `WindowFuncDesc.CanPushDownToTiFlash` is a method on a
//! `tidb-expression` type in Go; here it is the free function [`can_push_down_to_tiflash`]
//! (N0 rule 6). The rest of `window_func.go` is in `tidb_expression::aggregation::window_func`.

use protobuf::RepeatedField;
use tidb_base::kv::TiFlash;
use tidb_expression::aggregation::WindowFuncDesc;
use tidb_expression::ast_funcs as ast;
use tidb_expression::{EvalContext, EvalState};
use tidb_kv::kv::{Client, ReqTypeSelect};
use tipb::ScalarFuncSig;

use super::agg_to_pb::get_tipb_expr;
use crate::expr_to_pb::{new_pb_converter, to_pb_field_type};
use crate::infer_pushdown::{PushDownContext, can_exprs_push_down};

/// WindowFuncToPBExpr converts aggregate function to pb.
// go: WindowFuncToPBExpr L1
pub fn window_func_to_pb_expr(
    sctx: &dyn EvalContext,
    st: &EvalState,
    client: &dyn Client,
    desc: &WindowFuncDesc,
) -> Option<tipb::Expr> {
    let pc = new_pb_converter(client, sctx, st);
    let tp = get_tipb_expr(desc, true);
    if !client.is_request_type_supported(ReqTypeSelect, tp as i64) {
        return None;
    }

    let mut children = Vec::with_capacity(desc.args.len());
    for arg in &desc.args {
        let pb_arg = pc.expr_to_pb(arg)?;
        children.push(pb_arg);
    }
    if desc.ret_tp_is_nil() {
        // Go's `ToPBFieldType(nil)` panics on `ft.GetType()`.
        panic!("runtime error: invalid memory address or nil pointer dereference");
    }
    // &tipb.Expr{...}: the gogoproto non-nullable fields (tp, sig, has_distinct) are always
    // marshalled.
    let mut e = tipb::Expr::default();
    e.set_tp(tp);
    e.set_sig(ScalarFuncSig::Unspecified);
    e.set_has_distinct(false);
    e.set_children(RepeatedField::from_vec(children));
    e.set_field_type(to_pb_field_type(&desc.ret_tp));
    Some(e)
}

/// CanPushDownToTiFlash control whether a window function desc can be push down to tiflash.
// go: WindowFuncDesc.CanPushDownToTiFlash L1
pub fn can_push_down_to_tiflash(desc: &WindowFuncDesc, ctx: &PushDownContext<'_>) -> bool {
    // args
    if !can_exprs_push_down(ctx, &desc.args, TiFlash) {
        return false;
    }
    // window functions
    match desc.name.as_str() {
        ast::WindowFuncRowNumber
        | ast::WindowFuncRank
        | ast::WindowFuncDenseRank
        | ast::WindowFuncLead
        | ast::WindowFuncLag
        | ast::WindowFuncFirstValue
        | ast::WindowFuncLastValue => return true,
        _ => {}
    }

    false
}
