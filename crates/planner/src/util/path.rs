//! go-file: pkg/planner/util/path.go
//!
//! Mapping:
//! - `*model.IndexInfo` is `Option<Arc<IndexInfo>>`; `Clone` deep-copies it as Go does.
//! - `FullIdxCols` may hold Go's nil columns (`IndexInfo2FullCols`): `Vec<Option<Arc<Column>>>`.
//! - `[]*AccessPath` (partial paths of an IndexMerge) are owned `AccessPath`s; `Clone` copies
//!   them as Go does.
//! - `Col2Len` (`map[int64]int`) is a `HashMap`; `dominate` / `CompareCol2Len` do not depend
//!   on the iteration order (`M-MAPORDER`).
//! - `PlanContext` arguments: `SplitCorColAccessCondFromFilters` and
//!   `GetCol2LenFromAccessConds` only reach `ctx.GetExprCtx()`, which they take directly
//!   (`M-CTX`).

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::kv::{StoreType, TiFlash, TiKV};
use tidb_base::mysql;
use tidb_expression::{Column, EvalContext, Expr, ExprRef, ast_funcs};
use tidb_model::{IndexInfo, TableInfo};
use tidb_parser::types::ETString;
use tidb_parser::types::UnspecifiedLength;
use tidb_sessionctx::exprctx::ExprContext;
use tidb_sessionctx::ranger::types::{Range, has_full_range};
use tidb_types::collate;
use tidb_types::context::Context as TypeContext;

use super::misc::{clone_cols, clone_exprs, clone_ranges};

/// IndexLookUpPushDownByType indicates whether to use index lookup push down optimization where it comes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IndexLookUpPushDownByType(pub isize);

/// IndexLookUpPushDownNone indicates do not push down the index lookup.
pub const INDEX_LOOK_UP_PUSH_DOWN_NONE: IndexLookUpPushDownByType = IndexLookUpPushDownByType(0);
/// IndexLookUpPushDownByHint indicates the hint tells to push down the index lookup.
pub const INDEX_LOOK_UP_PUSH_DOWN_BY_HINT: IndexLookUpPushDownByType = IndexLookUpPushDownByType(1);
/// IndexLookUpPushDownBySysVar indicates the system variable tells to push down the index lookup.
pub const INDEX_LOOK_UP_PUSH_DOWN_BY_SYS_VAR: IndexLookUpPushDownByType =
    IndexLookUpPushDownByType(2);

/// AccessPath indicates the way we access a table: by using single index, or by using multiple indexes,
/// or just by using table scan.
#[derive(Clone, Debug, Default)]
pub struct AccessPath {
    /// Index.
    pub index: Option<Arc<IndexInfo>>,
    /// FullIdxCols.
    pub full_idx_cols: Vec<Option<Arc<Column>>>,
    /// FullIdxColLens.
    pub full_idx_col_lens: Vec<isize>,
    /// IdxCols.
    pub idx_cols: Vec<Arc<Column>>,
    /// IdxColLens.
    pub idx_col_lens: Vec<isize>,
    /// ConstCols indicates whether the column is constant under the given conditions for all index columns.
    pub const_cols: Vec<bool>,
    /// Ranges.
    pub ranges: Vec<Range>,
    /// CountAfterAccess is the row count after we apply range seek and before we use other filter to filter data.
    /// For index merge path, CountAfterAccess is the row count after partial paths and before we apply table filters.
    pub count_after_access: f64,
    /// CountAfterIndex is the row count after we apply filters on index and before we apply the table filters.
    pub count_after_index: f64,
    /// AccessConds.
    pub access_conds: Vec<ExprRef>,
    /// EqCondCount.
    pub eq_cond_count: isize,
    /// EqOrInCondCount.
    pub eq_or_in_cond_count: isize,
    /// IndexFilters.
    pub index_filters: Vec<ExprRef>,
    /// TableFilters.
    pub table_filters: Vec<ExprRef>,
    /// PartialIndexPaths store all index access paths.
    /// If there are extra filters, store them in TableFilters.
    pub partial_index_paths: Vec<AccessPath>,

