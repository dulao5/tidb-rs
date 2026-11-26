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

#[test]
fn window_frame_helpers_and_clone() {
    use tidb_parser::ast::{BoundType, FrameType};

    let (ok, frame) = use_default_frame("ROW_NUMBER");
    assert!(ok);
    assert_eq!(frame.tp, FrameType::Rows);
    assert_eq!(frame.extent.start.tp, BoundType::CurrentRow);
    assert_eq!(frame.extent.end.tp, BoundType::CurrentRow);
    assert!(!frame.extent.start.unbounded);
    assert!(!frame.extent.end.unbounded);

    let (ok, frame) = use_default_frame(ast::WindowFuncRank);
    assert!(!ok);
    assert_eq!(frame.tp, FrameType::Rows);
    assert_eq!(frame.extent.start.tp, BoundType::Following);

    for no_frame in [
        "cume_dist",
        "DENSE_RANK",
        "lag",
        "LEAD",
        "ntile",
        "Percent_Rank",
        "rank",
        "ROW_NUMBER",
    ] {
        assert!(!need_frame(no_frame), "{no_frame}");
    }
    for needs_frame in ["first_value", "LAST_VALUE", "nth_value", "sum", "count"] {
        assert!(need_frame(needs_frame), "{needs_frame}");
    }

    let ctx = es::new_expr_context(vec![]);
    let col = column(1, TypeLonglong);
    let desc = new_window_func_desc(&*ctx, ast::AggFuncSum, vec![Arc::clone(&col)], false)
        .unwrap()
        .unwrap();
    let mut cloned = desc.clone_desc();
    assert!(desc.base.equal(&*ctx.get_eval_ctx(), &cloned.base));
    cloned.args[0] = column(2, TypeVarchar);
    assert!(Arc::ptr_eq(&desc.args[0], &col));
    assert!(!desc.base.equal(&*ctx.get_eval_ctx(), &cloned.base));
}

#[test]
fn window_func_arg_validation_and_skip_check() {
    use tidb_types::datum::Datum;

    let ctx = es::new_expr_context(vec![]);
    let col = column(1, TypeLonglong);
    let int_const = |v: i64| {
        let mut d = Datum::default();
        d.set_int64(v);
        Expr::Constant(crate::Constant::new(d, new_field_type(TypeLonglong))).into_ref()
    };
    let null_const = || {
        Expr::Constant(crate::Constant::new(
            Datum::default(),
            new_field_type(tidb_base::mysql::TypeNull),
        ))
        .into_ref()
    };

    // nth_value rejects 0 and non-constants, allows NULL and positive integers.
    assert!(
        new_window_func_desc(
            &*ctx,
            "NTH_VALUE",
            vec![Arc::clone(&col), int_const(0)],
            false
        )
        .unwrap()
        .is_none()
    );
    assert!(
        new_window_func_desc(
            &*ctx,
            ast::WindowFuncNthValue,
            vec![Arc::clone(&col), Arc::clone(&col)],
            false
        )
        .unwrap()
        .is_none()
    );
    assert!(
        new_window_func_desc(
            &*ctx,
            ast::WindowFuncNthValue,
            vec![Arc::clone(&col), null_const()],
            false
        )
        .unwrap()
        .is_some()
    );
    assert!(
        new_window_func_desc(
            &*ctx,
            ast::WindowFuncNthValue,
            vec![Arc::clone(&col), int_const(0)],
            true
        )
        .unwrap()
        .is_some()
    );

    // ntile rejects 0 and non-constants, allows NULL and positive integers.
    assert!(
        new_window_func_desc(&*ctx, "NTILE", vec![int_const(0)], false)
            .unwrap()
            .is_none()
    );
    assert!(
        new_window_func_desc(&*ctx, ast::WindowFuncNtile, vec![null_const()], false)
            .unwrap()
            .is_some()
    );

    // lead / lag reject NULL and non-constant offset, allow 0 and 1-arg calls.
    assert!(
        new_window_func_desc(&*ctx, "LEAD", vec![Arc::clone(&col), null_const()], false)
            .unwrap()
            .is_none()
    );
    assert!(
        new_window_func_desc(
            &*ctx,
            ast::WindowFuncLag,
            vec![Arc::clone(&col), int_const(0)],
            false
        )
        .unwrap()
        .is_some()
    );
    assert!(
        new_window_func_desc(&*ctx, ast::WindowFuncLead, vec![Arc::clone(&col)], false)
            .unwrap()
            .is_some()
    );
}

