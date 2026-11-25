//! go-file: pkg/executor/internal/vecgroupchecker/vec_group_checker.go
//!
//! The vectorized group checker (block D12.0, the executor framework; ported with the stream
//! aggregation of D12.3 part A, spec `specs/D12.3-agg-executors/spec.md` §2.5).
//!
//! Differences in shape from Go (spec §4.1, §6):
//! - The evaluation state of the group-by items (N4 §3.4: correlated values, the vector
//!   evaluation's scratch columns) is passed to [`VecGroupChecker::split_into_groups`] by the
//!   executor, which owns it; the RPN scratch state of `EvalExpr` belongs to the checker.
//! - Go's `allocateBuffer` / `releaseBuffer` (test hooks over `expression.GetColumn` /
//!   `PutColumn`, a column pool) are not kept (M-POOL): each evaluation gets a new column.
//!   Go's `TestVecGroupCheckerDATARACE` sets them to evaluate into the input chunk's column;
//!   its port evaluates into a new column and checks the same thing (the datums are copies).

// Go identifiers for the evaluation types in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_chunk::chunk::Chunk;
use tidb_codec::codec::{convert_by_collation_str, encode_key};
use tidb_expression::ExprRef;
use tidb_expression::context::EvalContext;
use tidb_expression::eval::RpnState;
use tidb_expression::eval::rt::empty_column;
use tidb_expression::expression::{EvalState, eval_expr};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32,
};
use tidb_sessionctx::exprctx::EvalContextRef;
use tidb_types::MyDecimal;
use tidb_types::datum::Datum;
use tidb_types::json::compare_binary_json;

/// VecGroupChecker is used to split a given chunk according to the `group by` expression in a vectorized manner
/// It is usually used for streamAgg
pub struct VecGroupChecker {
    pub(crate) ctx: EvalContextRef,
    // Go's releaseBuffer / allocateBuffer (test hooks) are not kept (M-POOL, see the file
    // comment).
    pub(crate) last_row_datums: Vec<Datum>,

    /// lastGroupKeyOfPrevChk is the groupKey of the last group of the previous chunk
    pub(crate) last_group_key_of_prev_chk: Vec<u8>,
    /// firstGroupKey and lastGroupKey are used to store the groupKey of the first and last group of the current chunk
    pub(crate) first_group_key: Vec<u8>,
    pub(crate) last_group_key: Vec<u8>,

    /// firstRowDatums and lastRowDatums store the results of the expression evaluation
    /// for the first and last rows of the current chunk in datum
    /// They are used to encode to get firstGroupKey and lastGroupKey
    pub(crate) first_row_datums: Vec<Datum>,

    /// sameGroup is used to check whether the current row belongs to the same group as the previous row
    pub(crate) same_group: Vec<bool>,

    /// groupOffset holds the offset of the last row in each group of the current chunk
    pub(crate) group_offset: Vec<usize>,
    pub group_by_items: Vec<ExprRef>,

    /// nextGroupID records the group id of the next group to be consumed
    pub(crate) next_group_id: usize,

    /// groupCount is the count of groups in the current chunk
    pub(crate) group_count: usize,

    /// vecEnabled indicates whether to use vectorized evaluation or not.
    pub(crate) vec_enabled: bool,

    /// The scratch state of the vector evaluation of the group-by items (`EvalExpr`, N4).
    pub(crate) rpn: RpnState,
}

/// NewVecGroupChecker creates a new VecGroupChecker
// go: NewVecGroupChecker L1
pub fn new_vec_group_checker(
    ctx: EvalContextRef,
    vec_enabled: bool,
    items: Vec<ExprRef>,
) -> VecGroupChecker {
    VecGroupChecker {
        ctx,
        vec_enabled,
        group_by_items: items,
        group_count: 0,
        next_group_id: 0,
        same_group: vec![false; 1024],
        last_row_datums: Vec::new(),
        last_group_key_of_prev_chk: Vec::new(),
        first_group_key: Vec::new(),
        last_group_key: Vec::new(),
        first_row_datums: Vec::new(),
        group_offset: Vec::new(),
        rpn: RpnState::default(),
    }
}

/// Go's `errors.Errorf("unsupported type %s during evaluation", eType)`.
// glue: the default branch of getFirstAndLastRowDatum / evalGroupItemsAndResolveGroups
fn unsupported_type(e_type: tidb_parser::types::EvalType) -> Error {
    Error::plain(format!(
        "unsupported type {} during evaluation",
        e_type.to_string()
    ))
}