    // ************************************************** special field below *********************************************************
    /// For every dnf/cnf item, there maybe several matched partial index paths to be determined later in property detecting and cost model.
    /// when PartialAlternativeIndexPaths is not empty, it means a special state for index merge path, and it can't have PartialIndexPaths
    /// at same time. Normal single index or table path also doesn't use this field.
    pub partial_alternative_index_paths: Vec<Vec<AccessPath>>,
    /// KeepIndexMergeORSourceFilter and IndexMergeORSourceFilter are only used with PartialAlternativeIndexPaths, which means for
    /// the new state/type of access path. (undetermined index merge path)
    pub keep_index_merge_or_source_filter: bool,
    /// IndexMergeORSourceFilter indicates that there are some expression inside this dnf that couldn't be pushed down, and we should keep the entire dnf above.
    pub index_merge_or_source_filter: Option<ExprRef>,
    // ********************************************************************************************************************************
    /// IndexMergeIsIntersection means whether it's intersection type or union type IndexMerge path.
    /// It's only valid for a IndexMerge path.
    /// Intersection type is for expressions connected by `AND` and union type is for `OR`.
    pub index_merge_is_intersection: bool,
    /// IndexMergeAccessMVIndex indicates whether this IndexMerge path accesses a MVIndex.
    pub index_merge_access_mv_index: bool,

    /// StoreType.
    pub store_type: StoreType,

    /// If the top level of the filters is an OR list, IsDNFCond is true.
    /// In this case, MinAccessCondsForDNFCond will record the minimum number of access conditions among all DNF items.
    /// For example, if the filter is (a=1 and b=2) or (a=3 and b=4) or (a=5 and b=6 and c=7),
    /// for index (a) or index (b), MinAccessCondsForDNFCond will be 1;
    /// for index (a, b, c), MinAccessCondsForDNFCond will be 2.
    pub is_dnf_cond: bool,
    /// MinAccessCondsForDNFCond.
    pub min_access_conds_for_dnf_cond: isize,

    /// IsIntHandlePath indicates whether this path is table path.
    pub is_int_handle_path: bool,
    /// IsCommonHandlePath.
    pub is_common_handle_path: bool,
    /// Forced means this path is generated by `use/force index()`.
    pub forced: bool,
    /// ForceKeepOrder.
    pub force_keep_order: bool,
    /// ForceNoKeepOrder.
    pub force_no_keep_order: bool,
    /// ForcePartialOrder means whether to force using current path with partial order optimization.
    pub force_partial_order: bool,
    /// IsSingleScan indicates whether the path is a single index/table scan or table access after index scan.
    pub is_single_scan: bool,

    /// Maybe added in model.IndexInfo better, but the cache of model.IndexInfo may lead side effect
    pub is_uk_shard_index_path: bool,
    /// IndexLookUpPushDownBy indicates whether to use index lookup push down optimization and where it is from.
    pub index_look_up_push_down_by: IndexLookUpPushDownByType,

    /// GroupedRanges stores the result of grouping ranges by columns. Finally, we need a merge sort on the results of
    /// each range group.
    pub grouped_ranges: Vec<Vec<Range>>,
    /// GroupByColIdxs stores the column indices used for grouping ranges when using merge sort to satisfy the physical
    /// property.
    /// This field is used to rebuild GroupedRanges from ranges using GroupRangesByCols().
    /// It's used in plan cache or Apply.
    pub group_by_col_idxs: Vec<isize>,

    /// PartIdxCondNotAlwaysValid indicates that this index path is not guaranteed to be valid
    /// for all parameter values when a partial index condition is involved.
    pub part_idx_cond_not_always_valid: bool,
}

