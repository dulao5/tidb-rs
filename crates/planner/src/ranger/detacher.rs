//! go-file: pkg/util/ranger/detacher.go
//!
//! Detaching the access conditions of an index / a column from the filter conditions and
//! building their ranges; the `tidb_shard()` prefix of shard indexes.
//!
//! Mapping (D10.4 spec §3):
//! - `[]*valueInfo` is `Vec<Option<ValueInfo>>`; Go only replaces slots of these slices
//!   (never a `valueInfo` in place), so values behave the same. `valueInfo.value` points at
//!   the constant's datum in Go; here a copy.
//! - `ExtractEqAndInCondition`'s `accesses` (a slice with nil holes, cut at the first hole)
//!   is `Vec<Option<ExprRef>>` while it is built.
//! - `MergeDNFItems4Col` iterates a map in Go (random order); here ascending `UniqueID`
//!   (M-MAPORDER).
//! - The functions that rebuild conditions (`ExtractEqAndInCondition` through
//!   `points2EqOrInCond`) return `Result` for the D08.9 boundary of the IN builtin.

// AST function names keep their Go names (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::mutrow::mut_row_from_datums;
use tidb_expression::constant_fold::maybe_over_optimized4_plan_cache;
use tidb_expression::expression::{
    compose_cnf_condition, compose_dnf_condition, flatten_cnf_conditions, flatten_dnf_conditions,
};
use tidb_expression::scalar_function::{ast_func as ast, new_function};
use tidb_expression::util::{contains, extract_column_set, extract_columns, filter};
use tidb_expression::{Column, Constant, EvalState, Expr, ExprRef, ScalarFunction};
use tidb_parser::types::{ETInt, ETString};
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_sessionctx::exprctx::{BuildContext, EvalContext};
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::ranger::types::{
    Ranges, full_range, ranges_intersect_ranges, ranges_mem_usage, ranges_subset,
};
use tidb_types::collate;
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::*;
use tidb_types::field_type::new_field_type;

use super::checker::ConditionChecker;
use super::points::{Builder, Point, eval_const};
use super::ranger::{
    append_ranges2_point_ranges, convert_string_ft_to_binary_collate, has_prefix,
    new_field_type_for_ranger, points2_eq_or_in_cond, points2_ranges, union_ranges,
};
use crate::util::fixcontrol;

/// detachColumnCNFConditions detaches the condition for calculating range from the other conditions.
/// Please make sure that the top level is CNF form.
// go: detachColumnCNFConditions L1
fn detach_column_cnf_conditions(
    sctx: &dyn BuildContext,
    conditions: &[ExprRef],
    checker: &ConditionChecker<'_>,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut access_conditions: Vec<ExprRef> = Vec::new();
    let mut filter_conditions: Vec<ExprRef> = Vec::new();
    for cond in conditions {
        if let Expr::ScalarFunction(sf) = &**cond
            && sf.func_name.l == ast::LogicOr
        {
            let dnf_items = flatten_dnf_conditions(sf);
            let (column_dnf_items, has_residual) =
                detach_column_dnf_conditions(sctx, &dnf_items, checker);
            // If this CNF has expression that cannot be resolved as access condition, then the total DNF expression
            // should be also appended into filter condition.
            if has_residual {
                filter_conditions.push(Arc::clone(cond));
            }
            if column_dnf_items.is_empty() {
                continue;
            }
            if let Some(rebuild_dnf) = compose_dnf_condition(sctx, &column_dnf_items) {
                access_conditions.push(rebuild_dnf);
            }
            continue;
        }
        let (is_access_cond, should_reserve) = checker.check(cond);
        if !is_access_cond {
            filter_conditions.push(Arc::clone(cond));
            continue;
        }
        access_conditions.push(Arc::clone(cond));
        if should_reserve {
            filter_conditions.push(Arc::clone(cond));
        }
    }
    (access_conditions, filter_conditions)
}

/// detachColumnDNFConditions detaches the condition for calculating range from the other conditions.
/// Please make sure that the top level is DNF form.
// go: detachColumnDNFConditions L1
fn detach_column_dnf_conditions(
    sctx: &dyn BuildContext,
    conditions: &[ExprRef],
    checker: &ConditionChecker<'_>,
) -> (Vec<ExprRef>, bool) {
    let mut has_residual_conditions = false;
    let mut access_conditions: Vec<ExprRef> = Vec::new();
    for cond in conditions {
        if let Expr::ScalarFunction(sf) = &**cond
            && sf.func_name.l == ast::LogicAnd
        {
            let cnf_items = flatten_cnf_conditions(sf);
            let (column_cnf_items, others) =
                detach_column_cnf_conditions(sctx, &cnf_items, checker);
            if !others.is_empty() {
                has_residual_conditions = true;
            }
            // If one part of DNF has no access condition. Then this DNF cannot get range.
            if column_cnf_items.is_empty() {
                return (Vec::new(), true);
            }
            if let Some(rebuild_cnf) = compose_cnf_condition(sctx, &column_cnf_items) {
                access_conditions.push(rebuild_cnf);
            }
        } else {
            let (is_access_cond, should_reserve) = checker.check(cond);
            if !is_access_cond {
                return (Vec::new(), true);
            }
            access_conditions.push(Arc::clone(cond));
            if should_reserve {
                has_residual_conditions = true;
            }
        }
    }
    (access_conditions, has_residual_conditions)
}

/// getPotentialEqOrInColOffset checks if the expression is a eq/le/ge/lt/gt function that one side is constant and another is column or an
/// in function which is `column in (constant list)`.
/// If so, it will return the offset of this column in the slice, otherwise return -1 for not found.
/// Since combining `x >= 2` and `x <= 2` can lead to an eq condition `x = 2`, we take le/ge/lt/gt into consideration.
///
/// Notice: in points2EqOrInCond, when we convert points to acess condition reversely,
/// we will build isnull func again from a single point range with null value,
/// when we find the ref col is with not null flag, it will output zero constant
/// which breaks the function check outside. That's why we abandon the nulleq range detecting,
/// treat it as non-eq-in condition for later range build.
///
/// Go's `intest.AssertFunc` (a test-build assertion on `RegardNULLAsPoint`) is not ported.
// go: getPotentialEqOrInColOffset L1
fn get_potential_eq_or_in_col_offset(
    sctx: &RangerContext,
    expr: &Expr,
    cols: &[Arc<Column>],
) -> isize {
    let eval_ctx = sctx.expr_ctx.get_eval_ctx();
    let Expr::ScalarFunction(f) = expr else {
        return -1;
    };
    let (_, collation) = expr.charset_and_collation();
    let args = f.get_args();
    match f.func_name.l.as_str() {
        ast::LogicOr => {
            let dnf_items = flatten_dnf_conditions(f);
            let mut offset: isize = -1;
            for dnf_item in &dnf_items {
                let cur_offset = get_potential_eq_or_in_col_offset(sctx, dnf_item, cols);
                if cur_offset == -1 {
                    return -1;
                }
                if offset != -1 && cur_offset != offset {
                    return -1;
                }
                offset = cur_offset;
            }
            return offset;
        }
        ast::EQ | ast::NullEQ | ast::LE | ast::GE | ast::LT | ast::GT => {
            if let Expr::Column(c) = &*args[0] {
                if c.ret_type.eval_type() == ETString
                    && !collate::compatible_collate(c.ret_type.get_collate(), collation)
                {
                    return -1;
                }
                if (f.func_name.l == ast::LT || f.func_name.l == ast::GT)
                    && c.ret_type.eval_type() != ETInt
                {
                    return -1;
                }
                if let Expr::Constant(_) = &*args[1] {
                    let val = eval_const(sctx, &args[1]);
                    let null = val.as_ref().is_ok_and(|v| v.is_null());
                    if val.is_err()
                        || (!sctx.regard_null_as_point && null)
                        || (f.func_name.l == ast::NullEQ && null)
                    {
                        // treat col<=>null as range scan instead of point get to avoid incorrect results
                        // when nullable unique index has multiple matches for filter x is null
                        return -1;
                    }
                    for (i, col) in cols.iter().enumerate() {
                        // When cols are a generated expression col, compare them in terms of virtual expr.
                        if col.equal_by_expr_and_id(&*eval_ctx, c) {
                            return i as isize;
                        }
                    }
                }
            }
            if let Expr::Column(c) = &*args[1] {
                if c.ret_type.eval_type() == ETString
                    && !collate::compatible_collate(c.ret_type.get_collate(), collation)
                {
                    return -1;
                }
                if (f.func_name.l == ast::LT || f.func_name.l == ast::GT)
                    && c.ret_type.eval_type() != ETInt
                {
                    return -1;
                }
                if let Expr::Constant(_) = &*args[0] {
                    let val = eval_const(sctx, &args[0]);
                    let null = val.as_ref().is_ok_and(|v| v.is_null());
                    if val.is_err() || (!sctx.regard_null_as_point && null) {
                        return -1;
                    }
                    for (i, col) in cols.iter().enumerate() {
                        if col.equal_column(&args[1]) {
                            return i as isize;
                        }
                    }
                }
            }
        }
        ast::In => {
            let Expr::Column(c) = &*args[0] else {
                return -1;
            };
            if c.ret_type.eval_type() == ETString
                && !collate::compatible_collate(c.ret_type.get_collate(), collation)
            {
                return -1;
            }
            for arg in &args[1..] {
                if !matches!(&**arg, Expr::Constant(_)) {
                    return -1;
                }
            }
            for (i, col) in cols.iter().enumerate() {
                if col.equal_column(&args[0]) {
                    return i as isize;
                }
            }
        }
        ast::IsNull => {
            if !matches!(&*args[0], Expr::Column(_)) {
                return -1;
            }
            for (i, col) in cols.iter().enumerate() {
                if col.equal_column(&args[0]) {
                    return i as isize;
                }
            }
        }
        _ => {}
    }
    -1
}