impl VecGroupChecker {
    /// SplitIntoGroups splits a chunk into multiple groups which the row in the same group have the same groupKey
    /// `isFirstGroupSameAsPrev` indicates whether the groupKey of the first group of the newly passed chunk is equal to the groupKey of the last group left before
    /// TODO: Since all the group by items are only a column reference, guaranteed by building projection below aggregation, we can directly compare data in a chunk.
    ///
    /// `st` is the evaluation state of the group-by items, owned by the executor (N4).
    // go: VecGroupChecker.SplitIntoGroups L2 (the evaluation state is passed in, N4 §3.4)
    pub fn split_into_groups(&mut self, st: &EvalState, chk: &Chunk) -> Result<bool, Error> {
        // The numRows can not be zero. `fetchChild` is called before `splitIntoGroups` is called.
        // if numRows == 0, it will be returned in `fetchChild`. See `fetchChild` for more details.
        let num_rows = chk.num_rows();

        self.reset();
        self.next_group_id = 0;
        if self.group_by_items.is_empty() {
            self.group_offset.push(num_rows);
            self.group_count = 1;
            return Ok(true);
        }

        let items = self.group_by_items.clone();
        for item in &items {
            self.get_first_and_last_row_datum(st, item, chk, num_rows)?;
        }
        let ec = self.ctx.err_ctx();
        let loc = self.ctx.location();
        if let Err(err) = encode_key(&loc, &mut self.first_group_key, &self.first_row_datums) {
            ec.handle_error(err)?;
        }

        if let Err(err) = encode_key(&loc, &mut self.last_group_key, &self.last_row_datums) {
            ec.handle_error(err)?;
        }

        let is_first_group_same_as_prev = if self.last_group_key_of_prev_chk.is_empty() {
            false
        } else {
            self.last_group_key_of_prev_chk == self.first_group_key
        };

        // Go truncates or reallocates `lastGroupKeyOfPrevChk`, then copies the key into it.
        self.last_group_key_of_prev_chk.clear();
        self.last_group_key_of_prev_chk
            .extend_from_slice(&self.last_group_key);

        if self.first_group_key == self.last_group_key {
            self.group_offset.push(num_rows);
            self.group_count = 1;
            return Ok(is_first_group_same_as_prev);
        }

        self.same_group.reserve(num_rows);
        self.same_group.push(false);
        for _ in 1..num_rows {
            self.same_group.push(true);
        }

        let vec_enabled = self.vec_enabled;
        for item in &items {
            self.eval_group_items_and_resolve_groups(st, item, vec_enabled, chk, num_rows)?;
        }

        for i in 1..num_rows {
            if !self.same_group[i] {
                self.group_offset.push(i);
            }
        }
        self.group_offset.push(num_rows);
        self.group_count = self.group_offset.len();
        Ok(is_first_group_same_as_prev)
    }

