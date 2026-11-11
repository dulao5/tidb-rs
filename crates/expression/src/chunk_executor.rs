//! go-file: pkg/expression/chunk_executor.go
//!
//! Evaluation of expression lists over a chunk: whole columns (`VectorizedExecute`,
//! `evalOneVec`/`evalOneColumn`/`evalOneCell` for the `EvaluatorSuite`) and filters
//! (`VectorizedFilter*`), each with Go's row-by-row fallback for an expression that is not
//! vectorized (D08.1 part 2b).
//!
//! Go passes a `chunk.Iterator4Chunk`; the functions here take the chunk it iterates (the
//! iterator is only used for `GetChunk()` and to walk the rows of that chunk), so the
//! filters can clear and restore the chunk's `Sel()` as Go does through the pointer.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::{Chunk, Row};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType,
};
use tidb_types::binary_literal::new_binary_literal_from_uint;
use tidb_types::enum_type::{Enum, parse_enum_name, parse_enum_value};
use tidb_types::set_type::{Set, parse_set_name};

use crate::context::EvalContext;
use crate::eval::{RpnProgram, RpnState};
use crate::expression::{
    EvalState, Expr, ExprRef, eval_bool, unsupported_eval_type, vec_eval_bool,
};
use crate::scalar_function::ast_func as ast;

/// Vectorizable checks whether a list of expressions can employ vectorized execution.
// go: Vectorizable L1
pub fn vectorizable(exprs: &[ExprRef]) -> bool {
    for expr in exprs {
        if has_get_set_var_func(expr) {
            return false;
        }
    }
    check_sequence_function(exprs)
}

/// checkSequenceFunction indicates whether the exprs can be evaluated as a vector.
/// When two or more of this three(nextval, lastval, setval) exists in exprs list and one of them is nextval, it should be eval row by row.
// go: checkSequenceFunction L1
fn check_sequence_function(exprs: &[ExprRef]) -> bool {
    let (mut nextval, mut lastval, mut setval) = (0, 0, 0);
    for expr in exprs {
        let Some(scala_func) = expr.as_scalar_function() else {
            continue;
        };
        match scala_func.func_name.l.as_str() {
            ast::NextVal => nextval += 1,
            ast::LastVal => lastval += 1,
            ast::SetVal => setval += 1,
            _ => {}
        }
    }
    // case1: nextval && other sequence function.
    // case2: more than one nextval.
    !((nextval > 0 && (lastval > 0 || setval > 0)) || (nextval > 1))
}

/// HasGetSetVarFunc checks whether an expression contains SetVar/GetVar function.
// go: HasGetSetVarFunc L1
pub fn has_get_set_var_func(expr: &Expr) -> bool {
    let Some(scala_func) = expr.as_scalar_function() else {
        return false;
    };
    if scala_func.func_name.l == ast::SetVar {
        return true;
    }
    if scala_func.func_name.l == ast::GetVar {
        return true;
    }
    scala_func
        .get_args()
        .iter()
        .any(|arg| has_get_set_var_func(arg))
}

/// HasAssignSetVarFunc checks whether an expression contains SetVar function and assign a value
// go: HasAssignSetVarFunc L1
pub fn has_assign_set_var_func(expr: &Expr) -> bool {
    let Some(scala_func) = expr.as_scalar_function() else {
        return false;
    };
    if scala_func.func_name.l == ast::SetVar
        && scala_func
            .get_args()
            .iter()
            .any(|arg| arg.as_scalar_function().is_some())
    {
        return true;
    }
    scala_func
        .get_args()
        .iter()
        .any(|arg| has_assign_set_var_func(arg))
}

/// VectorizedExecute evaluates a list of expressions column by column and append their results to "output" Chunk.
// go: VectorizedExecute L1
pub fn vectorized_execute(
    ctx: &dyn EvalContext,
    st: &EvalState,
    exprs: &[ExprRef],
    input: &Chunk,
    output: &mut Chunk,
) -> Result<(), Error> {
    for (col_id, expr) in exprs.iter().enumerate() {
        eval_one_column(ctx, st, expr, input, output, col_id)?;
    }
    Ok(())
}