/// `cnfItemRangeResult`.
struct CnfItemRangeResult {
    range_result: DetachRangeResult,
    offset: usize,
    /// sameLenPointRanges means that each range is point range and all of them have the same column numbers(i.e., maxColNum = minColNum).
    same_len_point_ranges: bool,
    max_col_num: usize,
    min_col_num: usize,
}

// go: getCNFItemRangeResult L1
fn get_cnf_item_range_result(
    sctx: &RangerContext,
    range_result: DetachRangeResult,
    offset: usize,
) -> CnfItemRangeResult {
    let mut same_len_point_ranges = true;
    let (mut max_col_num, mut min_col_num) = (0usize, 0usize);
    for (i, ran) in range_result.ranges.iter().enumerate() {
        if !ran.is_point(sctx) {
            same_len_point_ranges = false;
        }
        if i == 0 {
            max_col_num = ran.low_val.len();
            min_col_num = ran.low_val.len();
        } else {
            max_col_num = max_col_num.max(ran.low_val.len());
            min_col_num = min_col_num.min(ran.low_val.len());
        }
    }
    if min_col_num != max_col_num {
        same_len_point_ranges = false;
    }
    CnfItemRangeResult {
        range_result,
        offset,
        same_len_point_ranges,
        max_col_num,
        min_col_num,
    }
}

// go: compareCNFItemRangeResult L1
fn compare_cnf_item_range_result(
    cur_result: &CnfItemRangeResult,
    best_result: &CnfItemRangeResult,
) -> bool {
    if cur_result.same_len_point_ranges && best_result.same_len_point_ranges {
        return cur_result.min_col_num > best_result.min_col_num;
    }
    if !cur_result.same_len_point_ranges && !best_result.same_len_point_ranges {
        if cur_result.min_col_num == best_result.min_col_num {
            return cur_result.max_col_num > best_result.max_col_num;
        }
        return cur_result.min_col_num > best_result.min_col_num;
    }
    // Point ranges is better than non-point ranges since we can append subsequent column ranges to point ranges.
    cur_result.same_len_point_ranges
}

/// mergeTwoCNFRanges merges two ranges results rangeResult and otherRangeResult.
/// The main objective of this function is to apply intersection between these two
/// ranges when possible. The overall logic is:
/// - if rangeResult is empty then result is otherRangeResult
/// - Skip intersection logic if it is turned off (Fix54337)
/// - If either range is a subset then set result to the subset.
/// - Result = intersection of the two ranges
/// - Try heuristic to pick which range is better if intersections fails or if feature is off.
///
/// Go's `otherRangeResult` is never nil (its caller passes a new result); a value here.
// go: mergeTwoCNFRanges L1
fn merge_two_cnf_ranges(
    sctx: &RangerContext,
    cond: &ExprRef,
    range_result: Option<CnfItemRangeResult>,
    other_range_result: CnfItemRangeResult,
) -> CnfItemRangeResult {
    let Some(range_result) = range_result else {
        return other_range_result;
    };
    let mut try_heuristic = false;
    let mut merged_result = range_result;
    let mut other = Some(other_range_result);
    if fixcontrol::get_bool_with_default(&sctx.optimizer_fix_control, fixcontrol::Fix54337, false) {
        let o = other.as_ref().expect("set");
        let merged_result_is_subset = ranges_subset(
            &merged_result.range_result.ranges,
            &sctx.type_ctx,
            &o.range_result.ranges,
        );
        // if mergedResult is a subset then do nothing
        if !merged_result_is_subset {
            let other_range_result_is_subset = ranges_subset(
                &o.range_result.ranges,
                &sctx.type_ctx,
                &merged_result.range_result.ranges,
            );
            // if otherRangeResult is subset (more selective) then make it mergedResult.
            if other_range_result_is_subset {
                // Go: mergedResult = otherRangeResult (the old merged result is dropped; the
                // heuristic below is not tried).
                let o = other.take().expect("set");
                merged_result = o;
                return merged_result;
            }
            // Try intersecting result of different conjuncts.
            let intersection = ranges_intersect_ranges(
                &o.range_result.ranges,
                &sctx.type_ctx,
                &merged_result.range_result.ranges,
            );
            // Skip intersection if an error occurred during the intersection computation.
            match intersection {
                None => try_heuristic = true,
                Some(intersection) => {
                    merged_result.range_result.ranges = intersection;
                    let ectx = sctx.expr_ctx.get_eval_ctx();
                    let access = std::mem::take(&mut merged_result.range_result.access_conds);
                    merged_result.range_result.access_conds =
                        append_conditions_if_not_exist(&*ectx, access, &[Arc::clone(cond)]);
                }
            }
        }
    } else {
        try_heuristic = true;
    }
    if try_heuristic {
        let o = other.take().expect("set");
        if compare_cnf_item_range_result(&o, &merged_result) {
            return o;
        }
    }
    merged_result
}

/// extractBestCNFItemRanges builds ranges for each CNF item from the input CNF expressions and returns the best CNF
/// item ranges.
/// e.g, for input CNF expressions ((a,b) in ((1,1),(2,2))) and a > 1 and ((a,b,c) in (1,1,1),(2,2,2))
/// ((a,b,c) in (1,1,1),(2,2,2)) would be extracted.
// go: extractBestCNFItemRanges L1
fn extract_best_cnf_item_ranges(
    sctx: &RangerContext,
    conds: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
    range_max_size: i64,
    convert_to_sort_key: bool,
) -> Result<(Option<CnfItemRangeResult>, Option<ColumnValues>), Error> {
    if conds.len() < 2 {
        return Ok((None, None));
    }
    let mut best_res: Option<CnfItemRangeResult> = None;
    let mut column_values: Option<Vec<Option<ValueInfo>>> = Some(vec![None; cols.len()]);
    for (i, cond) in conds.iter().enumerate() {
        let tmp_conds = vec![Arc::clone(cond)];
        let col_sets = extract_column_set(std::slice::from_ref(cond));
        if col_sets.is_empty() {
            continue;
        }
        // When we build ranges for the CNF item, we choose not to merge consecutive ranges because we hope to get point
        // ranges here. See https://github.com/pingcap/tidb/issues/41572 for more details.
        //
        // Here is an example. Assume that the index is `idx(a,b,c)` and the condition is `((a,b) in ((1,1),(1,2)) and c = 1`.
        // We build ranges for `(a,b) in ((1,1),(1,2))` and get `[1 1, 1 1] [1 2, 1 2]`, which are point ranges and we can
        // append `c = 1` to the point ranges. However, if we choose to merge consecutive ranges here, we get `[1 1, 1 2]`,
        // which are not point ranges, and we cannot append `c = 1` anymore.
        let res = detach_cond_and_build_range(
            sctx,
            &tmp_conds,
            cols,
            lengths,
            range_max_size,
            convert_to_sort_key,
            false,
        )?;
        if res.ranges.is_empty() {
            return Ok((
                Some(CnfItemRangeResult {
                    range_result: res,
                    offset: i,
                    same_len_point_ranges: false,
                    max_col_num: 0,
                    min_col_num: 0,
                }),
                None,
            ));
        }
        // take the union of the two columnValues
        column_values = union_column_values(column_values, res.column_values.clone());
        if res.access_conds.is_empty() {
            continue;
        }
        let cur_res = get_cnf_item_range_result(sctx, res, i);
        best_res = Some(merge_two_cnf_ranges(sctx, cond, best_res, cur_res));
    }
    if let Some(best) = best_res.as_mut() {
        best.range_result.is_dnf_cond = false;
    }
    Ok((best_res, column_values))
}

/// Go's nil `[]*valueInfo` is `None`.
// go: unionColumnValues L1
fn union_column_values(
    lhs: Option<Vec<Option<ValueInfo>>>,
    rhs: Option<Vec<Option<ValueInfo>>>,
) -> Option<Vec<Option<ValueInfo>>> {
    let Some(mut lhs) = lhs else {
        return rhs;
    };
    if let Some(rhs) = rhs {
        for i in 0..lhs.len() {
            if i >= rhs.len() {
                break;
            }
            if lhs[i].is_none() && rhs[i].is_some() {
                lhs[i] = rhs[i].clone();
            }
        }
    }
    Some(lhs)
}