    /// Go makes a copy of each string / decimal / JSON / vector value "to avoid DATA RACE";
    /// the copies are kept (a decimal goes through its string form, as Go).
    // go: VecGroupChecker.getFirstAndLastRowDatum L1
    fn get_first_and_last_row_datum(
        &mut self,
        st: &EvalState,
        item: &ExprRef,
        chk: &Chunk,
        num_rows: usize,
    ) -> Result<(), Error> {
        let ctx: &dyn EvalContext = &*self.ctx;
        let mut first_row_datum = Datum::default();
        let mut last_row_datum = Datum::default();
        let tp = item.get_type(ctx).into_owned();
        let e_type = tp.eval_type();
        let first_row = chk.get_row(0);
        let last_row = chk.get_row(num_rows - 1);
        match e_type {
            ETInt => {
                let first_row_val = item.eval_int(ctx, st, first_row)?;
                let last_row_val = item.eval_int(ctx, st, last_row)?;
                match first_row_val {
                    Some(v) => first_row_datum.set_int64(v),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => last_row_datum.set_int64(v),
                    None => last_row_datum.set_null(),
                }
            }
            ETReal => {
                let first_row_val = item.eval_real(ctx, st, first_row)?;
                let last_row_val = item.eval_real(ctx, st, last_row)?;
                match first_row_val {
                    Some(v) => first_row_datum.set_float64(v),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => last_row_datum.set_float64(v),
                    None => last_row_datum.set_null(),
                }
            }
            ETDecimal => {
                let first_row_val = item.eval_decimal(ctx, st, first_row)?;
                let last_row_val = item.eval_decimal(ctx, st, last_row)?;
                match first_row_val {
                    Some(v) => {
                        // make a copy to avoid DATA RACE
                        let mut first_datum = MyDecimal::default();
                        first_datum.from_string(&v.to_string())?;
                        first_row_datum.set_mysql_decimal(first_datum);
                    }
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => {
                        // make a copy to avoid DATA RACE
                        let mut last_datum = MyDecimal::default();
                        last_datum.from_string(&v.to_string())?;
                        last_row_datum.set_mysql_decimal(last_datum);
                    }
                    None => last_row_datum.set_null(),
                }
            }
            ETDatetime | ETTimestamp => {
                let first_row_val = item.eval_time(ctx, st, first_row)?;
                let last_row_val = item.eval_time(ctx, st, last_row)?;
                match first_row_val {
                    Some(v) => first_row_datum.set_mysql_time(v),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => last_row_datum.set_mysql_time(v),
                    None => last_row_datum.set_null(),
                }
            }
            ETDuration => {
                let first_row_val = item.eval_duration(ctx, st, first_row)?;
                let last_row_val = item.eval_duration(ctx, st, last_row)?;
                match first_row_val {
                    Some(v) => first_row_datum.set_mysql_duration(v),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => last_row_datum.set_mysql_duration(v),
                    None => last_row_datum.set_null(),
                }
            }
            ETJson => {
                let first_row_val = item.eval_json(ctx, st, first_row)?;
                let last_row_val = item.eval_json(ctx, st, last_row)?;
                match first_row_val {
                    // make a copy to avoid DATA RACE
                    Some(v) => first_row_datum.set_mysql_json(v.copy()),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    // make a copy to avoid DATA RACE
                    Some(v) => last_row_datum.set_mysql_json(v.copy()),
                    None => last_row_datum.set_null(),
                }
            }
            ETVectorFloat32 => {
                let first_row_val = item.eval_vector_float32(ctx, st, first_row)?;
                let last_row_val = item.eval_vector_float32(ctx, st, last_row)?;
                match first_row_val {
                    // make a copy to avoid DATA RACE
                    Some(v) => first_row_datum.set_vector_float32(v.clone()),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    // make a copy to avoid DATA RACE
                    Some(v) => last_row_datum.set_vector_float32(v.clone()),
                    None => last_row_datum.set_null(),
                }
            }
            ETString => {
                let first_row_val = item.eval_string(ctx, st, first_row)?;
                let last_row_val = item.eval_string(ctx, st, last_row)?;
                match first_row_val {
                    // make a copy to avoid DATA RACE (the evaluated string is owned already)
                    Some(v) => first_row_datum.set_string(v, tp.get_collate()),
                    None => first_row_datum.set_null(),
                }
                match last_row_val {
                    Some(v) => last_row_datum.set_string(v, tp.get_collate()),
                    None => last_row_datum.set_null(),
                }
            }
            _ => return Err(unsupported_type(e_type)),
        }

        self.first_row_datums.push(first_row_datum);
        self.last_row_datums.push(last_row_datum);
        Ok(())
    }