/// evalOneVec: `expr.VecEvalXxx` of its evaluation type into the output column, then the
/// conversion of a BIT, FLOAT, ENUM or SET result to the column's storage.
///
/// `prog` is `expr` compiled for the vector driver (the `EvaluatorSuite` compiles it once).
// go: evalOneVec L2 (the expression is evaluated through its RpnProgram, N4 §3.4)
#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // index loops kept in Go's shape
pub(crate) fn eval_one_vec(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut RpnState,
    prog: &RpnProgram,
    expr: &Expr,
    input: &Chunk,
    output: &mut Chunk,
    col_idx: usize,
) -> Result<(), Error> {
    let ft = expr.get_type(ctx).into_owned();
    let et = ft.eval_type();
    if !matches!(
        et,
        ETInt
            | ETReal
            | ETDecimal
            | ETDatetime
            | ETTimestamp
            | ETDuration
            | ETJson
            | ETVectorFloat32
            | ETString
    ) {
        return Err(unsupported_eval_type(et));
    }
    prog.eval(ctx, st, rpn, input, output.column_mut(col_idx))?;
    let n = input.num_rows();
    let result = output.column(col_idx);
    let tp = ft.get_type();
    if et == ETInt && tp == mysql::TypeBit {
        let i64s = result.int64s();
        let mut buf = tidb_chunk::column::new_column_for(&ft, n);
        buf.reserve_bytes(n);
        let byte_size = (ft.get_flen() + 7) >> 3;
        for i in 0..i64s.len() {
            if result.is_null(i) {
                buf.append_null();
            } else {
                buf.append_bytes(&new_binary_literal_from_uint(i64s[i].get() as u64, byte_size).0);
            }
        }
        // TODO: recycle all old Columns returned here.
        output.set_col(col_idx, tidb_chunk::ColArc::new(buf));
    } else if et == ETReal && tp == mysql::TypeFloat {
        let f64s = result.float64s();
        let mut buf = tidb_chunk::column::new_column_for(&ft, n);
        buf.resize_float32(n, false);
        for (i, f) in f64s.iter().enumerate() {
            if result.is_null(i) {
                buf.set_null(i, true);
            } else {
                buf.float32s_mut()[i] = zerocopy::little_endian::F32::new(f.get() as f32);
            }
        }
        output.set_col(col_idx, tidb_chunk::ColArc::new(buf));
    } else if et == ETString && (tp == mysql::TypeEnum || tp == mysql::TypeSet) {
        let elems = ft.get_elems().unwrap_or_default();
        let mut buf = tidb_chunk::column::new_column_for(&ft, n);
        if tp == mysql::TypeEnum {
            buf.reserve_enum(n);
        } else {
            buf.reserve_set(n);
        }
        for i in 0..n {
            if result.is_null(i) {
                buf.append_null();
            } else if tp == mysql::TypeEnum {
                // Go logs the parse error at debug level and appends the zero Enum it returns.
                let e = parse_enum_name(elems, result.get_string(i), ft.get_collate())
                    .unwrap_or_default();
                buf.append_enum(&e);
            } else {
                let s = parse_set_name(elems, result.get_string(i), ft.get_collate())
                    .unwrap_or_default();
                buf.append_set(&s);
            }
        }
        output.set_col(col_idx, tidb_chunk::ColArc::new(buf));
    }
    Ok(())
}

/// evalOneColumn: `expr` row by row over the rows of `input` (its `Sel()` if any), appended
/// to output column `col_id`.
// go: evalOneColumn L1
pub(crate) fn eval_one_column(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    input: &Chunk,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    let field_type = expr.get_type(ctx).into_owned();
    let exec: ExecuteTo = match field_type.eval_type() {
        ETInt => execute_to_int,
        ETReal => execute_to_real,
        ETDecimal => execute_to_decimal,
        ETDatetime | ETTimestamp => execute_to_datetime,
        ETDuration => execute_to_duration,
        ETJson => execute_to_json,
        ETVectorFloat32 => execute_to_vector_float32,
        ETString => execute_to_string,
        et => return Err(unsupported_eval_type(et)),
    };
    for i in 0..input.num_rows() {
        exec(ctx, st, expr, &field_type, input.get_row(i), output, col_id)?;
    }
    Ok(())
}

/// evalOneCell: `expr` on one row, appended to output column `col_id`.
// go: evalOneCell L1
pub(crate) fn eval_one_cell(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    let field_type = expr.get_type(ctx).into_owned();
    let exec: ExecuteTo = match field_type.eval_type() {
        ETInt => execute_to_int,
        ETReal => execute_to_real,
        ETDecimal => execute_to_decimal,
        ETDatetime | ETTimestamp => execute_to_datetime,
        ETDuration => execute_to_duration,
        ETJson => execute_to_json,
        ETVectorFloat32 => execute_to_vector_float32,
        ETString => execute_to_string,
        et => return Err(unsupported_eval_type(et)),
    };
    exec(ctx, st, expr, &field_type, row, output, col_id)
}