impl RangeDetacher<'_> {
    /// detachCNFCondAndBuildRangeForIndex will detach the index filters from table filters. These conditions are connected with `and`
    /// It will first find the point query column and then extract the range query column.
    /// considerDNF is true means it will try to extract access conditions from the DNF expressions.
    // go: rangeDetacher.detachCNFCondAndBuildRangeForIndex L1
    fn detach_cnf_cond_and_build_range_for_index(
        &self,
        conditions: &[ExprRef],
        new_tp_slice: &[FieldType],
        consider_dnf: bool,
    ) -> Result<DetachRangeResult, Error> {
        let mut eq_count = 0usize;
        let mut res = DetachRangeResult::default();
        let ectx = self.sctx.expr_ctx.get_eval_ctx();

        let ext = extract_eq_and_in_condition(self.sctx, conditions, self.cols, self.lengths)?;
        let Some((access_conds, mut filter_conds, mut new_conditions, column_values)) = ext else {
            return Ok(res);
        };
        let (mut ranges, access_conds, remained_conds) =
            self.build_range_on_cols_by_cnf_cond(new_tp_slice, access_conds.len(), access_conds)?;
        if !remained_conds.is_empty() {
            filter_conds = remove_conditions(&*ectx, &filter_conds, &remained_conds);
            new_conditions.extend(remained_conds.iter().cloned());
        }
        while eq_count < access_conds.len() {
            let Expr::ScalarFunction(sf) = &*access_conds[eq_count] else {
                panic!(
                    "interface conversion: expression.Expression is not *expression.ScalarFunction"
                );
            };
            if sf.func_name.l != ast::EQ {
                break;
            }
            eq_count += 1;
        }
        let mut eq_or_in_count = access_conds.len();
        res.eq_cond_count = eq_count as isize;
        res.eq_or_in_count = eq_or_in_count as isize;
        // If index has prefix column and d.mergeConsecutive is true, ranges may not be point ranges anymore after UnionRanges.
        // Therefore, we need to calculate pointRanges separately so that it can be used to append tail ranges in considerDNF branch.
        // See https://github.com/pingcap/tidb/issues/26029 for details.
        let mut point_ranges: Ranges;
        if has_prefix(self.lengths) {
            if self.merge_consecutive {
                point_ranges = Vec::with_capacity(ranges.len());
                for ran in &ranges {
                    point_ranges.push(ran.clone_range());
                }
                ranges = union_ranges(self.sctx, ranges, self.merge_consecutive)?;
                point_ranges = union_ranges(self.sctx, point_ranges, false)?;
            } else {
                ranges = union_ranges(self.sctx, ranges, self.merge_consecutive)?;
                point_ranges = ranges.clone();
            }
        } else {
            point_ranges = ranges.clone();
        }

        res.ranges = ranges;
        res.access_conds = access_conds;
        res.remained_conds = filter_conds;
        res.column_values = Some(column_values);
        if eq_or_in_count == self.cols.len() || new_conditions.is_empty() {
            res.remained_conds.extend(new_conditions);
            return Ok(res);
        }
        let checker = ConditionChecker {
            checker_col: Some(&self.cols[eq_or_in_count]),
            length: self.lengths[eq_or_in_count],
            opt_prefix_index_single_scan: self.sctx.opt_prefix_index_single_scan,
            ctx: &*ectx,
        };
        if consider_dnf {
            let (mut best_cnf_item_res, column_values) = extract_best_cnf_item_ranges(
                self.sctx,
                conditions,
                self.cols,
                self.lengths,
                self.range_max_size,
                self.convert_to_sort_key,
            )?;
            res.column_values = union_column_values(res.column_values.take(), column_values);
            if let Some(best) = best_cnf_item_res.as_mut() {
                if best.range_result.ranges.is_empty() {
                    return Ok(DetachRangeResult::default());
                }
                if best.same_len_point_ranges && best.min_col_num > eq_or_in_count {
                    let best = best_cnf_item_res.take().expect("set");
                    let offset = best.offset;
                    let mut new_res = best.range_result;
                    new_res.column_values = res.column_values.take();
                    res = new_res;
                    point_ranges = res.ranges.clone();
                    eq_or_in_count = res.ranges[0].low_val.len();
                    new_conditions.clear();
                    new_conditions.extend(conditions[..offset].iter().cloned());
                    new_conditions.extend(conditions[offset + 1..].iter().cloned());
                    if eq_or_in_count == self.cols.len() || new_conditions.is_empty() {
                        res.remained_conds.extend(new_conditions);
                        return Ok(res);
                    }
                } else {
                    let consider_cnf_item_non_point_ranges = fixcontrol::get_bool_with_default(
                        &self.sctx.optimizer_fix_control,
                        fixcontrol::Fix44389,
                        false,
                    );
                    if consider_cnf_item_non_point_ranges
                        && !best.same_len_point_ranges
                        && eq_or_in_count == 0
                        && best.min_col_num > 0
                        && best.max_col_num > 1
                    {
                        // When eqOrInCount is 0, if we don't enter the IF branch, we would use detachColumnCNFConditions to build
                        // ranges on the first index column.
                        // Considering minColNum > 0 and maxColNum > 1, bestCNFItemRes is better than the ranges built by detachColumnCNFConditions
                        // in most cases.
                        let best = best_cnf_item_res.take().expect("set");
                        let offset = best.offset;
                        let mut new_res = best.range_result;
                        new_res.column_values = res.column_values.take();
                        res = new_res;
                        new_conditions.clear();
                        new_conditions.extend(conditions[..offset].iter().cloned());
                        new_conditions.extend(conditions[offset + 1..].iter().cloned());
                        res.remained_conds.extend(new_conditions);
                        return Ok(res);
                    }
                }
            }
            if eq_or_in_count > 0 {
                let new_cols = &self.cols[eq_or_in_count..];
                let new_lengths = &self.lengths[eq_or_in_count..];
                let tail_res = detach_cond_and_build_range(
                    self.sctx,
                    &new_conditions,
                    new_cols,
                    new_lengths,
                    self.range_max_size,
                    self.convert_to_sort_key,
                    self.merge_consecutive,
                )?;
                if tail_res.ranges.is_empty() {
                    return Ok(DetachRangeResult::default());
                }
                if !tail_res.access_conds.is_empty() {
                    let (new_ranges, range_fallback) = append_ranges2_point_ranges(
                        point_ranges,
                        &tail_res.ranges,
                        self.range_max_size,
                    );
                    if range_fallback {
                        self.sctx
                            .range_fallback_handler
                            .record_range_fallback(self.range_max_size);
                        res.remained_conds
                            .extend(tail_res.access_conds.iter().cloned());
                        // Some conditions may be in both tailRes.AccessConds and tailRes.RemainedConds so we call AppendConditionsIfNotExist here.
                        let remained = std::mem::take(&mut res.remained_conds);
                        res.remained_conds = append_conditions_if_not_exist(
                            &*ectx,
                            remained,
                            &tail_res.remained_conds,
                        );
                        return Ok(res);
                    }
                    res.ranges = new_ranges;
                    res.access_conds
                        .extend(tail_res.access_conds.iter().cloned());
                    res.remained_conds
                        .extend(tail_res.remained_conds.iter().cloned());
                    // For cases like `((a = 1 and b = 1) or (a = 2 and b = 2)) and c = 1` on index (a,b,c), eqOrInCount is 2,
                    // res.EqOrInCount is 0, and tailRes.EqOrInCount is 1. We should not set res.EqOrInCount to 1, otherwise,
                    // `b = CorrelatedColumn` would be extracted as access conditions as well, which is not as expected at least for now.
                    if res.eq_or_in_count > 0 {
                        if res.eq_or_in_count == res.eq_cond_count {
                            res.eq_cond_count += tail_res.eq_cond_count;
                        }
                        res.eq_or_in_count += tail_res.eq_or_in_count;
                    }
                    return Ok(res);
                }
                res.remained_conds
                    .extend(tail_res.remained_conds.iter().cloned());
                return Ok(res);
            }
            // `eqOrInCount` must be 0 when coming here.
            let (access, remained) =
                detach_column_cnf_conditions(&*self.sctx.expr_ctx, &new_conditions, &checker);
            res.remained_conds = remained;
            let (ranges, access, remained_conds) =
                self.build_cnf_index_range(new_tp_slice, 0, access)?;
            res.access_conds = access;
            // detachColumnCNFConditions extracts `a = 10 or a = 30` from `(a = 10 and b = 20) or (a = 30 and b = 40)`. If
            // [10, 10] [30, 30] exceeds range mem limit, we add `a = 10 or a = 30` back to RemainedConds, which is actually
            // unnecessary because `(a = 10 and b = 20) or (a = 30 and b = 40)` is already in RemainedConds.
            // TODO: we will optimize it later.
            let remained = std::mem::take(&mut res.remained_conds);
            res.remained_conds = append_conditions_if_not_exist(&*ectx, remained, &remained_conds);
            res.ranges = ranges;
            // Choosing between point ranges and bestCNF is needed since bestCNF does not cover the intersection
            // of all conjuncts. Even when we add support for intersection, it could be turned off by a flag or it could be
            // incomplete due to a long list of conjuncts.
            if let Some(best) = best_cnf_item_res
                && !res.ranges.is_empty()
            {
                let best_cnf_is_subset =
                    ranges_subset(&best.range_result.ranges, &self.sctx.type_ctx, &res.ranges);
                let point_range_is_subset =
                    ranges_subset(&res.ranges, &self.sctx.type_ctx, &best.range_result.ranges);
                // Pick bestCNFIsSubset if it is more selective than point ranges(res).
                // Apply optimization if bestCNFItemRes is a proper subset of point ranges.
                if best_cnf_is_subset && !point_range_is_subset {
                    // Update final result and just update: Ranges, AccessConds and RemainedConds
                    res.remained_conds = remove_conditions(
                        &*ectx,
                        &res.remained_conds,
                        &best.range_result.access_conds,
                    );
                    res.ranges = best.range_result.ranges;
                    res.access_conds = best.range_result.access_conds;
                }
            }
            return Ok(res);
        }
        // TODO: The EQ/IN extraction path (ExtractEqAndInCondition, specifically getPotentialEqOrInColOffset)
        // still rejects columns with binary-casted literals, which causes suffix index columns to be treated
        // as filters. It should apply the same CAST(... AS BINARY) / collation-compatibility exception used
        // in the detacher loop so binary-cast equality predicates are treated as index-equalities. Fix this
        // in a later release and add a regression test for a composite index case (e.g., index on (f,g) with
        // f = CAST('a' AS BINARY) AND g = 1).
        let mut access_conds = std::mem::take(&mut res.access_conds);
        let mut filter_conds = std::mem::take(&mut res.remained_conds);
        for cond in &new_conditions {
            let (is_access_cond, should_reserve) = checker.check(cond);
            if !is_access_cond {
                filter_conds.push(Arc::clone(cond));
                continue;
            }
            access_conds.push(Arc::clone(cond));
            if should_reserve {
                filter_conds.push(Arc::clone(cond));
            }
            // TODO: if it's prefix column, we need to add cond to filterConds?
        }
        let (ranges, access_conds, remained_conds) =
            self.build_cnf_index_range(new_tp_slice, eq_or_in_count, access_conds)?;
        filter_conds.extend(remained_conds);
        res.ranges = ranges;
        res.access_conds = access_conds;
        res.remained_conds = filter_conds;
        Ok(res)
    }
}