    /// evalGroupItemsAndResolveGroups evaluates the chunk according to the expression item.
    /// And resolve the rows into groups according to the evaluation results
    ///
    /// The values are read with the column's per-row getters (Go reads its typed slices):
    /// the comparisons are Go's (`!=` of `int64` / `float64` / `time.Duration`, `Compare`).
    // go: VecGroupChecker.evalGroupItemsAndResolveGroups L2 (no column pool, M-POOL; the evaluation state is passed in, N4 §3.4; JSON / vector keys as row indices, M-ROW)
    // The loops index `sameGroup` and the column by row as Go does.
    #[allow(clippy::needless_range_loop)]
    fn eval_group_items_and_resolve_groups(
        &mut self,
        st: &EvalState,
        item: &ExprRef,
        vec_enabled: bool,
        chk: &Chunk,
        num_rows: usize,
    ) -> Result<(), Error> {
        let ctx = Arc::clone(&self.ctx);
        let ctx: &dyn EvalContext = &*ctx;
        let tp = item.get_type(ctx).into_owned();
        let e_type = tp.eval_type();
        let mut col = empty_column();
        eval_expr(
            ctx,
            st,
            &mut self.rpn,
            vec_enabled,
            item,
            e_type,
            chk,
            &mut col,
        )?;

        let same_group = &mut self.same_group;
        let mut previous_is_null = col.is_null(0);
        match e_type {
            ETInt => {
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if same_group[i] {
                        if !previous_is_null && !is_null {
                            if col.get_int64(i) != col.get_int64(i - 1) {
                                same_group[i] = false;
                            }
                        } else if is_null != previous_is_null {
                            same_group[i] = false;
                        }
                    }
                    previous_is_null = is_null;
                }
            }
            ETReal => {
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if same_group[i] {
                        if !previous_is_null && !is_null {
                            #[allow(clippy::float_cmp)]
                            if col.get_float64(i) != col.get_float64(i - 1) {
                                same_group[i] = false;
                            }
                        } else if is_null != previous_is_null {
                            same_group[i] = false;
                        }
                    }
                    previous_is_null = is_null;
                }
            }
            ETDecimal => {
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if same_group[i] {
                        if !previous_is_null && !is_null {
                            if col.get_decimal(i).compare(&col.get_decimal(i - 1)) != 0 {
                                same_group[i] = false;
                            }
                        } else if is_null != previous_is_null {
                            same_group[i] = false;
                        }
                    }
                    previous_is_null = is_null;
                }
            }
            ETDatetime | ETTimestamp => {
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if same_group[i] {
                        if !previous_is_null && !is_null {
                            if col.get_time(i).compare(&col.get_time(i - 1)) != 0 {
                                same_group[i] = false;
                            }
                        } else if is_null != previous_is_null {
                            same_group[i] = false;
                        }
                    }
                    previous_is_null = is_null;
                }
            }
            ETDuration => {
                let vals = col.go_durations();
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if same_group[i] {
                        if !previous_is_null && !is_null {
                            if vals[i].get() != vals[i - 1].get() {
                                same_group[i] = false;
                            }
                        } else if is_null != previous_is_null {
                            same_group[i] = false;
                        }
                    }
                    previous_is_null = is_null;
                }
            }
            ETJson => {
                // The keys are kept as row indices (Go keeps the values). Go's zero
                // `BinaryJSON` before the first non-null value is `None`: it is never
                // compared (a comparison needs the previous row to be non-null).
                let mut previous_key: Option<usize> = None;
                let mut key: Option<usize> = None;
                if !previous_is_null {
                    previous_key = Some(0);
                }
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if !is_null {
                        key = Some(i);
                    }
                    if same_group[i] {
                        if is_null == previous_is_null {
                            if !is_null
                                && let (Some(p), Some(k)) = (previous_key, key)
                                && compare_binary_json(col.get_json(p), col.get_json(k)) != 0
                            {
                                same_group[i] = false;
                            }
                        } else {
                            same_group[i] = false;
                        }
                    }
                    if !is_null {
                        previous_key = key;
                    }
                    previous_is_null = is_null;
                }
            }
            ETVectorFloat32 => {
                // As for JSON: row indices, `None` for Go's zero vector.
                let mut previous_key: Option<usize> = None;
                let mut key: Option<usize> = None;
                if !previous_is_null {
                    previous_key = Some(0);
                }
                for i in 1..num_rows {
                    let is_null = col.is_null(i);
                    if !is_null {
                        key = Some(i);
                    }
                    if same_group[i] {
                        if is_null == previous_is_null {
                            if !is_null
                                && let (Some(p), Some(k)) = (previous_key, key)
                                && col
                                    .get_vector_float32(p)
                                    .compare(&col.get_vector_float32(k))
                                    != 0
                            {
                                same_group[i] = false;
                            }
                        } else {
                            same_group[i] = false;
                        }
                    }
                    if !is_null {
                        previous_key = key;
                    }
                    previous_is_null = is_null;
                }
            }
            ETString => {
                let mut previous_key = convert_by_collation_str(col.get_string(0), &tp);
                for i in 1..num_rows {
                    let key = convert_by_collation_str(col.get_string(i), &tp);
                    let is_null = col.is_null(i);
                    if same_group[i] && (is_null != previous_is_null || previous_key != key) {
                        same_group[i] = false;
                    }
                    previous_key = key;
                    previous_is_null = is_null;
                }
            }
            _ => return Err(unsupported_type(e_type)),
        }
        Ok(())
    }

    /// GetNextGroup returns the begin and end position of the next group.
    // go: VecGroupChecker.GetNextGroup L1
    pub fn get_next_group(&mut self) -> (usize, usize) {
        let begin = if self.next_group_id == 0 {
            0
        } else {
            self.group_offset[self.next_group_id - 1]
        };
        let end = self.group_offset[self.next_group_id];
        self.next_group_id += 1;
        (begin, end)
    }

    /// IsExhausted returns true if there is no more group to check.
    // go: VecGroupChecker.IsExhausted L1
    pub fn is_exhausted(&self) -> bool {
        self.next_group_id >= self.group_count
    }

    /// Reset resets the group checker.
    ///
    /// Go resets `groupCount` only when `groupOffset` is not nil; it is nil only before the
    /// first split, when `groupCount` is 0 anyway, so the reset is unconditional here.
    // go: VecGroupChecker.Reset L1
    pub fn reset(&mut self) {
        self.group_offset.clear();
        self.group_count = 0;
        self.same_group.clear();
        self.first_group_key.clear();
        self.last_group_key.clear();
        self.first_row_datums.clear();
        self.last_row_datums.clear();
    }

    /// GroupCount returns the number of groups.
    // go: VecGroupChecker.GroupCount L1
    pub fn group_count(&self) -> usize {
        self.group_count
    }
}