type ExecuteTo = fn(
    &dyn EvalContext,
    &EvalState,
    &Expr,
    &FieldType,
    Row<'_>,
    &mut Chunk,
    usize,
) -> Result<(), Error>;

// go: executeToInt L1
fn execute_to_int(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    let Some(res) = expr.eval_int(ctx, st, row)? else {
        output.append_null(col_id);
        return Ok(());
    };
    if field_type.get_type() == mysql::TypeBit {
        let byte_size = (field_type.get_flen() + 7) >> 3;
        output.append_bytes(
            col_id,
            &new_binary_literal_from_uint(res as u64, byte_size).0,
        );
        return Ok(());
    }
    if field_type.get_type() == mysql::TypeEnum {
        let e = parse_enum_value(field_type.get_elems().unwrap_or_default(), res as u64)?;
        output.append_enum(col_id, &e);
        return Ok(());
    }
    if mysql::has_unsigned_flag(field_type.get_flag()) {
        output.append_uint64(col_id, res as u64);
        return Ok(());
    }
    output.append_int64(col_id, res);
    Ok(())
}

// go: executeToReal L1
fn execute_to_real(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    let Some(res) = expr.eval_real(ctx, st, row)? else {
        output.append_null(col_id);
        return Ok(());
    };
    if field_type.get_type() == mysql::TypeFloat {
        output.append_float32(col_id, res as f32);
        return Ok(());
    }
    output.append_float64(col_id, res);
    Ok(())
}

// go: executeToDecimal L1
fn execute_to_decimal(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    _field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_decimal(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) => output.append_my_decimal(col_id, &res),
    }
    Ok(())
}

// go: executeToDatetime L1
fn execute_to_datetime(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    _field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_time(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) => output.append_time(col_id, res),
    }
    Ok(())
}

// go: executeToDuration L1
fn execute_to_duration(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    _field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_duration(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) => output.append_duration(col_id, res),
    }
    Ok(())
}

// go: executeToJSON L1
fn execute_to_json(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    _field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_json(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) => output.append_json(col_id, &res),
    }
    Ok(())
}

// go: executeToVectorFloat32 L1
fn execute_to_vector_float32(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    _field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_vector_float32(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) => output.append_vector_float32(col_id, &res),
    }
    Ok(())
}

// go: executeToString L1
fn execute_to_string(
    ctx: &dyn EvalContext,
    st: &EvalState,
    expr: &Expr,
    field_type: &FieldType,
    row: Row<'_>,
    output: &mut Chunk,
    col_id: usize,
) -> Result<(), Error> {
    match expr.eval_string(ctx, st, row)? {
        None => output.append_null(col_id),
        Some(res) if field_type.get_type() == mysql::TypeEnum => {
            let val = Enum {
                value: 0,
                name: res,
            };
            output.append_enum(col_id, &val);
        }
        Some(res) if field_type.get_type() == mysql::TypeSet => {
            let val = Set {
                value: 0,
                name: res,
            };
            output.append_set(col_id, &val);
        }
        Some(res) => output.append_string(col_id, &res),
    }
    Ok(())
}

/// VectorizedFilter applies a list of filters to a Chunk and
/// returns a bool slice, which indicates whether a row is passed the filters.
/// Filters is executed vectorized.
// go: VectorizedFilter L2 (takes the iterator's chunk; selected by value, M-POOL)
pub fn vectorized_filter(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut RpnState,
    vec_enabled: bool,
    filters: &[ExprRef],
    input: &mut Chunk,
    selected: Vec<bool>,
) -> Result<Vec<bool>, Error> {
    let (selected, _) =
        vectorized_filter_consider_null(ctx, st, rpn, vec_enabled, filters, input, selected, None)?;
    Ok(selected)
}