/// excludeToIncludeForIntPoint converts `(i` to `[i+1` and `i)` to `i-1]` if `i` is integer.
/// For example, if p is `(3`, i.e., point { value: int(3), excl: true, start: true }, it is equal to `[4`, i.e., point { value: int(4), excl: false, start: true }.
/// Similarly, if p is `8)`, i.e., point { value: int(8), excl: true, start: false}, it is equal to `7]`, i.e., point { value: int(7), excl: false, start: false }.
/// If return value is nil, it means p is unsatisfiable. For example, `(MaxUint64` is unsatisfiable.
/// The boundary value will be treated as the bigger type: For example, `(MaxInt64` of type KindInt64 will become `[MaxInt64+1` of type KindUint64,
/// and vice versa for `0)` of type KindUint64 will become `-1]` of type KindInt64.
///
/// Go changes `*p` in place and returns `p` or nil; here `false` for nil.
// go: excludeToIncludeForIntPoint L1
fn exclude_to_include_for_int_point(p: &mut Point) -> bool {
    if !p.excl {
        return true;
    }
    if p.value.kind() == KindInt64 {
        let val = p.value.get_int64();
        if p.start {
            if val == i64::MAX {
                p.value.set_uint64(val.wrapping_add(1) as u64);
            } else {
                p.value.set_int64(val + 1);
            }
            p.excl = false;
        } else {
            if val == i64::MIN {
                return false;
            }
            p.value.set_int64(val - 1);
            p.excl = false;
        }
    } else if p.value.kind() == KindUint64 {
        let val = p.value.get_uint64();
        if p.start {
            if val == u64::MAX {
                return false;
            }
            p.value.set_uint64(val + 1);
            p.excl = false;
        } else {
            if val == 0 {
                p.value.set_int64(val.wrapping_sub(1) as i64);
            } else {
                p.value.set_uint64(val - 1);
            }
            p.excl = false;
        }
    }
    true
}

/// If there exists an interval whose length is large than 0, return nil. Otherwise remove all unsatisfiable intervals
/// and return array of single point intervals.
///
/// Go's nil (an interval longer than a point) is `None`.
// go: allSinglePoints L1
fn all_single_points(type_ctx: &TypeContext, mut points: Vec<Point>) -> Option<Vec<Point>> {
    let mut pos = 0usize;
    let mut i = 0usize;
    while i < points.len() {
        // Remove unsatisfiable interval. For example, (MaxInt64, +inf) and (-inf, MinInt64) is unsatisfiable.
        if !exclude_to_include_for_int_point(&mut points[i]) {
            i += 2;
            continue;
        }
        if !exclude_to_include_for_int_point(&mut points[i + 1]) {
            i += 2;
            continue;
        }
        let (left, right) = (&points[i], &points[i + 1]);
        // If interval is not a single point, just return nil.
        if !left.start || right.start || left.excl || right.excl {
            return None;
        }
        // Since the point's collations are equal to the column's collation, we can use any of them.
        let (cmp, err) = left.value.compare(
            type_ctx,
            &right.value,
            collate::get_collator(left.value.collation()),
        );
        if err.is_err() || cmp != 0 {
            return None;
        }
        // If interval is a single point, add it back to array.
        points.swap(pos, i);
        points.swap(pos + 1, i + 1);
        pos += 2;
        i += 2;
    }
    points.truncate(pos);
    Some(points)
}

// go: allEqOrIn L1
fn all_eq_or_in(expr: &Expr) -> bool {
    let Expr::ScalarFunction(f) = expr else {
        return false;
    };
    match f.func_name.l.as_str() {
        ast::LogicOr => {
            for arg in f.get_args() {
                if !all_eq_or_in(arg) {
                    return false;
                }
            }
            return true;
        }
        ast::EQ | ast::NullEQ | ast::In | ast::IsNull => return true,
        _ => {}
    }
    false
}

// go: extractValueInfo L1
fn extract_value_info(expr: &Expr) -> Option<ValueInfo> {
    if let Expr::ScalarFunction(f) = expr {
        if f.func_name.l == ast::IsNull {
            let mut val = Datum::default();
            val.set_null();
            return Some(ValueInfo {
                value: Some(val),
                mutable: false,
            });
        }
        if f.func_name.l == ast::EQ || f.func_name.l == ast::NullEQ {
            let get_value_info = |c: &Constant| -> ValueInfo {
                let mutable = c.param_marker.is_some() || c.deferred_expr.is_some();
                let mut value = None;
                if !mutable {
                    value = Some(c.value.clone());
                }
                ValueInfo { value, mutable }
            };
            if let Expr::Constant(c) = &*f.get_args()[0] {
                return Some(get_value_info(c));
            }
            if let Expr::Constant(c) = &*f.get_args()[1] {
                return Some(get_value_info(c));
            }
        }
    }
    None
}

/// The result of [`extract_eq_and_in_condition`] when the range is not empty: `accesses`,
/// `filters`, `newConditions`, `columnValues`.
pub type EqAndInCondition = (
    Vec<ExprRef>,
    Vec<ExprRef>,
    Vec<ExprRef>,
    Vec<Option<ValueInfo>>,
);

/// ExtractEqAndInCondition will split the given condition into three parts by the information of index columns and their lengths.
/// accesses: The condition will be used to build range.
/// filters: filters is the part that some access conditions need to be evaluated again since it's only the prefix part of char column.
/// newConditions: We'll simplify the given conditions if there're multiple in conditions or eq conditions on the same column.
///
///   e.g. if there're a in (1, 2, 3) and a in (2, 3, 4). This two will be combined to a in (2, 3) and pushed to newConditions.
///
/// columnValues: the constant column values for all index columns. columnValues[i] is nil if cols[i] is not constant.
/// bool: indicate whether there's nil range when merging eq and in conditions.
///
/// Go's last result (an empty range) is `Ok(None)`; `Err` is the D08.9 boundary of
/// rebuilding an IN condition.
// go: ExtractEqAndInCondition L2 (Ok(None) for the empty range; Result for the D08.9 boundary)
pub fn extract_eq_and_in_condition(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
) -> Result<Option<EqAndInCondition>, Error> {
    let mut filters: Vec<ExprRef> = Vec::new();
    let mut rb = Builder::new(sctx);
    let mut accesses: Vec<Option<ExprRef>> = vec![None; cols.len()];
    let mut points: Vec<Vec<Point>> = vec![Vec::new(); cols.len()];
    let mut merged_accesses: Vec<Option<ExprRef>> = vec![None; cols.len()];
    let mut new_conditions: Vec<ExprRef> = Vec::with_capacity(conditions.len());
    let mut column_values: Vec<Option<ValueInfo>> = vec![None; cols.len()];
    let mut offsets: Vec<isize> = vec![0; conditions.len()];
    let ectx = sctx.expr_ctx.get_eval_ctx();
    for (i, cond) in conditions.iter().enumerate() {
        let offset = get_potential_eq_or_in_col_offset(sctx, cond, cols);
        offsets[i] = offset;
        if offset == -1 {
            continue;
        }
        let offset = offset as usize;
        if accesses[offset].is_none() {
            accesses[offset] = Some(Arc::clone(cond));
            continue;
        }
        // Multiple Eq/In conditions for one column in CNF, apply intersection on them
        // Lazily compute the points for the previously visited Eq/In
        let new_tp = new_field_type_for_ranger(cols[offset].get_type());
        let collator = collate::get_collator(cols[offset].get_type().get_collate());
        if merged_accesses[offset].is_none() {
            merged_accesses[offset] = accesses[offset].clone();
            // Note that this is a relatively special usage of build(). We will restore the points back to Expression for
            // later use and may build the Expression to points again.
            // We need to keep the original value here, which means we neither cut prefix nor convert to sort key.
            points[offset] = rb.build(
                accesses[offset].as_ref().expect("set"),
                &new_tp,
                UnspecifiedLength,
                false,
            );
        }
        let b = rb.build(cond, &new_tp, UnspecifiedLength, false);
        let a = std::mem::take(&mut points[offset]);
        points[offset] = rb.intersection(a, b, collator);
        if points[offset].is_empty() {
            // Early termination if false expression found
            if maybe_over_optimized4_plan_cache(&*sctx.expr_ctx, conditions) {
                // `a>@x and a<@y` --> `invalid-range if @x>=@y`
                sctx.plan_cache_tracker
                    .set_skip_plan_cache("some parameters may be overwritten");
            }
            return Ok(None);
        }
    }
    for i in 0..merged_accesses.len() {
        if merged_accesses[i].is_none() {
            if let Some(access) = accesses[i].clone() {
                if all_eq_or_in(&access) {
                    column_values[i] = extract_value_info(&access);
                    if column_values[i]
                        .as_ref()
                        .is_some_and(|v| v.value.as_ref().is_some_and(|d| d.is_null()))
                    {
                        accesses[i] = None;
                    } else {
                        new_conditions.push(access);
                    }
                } else {
                    accesses[i] = None;
                }
            }
            continue;
        }
        let pts = std::mem::take(&mut points[i]);
        match all_single_points(&sctx.type_ctx, pts) {
            None => {
                // There exists an interval whose length is larger than 0
                accesses[i] = None;
            }
            Some(pts) if pts.is_empty() => {
                // Early termination if false expression found
                if maybe_over_optimized4_plan_cache(&*sctx.expr_ctx, conditions) {
                    // `a>@x and a<@y` --> `invalid-range if @x>=@y`
                    sctx.plan_cache_tracker
                        .set_skip_plan_cache("some parameters may be overwritten");
                }
                return Ok(None);
            }
            Some(pts) => {
                // All Intervals are single points
                accesses[i] = points2_eq_or_in_cond(&*sctx.expr_ctx, &pts, &cols[i])?;
                if let Some(access) = &accesses[i] {
                    new_conditions.push(Arc::clone(access));
                    if let Expr::ScalarFunction(f) = &**access
                        && f.func_name.l == ast::EQ
                    {
                        // Actually the constant column value may not be mutable. Here we assume it is mutable to keep it simple.
                        // Maybe we can improve it later.
                        column_values[i] = Some(ValueInfo {
                            value: None,
                            mutable: true,
                        });
                    }
                }
                if maybe_over_optimized4_plan_cache(&*sctx.expr_ctx, conditions) {
                    // `a=@x and a=@y` --> `a=@x if @x==@y`
                    sctx.plan_cache_tracker
                        .set_skip_plan_cache("some parameters may be overwritten");
                }
                points[i] = pts;
            }
        }
    }
    for (i, &offset) in offsets.iter().enumerate() {
        if offset == -1 || accesses[offset as usize].is_none() {
            new_conditions.push(Arc::clone(&conditions[i]));
        }
    }
    let mut cut_accesses: Vec<ExprRef> = Vec::with_capacity(accesses.len());
    for (i, cond) in accesses.iter().enumerate() {
        let Some(cond) = cond else {
            break;
        };
        cut_accesses.push(Arc::clone(cond));

        // Currently, if the access cond is on a prefix index, we will also add this cond to table filters.
        // A possible optimization is that, if the value in the cond is shorter than the length of the prefix index, we don't
        // need to add this cond to table filters.
        // e.g. CREATE TABLE t(a varchar(10), index i(a(5)));  SELECT * FROM t USE INDEX i WHERE a > 'aaa';
        // However, please notice that if you're implementing this, please (1) set StatementContext.OptimDependOnMutableConst to true,
        // or (2) don't do this optimization when StatementContext.UseCache is true. That's because this plan is affected by
        // flen of user variable, we cannot cache this plan.
        let is_full_length =
            lengths[i] == UnspecifiedLength || lengths[i] == cols[i].get_type().get_flen();
        if !is_full_length {
            filters.push(Arc::clone(cond));
        }
    }
    // We should remove all accessConds, so that they will not be added to filter conditions.
    let new_conditions = remove_conditions(&*ectx, &new_conditions, &cut_accesses);
    Ok(Some((cut_accesses, filters, new_conditions, column_values)))
}

