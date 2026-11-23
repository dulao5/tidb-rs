//! go-file: pkg/planner/core/plan_cache_rebuild.go
//!
//! Rebuilding a cached plan for the current parameters (the plan-cache exception A3, spec
//! §3.2). Go clones the whole cached plan on every hit and rewrites the ranges of the clone
//! in place. Here the cached plan is an immutable shared `Arc` tree: `rebuild_range` walks
//! it and, where the new range differs from the cached one, writes through
//! [`make_mut_plan`], which copies that node (and, through [`map_children`] and the
//! reader's chain, the nodes on the path to it); everything else stays shared with the
//! cache. A parameter that gives the cached range again copies nothing. The branches,
//! the safety check `isSafeRange` and `convertConstant2Datum` are Go's.
//!
//! First stage (A3 spec §7): `PointGetPlan` and `PhysicalTableReader` → `PhysicalTableScan`;
//! the other cacheable operators take Go's "rebuild the children" branch. The index readers,
//! `BatchPointGet`, the index joins, the index merge reader and the DML plans cannot be
//! cached yet (`instance_cacheable` is false), so their branches are never reached.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_chunk::Row;
use tidb_expression::constant::Constant;
use tidb_expression::expression::{EvalState, ExprRef};
use tidb_expression::{Column, col_info_to_col};
use tidb_kv::key::{Handle, IntHandle};
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_sessionctx::ranger::types::{Range, full_int_range, full_range, has_full_range};
use tidb_types::collate;
use tidb_types::datum::Datum;

use crate::base::PhysicalPlan;
use crate::core::initialize::flatten_list_push_down_plan;
use crate::core::physical_plans::{PhysicalTableReader, PhysicalTableScan};
use crate::core::point_get_plan::PointGetPlan;
use crate::planctx::PlanCtx;
use crate::ranger::detacher::{DetachRangeResult, detach_cond_and_build_range_for_index};
use crate::ranger::ranger::build_table_range;
use crate::util::fixcontrol;
use crate::{PhysicalKind, PhysicalPlanRef, make_mut_plan, map_children};

/// RebuildPlan4CachedPlan will rebuild this plan under current user parameters.
///
/// Returns the statement's own root (the cached root itself when nothing changed), or
/// `None` when the cached plan cannot be used (Go's `false`: the cache is off for this
/// statement, or the rebuild failed, with Go's warning). A boundary stub reached by the
/// rebuild is returned as an error (the statement stops, as everywhere in the new stack).
// go: RebuildPlan4CachedPlan L2 (path copying instead of rewriting a clone, A3 spec §3.2)
pub fn rebuild_plan4_cached_plan(
    ctx: &mut PlanCtx<'_>,
    cached: &PhysicalPlanRef,
) -> Result<Option<PhysicalPlanRef>, Error> {
    if !ctx.vars.stmt_ctx().plan_cache_tracker.use_cache() {
        return Ok(None); // plan-cache is disabled for this query
    }

    ctx.vars.stmt_ctx_mut().in_prepared_plan_building = true;
    let mut p = Arc::clone(cached);
    let res = rebuild_range(ctx, &mut p);
    ctx.vars.stmt_ctx_mut().in_prepared_plan_building = false;
    if let Err(err) = res {
        if blocked_by(&err).is_some() {
            return Err(err);
        }
        ctx.vars.stmt_ctx().append_warning(Error::plain(format!(
            "skip plan-cache: plan rebuild failed, {}",
            String::from_utf8_lossy(&err.error())
        )));
        return Ok(None); // fail to rebuild ranges
    }
    if !ctx.vars.stmt_ctx().plan_cache_tracker.use_cache() {
        // in this case, the UseCache flag changes from `true` to `false`, then there must be some
        // over-optimized operations were triggered, return `false` for safety here.
        return Ok(None);
    }
    Ok(Some(p))
}

/// `errors.New(msg)` of the rebuild.
// glue: errors.New
fn rebuild_err(msg: &str) -> Error {
    tidb_base::errors::new(msg.to_owned())
}