/// VectorizedFilterConsiderNull applies a list of filters to a Chunk and
/// returns two bool slices, `selected` indicates whether a row passed the
/// filters, `isNull` indicates whether the result of the filter is null.
/// Filters is executed vectorized.
///
/// `is_null` is `None` for Go's nil slice: the row-based path then leaves it `None`, the
/// vector path (`VecEvalBool`) always returns one.
// go: VectorizedFilterConsiderNull L2 (takes the iterator's chunk; slices by value, M-POOL)
#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // index loops kept in Go's shape
pub fn vectorized_filter_consider_null(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut RpnState,
    vec_enabled: bool,
    filters: &[ExprRef],
    input: &mut Chunk,
    selected: Vec<bool>,
    is_null: Option<Vec<bool>>,
) -> Result<(Vec<bool>, Option<Vec<bool>>), Error> {
    // canVectorized used to check whether all of the filters can be vectorized evaluated
    let can_vectorized = filters.iter().all(|f| f.vectorized());

    let sel = input.sel().map(<[usize]>::to_vec);
    let (mut selected, is_null) = if can_vectorized && vec_enabled {
        vectorized_filter_inner(ctx, st, rpn, vec_enabled, filters, input, selected, is_null)?
    } else {
        row_based_filter(ctx, st, filters, input, selected, is_null)?
    };
    let Some(sel) = sel else {
        return Ok((selected, is_null));
    };

    // When the input.Sel() != nil, we need to handle the selected slice and input.Sel()
    // Get the index which is not appeared in input.Sel() and set the selected[index] = false
    let selected_length = selected.len();
    // unselected[i] == 1 means that the i-th row is not selected
    let mut unselected = vec![1i8; selected_length];
    for ind in sel {
        unselected[ind] = 0;
    }
    for i in 0..selected_length {
        if selected[i] && unselected[i] == 1 {
            selected[i] = false;
        }
    }
    Ok((selected, is_null))
}

/// rowBasedFilter filters by row.
// go: rowBasedFilter L1
fn row_based_filter(
    ctx: &dyn EvalContext,
    st: &EvalState,
    filters: &[ExprRef],
    input: &mut Chunk,
    selected: Vec<bool>,
    is_null: Option<Vec<bool>>,
) -> Result<(Vec<bool>, Option<Vec<bool>>), Error> {
    // If input.Sel() != nil, we will call input.SetSel(nil) to clear the sel slice in input chunk.
    // After the function finished, then we reset the sel in input chunk.
    // Then the caller will handle the input.sel and selected slices.
    let saved = input.sel().map(<[usize]>::to_vec);
    if saved.is_some() {
        input.set_sel(None);
    }
    let r = row_based_filter_inner(ctx, st, filters, input, selected, is_null);
    if saved.is_some() {
        input.set_sel(saved);
    }
    r
}

// glue: body of rowBasedFilter between Go's SetSel(nil) and the deferred SetSel
#[allow(clippy::needless_range_loop)] // index loops kept in Go's shape
fn row_based_filter_inner(
    ctx: &dyn EvalContext,
    st: &EvalState,
    filters: &[ExprRef],
    input: &Chunk,
    mut selected: Vec<bool>,
    mut is_null: Option<Vec<bool>>,
) -> Result<(Vec<bool>, Option<Vec<bool>>), Error> {
    let num_rows = input.num_rows();
    selected.clear();
    selected.resize(num_rows, true);
    if let Some(n) = is_null.as_mut() {
        n.clear();
        n.resize(num_rows, false);
    }
    for filter in filters {
        let is_int_type = filter.get_type(ctx).eval_type() == ETInt;
        for idx in 0..num_rows {
            let row = input.get_row(idx);
            if !selected[idx] {
                continue;
            }
            let is_null_result;
            if is_int_type {
                let r = filter.eval_int(ctx, st, row)?;
                is_null_result = r.is_none();
                selected[idx] = selected[idx] && !is_null_result && r != Some(0);
            } else {
                // TODO: should rewrite the filter to `cast(expr as SIGNED) != 0` and always use `EvalInt`.
                let (b_val, n) = eval_bool(ctx, st, std::slice::from_ref(filter), row)?;
                is_null_result = n;
                selected[idx] = selected[idx] && b_val;
            }
            if let Some(n) = is_null.as_mut() {
                n[idx] = n[idx] || is_null_result;
            }
        }
    }
    Ok((selected, is_null))
}

/// vectorizedFilter filters by vector.
// go: vectorizedFilter L1
#[allow(clippy::too_many_arguments)]
fn vectorized_filter_inner(
    ctx: &dyn EvalContext,
    st: &EvalState,
    rpn: &mut RpnState,
    vec_enabled: bool,
    filters: &[ExprRef],
    input: &mut Chunk,
    selected: Vec<bool>,
    is_null: Option<Vec<bool>>,
) -> Result<(Vec<bool>, Option<Vec<bool>>), Error> {
    let was_nil = is_null.is_none();
    let (selected, is_null) = vec_eval_bool(
        ctx,
        st,
        rpn,
        vec_enabled,
        filters,
        input,
        selected,
        is_null.unwrap_or_default(),
    )?;
    // Go: VecEvalBool appends to `nulls[:0]`, so a nil slice stays nil when there are no rows.
    Ok((
        selected,
        (!(was_nil && is_null.is_empty())).then_some(is_null),
    ))
}