// Regression test for #398 (ruling R25): lowercase bit_* and 3-arg NOT NULL lead/lag preserve
// UnsignedFlag instead of wiping flags with SetFlag(NotNullFlag).
#[test]
fn window_bit_and_lead_lag_preserve_unsigned_flag() {
    use tidb_base::mysql::{BinaryFlag, UnsignedFlag};
    use tidb_types::datum::Datum;

    let ctx = es::new_expr_context(vec![]);
    let mut unsigned_nn = new_field_type(TypeLonglong);
    unsigned_nn.add_flag(UnsignedFlag | NotNullFlag | BinaryFlag);
    let col_a = Expr::Column(Column {
        unique_id: 1,
        ret_type: Arc::new(unsigned_nn.clone()),
        ..Column::default()
    })
    .into_ref();
    let col_b = Expr::Column(Column {
        unique_id: 2,
        ret_type: Arc::new(unsigned_nn),
        ..Column::default()
    })
    .into_ref();
    let mut one = Datum::default();
    one.set_int64(1);
    let offset = Expr::Constant(crate::Constant::new(one, new_field_type(TypeLonglong))).into_ref();

    for name in [ast::AggFuncBitAnd, ast::AggFuncBitOr, ast::AggFuncBitXor] {
        let desc = new_window_func_desc(&*ctx, name, vec![Arc::clone(&col_a)], false)
            .unwrap()
            .unwrap();
        let flag = desc.ret_tp.get_flag();
        assert_eq!(
            flag,
            NotNullFlag | UnsignedFlag | BinaryFlag,
            "{name} should keep 0xa1 (matching non-window bit_*)"
        );
    }

    for name in [ast::WindowFuncLead, ast::WindowFuncLag] {
        let desc = new_window_func_desc(
            &*ctx,
            name,
            vec![Arc::clone(&col_a), Arc::clone(&offset), Arc::clone(&col_b)],
            false,
        )
        .unwrap()
        .unwrap();
        let flag = desc.ret_tp.get_flag();
        assert_ne!(flag & NotNullFlag, 0, "{name} should keep NotNullFlag");
        assert_ne!(flag & UnsignedFlag, 0, "{name} should keep UnsignedFlag");
    }

    // Signed default column keeps signed result (no spurious UnsignedFlag).
    let mut signed_nn = new_field_type(TypeLonglong);
    signed_nn.add_flag(NotNullFlag | BinaryFlag);
    let col_signed = Expr::Column(Column {
        unique_id: 3,
        ret_type: Arc::new(signed_nn),
        ..Column::default()
    })
    .into_ref();
    for name in [ast::WindowFuncLead, ast::WindowFuncLag] {
        let desc = new_window_func_desc(
            &*ctx,
            name,
            vec![
                Arc::clone(&col_a),
                Arc::clone(&offset),
                Arc::clone(&col_signed),
            ],
            false,
        )
        .unwrap()
        .unwrap();
        let flag = desc.ret_tp.get_flag();
        assert_ne!(
            flag & NotNullFlag,
            0,
            "{name} signed default keeps NotNullFlag"
        );
        assert_eq!(
            flag & UnsignedFlag,
            0,
            "{name} signed default must remain signed"
        );
    }
}

// Regression test for #402 (confirmed V1, M-FT aliasing): DelFlag(NotNullFlag) on
// first_value/last_value/nth_value clears NotNullFlag on the aliased args[0] FieldType.
#[test]
fn window_first_last_nth_value_aliasing_clears_arg_not_null() {
    use tidb_types::datum::Datum;

    let ctx = es::new_expr_context(vec![]);
    let mut int_nn = new_field_type(TypeLonglong);
    int_nn.add_flag(NotNullFlag);
    let col = Expr::Column(Column {
        unique_id: 1,
        ret_type: Arc::new(int_nn),
        ..Column::default()
    })
    .into_ref();
    let mut one = Datum::default();
    one.set_int64(1);
    let pos = Expr::Constant(crate::Constant::new(one, new_field_type(TypeLonglong))).into_ref();

    for (name, args) in [
        (ast::WindowFuncFirstValue, vec![Arc::clone(&col)]),
        (ast::WindowFuncLastValue, vec![Arc::clone(&col)]),
        (ast::WindowFuncNthValue, vec![Arc::clone(&col), pos]),
    ] {
        let desc = new_window_func_desc(&*ctx, name, args, false)
            .unwrap()
            .unwrap();
        assert_eq!(desc.ret_tp.get_flag() & NotNullFlag, 0, "{name} ret_tp");
        assert_eq!(
            desc.args[0].get_type(&*ctx.get_eval_ctx()).get_flag() & NotNullFlag,
            0,
            "{name} aliased args[0] should also lose NotNullFlag"
        );
    }

    // Verify #399 and #400 behaviors match Go: case sensitivity & percent_rank flags.
    let pr_lower = new_window_func_desc(&*ctx, "percent_rank", vec![], false)
        .unwrap()
        .unwrap();
    assert_eq!(pr_lower.ret_tp.get_flag(), NotNullFlag);
    assert_eq!(pr_lower.ret_tp.get_flen(), -1);
    let pr_upper = new_window_func_desc(&*ctx, "PERCENT_RANK", vec![], false)
        .unwrap()
        .unwrap();
    assert_eq!(pr_upper.ret_tp.get_flag(), 0x16);

    let rn_lower = new_window_func_desc(&*ctx, "row_number", vec![], false)
        .unwrap()
        .unwrap();
    assert_eq!(rn_lower.ret_tp.get_flag(), NotNullFlag);
    let rn_upper = new_window_func_desc(&*ctx, "ROW_NUMBER", vec![], false)
        .unwrap()
        .unwrap();
    assert_eq!(rn_upper.ret_tp.get_flag() & NotNullFlag, 0);
}

// Regression test for #401: 3-arg lead with conflicting collations panics on nil RetTp as in Go.
#[test]
#[should_panic(expected = "runtime error: invalid memory address or nil pointer dereference")]
fn window_lead_conflicting_collations_panics_like_go() {
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
    let _ = new_window_func_desc(&*ctx, ast::WindowFuncLead, vec![arg0, arg1, arg2], false);
}