/// rebuildRange doesn't set mem limit for building ranges (see Go's comment: the cached
/// plan built its complete ranges under the limit, and a range fallback here could fetch
/// more rows than asked for).
///
/// `p` is a slot: when something below changes, the node in the slot is replaced by a copy
/// (A3 spec §3.2).
// go: rebuildRange L2 (path copying; the first-stage operators, see the module doc)
pub fn rebuild_range(ctx: &mut PlanCtx<'_>, p: &mut PhysicalPlanRef) -> Result<(), Error> {
    match p.kind() {
        PhysicalKind::TableScan => {
            let ts = p
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
                .expect("PhysicalTableScan");
            if let Some(ranges) = build_range_for_table_scan(ctx, ts)? {
                make_mut_plan(p)
                    .as_any_mut()
                    .downcast_mut::<PhysicalTableScan>()
                    .expect("PhysicalTableScan")
                    .ranges = ranges;
            }
        }
        PhysicalKind::TableReader => {
            // rebuildRange(x.TablePlans[0]): the scan at the bottom of the reader's chain;
            // the chain and the flat list are rebuilt when it changes (N1 §3 item 9).
            let x = p
                .as_any()
                .downcast_ref::<PhysicalTableReader>()
                .expect("PhysicalTableReader");
            let Some(chain) = &x.table_plan else {
                return Ok(());
            };
            let mut new_chain = Arc::clone(chain);
            rebuild_chain_bottom(ctx, &mut new_chain)?;
            if !Arc::ptr_eq(&new_chain, chain) {
                let x = make_mut_plan(p)
                    .as_any_mut()
                    .downcast_mut::<PhysicalTableReader>()
                    .expect("PhysicalTableReader");
                x.table_plans = flatten_list_push_down_plan(&new_chain);
                x.table_plan = Some(new_chain);
            }
        }
        PhysicalKind::PointGet => rebuild_point_get(ctx, p)?,
        // case base.PhysicalPlan: rebuild the children.
        _ => {
            let mut err = None;
            map_children(p, |mut child| {
                if err.is_none()
                    && let Err(e) = rebuild_range(ctx, &mut child)
                {
                    err = Some(e);
                }
                child
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// `rebuildRange(x.TablePlans[0])` on the reader's chain: `TablePlans[0]` is the leaf of the
/// chain (the flat list starts at the bottom), reached through the chain's single children.
// glue: N1 §3 item 9 (TablePlans[0] is the bottom of the chain)
fn rebuild_chain_bottom(ctx: &mut PlanCtx<'_>, p: &mut PhysicalPlanRef) -> Result<(), Error> {
    if p.children().is_empty() {
        return rebuild_range(ctx, p);
    }
    let mut err = None;
    let mut first = true;
    map_children(p, |mut child| {
        if first
            && err.is_none()
            && let Err(e) = rebuild_chain_bottom(ctx, &mut child)
        {
            err = Some(e);
        }
        first = false;
        child
    });
    err.map_or(Ok(()), Err)
}

/// The `*PointGetPlan` arm of `rebuildRange`: computes the handle and the index values for
/// the current parameters, and writes them only if they differ from the cached ones.
// go-part: rebuildRange (case *PointGetPlan)
fn rebuild_point_get(ctx: &mut PlanCtx<'_>, p: &mut PhysicalPlanRef) -> Result<(), Error> {
    let x = p
        .as_any()
        .downcast_ref::<PointGetPlan>()
        .expect("PointGetPlan");
    if x.tbl_info.get_partition_info().is_some()
        && fixcontrol::get_bool_with_default(
            ctx.vars.get_optimizer_fix_control_map(),
            fixcontrol::Fix33031,
            false,
        )
    {
        return Err(Error::plain(
            "Fix33031 fix-control set and partitioned table in cached Point Get plan",
        ));
    }
    let mut handle: Option<Handle> = None;
    let mut index_values: Option<Vec<Datum>> = None;
    // if access condition is not nil, which means it's a point get generated by cbo.
    if !x.access_conditions.is_empty() {
        let rctx = ctx.ranger_ctx();
        if x.index_info.is_some() {
            let ranges = detach_cond_and_build_range_for_index(
                &rctx,
                &x.access_conditions,
                &x.idx_cols,
                &x.idx_col_lens,
                0,
            )?;
            if ranges.ranges.len() != 1
                || !is_safe_range(&x.access_conditions, &ranges, false, None)
            {
                return Err(rebuild_err("rebuild to get an unsafe range"));
            }
            let mut vals = x.index_values.clone();
            for (i, v) in vals.iter_mut().enumerate() {
                *v = ranges.ranges[0].low_val[i].clone();
            }
            index_values = Some(vals);
        } else {
            let mut pk_col: Option<&Arc<Column>> = None;
            let mut unsigned_int_handle = false;
            if x.tbl_info.pk_is_handle {
                if let Some(pk_col_info) = x.tbl_info.get_pk_col_info() {
                    pk_col = col_info_to_col(&x.schema.columns, pk_col_info);
                }
                if !x.tbl_info.is_common_handle {
                    unsigned_int_handle = true;
                }
            }
            if let Some(pk_col) = pk_col {
                let (ranges, access_conds, remaining_conds) =
                    build_table_range(x.access_conditions.clone(), &rctx, &pk_col.ret_type, 0)?;
                let res = DetachRangeResult {
                    ranges,
                    access_conds,
                    remained_conds: remaining_conds,
                    ..Default::default()
                };
                if res.ranges.len() != 1
                    || !is_safe_range(&x.access_conditions, &res, unsigned_int_handle, None)
                {
                    return Err(rebuild_err("rebuild to get an unsafe range"));
                }
                handle = Some(Handle::Int(IntHandle(res.ranges[0].low_val[0].get_int64())));
            }
        }
    }
    if let Some(con) = &x.handle_constant {
        let ft = x
            .handle_field_type
            .as_deref()
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let d_val = convert_constant2_datum(ctx, con, ft)?;
        let tc = ctx.vars.stmt_ctx().type_ctx();
        let (iv, err) = d_val.to_int64(&tc);
        err?;
        handle = Some(Handle::Int(IntHandle(iv)));
    } else {
        for (i, param) in x.index_constants.iter().enumerate() {
            if let Some(param) = param {
                let d_val = convert_constant2_datum(ctx, param, &x.cols_field_type[i])?;
                index_values.get_or_insert_with(|| x.index_values.clone())[i] = d_val;
            }
        }
    }

    // Write only what changed (A3 spec §3.2: the same parameters copy nothing).
    let handle_changed = handle
        .as_ref()
        .is_some_and(|h| !same_int_handle(x.handle.as_ref(), h));
    let values_changed = index_values.as_ref().is_some_and(|v| {
        v.len() != x.index_values.len()
            || v.iter()
                .zip(&x.index_values)
                .any(|(a, b)| !a.equals(Some(b)))
    });
    if handle_changed || values_changed {
        let x = make_mut_plan(p)
            .as_any_mut()
            .downcast_mut::<PointGetPlan>()
            .expect("PointGetPlan");
        if handle_changed {
            x.handle = handle;
        }
        if values_changed && let Some(v) = index_values {
            x.index_values = v;
        }
    }
    Ok(())
}

/// Whether the cached handle is the int handle `h`.
// glue: A3 spec §3.2 (the comparison of "same, do not write")
fn same_int_handle(cached: Option<&Handle>, h: &Handle) -> bool {
    match (cached, h) {
        (Some(Handle::Int(a)), Handle::Int(b)) => a == b,
        _ => false,
    }
}

/// convertConstant2Datum: the constant's value for the current parameters, converted to
/// `target`; the converted result must be the same as the original value.
// go: convertConstant2Datum L1 [M-CTX]
pub fn convert_constant2_datum(
    ctx: &PlanCtx<'_>,
    con: &Constant,
    target: &FieldType,
) -> Result<Datum, Error> {
    let eval_ctx = ctx.eval_ctx();
    let mut val = con.eval(&*eval_ctx, &EvalState::new(), Row::default())?;
    let tc = ctx.vars.stmt_ctx().type_ctx();
    let (d_val, err) = val.convert_to(&tc, target);
    err?;
    // The converted result must be same as original datum.
    let (cmp, err) = d_val.compare(&tc, &val, collate::get_collator(target.get_collate()));
    if err.is_err() || cmp != 0 {
        return Err(rebuild_err(
            "Convert constant to datum is failed, because the constant has changed after the covert",
        ));
    }
    Ok(d_val)
}

/// buildRangeForTableScan: the ranges of `ts` for the current parameters; `None` when they
/// are the cached ones (nothing to write).
// go: buildRangeForTableScan L2 (returns the ranges instead of writing them; GroupRangesByCols is blocked-by:D10.2)
pub fn build_range_for_table_scan(
    ctx: &PlanCtx<'_>,
    ts: &PhysicalTableScan,
) -> Result<Option<Vec<Range>>, Error> {
    let rctx = ctx.ranger_ctx();
    let ranges;
    if ts.table.is_common_handle {
        let pk = tidb_infoschema::tables::find_primary_index(&ts.table);
        let mut pk_cols: Vec<Arc<Column>> = Vec::new();
        let mut pk_cols_len: Vec<isize> = Vec::new();
        let table_columns = ts.table.columns.as_deref().unwrap_or_default();
        if let Some(pk) = pk {
            for col_info in pk.columns.iter().flatten() {
                if let Some(pk_col) = col_info_to_col(
                    &ts.schema().columns,
                    &table_columns[col_info.offset as usize],
                ) {
                    pk_cols.push(Arc::clone(pk_col));
                    // We need to consider the prefix index.
                    // For example: when we have 'a varchar(50), index idx(a(10))'
                    // So we will get 'colInfo.Length = 50' and 'pkCol.RetType.flen = 10'.
                    // In 'hasPrefix' function from 'util/ranger/ranger.go' file,
                    // we use 'columnLength == types.UnspecifiedLength' to check whether we have prefix index.
                    if col_info.length != UnspecifiedLength
                        && col_info.length == pk_col.ret_type.get_flen()
                    {
                        pk_cols_len.push(UnspecifiedLength);
                    } else {
                        pk_cols_len.push(col_info.length);
                    }
                }
            }
        }
        if !pk_cols.is_empty() {
            let res = detach_cond_and_build_range_for_index(
                &rctx,
                &ts.access_condition,
                &pk_cols,
                &pk_cols_len,
                0,
            )?;
            if !is_safe_range(&ts.access_condition, &res, false, Some(&ts.ranges)) {
                return Err(rebuild_err("rebuild to get an unsafe range"));
            }
            ranges = res.ranges;
        } else {
            if !ts.access_condition.is_empty() {
                return Err(rebuild_err(
                    "fail to build ranges, cannot get the primary key column",
                ));
            }
            ranges = full_range();
        }
    } else {
        let mut pk_col: Option<&Arc<Column>> = None;
        if ts.table.pk_is_handle
            && let Some(pk_col_info) = ts.table.get_pk_col_info()
        {
            pk_col = col_info_to_col(&ts.schema().columns, pk_col_info);
        }
        if let Some(pk_col) = pk_col {
            let (new_ranges, access_conds, remaining_conds) =
                build_table_range(ts.access_condition.clone(), &rctx, &pk_col.ret_type, 0)?;
            // Go checks the cached ranges here (`Ranges: ts.Ranges`), not the new ones.
            let check = DetachRangeResult {
                ranges: ts.ranges.clone(),
                access_conds,
                remained_conds: remaining_conds,
                ..Default::default()
            };
            if !is_safe_range(&ts.access_condition, &check, true, Some(&ts.ranges)) {
                return Err(rebuild_err("rebuild to get an unsafe range"));
            }
            ranges = new_ranges;
        } else {
            if !ts.access_condition.is_empty() {
                return Err(rebuild_err(
                    "fail to build ranges, cannot get the primary key column",
                ));
            }
            ranges = full_int_range(false);
        }
    }

    // Rebuild GroupedRanges if GroupByColIdxs is set
    if !ts.group_by_col_idxs.is_empty() {
        // glue: blocked-by:D10.2 (GroupRangesByCols, the merge sort of grouped ranges)
        return Err(blocked("D10.2", "GroupRangesByCols of a cached table scan"));
    }
    if same_ranges(&ranges, &ts.ranges) {
        return Ok(None);
    }
    Ok(Some(ranges))
}

/// Whether two range lists are the same (A3 spec §3.2: equal ranges are not written).
// glue: A3 spec §3.2
fn same_ranges(a: &[Range], b: &[Range]) -> bool {
    let same_datums = |x: &[Datum], y: &[Datum]| {
        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.equals(Some(q)))
    };
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.low_exclude == y.low_exclude
                && x.high_exclude == y.high_exclude
                && same_datums(&x.low_val, &y.low_val)
                && same_datums(&x.high_val, &y.high_val)
        })
}

/// updateRange writes an index join's rebuilt ranges into its inner scan, and
/// buildRangeForIndexScan rebuilds an index scan's ranges: index joins and index scans are
/// not ported (the index readers and the index join operators, D10.1), so a plan with them is
/// not cacheable (`instance_cacheable` is false) and these are never reached.
// glue: stub of updateRange, blocked-by:D10.1 (index joins are not ported: their plans are not cached)
// glue: stub of buildRangeForIndexScan, blocked-by:D10.1 (index scans are not ported: their plans are not cached)
pub fn build_range_for_index_scan() -> Result<(), Error> {
    Err(blocked(
        "D10.1",
        "the range rebuild of a cached index scan or index join",
    ))
}

/// checkRebuiltRange checks whether the re-built range is safe.
/// To re-use a cached plan, the planner needs to rebuild the access range, but as
/// parameters change, some unsafe ranges may occur.
/// For example, the first time the planner can build a range `(2, 5)` from `a>2 and a<(?)5`, but if the
/// parameter changes to `(?)1`, then it'll get an unsafe range `(empty)`.
/// To make plan-cache safer, let the planner abandon the cached plan if it gets an unsafe range here.
// go: isSafeRange L1
pub fn is_safe_range(
    access_conds: &[ExprRef],
    rebuilt_result: &DetachRangeResult,
    unsigned_int_handle: bool,
    original_range: Option<&[Range]>,
) -> bool {
    if !rebuilt_result.remained_conds.is_empty() // the ranger generates some other extra conditions
        || rebuilt_result.access_conds.len() != access_conds.len() // not all access conditions are used
        || rebuilt_result.ranges.is_empty()
    // get an empty range
    {
        return false;
    }

    if !access_conds.is_empty() // if have accessConds, and
        && has_full_range(&rebuilt_result.ranges, unsigned_int_handle) // get an full range, and
        && original_range.is_some_and(|o| !has_full_range(o, unsigned_int_handle))
    // the original range is not a full range
    {
        return false;
    }

    true
}