/// The result of `detachDNFCondAndBuildRangeForIndex`: Go's `(Ranges, accesses,
/// columnValues, hasResidual, minAccessConds, error)`.
type DnfDetachResult = (
    Ranges,
    Vec<ExprRef>,
    Option<Vec<Option<ValueInfo>>>,
    bool,
    isize,
);

impl RangeDetacher<'_> {
    /// detachDNFCondAndBuildRangeForIndex will detach the index filters from table filters when it's a DNF(Disjunctive Normal Form).
    /// We will detach the conditions of every DNF items, then compose them to a DNF.
    ///
    /// Go returns `nil` access conditions on some early returns and the composed DNF
    /// otherwise; the early returns give an empty list here.
    // go: rangeDetacher.detachDNFCondAndBuildRangeForIndex L1
    fn detach_dnf_cond_and_build_range_for_index(
        &self,
        condition: &ScalarFunction,
        new_tp_slice: &[FieldType],
    ) -> Result<DnfDetachResult, Error> {
        let ectx = self.sctx.expr_ctx.get_eval_ctx();
        let first_column_checker = ConditionChecker {
            checker_col: Some(&self.cols[0]),
            length: self.lengths[0],
            opt_prefix_index_single_scan: self.sctx.opt_prefix_index_single_scan,
            ctx: &*ectx,
        };
        let mut rb = Builder::new(self.sctx);
        let dnf_items = flatten_dnf_conditions(condition);
        let mut new_access_items: Vec<ExprRef> = Vec::with_capacity(dnf_items.len());
        let mut min_access_conds: isize = -1;
        let mut total_ranges: Ranges = Vec::new();
        let mut total_ranges_mem_usage: i64 = 0;
        let mut column_values: Vec<Option<ValueInfo>> = vec![None; self.cols.len()];
        let mut has_residual = false;
        for (i, item) in dnf_items.iter().enumerate() {
            if let Expr::ScalarFunction(sf) = &**item
                && sf.func_name.l == ast::LogicAnd
            {
                let cnf_items = flatten_cnf_conditions(sf);
                let res =
                    self.detach_cnf_cond_and_build_range_for_index(&cnf_items, new_tp_slice, true)?;
                let ranges = res.ranges;
                // If DNF item always false, we can return ignore this DNF item.
                if ranges.is_empty() {
                    continue;
                }
                let accesses = res.access_conds;
                let filters = res.remained_conds;
                if accesses.is_empty() {
                    return Ok((full_range(), Vec::new(), None, true, -1));
                }
                if !filters.is_empty() {
                    has_residual = true;
                }
                total_ranges_mem_usage += ranges_mem_usage(&ranges);
                total_ranges.extend(ranges);
                if self.range_max_size > 0 && total_ranges_mem_usage > self.range_max_size {
                    self.sctx
                        .range_fallback_handler
                        .record_range_fallback(self.range_max_size);
                    return Ok((full_range(), Vec::new(), None, true, -1));
                }
                if let Some(e) = compose_cnf_condition(&*self.sctx.expr_ctx, &accesses) {
                    new_access_items.push(e);
                }
                if let Some(res_column_values) = res.column_values {
                    if i == 0 {
                        column_values = res_column_values;
                    } else {
                        // take the intersection of the two columnValues
                        for (j, slot) in column_values.iter_mut().enumerate() {
                            let Some(val_info) = slot.as_ref() else {
                                continue;
                            };
                            let same_value = is_same_value(
                                &self.sctx.type_ctx,
                                Some(val_info),
                                res_column_values.get(j).and_then(|v| v.as_ref()),
                            )?;
                            if !same_value {
                                *slot = None;
                            }
                        }
                    }
                }
                if min_access_conds == -1 || (accesses.len() as isize) < min_access_conds {
                    min_access_conds = accesses.len() as isize;
                }
            } else {
                let (is_access_cond, should_reserve) = first_column_checker.check(item);
                if !is_access_cond {
                    return Ok((full_range(), Vec::new(), None, true, -1));
                }
                if should_reserve {
                    has_residual = true;
                }
                let points = rb.build(
                    item,
                    &new_tp_slice[0],
                    self.lengths[0],
                    self.convert_to_sort_key,
                );
                let tmp_new_tp = if self.convert_to_sort_key {
                    convert_string_ft_to_binary_collate(&new_tp_slice[0])
                } else {
                    new_tp_slice[0].clone()
                };
                // TODO: restrict the mem usage of ranges
                let (ranges, range_fallback) =
                    points2_ranges(self.sctx, points, &tmp_new_tp, self.range_max_size)?;
                if range_fallback {
                    self.sctx
                        .range_fallback_handler
                        .record_range_fallback(self.range_max_size);
                    return Ok((full_range(), Vec::new(), None, true, -1));
                }
                total_ranges_mem_usage += ranges_mem_usage(&ranges);
                total_ranges.extend(ranges);
                if self.range_max_size > 0 && total_ranges_mem_usage > self.range_max_size {
                    self.sctx
                        .range_fallback_handler
                        .record_range_fallback(self.range_max_size);
                    return Ok((full_range(), Vec::new(), None, true, -1));
                }
                new_access_items.push(Arc::clone(item));
                if i == 0 {
                    column_values[0] = extract_value_info(item);
                } else if column_values[0].is_some() {
                    let val_info = extract_value_info(item);
                    let same_value = is_same_value(
                        &self.sctx.type_ctx,
                        column_values[0].as_ref(),
                        val_info.as_ref(),
                    )?;
                    if !same_value {
                        column_values[0] = None;
                    }
                }
                if min_access_conds == -1 || min_access_conds > 1 {
                    min_access_conds = 1;
                }
            }
        }

        let total_ranges = union_ranges(self.sctx, total_ranges, self.merge_consecutive)?;

        Ok((
            total_ranges,
            compose_dnf_condition(&*self.sctx.expr_ctx, &new_access_items)
                .into_iter()
                .collect(),
            Some(column_values),
            has_residual,
            min_access_conds,
        ))
    }
}

/// `[]*valueInfo`: the constant value of each index column, `None` for a nil slot.
pub type ColumnValues = Vec<Option<ValueInfo>>;

/// valueInfo is used for recording the constant column value in DetachCondAndBuildRangeForIndex.
#[derive(Clone, Debug)]
pub struct ValueInfo {
    /// If not mutable, value is the constant column value. Otherwise value is nil.
    pub value: Option<Datum>,
    /// If true, the constant column value depends on mutable constant.
    pub mutable: bool,
}

// go: isSameValue L1
fn is_same_value(
    type_ctx: &TypeContext,
    lhs: Option<&ValueInfo>,
    rhs: Option<&ValueInfo>,
) -> Result<bool, Error> {
    // We assume `lhs` and `rhs` are not the same when either `lhs` or `rhs` is mutable to keep it simple. If we consider
    // mutable valueInfo, we need to set `sc.OptimDependOnMutableConst = true`, which makes the plan not able to be cached.
    // On the other hand, the equal condition may not be used for optimization. Hence we simply regard mutable valueInfos different
    // from others. Maybe we can improve it later.
    // TODO: is `lhs.value.Kind() != rhs.value.Kind()` necessary?
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return Ok(false);
    };
    if lhs.mutable || rhs.mutable {
        return Ok(false);
    }
    let (lv, rv) = (
        lhs.value
            .as_ref()
            .expect("value of a non-mutable valueInfo"),
        rhs.value
            .as_ref()
            .expect("value of a non-mutable valueInfo"),
    );
    if lv.kind() != rv.kind() {
        return Ok(false);
    }
    // binary collator may not the best choice, but it can make sure the result is correct.
    let (cmp, err) = lv.compare(type_ctx, rv, collate::get_binary_collator());
    err?;
    Ok(cmp == 0)
}