impl AccessPath {
    /// Clone returns a deep copy of the original AccessPath.
    /// Note that we rely on the Expression.Clone(), (*IndexInfo).Clone() and (*Range).Clone() in this method, so there are
    /// some fields like FieldType are not deep-copied.
    ///
    /// Go leaves `IndexMergeAccessMVIndex` and `PartialAlternativeIndexPaths`'s absence as the
    /// zero value; both are copied the same way here (the first is not copied).
    // go: AccessPath.Clone L1
    pub fn clone_path(&self) -> AccessPath {
        let mut ret = AccessPath {
            index: self.index.as_ref().map(|i| Arc::new((**i).clone())),
            full_idx_cols: clone_cols(&self.full_idx_cols),
            full_idx_col_lens: self.full_idx_col_lens.clone(),
            idx_cols: clone_cols(&self.idx_cols),
            idx_col_lens: self.idx_col_lens.clone(),
            const_cols: self.const_cols.clone(),
            ranges: clone_ranges(&self.ranges),
            count_after_access: self.count_after_access,
            count_after_index: self.count_after_index,
            access_conds: clone_exprs(&self.access_conds),
            eq_cond_count: self.eq_cond_count,
            eq_or_in_cond_count: self.eq_or_in_cond_count,
            index_filters: clone_exprs(&self.index_filters),
            table_filters: clone_exprs(&self.table_filters),
            index_merge_is_intersection: self.index_merge_is_intersection,
            partial_index_paths: Vec::new(),
            store_type: self.store_type,
            is_dnf_cond: self.is_dnf_cond,
            min_access_conds_for_dnf_cond: self.min_access_conds_for_dnf_cond,
            is_int_handle_path: self.is_int_handle_path,
            is_common_handle_path: self.is_common_handle_path,
            forced: self.forced,
            force_keep_order: self.force_keep_order,
            force_no_keep_order: self.force_no_keep_order,
            force_partial_order: self.force_partial_order,
            is_single_scan: self.is_single_scan,
            is_uk_shard_index_path: self.is_uk_shard_index_path,
            keep_index_merge_or_source_filter: self.keep_index_merge_or_source_filter,
            index_look_up_push_down_by: self.index_look_up_push_down_by,
            grouped_ranges: Vec::with_capacity(self.grouped_ranges.len()),
            group_by_col_idxs: self.group_by_col_idxs.clone(),
            part_idx_cond_not_always_valid: self.part_idx_cond_not_always_valid,
            ..Default::default()
        };
        if let Some(f) = &self.index_merge_or_source_filter {
            ret.index_merge_or_source_filter = Some(Arc::clone(f));
        }
        for partial_path in &self.partial_index_paths {
            ret.partial_index_paths.push(partial_path.clone_path());
        }
        for one_partial_alternative in &self.partial_alternative_index_paths {
            let mut tmp = Vec::with_capacity(one_partial_alternative.len());
            for one_alternative in one_partial_alternative {
                tmp.push(one_alternative.clone_path());
            }
            ret.partial_alternative_index_paths.push(tmp);
        }
        for ranges in &self.grouped_ranges {
            ret.grouped_ranges.push(clone_ranges(ranges));
        }
        ret
    }

    /// IsTablePath returns true if it's IntHandlePath or CommonHandlePath. Including tiflash table scan.
    // go: AccessPath.IsTablePath L1
    pub fn is_table_path(&self) -> bool {
        self.is_int_handle_path
            || self.is_common_handle_path
            || (self.index.is_some() && self.store_type == TiFlash)
    }

    /// IsTiKVTablePath returns true if it's IntHandlePath or CommonHandlePath. And the store type is TiKV.
    // go: AccessPath.IsTiKVTablePath L1
    pub fn is_tikv_table_path(&self) -> bool {
        (self.is_int_handle_path || self.is_common_handle_path) && self.store_type == TiKV
    }

    /// IsTiFlashSimpleTablePath returns true if it's a TiFlash path and will not use any special indexes like vector index.
    // go: AccessPath.IsTiFlashSimpleTablePath L1
    pub fn is_tiflash_simple_table_path(&self) -> bool {
        self.store_type == TiFlash && self.index.is_none()
    }

