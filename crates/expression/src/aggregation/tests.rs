//! Go unit tests of `pkg/expression/aggregation` that do not need storage (D08.11 spec §6.1):
//! `base_func_test.go` (`TestClone`, `TestBaseFunc_InferAggRetType`), plus the checks of
//! spec §6.3. The legacy evaluators' tests (`aggregation_test.go`, `util_test.go`) are not
//! ported with them (tracker block X); `TestAggFuncDesc` tests cascades' `Hash64` (EXC.B).

use std::sync::Arc;

use tidb_base::mysql::{NotNullFlag, TypeBit, TypeDouble, TypeLonglong, TypeVarchar};
use tidb_sessionctx::exprstatic as es;
use tidb_types::field_type::new_field_type;

use super::base_func::new_base_func_desc;
use super::*;
use crate::ast_funcs as ast;
use crate::byitem::ByItems;
use crate::{BuildContext, Column, Expr, ExprRef};

fn column(unique_id: i64, tp: u8) -> ExprRef {
    Expr::Column(Column {
        unique_id,
        ret_type: Arc::new(new_field_type(tp)),
        ..Column::default()
    })
    .into_ref()
}

// TestClone (base_func_test.go).
#[test]
fn clone_base_func_desc() {
    let ctx = es::new_expr_context(vec![]);
    let col = column(0, TypeLonglong);
    let (desc, err) = new_base_func_desc(&*ctx, ast::AggFuncFirstRow, vec![Arc::clone(&col)]);
    err.unwrap();
    let mut cloned = desc.clone_base();
    assert!(desc.equal(&*ctx.get_eval_ctx(), &cloned));

    let col1 = column(1, TypeVarchar);
    cloned.args[0] = col1;

    assert!(Arc::ptr_eq(&col, &desc.args[0]));
    assert!(!desc.equal(&*ctx.get_eval_ctx(), &cloned));
}

// TestBaseFunc_InferAggRetType (base_func_test.go).
#[test]
fn infer_agg_ret_type() {
    let ctx = es::new_expr_context(vec![]);
    let double_type = new_field_type(TypeDouble);
    let bit_type = new_field_type(TypeBit);

    let func_names = [ast::AggFuncMax, ast::AggFuncMin];
    let data_types = [double_type, bit_type];

    for data_type in &data_types {
        let mut not_null_type = data_type.clone();
        not_null_type.add_flag(NotNullFlag);
        let col = Expr::Column(Column {
            unique_id: 0,
            ret_type: Arc::new(not_null_type),
            ..Column::default()
        })
        .into_ref();
        for name in func_names {
            let (mut desc, err) = new_base_func_desc(&*ctx, name, vec![Arc::clone(&col)]);
            err.unwrap();
            desc.type_infer(&*ctx).unwrap();
            assert_eq!(data_type, &*desc.ret_tp);
        }
    }
}

// Spec §6.3: plan nodes share descriptors across sessions (plan cache).
#[test]
fn descriptors_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AggFuncDesc>();
    assert_send_sync::<WindowFuncDesc>();
}

// Spec §6.3 (the Rust side of TestClone): changing a clone's arguments, return type or ORDER
// BY items leaves the original as it was (copy on write).
#[test]
fn clone_desc_is_independent() {
    let ctx = es::new_expr_context(vec![]);
    let col = column(1, TypeLonglong);
    let mut desc = new_agg_func_desc(&*ctx, ast::AggFuncSum, vec![Arc::clone(&col)], true).unwrap();
    desc.order_by_items.push(ByItems {
        expr: Arc::clone(&col),
        desc: true,
    });
    let ret = (*desc.ret_tp).clone();

    let mut clone = desc.clone_desc();
    assert!(desc.equal(&*ctx.get_eval_ctx(), &clone));
    clone.args[0] = column(2, TypeVarchar);
    clone.order_by_items[0].desc = false;
    clone.update_not_null_flag4_ret_type(false, false).unwrap();
    Arc::make_mut(&mut clone.base.ret_tp).set_flen(3);

    assert!(Arc::ptr_eq(&desc.args[0], &col));
    assert!(desc.order_by_items[0].desc);
    assert_eq!(*desc.ret_tp, ret);
    assert!(!desc.equal(&*ctx.get_eval_ctx(), &clone));
}

// Split keeps the original descriptor and gives the final phase the partial results'
// columns (descriptor.go Split).
#[test]
fn split_modes() {
    let ctx = es::new_expr_context(vec![]);
    let col = column(1, TypeLonglong);
    let desc = new_agg_func_desc(&*ctx, ast::AggFuncAvg, vec![col], false).unwrap();
    let (partial, final_) = desc.split(&[3, 4]);
    assert_eq!(desc.mode, AggFunctionMode::CompleteMode);
    assert_eq!(partial.mode, AggFunctionMode::Partial1Mode);
    assert_eq!(final_.mode, AggFunctionMode::FinalMode);
    assert_eq!(final_.args.len(), 2);
    let Expr::Column(c) = &*final_.args[1] else {
        panic!("not a column")
    };
    assert_eq!(c.index, 4);
    assert!(Arc::ptr_eq(&c.ret_type, &desc.ret_tp));
    assert!(Arc::ptr_eq(&final_.ret_tp, &desc.ret_tp));
}

// typeInfer4LeadLag keeps Go's nil RetTp when InferType4ControlFuncs fails (spec §8 item 4).
#[test]
fn lead_with_conflicting_collations_has_nil_ret_tp() {
    use tidb_base::mysql::TypeVarString;
    use tidb_types::datum::Datum;
    let ctx = es::new_expr_context(vec![]);
    let mut a = new_field_type(TypeVarString);
    a.set_charset("utf8mb4");
    a.set_collate("utf8mb4_unicode_ci");
    let c0 = Column {
        unique_id: 1,
        ret_type: Arc::new(a),
        ..Column::default()
    };
    c0.collation_info
        .set_coercibility(crate::collation::CoercibilityExplicit);
    let arg0 = Expr::Column(c0).into_ref();
    let mut b = new_field_type(TypeVarString);
    b.set_charset("utf8mb4");
    b.set_collate("utf8mb4_general_ci");
    let c2 = Column {
        unique_id: 2,
        ret_type: Arc::new(b),
        ..Column::default()
    };
    c2.collation_info
        .set_coercibility(crate::collation::CoercibilityExplicit);
    let arg2 = Expr::Column(c2).into_ref();
    let mut one = Datum::default();
    one.set_int64(1);
    let arg1 = Expr::Constant(crate::Constant::new(one, new_field_type(TypeLonglong))).into_ref();
    let desc = new_agg_func_desc(&*ctx, ast::WindowFuncLead, vec![arg0, arg1, arg2], false);
    let desc = desc.unwrap();
    assert!(desc.ret_tp_is_nil());
    // MemoryUsage skips a nil RetTp, as Go does.
    assert_eq!(
        desc.memory_usage(),
        desc.base.memory_usage() + crate::size::SizeOfInt + crate::size::SizeOfBool
    );
}

#[test]
fn mode_names_and_name_helpers() {
    assert_eq!(AggFunctionMode::CompleteMode.to_string(), "complete");
    assert_eq!(AggFunctionMode::DedupMode.to_string(), "deduplicate");
    assert!(need_count(ast::AggFuncAvg));
    assert!(!need_count(ast::AggFuncSum));
    assert!(need_value(ast::AggFuncApproxPercentile));
    assert!(!need_value(ast::AggFuncCount));
    assert!(is_all_first_row(&[]));
}