/// DetachRangeResult wraps up results when detaching conditions and builing ranges.
#[derive(Clone, Debug, Default)]
pub struct DetachRangeResult {
    /// Ranges is the ranges extracted and built from conditions.
    pub ranges: Ranges,
    /// AccessConds is the extracted conditions for access.
    pub access_conds: Vec<ExprRef>,
    /// RemainedConds is the filter conditions which should be kept after access.
    pub remained_conds: Vec<ExprRef>,
    /// ColumnValues records the constant column values for all index columns.
    /// For the ith column, if it is evaluated as constant, ColumnValues[i] is its value. Otherwise ColumnValues[i] is nil.
    /// Go's nil slice is `None`.
    pub column_values: Option<Vec<Option<ValueInfo>>>,
    /// EqCondCount is the number of equal conditions extracted.
    pub eq_cond_count: isize,
    /// EqOrInCount is the number of equal/in conditions extracted.
    pub eq_or_in_count: isize,
    /// IsDNFCond indicates if the top layer of conditions are in DNF.
    /// Please see comments of planner/util/AccessPath.MinAccessCondsForDNFCond for more details.
    pub is_dnf_cond: bool,
    pub min_access_conds_for_dnf_cond: isize,
}

/// DetachCondAndBuildRangeForIndex will detach the index filters from table filters.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit. If you ask that all conditions must be used
/// for building ranges, set rangeMemQuota to 0 to avoid range fallback.
/// The returned values are encapsulated into a struct DetachRangeResult, see its comments for explanation.
// go: DetachCondAndBuildRangeForIndex L1
pub fn detach_cond_and_build_range_for_index(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
    range_max_size: i64,
) -> Result<DetachRangeResult, Error> {
    let d = RangeDetacher {
        sctx,
        all_conds: conditions,
        cols,
        lengths,
        merge_consecutive: true,
        convert_to_sort_key: true,
        range_max_size,
    };
    d.detach_cond_and_build_range_for_cols()
}

/// detachCondAndBuildRange detaches the index filters from table filters and uses them to build ranges.
// go: detachCondAndBuildRange L1
fn detach_cond_and_build_range(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
    range_max_size: i64,
    convert_to_sort_key: bool,
    merge_consecutive: bool,
) -> Result<DetachRangeResult, Error> {
    let d = RangeDetacher {
        sctx,
        all_conds: conditions,
        cols,
        lengths,
        merge_consecutive,
        convert_to_sort_key,
        range_max_size,
    };
    d.detach_cond_and_build_range_for_cols()
}

/// DetachCondAndBuildRangeForPartition will detach the index filters from table filters.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit. If you ask that all conditions must be used
/// for building ranges, set rangeMemQuota to 0 to avoid range fallback.
/// The returned values are encapsulated into a struct DetachRangeResult, see its comments for explanation.
// go: DetachCondAndBuildRangeForPartition L1
pub fn detach_cond_and_build_range_for_partition(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
    range_max_size: i64,
) -> Result<DetachRangeResult, Error> {
    detach_cond_and_build_range(
        sctx,
        conditions,
        cols,
        lengths,
        range_max_size,
        false,
        false,
    )
}

/// `rangeDetacher`.
pub(crate) struct RangeDetacher<'a> {
    pub sctx: &'a RangerContext,
    pub all_conds: &'a [ExprRef],
    pub cols: &'a [Arc<Column>],
    pub lengths: &'a [isize],
    pub merge_consecutive: bool,
    pub convert_to_sort_key: bool,
    pub range_max_size: i64,
}

impl RangeDetacher<'_> {
    /// Go returns the partly filled result with an error; callers drop it.
    // go: rangeDetacher.detachCondAndBuildRangeForCols L1
    fn detach_cond_and_build_range_for_cols(&self) -> Result<DetachRangeResult, Error> {
        let mut res = DetachRangeResult::default();
        let mut new_tp_slice: Vec<FieldType> = Vec::with_capacity(self.cols.len());
        for col in self.cols {
            new_tp_slice.push(new_field_type_for_ranger(&col.ret_type));
        }
        if self.all_conds.len() == 1
            && let Expr::ScalarFunction(sf) = &*self.all_conds[0]
            && sf.func_name.l == ast::LogicOr
        {
            let (ranges, accesses, column_values, has_residual, min_access_conds) =
                self.detach_dnf_cond_and_build_range_for_index(sf, &new_tp_slice)?;
            res.ranges = ranges;
            res.access_conds = accesses;
            res.column_values = column_values;
            res.is_dnf_cond = true;
            if min_access_conds != -1 {
                res.min_access_conds_for_dnf_cond = min_access_conds;
            }
            // If this DNF have something cannot be to calculate range, then all this DNF should be pushed as filter condition.
            if has_residual {
                res.remained_conds = self.all_conds.to_vec();
                return Ok(res);
            }
            return Ok(res);
        }
        self.detach_cnf_cond_and_build_range_for_index(self.all_conds, &new_tp_slice, true)
    }
}

/// DetachSimpleCondAndBuildRangeForIndex will detach the index filters from table filters.
/// It will find the point query column firstly and then extract the range query column.
/// rangeMaxSize is the max memory limit for ranges. O indicates no memory limit. If you ask that all conditions must be used
/// for building ranges, set rangeMemQuota to 0 to avoid range fallback.
/// The returned remainedConds are conditions that must be re-applied as filters (e.g. when a
/// collation mismatch makes the range approximate but the condition is still needed for correctness).
// go: DetachSimpleCondAndBuildRangeForIndex L1
pub fn detach_simple_cond_and_build_range_for_index(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
    lengths: &[isize],
    range_max_size: i64,
) -> Result<(Ranges, Vec<ExprRef>, Vec<ExprRef>), Error> {
    let mut new_tp_slice: Vec<FieldType> = Vec::with_capacity(cols.len());
    for col in cols {
        new_tp_slice.push(new_field_type_for_ranger(&col.ret_type));
    }
    let d = RangeDetacher {
        sctx,
        all_conds: conditions,
        cols,
        lengths,
        merge_consecutive: true,
        convert_to_sort_key: true,
        range_max_size,
    };
    let res = d.detach_cnf_cond_and_build_range_for_index(conditions, &new_tp_slice, false)?;
    Ok((res.ranges, res.access_conds, res.remained_conds))
}

// go: removeConditions L1
fn remove_conditions(
    ectx: &dyn EvalContext,
    conditions: &[ExprRef],
    conds_to_remove: &[ExprRef],
) -> Vec<ExprRef> {
    let mut filter_conds: Vec<ExprRef> = Vec::with_capacity(conditions.len());
    for cond in conditions {
        if !contains(ectx, conds_to_remove, cond) {
            filter_conds.push(Arc::clone(cond));
        }
    }
    filter_conds
}

/// AppendConditionsIfNotExist appends conditions if they are absent.
// go: AppendConditionsIfNotExist L1
pub fn append_conditions_if_not_exist(
    ectx: &dyn EvalContext,
    mut conditions: Vec<ExprRef>,
    conds_to_append: &[ExprRef],
) -> Vec<ExprRef> {
    let mut should_append: Vec<ExprRef> = Vec::with_capacity(conds_to_append.len());
    for cond in conds_to_append {
        if !contains(ectx, &conditions, cond) {
            should_append.push(Arc::clone(cond));
        }
    }
    conditions.extend(should_append);
    conditions
}

/// ExtractAccessConditionsForColumn extracts the access conditions used for range calculation. Since
/// we don't need to return the remained filter conditions, it is much simpler than DetachCondsForColumn.
// go: ExtractAccessConditionsForColumn L1
pub fn extract_access_conditions_for_column(
    ctx: &RangerContext,
    conds: &[ExprRef],
    col: &Column,
) -> Vec<ExprRef> {
    let ectx = ctx.expr_ctx.get_eval_ctx();
    let checker = ConditionChecker {
        checker_col: Some(col),
        length: UnspecifiedLength,
        opt_prefix_index_single_scan: ctx.opt_prefix_index_single_scan,
        ctx: &*ectx,
    };
    let access_conds: Vec<ExprRef> = Vec::with_capacity(8);
    filter(access_conds, conds, |expr| {
        let (is_access_cond, _) = checker.check(expr);
        is_access_cond
    })
}

/// DetachCondsForColumn detaches access conditions for specified column from other filter conditions.
// go: DetachCondsForColumn L1
pub fn detach_conds_for_column(
    sctx: &RangerContext,
    conds: &[ExprRef],
    col: &Column,
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let ectx = sctx.expr_ctx.get_eval_ctx();
    let checker = ConditionChecker {
        checker_col: Some(col),
        length: UnspecifiedLength,
        opt_prefix_index_single_scan: sctx.opt_prefix_index_single_scan,
        ctx: &*ectx,
    };
    detach_column_cnf_conditions(&*sctx.expr_ctx, conds, &checker)
}