    /// IsFullScanRange checks whether this access path covers the full scan range without any
    /// filtering that limits the scanned table or index ranges. For integer-handle table paths,
    /// tableInfo is used to account for unsigned primary-key handle ranges.
    // go: AccessPath.IsFullScanRange L1
    pub fn is_full_scan_range(&self, table_info: &TableInfo) -> bool {
        let mut unsigned_int_handle = false;
        if self.is_int_handle_path
            && table_info.pk_is_handle
            && let Some(pk_col_info) = table_info.get_pk_col_info()
        {
            unsigned_int_handle = mysql::has_unsigned_flag(pk_col_info.get_flag());
        }
        has_full_range(&self.ranges, unsigned_int_handle)
    }

    /// SplitCorColAccessCondFromFilters move the necessary filter in the form of index_col = corrlated_col to access conditions.
    /// The function consider the `idx_col_1 = const and index_col_2 = cor_col and index_col_3 = const` case.
    /// It enables more index columns to be considered. The range will be rebuilt in 'ResolveCorrelatedColumns'.
    ///
    /// Go's `access` slice is allocated with `len(path.IdxCols)-eqOrInCount` nil entries and
    /// cut at the first unmatched column; every entry before the cut is set, so the result is
    /// the set entries.
    // go: AccessPath.SplitCorColAccessCondFromFilters L1 [M-CTX]
    pub fn split_cor_col_access_cond_from_filters(
        &self,
        expr_ctx: &dyn ExprContext,
        eq_or_in_count: isize,
    ) -> (Vec<ExprRef>, Vec<ExprRef>) {
        let eq_or_in_count = eq_or_in_count as usize;
        let mut access: Vec<Option<ExprRef>> = vec![None; self.idx_cols.len() - eq_or_in_count];
        let mut used = vec![false; self.table_filters.len()];
        for i in eq_or_in_count..self.idx_cols.len() {
            let mut matched = false;
            for (j, filter) in self.table_filters.iter().enumerate() {
                if used[j] {
                    continue;
                }
                let col_eq_constant = is_col_eq_constant(filter, &self.idx_cols[i]);
                if i == eq_or_in_count && col_eq_constant {
                    // If there is a col-eq-constant condition for path.IdxCols[eqOrInCount], it means that range fallback happens
                    // in DetachCondAndBuildRangeForIndex. In this case we don't consider adding access conditions. Besides, the IF
                    // branch also ensures that there must be some col-eq-corcol condition in access if len(access) > 0, which is
                    // important. If there is no col-eq-corcol condition in access, we would not rebuild ranges, which brings the
                    // correctness issue.
                    return (Vec::new(), self.table_filters.clone());
                }
                let col_eq_cor_col = is_col_eq_cor_col(filter, &self.idx_cols[i]);
                if !col_eq_constant && !col_eq_cor_col {
                    continue;
                }
                // The plan cache do not support subquery now. So we skip the plan cache when there are correlated subqueries.
                // Future judgement should be aligned with the function `isPhysicalPlanCacheable`.
                expr_ctx.set_skip_plan_cache("Correlated subquery is not cached currently");
                matched = true;
                access[i - eq_or_in_count] = Some(Arc::clone(filter));
                if self.idx_col_lens[i] == UnspecifiedLength {
                    used[j] = true;
                }
                break;
            }
            if !matched {
                access.truncate(i - eq_or_in_count);
                break;
            }
        }
        let mut remained = Vec::new();
        for (i, ok) in used.iter().enumerate() {
            if !ok {
                remained.push(Arc::clone(&self.table_filters[i])); // nozero
            }
        }
        (access.into_iter().flatten().collect(), remained)
    }

