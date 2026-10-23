//! Uses the vendored TiKV query crates without any TiKV storage layer: a row
//! format v2 round trip and one RPN expression evaluated over an in-memory
//! batch.

use tidb_query_datatype::{
    FieldTypeTp,
    codec::{
        batch::LazyBatchColumnVec,
        data_type::{ScalarValueRef, VectorValue},
        row::v2::{
            RowSlice, decode_v2_u64,
            encoder_for_test::{Column, RowEncoder},
        },
    },
    expr::EvalContext,
};
use tidb_query_expr::RpnExpressionBuilder;
use tipb::{FieldType, ScalarFuncSig};
use tipb_helper::ExprDefBuilder;

#[test]
fn row_v2_round_trip() {
    let mut ctx = EvalContext::default();
    let mut row = vec![];
    row.write_row(
        &mut ctx,
        vec![
            Column::new(1, 1000i64),
            Column::new(3, b"abc".to_vec()),
            Column::new(5, None::<i64>),
        ],
    )
    .unwrap();

    let slice = RowSlice::from_bytes(&row).unwrap();
    // Integers are stored in 1/2/4/8 bytes; the value is positive, so the
    // public unsigned decoder reads it back.
    assert_eq!(decode_v2_u64(slice.get(1).unwrap().unwrap()).unwrap(), 1000);
    assert_eq!(slice.get(3).unwrap().unwrap(), b"abc");
    assert!(slice.search_in_null_ids(5));
    assert_eq!(slice.get(2).unwrap(), None);
}

#[test]
fn rpn_eval_over_in_memory_batch() {
    let mut ctx = EvalContext::default();
    let schema: Vec<FieldType> = vec![FieldTypeTp::LongLong.into()];
    let c0 = VectorValue::Int(vec![Some(1), Some(7), None, Some(42)].into());
    let logical_rows = [0, 1, 2, 3];

    // (c0 + 10) > 15
    let expr = ExprDefBuilder::scalar_func(ScalarFuncSig::GtInt, FieldTypeTp::LongLong)
        .push_child(
            ExprDefBuilder::scalar_func(ScalarFuncSig::PlusInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::constant_int(10)),
        )
        .push_child(ExprDefBuilder::constant_int(15))
        .build();
    let rpn = RpnExpressionBuilder::build_from_expr_tree(expr, &mut ctx, schema.len()).unwrap();

    let mut columns = LazyBatchColumnVec::from(vec![c0]);
    let result = rpn
        .eval(
            &mut ctx,
            &schema,
            &mut columns,
            &logical_rows,
            logical_rows.len(),
        )
        .unwrap();
    let vector = result.vector_value().unwrap();
    let values: Vec<_> = vector
        .logical_rows()
        .iter()
        .map(|&i| vector.as_ref().get_scalar_ref(i))
        .collect();
    assert_eq!(
        values,
        [
            ScalarValueRef::Int(Some(&0)),
            ScalarValueRef::Int(Some(&1)),
            ScalarValueRef::Int(None),
            ScalarValueRef::Int(Some(&1)),
        ]
    );
}