/// MergeDNFItems4Col receives a slice of DNF conditions, merges some of them which can be built into ranges on a single column, then returns.
/// For example, [a > 5, b > 6, c > 7, a = 1, b > 3] will become [a > 5 or a = 1, b > 6 or b > 3, c > 7].
///
/// The merged groups come out in ascending `UniqueID` (Go: map order, M-MAPORDER).
// go: MergeDNFItems4Col L1 [M-MAPORDER]
pub fn merge_dnf_items4_col(ctx: &RangerContext, dnf_items: &[ExprRef]) -> Vec<ExprRef> {
    let ectx = ctx.expr_ctx.get_eval_ctx();
    let mut merged_dnf_items: Vec<ExprRef> = Vec::with_capacity(dnf_items.len());
    let mut col2_dnf_items: BTreeMap<i64, Vec<ExprRef>> = BTreeMap::new();
    for dnf_item in dnf_items {
        let cols = extract_columns(dnf_item);
        // If this condition contains multiple columns, we can't merge it.
        // If this column is _tidb_rowid, we also can't merge it since Selectivity() doesn't handle it, or infinite recursion will happen.
        if cols.len() != 1 || cols[0].id == tidb_model::ExtraHandleID as i64 {
            merged_dnf_items.push(Arc::clone(dnf_item));
            continue;
        }

        let unique_id = cols[0].unique_id;
        let checker = ConditionChecker {
            checker_col: Some(&cols[0]),
            length: UnspecifiedLength,
            opt_prefix_index_single_scan: ctx.opt_prefix_index_single_scan,
            ctx: &*ectx,
        };
        // If we can't use this condition to build range, we can't merge it.
        // Currently, we assume if every condition in a DNF expression can pass this check, then `Selectivity` must be able to
        // cover this entire DNF directly without recursively call `Selectivity`. If this doesn't hold in the future, this logic
        // may cause infinite recursion in `Selectivity`.
        let (is_access_cond, _) = checker.check(dnf_item);
        if !is_access_cond {
            merged_dnf_items.push(Arc::clone(dnf_item));
            continue;
        }

        col2_dnf_items
            .entry(unique_id)
            .or_default()
            .push(Arc::clone(dnf_item));
    }
    for items in col2_dnf_items.values() {
        if let Some(e) = compose_dnf_condition(&*ctx.expr_ctx, items) {
            merged_dnf_items.push(e);
        }
    }
    merged_dnf_items
}

/// `expr.Eval(evalCtx, chunk.MutRowFromDatums(record).ToRow())` of a shard index's
/// virtual expression.
// glue: Expression.Eval over chunk.MutRowFromDatums(record).ToRow()
fn eval_on_record(sctx: &RangerContext, expr: &Expr, record: &[Datum]) -> Result<Datum, Error> {
    let mut_row = mut_row_from_datums(record);
    let ectx = sctx.expr_ctx.get_eval_ctx();
    expr.eval(&*ectx, &EvalState::new(), mut_row.to_row())
}

/// AddGcColumnCond add the `tidb_shard(x) = xxx` to the condition
/// @param[in] cols          the columns of shard index, such as [tidb_shard(a), a, ...]
/// @param[in] accessCond    the conditions relative to the index and arranged by the index column order.
///
///   e.g. the index is uk(tidb_shard(a), a, b) and the where clause is
///   `WHERE b = 1 AND a = 2 AND c = 3`, the param accessCond is {a = 2, b = 1} that is
///   only relative to uk's columns.
///
/// @param[in] columnValues  the values of index columns in param accessCond. if accessCond is {a = 2, b = 1},
///
///   columnValues is {2, 1}. if accessCond the "IN" function like `a IN (1, 2)`, columnValues
///   is empty.
///
/// @retval -  []expression.Expression   the new conditions after adding `tidb_shard() = xxx` prefix
///
///   error                     if error gernerated, return error
///
/// Go's slices with nil holes are `Option` slots; Go returns `accessesCond` with its
/// errors, `Err` only here (M-ERRVAL).
// go: AddGcColumnCond L1 [M-ERRVAL]
pub fn add_gc_column_cond(
    sctx: &RangerContext,
    cols: &[Arc<Column>],
    accesses_cond: Vec<Option<ExprRef>>,
    column_values: &mut [Option<ValueInfo>],
) -> Result<Vec<Option<ExprRef>>, Error> {
    if let Some(cond) = &accesses_cond[1]
        && let Expr::ScalarFunction(f) = &**cond
    {
        match f.func_name.l.as_str() {
            ast::EQ => return add_gc_column4_eq_cond(sctx, cols, accesses_cond, column_values),
            ast::In => {
                return Ok(add_gc_column4_in_cond(sctx, cols, &accesses_cond)?
                    .into_iter()
                    .map(Some)
                    .collect());
            }
            _ => {}
        }
    }

    Ok(accesses_cond)
}

/// AddGcColumn4InCond add the `tidb_shard(x) = xxx` for `IN` condition
/// For param explanation, please refer to the function `AddGcColumnCond`.
/// @retval -  []expression.Expression   the new conditions after adding `tidb_shard() = xxx` prefix
///
///   error                     if error gernerated, return error
// go: AddGcColumn4InCond L1 [M-ERRVAL]
pub fn add_gc_column4_in_cond(
    sctx: &RangerContext,
    cols: &[Arc<Column>],
    accesses_cond: &[Option<ExprRef>],
) -> Result<Vec<ExprRef>, Error> {
    let mut new_access_cond: Vec<ExprRef> = Vec::new();
    let mut record: Vec<Datum> = vec![Datum::default(); 1];

    let expr = cols[0]
        .virtual_expr
        .clone()
        .expect("VirtualExpr of a shard index column");
    let and_type = new_field_type(mysql::TypeTiny);

    let cond = accesses_cond[1].as_ref().expect("checked by the caller");
    let Expr::ScalarFunction(sf) = &**cond else {
        panic!("interface conversion: expression.Expression is not *expression.ScalarFunction");
    };
    let Expr::Column(c) = &*sf.get_args()[0] else {
        panic!("interface conversion: expression.Expression is not *expression.Column");
    };
    let mut and_or_expr: Option<ExprRef> = None;
    for (i, arg) in sf.get_args()[1..].iter().enumerate() {
        // get every const value and calculate tidb_shard(val)
        if !matches!(&**arg, Expr::Constant(_)) {
            panic!("interface conversion: expression.Expression is not *expression.Constant");
        }
        let con_val = eval_const(sctx, arg)?;

        record[0] = con_val;
        let expr_val = eval_on_record(sctx, &expr, &record)?;

        // tmpArg1 is like `tidb_shard(a) = 8`, tmpArg2 is like `a = 100`
        let expr_con = Constant::new(expr_val, (*cols[0].ret_type).clone());
        let tmp_arg1 = new_function(
            &*sctx.expr_ctx,
            ast::EQ,
            (*cols[0].ret_type).clone(),
            vec![
                Expr::Column(Column::clone(&cols[0])).into_ref(),
                Expr::Constant(expr_con).into_ref(),
            ],
        )?;
        let tmp_arg2 = new_function(
            &*sctx.expr_ctx,
            ast::EQ,
            (*c.ret_type).clone(),
            vec![Expr::Column(c.clone()).into_ref(), Arc::clone(arg)],
        )?;

        // make a LogicAnd, e.g. `tidb_shard(a) = 8 AND a = 100`
        let and_expr = new_function(
            &*sctx.expr_ctx,
            ast::LogicAnd,
            and_type.clone(),
            vec![tmp_arg1, tmp_arg2],
        )?;

        if i == 0 {
            and_or_expr = Some(and_expr);
        } else {
            // if the LogicAnd more than one, make a LogicOr,
            // e.g. `(tidb_shard(a) = 8 AND a = 100) OR (tidb_shard(a) = 161 AND a = 200)`
            and_or_expr = Some(new_function(
                &*sctx.expr_ctx,
                ast::LogicOr,
                and_type.clone(),
                vec![and_or_expr.take().expect("set"), and_expr],
            )?);
        }
    }

    // Go appends a nil interface when the IN list is empty (not reachable: IN has at least
    // one value).
    if let Some(e) = and_or_expr {
        new_access_cond.push(e);
    }

    Ok(new_access_cond)
}

/// AddGcColumn4EqCond add the `tidb_shard(x) = xxx` prefix for equal condition
/// For param explanation, please refer to the function `AddGcColumnCond`.
/// @retval -  []expression.Expression   the new conditions after adding `tidb_shard() = xxx` prefix
///
///   []*valueInfo              the values of every columns in the returned new conditions
///   error                     if error gernerated, return error
// go: AddGcColumn4EqCond L1 [M-ERRVAL]
pub fn add_gc_column4_eq_cond(
    sctx: &RangerContext,
    cols: &[Arc<Column>],
    mut accesses_cond: Vec<Option<ExprRef>>,
    column_values: &mut [Option<ValueInfo>],
) -> Result<Vec<Option<ExprRef>>, Error> {
    let expr = cols[0]
        .virtual_expr
        .clone()
        .expect("VirtualExpr of a shard index column");
    let mut record: Vec<Datum> = vec![Datum::default(); column_values.len() - 1];

    for i in 1..column_values.len() {
        let Some(cv) = &column_values[i] else {
            break;
        };
        record[i - 1] = cv.value.clone().expect("value of a non-mutable valueInfo");
    }

    let evaluated = eval_on_record(sctx, &expr, &record)?;
    let vi = ValueInfo {
        value: Some(evaluated.clone()),
        mutable: false,
    };
    let con = Constant::new(evaluated, (*cols[0].ret_type).clone());
    // make a tidb_shard() function, e.g. `tidb_shard(a) = 8`
    let cond = new_function(
        &*sctx.expr_ctx,
        ast::EQ,
        (*cols[0].ret_type).clone(),
        vec![
            Expr::Column(Column::clone(&cols[0])).into_ref(),
            Expr::Constant(con).into_ref(),
        ],
    )?;

    accesses_cond[0] = Some(cond);
    column_values[0] = Some(vi);
    Ok(accesses_cond)
}