    /// OnlyPointRange checks whether each range is a point(no interval range exists).
    // go: AccessPath.OnlyPointRange L1
    pub fn only_point_range(&self, tc: &TypeContext) -> bool {
        if self.is_int_handle_path {
            for ran in &self.ranges {
                if !ran.is_point_nullable(tc) {
                    return false;
                }
            }
            return true;
        }
        let index_cols = self
            .index
            .as_ref()
            .map_or(0, |i| i.columns.as_deref().map_or(0, <[_]>::len));
        for ran in &self.ranges {
            // Not point or the not full matched.
            if !ran.is_point_non_nullable(tc) || ran.high_val.len() != index_cols {
                return false;
            }
        }
        true
    }

    /// GetCol2LenFromAccessConds returns columns with lengths from path.AccessConds.
    // go: AccessPath.GetCol2LenFromAccessConds L1 [M-CTX]
    pub fn get_col2_len_from_access_conds(&self, expr_ctx: &dyn ExprContext) -> Col2Len {
        if self.is_table_path() {
            return extract_col2_len(&*expr_ctx.get_eval_ctx(), &self.access_conds, None, &[]);
        }
        extract_col2_len(
            &*expr_ctx.get_eval_ctx(),
            &self.access_conds,
            Some(&self.idx_cols),
            &self.idx_col_lens,
        )
    }

    /// IsUndetermined checks if the path is undetermined.
    /// The undetermined path is the one that may not be always valid.
    /// e.g. The multi value index for JSON is not always valid, because the index must be used with JSON functions.
    // go: AccessPath.IsUndetermined L1
    pub fn is_undetermined(&self) -> bool {
        if self.is_table_path() {
            return false;
        }
        let Some(index) = &self.index else {
            return false;
        };
        if index.mv_index || index.has_condition() {
            return true;
        }
        false
    }

    /// IsIndexJoinUnapplicable checks if the path is unapplicable for index join.
    /// If path is mv index path:
    /// for mv index like mvi(a, json, b), if driving condition is a=1, and we build a prefix scan with range [1,1]
    /// on mvi, it will return many index rows which breaks handle-unique attribute here.
    /// So we cannot use mv index path for index join.
    /// If path is partial index path:
    /// We need to first determine whether we already meet the partial index condition.
    /// Currently we don't support that, so we conservatively return true here.
    // go: AccessPath.IsIndexJoinUnapplicable L1
    pub fn is_index_join_unapplicable(&self) -> bool {
        self.is_undetermined()
    }
}

/// isColEqConstant checks if the expression is eq function that one side is column and the other side is constant.
// go: isColEqConstant L1
fn is_col_eq_constant(expr: &Expr, col: &Column) -> bool {
    let is_constant = |arg: &Expr| matches!(arg, Expr::Constant(_));
    is_col_eq_expr(expr, col, is_constant)
}

/// isColEqCorCol checks if the expression is eq function that one side is column and the other side is correlated column.
// go: isColEqCorCol L1
fn is_col_eq_cor_col(expr: &Expr, col: &Column) -> bool {
    let is_cor_col = |arg: &Expr| matches!(arg, Expr::CorrelatedColumn(_));
    is_col_eq_expr(expr, col, is_cor_col)
}

/// isColEqExpr checks if the expression is eq function that one side is column and the other side passes checkFn.
// go: isColEqExpr L1
fn is_col_eq_expr(expr: &Expr, col: &Column, check_fn: impl Fn(&Expr) -> bool) -> bool {
    let Expr::ScalarFunction(f) = expr else {
        return false;
    };
    if f.func_name.l != ast_funcs::EQ {
        return false;
    }
    let (_, collation) = f.charset_and_collation();
    if let Expr::Column(c) = &*f.get_args()[0] {
        if c.ret_type.eval_type() == ETString
            && !collate::compatible_collate(collation, c.ret_type.get_collate())
        {
            return false;
        }
        if check_fn(&f.get_args()[1]) && col.equal_col(c) {
            return true;
        }
    }
    if let Expr::Column(c) = &*f.get_args()[1] {
        if c.ret_type.eval_type() == ETString
            && !collate::compatible_collate(collation, c.ret_type.get_collate())
        {
            return false;
        }
        if check_fn(&f.get_args()[0]) && col.equal_col(c) {
            return true;
        }
    }
    false
}

/// Col2Len maps expression.Column.UniqueID to column length
pub type Col2Len = HashMap<i64, isize>;

/// ExtractCol2Len collects index/table columns with lengths from expressions. If idxCols and idxColLens are not nil, it collects index columns with lengths(maybe prefix lengths).
/// Otherwise it collects table columns with full lengths.
///
/// Go's nil `idxCols` is `None`.
// go: ExtractCol2Len L1
pub fn extract_col2_len(
    ctx: &dyn EvalContext,
    exprs: &[ExprRef],
    idx_cols: Option<&[Arc<Column>]>,
    idx_col_lens: &[isize],
) -> Col2Len {
    let mut col2len = Col2Len::with_capacity(idx_cols.map_or(0, <[_]>::len));
    for expr in exprs {
        extract_col2_len_from_expr(ctx, expr, idx_cols, idx_col_lens, &mut col2len);
    }
    col2len
}

// go: extractCol2LenFromExpr L1
fn extract_col2_len_from_expr(
    ctx: &dyn EvalContext,
    expr: &Expr,
    idx_cols: Option<&[Arc<Column>]>,
    idx_col_lens: &[isize],
    col2_len: &mut Col2Len,
) {
    match expr {
        Expr::Column(v) => match idx_cols {
            None => {
                col2_len.insert(v.unique_id, UnspecifiedLength);
            }
            Some(idx_cols) => {
                for (i, col) in idx_cols.iter().enumerate() {
                    if v.equal_by_expr_and_id(ctx, col) {
                        col2_len.insert(v.unique_id, idx_col_lens[i]);
                        break;
                    }
                }
            }
        },
        Expr::ScalarFunction(v) => {
            for arg in v.get_args() {
                extract_col2_len_from_expr(ctx, arg, idx_cols, idx_col_lens, col2_len);
            }
        }
        _ => {}
    }
}

/// compareLength will compare the two column lengths. The return value:
/// (1) -1 means that l is shorter than r;
/// (2) 0 means that l equals to r;
/// (3) 1 means that l is longer than r;
// go: compareLength L1
fn compare_length(l: isize, r: isize) -> isize {
    if l == r {
        return 0;
    }
    if l == UnspecifiedLength {
        return 1;
    }
    if r == UnspecifiedLength {
        return -1;
    }
    if l > r {
        return 1;
    }
    -1
}

/// dominate return true if each column of c2 exists in c1 and c2's column length is no longer than c1's column length.
// go: Col2Len.dominate L1
fn dominate(c1: &Col2Len, c2: &Col2Len) -> bool {
    if c2.len() > c1.len() {
        return false;
    }
    for (col_id, len2) in c2 {
        match c1.get(col_id) {
            Some(len1) if compare_length(*len2, *len1) != 1 => {}
            _ => return false,
        }
    }
    true
}

/// CompareCol2Len will compare the two Col2Len maps. The last return value is used to indicate whether they are comparable.
/// When the second return value is true, the first return value:
/// (1) -1 means that c1 is worse than c2;
/// (2) 0 means that c1 equals to c2;
/// (3) 1 means that c1 is better than c2;
// go: CompareCol2Len L1
pub fn compare_col2_len(c1: &Col2Len, c2: &Col2Len) -> (isize, bool) {
    let (l1, l2) = (c1.len(), c2.len());
    if l1 > l2 {
        if dominate(c1, c2) {
            return (1, true);
        }
        return (0, false);
    }
    if l1 < l2 {
        if dominate(c2, c1) {
            return (-1, true);
        }
        return (0, false);
    }
    // If c1 and c2 have the same columns but have different lengths on some column, we regard c1 and c2 incomparable.
    for (col_id, col_len2) in c2 {
        match c1.get(col_id) {
            Some(col_len1) if col_len1 == col_len2 => {}
            _ => return (0, false),
        }
    }
    (0, true)
}