/// AddExpr4EqAndInCondition add the `tidb_shard(x) = xxx` prefix
/// Add tidb_shard() for EQ and IN function. e.g. input condition is `WHERE a = 1`,
/// output condition is `WHERE tidb_shard(a) = 214 AND a = 1`. e.g. input condition
/// is `WHERE a IN (1, 2 ,3)`, output condition is `WHERE (tidb_shard(a) = 214 AND a = 1)
/// OR (tidb_shard(a) = 143 AND a = 2) OR (tidb_shard(a) = 156 AND a = 3)`
/// @param[in] conditions  the original condition to be processed
/// @param[in] cols        the columns of shard index, such as [tidb_shard(a), a, ...]
/// @param[in] lengths     the length for every column of shard index
/// @retval - the new condition after adding tidb_shard() prefix
// go: AddExpr4EqAndInCondition L1 [M-ERRVAL]
pub fn add_expr4_eq_and_in_condition(
    sctx: &RangerContext,
    conditions: &[ExprRef],
    cols: &[Arc<Column>],
) -> Result<Vec<ExprRef>, Error> {
    let mut accesses: Vec<Option<ExprRef>> = vec![None; cols.len()];
    let mut column_values: Vec<Option<ValueInfo>> = vec![None; cols.len()];
    let mut offsets: Vec<isize> = vec![0; conditions.len()];
    let mut add_gc_cond = true;
    let ectx = sctx.expr_ctx.get_eval_ctx();

    // the array accesses stores conditions of every column in the index in the definition order
    // e.g. the original condition is `WHERE b = 100 AND a = 200 AND c = 300`, the definition of
    // index is (tidb_shard(a), a, b), then accesses is "[a = 200, b = 100]"
    for (i, cond) in conditions.iter().enumerate() {
        let offset = get_potential_eq_or_in_col_offset(sctx, cond, cols);
        offsets[i] = offset;
        if offset == -1 {
            continue;
        }
        if accesses[offset as usize].is_none() {
            accesses[offset as usize] = Some(Arc::clone(cond));
            continue;
        }
        // if the same field appear twice or more, don't add tidb_shard()
        // e.g. `WHERE a > 100 and a < 200`
        add_gc_cond = false;
    }

    for (i, cond) in accesses.iter().enumerate() {
        let Some(cond) = cond else {
            continue;
        };
        if !all_eq_or_in(cond) {
            add_gc_cond = false;
            break;
        }
        column_values[i] = extract_value_info(cond);
    }

    if !add_gc_cond || !need_add_gc_column4_shard_index(cols, &accesses, &column_values) {
        return Ok(conditions.to_vec());
    }

    // remove the accesses from newConditions
    // (`expression.Contains` skips the nil slots of `accesses`.)
    let present: Vec<ExprRef> = accesses.iter().flatten().cloned().collect();
    let new_conditions = remove_conditions(&*ectx, conditions, &present);

    // add Gc condition for accesses and return new condition to newAccesses
    let new_accesses = add_gc_column_cond(sctx, cols, accesses, &mut column_values)?;

    // merge newAccesses and original condition execept accesses
    let mut new_conditions = new_conditions;
    for a in new_accesses {
        // Go appends the slots as they are (a nil slot appends a nil interface; on this
        // path every slot is set, NeedAddColumn4EqCond / NeedAddColumn4InCond).
        new_conditions.extend(a);
    }

    Ok(new_conditions)
}

/// NeedAddGcColumn4ShardIndex check whether to add `tidb_shard(x) = xxx`
/// @param[in] cols          the columns of shard index, such as [tidb_shard(a), a, ...]
/// @param[in] accessCond    the conditions relative to the index and arranged by the index column order.
///
///   e.g. the index is uk(tidb_shard(a), a, b) and the where clause is
///   `WHERE b = 1 AND a = 2 AND c = 3`, the param accessCond is {a = 2, b = 1} that is
///   only relative to uk's columns.
///
/// @param[in] columnValues  the values of index columns in param accessCond. if accessCond is {a = 2, b = 1},
///
///   columnValues is {2, 1}. if accessCond the "IN" function like `a IN (1, 2)`, columnValues
///   is empty.
///
/// @retval -  return true if it needs to addr tidb_shard() prefix, ohterwise return false
// go: NeedAddGcColumn4ShardIndex L1
pub fn need_add_gc_column4_shard_index(
    cols: &[Arc<Column>],
    access_cond: &[Option<ExprRef>],
    column_values: &[Option<ValueInfo>],
) -> bool {
    // the columns of shard index shoude be more than 2, like (tidb_shard(a),a,...)
    // check cols and columnValues in the sub call function
    if access_cond.len() < 2 || cols.len() < 2 {
        return false;
    }

    if !is_valid_shard_index(cols) {
        return false;
    }

    // accessCond[0] shoudle be nil, because it has no access condition for
    // the prefix tidb_shard() of the shard index
    if let Some(cond) = &access_cond[1]
        && let Expr::ScalarFunction(f) = &**cond
    {
        match f.func_name.l.as_str() {
            ast::EQ => return need_add_column4_eq_cond(cols, access_cond, column_values),
            ast::In => return need_add_column4_in_cond(cols, access_cond, Some(f)),
            _ => {}
        }
    }

    false
}

/// NeedAddColumn4EqCond `tidb_shard(x) = xxx`
/// For param explanation, please refer to the function `NeedAddGcColumn4ShardIndex`.
/// It checks whether EQ conditions need to be added tidb_shard() prefix.
/// (1) columns in accessCond are all columns of the index except the first.
/// (2) every column in accessCond has a constan value
// go: NeedAddColumn4EqCond L1
pub fn need_add_column4_eq_cond(
    cols: &[Arc<Column>],
    access_cond: &[Option<ExprRef>],
    column_values: &[Option<ValueInfo>],
) -> bool {
    let mut val_cnt = 0usize;
    let mut matched_key_fld_cnt = 0usize;

    // the columns of shard index shoude be more than 2, like (tidb_shard(a),a,...)
    if column_values.len() < 2 {
        return false;
    }

    for cond in &access_cond[1..] {
        let Some(cond) = cond else {
            break;
        };

        let Expr::ScalarFunction(f) = &**cond else {
            return false;
        };
        if f.func_name.l != ast::EQ {
            return false;
        }

        matched_key_fld_cnt += 1;
    }
    for val in &column_values[1..] {
        if val.is_none() {
            break;
        }
        val_cnt += 1;
    }

    if matched_key_fld_cnt != cols.len() - 1
        || val_cnt != cols.len() - 1
        || access_cond[0].is_some()
        || column_values[0].is_some()
    {
        return false;
    }

    true
}

/// NeedAddColumn4InCond `tidb_shard(x) = xxx`
/// For param explanation, please refer to the function `NeedAddGcColumn4ShardIndex`.
/// It checks whether "IN" conditions need to be added tidb_shard() prefix.
/// (1) columns in accessCond are all columns of the index except the first.
/// (2) the first param of "IN" function should be a column not a expression like `a + b`
/// (3) the rest params of "IN" function all should be constant
/// (4) the first param of "IN" function should be the column in the expression of first index field.
///
///   e.g. uk(tidb_shard(a), a). If the conditions is `WHERE b in (1, 2, 3)`, the first param of "IN" function
///   is `b` that's not the column in `tidb_shard(a)`.
///
/// @param  sf    "IN" function, e.g. `a IN (1, 2, 3)`
// go: NeedAddColumn4InCond L1
pub fn need_add_column4_in_cond(
    cols: &[Arc<Column>],
    access_cond: &[Option<ExprRef>],
    sf: Option<&ScalarFunction>,
) -> bool {
    let Some(sf) = sf else {
        return false;
    };
    if cols.is_empty() || access_cond.is_empty() {
        return false;
    }

    if access_cond[0].is_some() {
        return false;
    }

    let virtual_expr = match cols[0].virtual_expr.as_deref() {
        Some(Expr::ScalarFunction(f)) => f,
        _ => {
            panic!("interface conversion: expression.Expression is not *expression.ScalarFunction")
        }
    };
    let fields = extract_columns_from_expr(Some(virtual_expr));

    let Expr::Column(c) = &*sf.get_args()[0] else {
        return false;
    };

    for arg in &sf.get_args()[1..] {
        if !matches!(&**arg, Expr::Constant(_)) {
            return false;
        }
    }

    if fields.len() != 1 || !fields[0].equal_column(&Expr::Column(c.clone())) {
        return false;
    }

    true
}

/// ExtractColumnsFromExpr get all fields from input expression virtaulExpr
// go: ExtractColumnsFromExpr L1
pub fn extract_columns_from_expr(virtaul_expr: Option<&ScalarFunction>) -> Vec<Arc<Column>> {
    let mut fields: Vec<Arc<Column>> = Vec::new();

    let Some(virtaul_expr) = virtaul_expr else {
        return fields;
    };

    for arg in virtaul_expr.get_args() {
        match &**arg {
            Expr::ScalarFunction(sf) => fields.extend(extract_columns_from_expr(Some(sf))),
            Expr::Column(c) if !c.in_column_array(&fields) => {
                fields.push(Arc::new(c.clone()));
            }
            _ => {}
        }
    }

    fields
}

/// IsValidShardIndex Check whether the definition of shard index is valid. The form of index
/// should like `index(tidb_shard(a), a, ....)`.
/// 1) the column count shoudle be >= 2
///    2) the first column should be tidb_shard(xxx)
///    3) the parameter of tidb_shard shoudle be a column that is the second column of index
///
/// @param[in] cols        the columns of shard index, such as [tidb_shard(a), a, ...]
/// @retval - if the shard index is valid return true, otherwise return false
// go: IsValidShardIndex L1
pub fn is_valid_shard_index(cols: &[Arc<Column>]) -> bool {
    // definition of index should like the form: index(tidb_shard(a), a, ....)
    if cols.len() < 2 {
        return false;
    }

    // the first coulmn of index must be GC column and the expr must be tidb_shard
    if !tidb_expression::column::gc_column_expr_is_tidb_shard(cols[0].virtual_expr.as_deref()) {
        return false;
    }

    let Some(Expr::ScalarFunction(shard_func)) = cols[0].virtual_expr.as_deref() else {
        return false;
    };

    let arg_count = shard_func.get_args().len();
    if arg_count != 1 {
        return false;
    }

    // parameter of tidb_shard must be the second column of the input index columns
    let Expr::Column(col) = &*shard_func.get_args()[0] else {
        return false;
    };
    if !col.equal_column(&Expr::Column(Column::clone(&cols[1]))) {
        return false;
    }

    true
}
