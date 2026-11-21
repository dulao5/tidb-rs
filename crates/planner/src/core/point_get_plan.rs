//! go-file: pkg/planner/core/point_get_plan.go
//!
//! The point get fast path (block D10.8, VS1-11): `TryFastPlan` recognises a single-table
//! `SELECT` whose `WHERE` is an equality on the int handle (or `_tidb_rowid`) or on every
//! column of a unique index and builds a `PointGetPlan` without the optimizer
//! (`specs/D10.8-point-get/spec.md`).
//!
//! Mapping:
//! - **The context (`M-CTX`).** Go's plan keeps `ctx base.PlanContext`; a plan node keeps no
//!   context here (N1 §7 item 2), so the methods that read it (`OperatorInfo`,
//!   `PrunePartitions`) take what they read.
//! - **Shared metadata (N13).** `TblInfo` / `IndexInfo` are the info schema's `Arc`s
//!   (`plan-cache-clone:"shallow"`). `handleFieldType` / `ColsFieldType` point into the
//!   column's `FieldType` in Go; `ColumnInfo.field_type` is a value here, so they are a copy
//!   in an `Arc` taken when the plan is built (never written afterwards).
//! - **The embedding (`M-SELF`).** Go's `PointGetPlan` embeds `baseimpl.Plan` and implements
//!   every `PhysicalPlan` method itself; it embeds a `BasePhysicalPlan` here for the trait's
//!   accessors (its `plan` is the `baseimpl.Plan`), and overrides each method Go defines.
//! - **Not ported here (stubs):** `BatchPointGetPlan` and `tryWhereIn2BatchPointGet`
//!   (a `WHERE ... IN (...)` that may become a batch point get is `blocked-by:D10.8`);
//!   point UPDATE / DELETE (`blocked-by:X`); `PhysicalTableDual` for an impossible condition
//!   (`blocked-by:D10.1`); the explain / trace text needs the access object (`blocked-by:D10.9`);
//!   partition pruning by value (`blocked-by:D06.4`); the latest-schema index check of
//!   `FOR UPDATE` / read-committed sessions (`blocked-by:D06.5`); parameter markers
//!   (`blocked-by:D13.1`); optimizer hints in comments (`blocked-by:D02.2`).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::config::get_global_config;
use tidb_base::dbterror::plannererrors::{ErrColumnaccessDenied, ErrTableaccessDenied};
use tidb_base::errors::RedactLogEnable;
use tidb_base::mysql::{
    self, SelectPriv, TypeBit, TypeLonglong, TypeString, has_pri_key_flag, has_unsigned_flag,
};
use tidb_base::redact;
use tidb_execdetails::execdetails::RuntimeStatsColl;
use tidb_expression::constant::Constant;
use tidb_expression::expression::ExprRef;
use tidb_expression::{Column, Schema, new_schema};
use tidb_infoschema::metadef::is_mem_db;
use tidb_kv::key::{Handle, IntHandle};
use tidb_model::{
    ColumnInfo, ExtraHandleName, ExtraRowChecksumID, IndexInfo, PartitionInfo, StatePublic,
    TableInfo, find_column_info, new_extra_handle_col_info,
};
use tidb_parser::ast::{
    self, BinaryOperationExpr, ColumnName, Expr, IndexHint, IndexHintScope, IndexHintType, Op,
    ResultSetNode, SelectField, SelectLockInfo, SelectLockType, SelectStmt, Stmt, TableRefsClause,
};
use tidb_parser::auth::get_user_and_host_name;
use tidb_parser::gocharset::CollationBin;
use tidb_parser::model::CIStr;
use tidb_parser::types::{ETString, FieldType};
use tidb_sessionctx::stmtctx::{StatementContext, TableEntry};
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::datum::{
    Datum, KindBinaryLiteral, KindFloat32, KindFloat64, KindInt64, KindMysqlDecimal, KindString,
    KindUint64,
};
use tidb_types::errors as terr;
use tidb_types::field_name::{FieldName, NameSlice};
use tidb_types::field_type::{default_charset_for_type, new_field_type};
use tidb_types::mydecimal::MyDecimal;

use crate::base::PhysicalPlan;
use crate::baseimpl::new_base_plan;
use crate::core::logical_plan_builder::extract_limit_count_offset;
use crate::core::optimizer::{check_stable_result_mode, check_table_lock};
use crate::core::plan::{
    get_actual_probe_cnt_from_probe_parents, get_estimated_probe_cnt_from_probe_parents,
};
use crate::core::planbuilder::VisitInfo;
use crate::core::planbuilder::is_for_update_read_select_lock;
use crate::core::resolve::{ContextRef, NodeW, TableNameW};
use crate::physicalop::{BasePhysicalPlan, PhysicalPlanRef};
use crate::planctx::PlanCtx;
use crate::property::{PhysicalProperty, StatsInfo};
use crate::util::costusage::CostVer2;
use crate::util::fixcontrol::{self, Fix52592};
use crate::util::handle_cols::{HandleCols, new_common_handle_cols, new_int_handle_cols};

/// GlobalWithoutColumnPos marks the index has no partition column.
pub const GlobalWithoutColumnPos: isize = -1;

/// `plancodec.TypePointGet`.
const TypePointGet: &str = "Point_Get";

/// `unsafe.Sizeof(PointGetPlan{})` on 64-bit Go (`emptyPointGetPlanSize`).
const EMPTY_POINT_GET_PLAN_SIZE: i64 = 528;
/// `size.SizeOfInt`, `size.SizeOfPointer`.
const SIZE_OF_INT: i64 = 8;
const SIZE_OF_POINTER: i64 = 8;

/// The parser's `CIStr` (the Rust AST keeps the original text) as the model's.
// glue: the AST's CIStr is the model's in Go
fn ci(s: &ast::CIStr) -> CIStr {
    CIStr::new(&s.o)
}

/// Go's nil slice of the metadata is an empty one.
// glue: Go nil slice
fn sl<T>(s: &Option<Vec<T>>) -> &[T] {
    s.as_deref().unwrap_or(&[])
}

/// PointGetPlan is a fast plan for simple point get.
/// When we detect that the statement has a unique equal access condition, this plan is used.
/// This plan is much faster to build and to execute because it avoids the optimization and coprocessor cost.
///
/// Go's unexported fields are public here: the executor (D12.1) and its tests build plans.
#[derive(Clone, Default)]
pub struct PointGetPlan {
    /// Go embeds `baseimpl.Plan` (`base.plan`); see the module doc.
    pub base: BasePhysicalPlan,

    /// probeParents records the IndexJoins and Applys with this operator in their inner children.
    /// Please see comments in PhysicalPlan for details.
    pub probe_parents: Vec<PhysicalPlanRef>,
    /// explicit partition selection
    pub partition_names: Vec<CIStr>,

    pub db_name: String,
    pub schema: Schema,
    pub tbl_info: Arc<TableInfo>,
    pub index_info: Option<Arc<IndexInfo>>,
    /// Go's `*int`.
    pub partition_idx: Option<isize>,
    pub handle: Option<Handle>,
    pub handle_constant: Option<Arc<Constant>>,
    pub handle_field_type: Option<Arc<FieldType>>,
    pub handle_col_offset: usize,
    pub index_values: Vec<Datum>,
    /// Go's `[]*expression.Constant` with nil entries (no parameter marker).
    pub index_constants: Vec<Option<Arc<Constant>>>,
    pub cols_field_type: Vec<Arc<FieldType>>,
    pub idx_cols: Vec<Arc<Column>>,
    pub idx_col_lens: Vec<isize>,
    pub access_conditions: Vec<ExprRef>,
    pub unsigned_handle: bool,
    pub is_table_dual: bool,
    pub lock: bool,
    pub output_names: NameSlice,
    pub lock_wait_time: i64,
    pub columns: Vec<Arc<ColumnInfo>>,
    pub cost: f64,

    /// unfoldFromWildCard indicates whether the FieldName in 'outputNames' is unfolded from wildcard
    /// It is used by checking column privileges. Why do not add the field name to visitInfo onsite?
    /// Because if the PointGetPlan is inside UPDATE or DELETE, we need not to check the SelectPriv of
    /// all output names. In the above two statements, only columns in WHERE clause are needed to check.
    pub unfold_from_wild_card: Vec<bool>,
    pub cols_in_where_clause: Vec<String>,

    // required by cost model
    pub plan_cost_init: bool,
    pub plan_cost: f64,
    pub plan_cost_ver2: CostVer2,
    /// accessCols represents actual columns the PointGet will access, which are used to calculate row-size
    pub access_cols: Vec<Arc<Column>>,
}

/// `nameValuePair`: a `column = constant` condition of the WHERE clause.
#[derive(Clone, Debug, Default)]
pub struct NameValuePair {
    pub col_name: String,
    pub col_field_type: Option<Arc<FieldType>>,
    pub value: Datum,
    pub con: Option<Arc<Constant>>,
}

impl PhysicalPlan for PointGetPlan {
    crate::physical_plan_boilerplate!(PointGet);

    /// Schema implements the Plan interface.
    // go: PointGetPlan.Schema L1
    fn schema(&self) -> &Schema {
        &self.schema
    }
    /// The schema field (Go writes `p.schema`; `SetSchema` is not a method of the plan).
    // glue: accessor (Go writes PointGetPlan.schema)
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
    /// OutputNames returns the outputting names of each column.
    // go: PointGetPlan.OutputNames L1
    fn output_names(&self) -> NameSlice {
        self.output_names.clone()
    }
    /// SetOutputNames sets the outputting name by the given slice.
    // go: PointGetPlan.SetOutputNames L1
    fn set_output_names(&mut self, names: NameSlice) {
        self.output_names = names;
    }
    /// ExplainInfo implements Plan interface.
    ///
    /// `AccessObject()` (`access_object.go`) is D10.9 (VS1-31): the text needs it.
    // glue: stub of PointGetPlan.ExplainInfo, blocked-by:D10.9 (the access object of a point get)
    fn explain_info(&self) -> String {
        panic!(
            "{}",
            blocked("D10.9", "PointGetPlan.ExplainInfo (AccessObject)")
        );
    }
    /// ExplainNormalizedInfo implements Plan interface.
    // glue: stub of PointGetPlan.ExplainNormalizedInfo, blocked-by:D10.9 (the access object of a point get)
    fn explain_normalized_info(&self) -> String {
        panic!(
            "{}",
            blocked("D10.9", "PointGetPlan.ExplainNormalizedInfo (AccessObject)")
        );
    }
    /// GetChildReqProps gets the required property by child index.
    // go: PointGetPlan.GetChildReqProps L1
    fn get_child_req_props(&self, _idx: isize) -> Option<&Arc<PhysicalProperty>> {
        None
    }
    /// StatsCount will return the RowCount of property.StatsInfo for this plan.
    // go: PointGetPlan.StatsCount L1
    fn stats_count(&self) -> f64 {
        1.0
    }
    /// StatsInfo will return the RowCount of property.StatsInfo for this plan.
    ///
    /// Go sets `{RowCount: 1}` on first read when the plan has none; `newPointGetPlan` always
    /// sets it, and a read through `&self` cannot (a plan built otherwise must set it).
    // go: PointGetPlan.StatsInfo L2 (the lazy default is set by newPointGetPlan, the read is &self)
    fn stats_info(&self) -> Option<&Arc<StatsInfo>> {
        self.base.plan.stats_info()
    }
    /// ExtractCorrelatedCols implements PhysicalPlan interface.
    // go: PointGetPlan.ExtractCorrelatedCols L1
    fn extract_correlated_cols(&self) -> Vec<Arc<tidb_expression::column::CorrelatedColumn>> {
        Vec::new()
    }
    /// Children gets all the children.
    // go: PointGetPlan.Children L1
    fn children(&self) -> &[PhysicalPlanRef] {
        &[]
    }
    /// SetChildren sets the children for the plan.
    // go: PointGetPlan.SetChildren L1
    fn set_children(&mut self, _children: Vec<PhysicalPlanRef>) {}
    /// SetChild sets a specific child for the plan.
    // go: PointGetPlan.SetChild L1
    fn set_child(&mut self, _i: isize, _child: PhysicalPlanRef) {}
    /// ResolveIndices resolves the indices for columns. After doing this, the columns can evaluate the rows by their indices.
    ///
    /// `resolveIndicesForVirtualColumn` (D10.9) resolves the virtual expressions of the
    /// schema; the fast path refuses tables with generated columns, so its schema has none, and
    /// a virtual column is a boundary stub.
    // go: PointGetPlan.ResolveIndices L2 (a virtual column needs resolveIndicesForVirtualColumn: blocked-by:D10.9)
    fn resolve_indices(&mut self) -> Result<(), Error> {
        if self.schema.columns.iter().any(|c| c.virtual_expr.is_some()) {
            // glue: blocked-by:D10.9 (resolveIndicesForVirtualColumn of a virtual column)
            return Err(blocked(
                "D10.9",
                "resolveIndicesForVirtualColumn (a virtual column of a point get)",
            ));
        }
        Ok(())
    }
    /// Clone implements PhysicalPlan interface.
    // go: PointGetPlan.Clone L1 [M-ERR]
    fn clone_plan(&self) -> Result<PhysicalPlanRef, Error> {
        Err(Error::plain(
            "*core.PointGetPlan doesn't support cloning".to_owned(),
        ))
    }
    /// AppendChildCandidate implements PhysicalPlan interface.
    // go: PointGetPlan.AppendChildCandidate L1
    fn append_child_candidate(&self, _op: &crate::util::optimizetrace::PhysicalOptimizeOp) {}
    /// MemoryUsage return the memory usage of PointGetPlan
    // go: PointGetPlan.MemoryUsage L1
    fn memory_usage(&self) -> i64 {
        let mut sum = EMPTY_POINT_GET_PLAN_SIZE
            + self.base.plan.memory_usage()
            + self.db_name.len() as i64
            + self.idx_col_lens.capacity() as i64 * SIZE_OF_INT
            + (self.index_constants.capacity()
                + self.cols_field_type.capacity()
                + self.idx_cols.capacity()
                + self.output_names.capacity()
                + self.columns.capacity()
                + self.access_cols.capacity()) as i64
                * SIZE_OF_POINTER;
        // p.schema != nil
        sum += self.schema.memory_usage();
        if self.partition_idx.is_some() {
            sum += SIZE_OF_INT;
        }
        if let Some(c) = &self.handle_constant {
            sum += c.memory_usage();
        }
        if let Some(ft) = &self.handle_field_type {
            sum += ft.memory_usage();
        }

        for datum in &self.index_values {
            sum += datum.mem_usage();
        }
        for idx_const in self.index_constants.iter().flatten() {
            sum += idx_const.memory_usage();
        }
        for ft in &self.cols_field_type {
            sum += ft.memory_usage();
        }
        for col in &self.idx_cols {
            sum += col.memory_usage();
        }
        for cond in &self.access_conditions {
            sum += cond.memory_usage();
        }
        for name in &self.output_names {
            sum += name.memory_usage();
        }
        for col in &self.access_cols {
            sum += col.memory_usage();
        }
        sum
    }
    /// SetProbeParents implements PhysicalPlan interface.
    // go: PointGetPlan.SetProbeParents L1
    fn set_probe_parents(&mut self, probe_parents: Vec<PhysicalPlanRef>) {
        self.probe_parents = probe_parents;
    }
    /// GetEstRowCountForDisplay implements PhysicalPlan interface.
    // go: PointGetPlan.GetEstRowCountForDisplay L1
    fn get_est_row_count_for_display(&self) -> f64 {
        let row_count = self.stats_info().map_or(1.0, |s| s.row_count);
        row_count * get_estimated_probe_cnt_from_probe_parents(&self.probe_parents)
    }
    /// GetActualProbeCnt implements PhysicalPlan interface.
    // go: PointGetPlan.GetActualProbeCnt L1
    fn get_actual_probe_cnt(&self, stats_coll: &RuntimeStatsColl) -> i64 {
        get_actual_probe_cnt_from_probe_parents(&self.probe_parents, stats_coll)
    }
}

impl PointGetPlan {
    /// Cost implements PhysicalPlan interface
    // go: PointGetPlan.Cost L1
    pub fn cost(&self) -> f64 {
        self.cost
    }

    /// SetCost implements PhysicalPlan interface
    // go: PointGetPlan.SetCost L1
    pub fn set_cost(&mut self, cost: f64) {
        self.cost = cost;
    }

    /// Attach2Task makes the current physical plan as the father of task's physicalPlan and updates the cost of
    /// current task. If the child's task is cop task, some operator may close this task and return a new rootTask.
    ///
    /// A point get attaches to no task: Go returns nil (`base.Task` is D10.1's, so the result
    /// is `None` of nothing).
    // go: PointGetPlan.Attach2Task L1
    pub fn attach2_task(&self) -> Option<std::convert::Infallible> {
        None
    }

    /// ToPB converts physical plan to tipb executor.
    // go: PointGetPlan.ToPB L1
    pub fn to_pb(&self) -> Result<Option<tipb::Executor>, Error> {
        Ok(None)
    }

    /// OperatorInfo implements dataAccesser interface.
    ///
    /// `enable_redact_log` is `p.SCtx().GetSessionVars().EnableRedactLog` (`M-CTX`).
    // go: PointGetPlan.OperatorInfo L1 [M-CTX]
    pub fn operator_info(&self, normalized: bool, enable_redact_log: &str) -> String {
        if self.handle.is_none() && !self.lock {
            return String::new();
        }
        let mut buffer: Vec<u8> = Vec::new();
        if let Some(handle) = &self.handle {
            if normalized {
                buffer.extend_from_slice(b"handle:?");
            } else {
                let redact_mode = enable_redact_log;
                let redact_on = redact_mode == RedactLogEnable;
                buffer.extend_from_slice(b"handle:");
                if redact_on {
                    buffer.push(b'?');
                } else if self.unsigned_handle {
                    redact::write_redact(
                        &mut buffer,
                        (handle.int_value() as u64).to_string().as_bytes(),
                        redact_mode,
                    );
                } else {
                    redact::write_redact(&mut buffer, handle.string().as_bytes(), redact_mode);
                }
            }
        }
        if self.lock {
            if self.handle.is_some() {
                buffer.extend_from_slice(b", lock");
            } else {
                buffer.extend_from_slice(b"lock");
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// LoadTableStats preloads the stats data for the physical table
    ///
    /// `loadTableStats` (`stats.go`) is D10.2's; a table without a matching partition loads
    /// nothing (Go's early return).
    // go: PointGetPlan.LoadTableStats L2 (loadTableStats is blocked-by:D10.2)
    pub fn load_table_stats(&self) -> Result<(), Error> {
        let mut table_id = self.tbl_info.id;
        if let Some(idx) = self.partition_idx {
            if idx < 0 {
                // No matching partitions
                return Ok(());
            }
            if let Some(pi) = self.tbl_info.get_partition_info() {
                table_id = sl(&pi.definitions)[idx as usize].id;
            }
        }
        let _ = table_id;
        // glue: blocked-by:D10.2 (loadTableStats)
        Err(blocked(
            "D10.2",
            "loadTableStats (the stats of a point get)",
        ))
    }

    /// PrunePartitions will check which partition to use
    /// returns true if no matching partition
    ///
    /// `use_cache` is `p.SCtx().GetSessionVars().StmtCtx.UseCache()` (`M-CTX`). Pruning by the
    /// handle or index values needs the partitioned table (`GetPartitionIdxByRow`, D06.4 part
    /// 2) and, for non-binary collations, the ranger (D10.4): those branches are a boundary
    /// stub.
    // go: PointGetPlan.PrunePartitions L2 (pruning by value is blocked-by:D06.4: GetPartitionedTable / GetPartitionIdxByRow)
    pub fn prune_partitions(&mut self, use_cache: bool) -> Result<bool, Error> {
        let Some(pi) = self.tbl_info.get_partition_info() else {
            return Ok(false);
        };
        if self.index_info.as_ref().is_some_and(|i| i.global) {
            // reading for the Global Index / table id
            return Ok(false);
        }
        // _tidb_rowid + specify a partition
        if self.index_info.is_none()
            && !self.tbl_info.has_clustered_index()
            && self.partition_names.len() == 1
        {
            for (i, def) in sl(&pi.definitions).iter().enumerate() {
                if def.name.l == self.partition_names[0].l {
                    self.partition_idx = Some(i as isize);
                    break;
                }
            }
            return Ok(false);
        }
        // If tryPointGetPlan did generate the plan,
        // then PartitionIdx is not set and needs to be set here!
        // There are two ways to get here from static mode partition pruning:
        // 1) Converting a set of partitions into a Union scan
        //    - This should NOT be cached and should already be having PartitionIdx set!
        // 2) Converted to PointGet from checkTblIndexForPointPlan
        //    and it does not have the PartitionIdx set
        if !use_cache && self.partition_idx.is_some() {
            return Ok(false);
        }
        // is.TableByID(...).GetPartitionedTable(), then GetPartitionIdxByRow of the handle or
        // the index values (via the ranger for non-binary collations).
        // glue: blocked-by:D06.4 (GetPartitionIdxByRow of the partitioned table)
        Err(blocked(
            "D06.4",
            "partition pruning of a point get (GetPartitionIdxByRow)",
        ))
    }
}

// go: isInExplicitPartitions L1
pub fn is_in_explicit_partitions(pi: Option<&PartitionInfo>, idx: isize, names: &[CIStr]) -> bool {
    if names.is_empty() {
        return true;
    }
    // partIdx can be -1 more to check issue #62458
    let Some(pi) = pi else {
        return false;
    };
    let defs = sl(&pi.definitions);
    if idx < 0 || idx as usize >= defs.len() {
        return false;
    }
    let s = &defs[idx as usize].name.l;
    for name in names {
        if *s == name.l {
            return true;
        }
    }
    false
}

/// TryFastPlan tries to use the PointGetPlan for the query.
///
/// Go returns a `base.Plan` (nil: no fast plan); here `Ok(None)` is nil and `Ok(Some(p))` the
/// point get. The other results of Go are boundary stubs (`Err(blocked)`): a batch point get
/// for `WHERE ... IN (...)`, the table dual of an impossible condition, the point UPDATE /
/// DELETE plans and the optimizer trace (see the module doc).
// go: TryFastPlan L2 (BatchPointGet, TableDual, point UPDATE / DELETE and the optimizer trace are stubs)
pub fn try_fast_plan(
    ctx: &mut PlanCtx<'_>,
    node: &NodeW<'_>,
) -> Result<Option<PhysicalPlanRef>, Error> {
    if check_stable_result_mode(ctx.vars)
        || fixcontrol::get_bool_with_default(&ctx.vars.state.optimizer_fix_control, Fix52592, false)
    {
        // the rule of stabilizing results has not taken effect yet, so cannot generate a plan here in this mode
        // or Fix52592 is turn on to disable fast path for select, update and delete
        return Ok(None);
    }

    ctx.vars
        .state
        .plan_id
        .0
        .store(0, std::sync::atomic::Ordering::SeqCst);
    ctx.vars
        .state
        .plan_column_id
        .0
        .store(0, std::sync::atomic::Ordering::SeqCst);
    // The session user (`SessionVars.User`, D05.2) is not kept on the Rust session vars: it
    // only goes into the privilege errors, which the privilege manager raises (none, D04.1).
    let (user_name, host_name) = get_user_and_host_name(None);
    match &*node.node {
        Stmt::Select(x) => {
            if x.select_into_opt.is_some() {
                return Ok(None);
            }
            let mut p: Option<PointGetPlan> = None;
            // `p` is a PhysicalTableDual (not ported: D10.1).
            let mut table_dual = false;
            // Try to convert the `SELECT a, b, c FROM t WHERE (a, b, c) in ((1, 2, 4), (1, 3, 5))` to
            // `PhysicalUnionAll` which children are `PointGet` if exists an unique key (a, b, c) in table `t`
            try_where_in2_batch_point_get(ctx, x, &node.get_resolve_context())?;
            if let Some(mut fp) = try_point_get_plan(
                ctx,
                x,
                &node.get_resolve_context(),
                is_for_update_read_select_lock(x.lock_info.as_deref()),
            )? {
                let Some(visit_infos) = get_visit_info(
                    &user_name,
                    &host_name,
                    &fp.db_name,
                    &fp.tbl_info.name.l,
                    &fp.output_names,
                    &fp.unfold_from_wild_card,
                    &fp.cols_in_where_clause,
                ) else {
                    return Ok(None);
                };
                if check_fast_plan_privilege(&visit_infos).is_err() {
                    return Ok(None);
                }
                if is_mem_db(&fp.db_name) {
                    return Ok(None);
                }
                if fp.is_table_dual {
                    // tableDual := PhysicalTableDual{names: fp.outputNames}, schema fp.Schema(),
                    // Init(ctx, &property.StatsInfo{}, 0): stopped below, after the deferred
                    // checks that would drop it.
                    table_dual = true;
                } else {
                    (fp.lock, fp.lock_wait_time) =
                        get_lock_wait_time(ctx.vars, x.lock_info.as_deref())?;
                    p = Some(fp);
                }
            }
            // The deferred function of Go.
            if ctx.vars.select_limit != u64::MAX && (p.is_some() || table_dual) {
                ctx.vars.stmt_ctx().append_warning(Error::plain(
                    "sql_select_limit is set, so point get plan is not activated".to_owned(),
                ));
                p = None;
                table_dual = false;
            }
            if table_dual {
                // glue: blocked-by:D10.1 (PhysicalTableDual)
                return Err(blocked(
                    "D10.1",
                    "PhysicalTableDual (a point get condition that matches no row)",
                ));
            }
            if ctx.vars.stmt_ctx().enable_optimize_trace && p.is_some() {
                // vars.StmtCtx.OptimizeTracer.SetFastPlan(p.BuildPlanTrace()): the trace
                // reads ExplainInfo, which needs the access object.
                // glue: blocked-by:D10.9 (the optimizer trace of the fast plan)
                return Err(blocked(
                    "D10.9",
                    "the optimizer trace of a fast plan (ExplainInfo)",
                ));
            }
            Ok(p.map(|p| Arc::new(p) as PhysicalPlanRef))
        }
        Stmt::Unported(n) if n.kind.starts_with("Update") || n.kind.starts_with("Delete") => {
            // tryUpdatePointPlan / tryDeletePointPlan: the write path.
            Err(blocked("X", "the point UPDATE / DELETE plans"))
        }
        _ => Ok(None),
    }
}

/// `logicalop.IsSupportedSelectLockType` (`logical_lock.go`, D09.14) checks if the lockType
/// is supported to acquire pessimsitic locks if necessary.
// go: pkg/planner/core/operator/logicalop/logical_lock.go IsSupportedSelectLockType L1
fn is_supported_select_lock_type(lock_type: SelectLockType) -> bool {
    is_select_for_update_lock_type(lock_type) || is_select_for_share_lock_type(lock_type)
}

// go: pkg/planner/core/operator/logicalop/logical_lock.go isSelectForUpdateLockType L1
fn is_select_for_update_lock_type(lock_type: SelectLockType) -> bool {
    lock_type == SelectLockType::ForUpdate
        || lock_type == SelectLockType::ForUpdateNoWait
        || lock_type == SelectLockType::ForUpdateWaitN
}

// go: pkg/planner/core/operator/logicalop/logical_lock.go isSelectForShareLockType L1
fn is_select_for_share_lock_type(lock_type: SelectLockType) -> bool {
    lock_type == SelectLockType::ForShare || lock_type == SelectLockType::ForShareNoWait
}

/// `SessionVars.PessimisticLockEligible` (D05.2) indicates whether pessimistic lock should
/// not be ignored for the current statement execution. There are cases the `for update`
/// clause should not take effect, like autocommit statements with "pessimistic-auto-commit
/// disabled.
///
/// `BulkDMLEnabled` (`tidb_dml_type = bulk`) is a write-path field the Rust session vars do
/// not keep (D05.2 scoped it to block X): with `pessimistic-auto-commit` on (off by default)
/// the answer needs it and is a boundary stub.
// go: pkg/sessionctx/variable/session.go SessionVars.PessimisticLockEligible L2 (BulkDMLEnabled is the write path, X: with pessimistic-auto-commit on it is a boundary stub)
fn pessimistic_lock_eligible(s: &SessionVars) -> Result<bool, Error> {
    if s.stmt_ctx().for_share_lock_enabled_by_noop {
        return Ok(false);
    }
    if !s.is_autocommit() || s.in_txn() {
        return Ok(true);
    }
    if get_global_config()
        .pessimistic_txn
        .pessimistic_auto_commit
        .load()
    {
        // && !s.BulkDMLEnabled
        // glue: blocked-by:X (SessionVars.BulkDMLEnabled)
        return Err(blocked(
            "X",
            "SessionVars.BulkDMLEnabled (tidb_dml_type) with pessimistic-auto-commit",
        ));
    }
    Ok(false)
}

/// `ast.ReadCommitted`.
const ReadCommitted: &str = "READ-COMMITTED";

/// `tikvstore.LockNoWait` (client-go `kv`).
const LockNoWait: i64 = -1;

/// getVisitInfo collects 2 source of visitInfos:
///  1. output names. To distinguish between table-level and column-level privilege error report, unfoldFromWildCard is needed.
///     If the PointPlan is inside UPDATE or DELETE statements, the checking of output names should be ignored.
///  2. columns in WHERE clause.
// go: getVisitInfo L2 (the intest panic of a length mismatch is the logged branch)
pub fn get_visit_info(
    user: &str,
    host: &str,
    db: &str,
    tbl: &str,
    output_names: &NameSlice,
    unfold: &[bool],
    cols_in_where: &[String],
) -> Option<Vec<VisitInfo>> {
    if output_names.len() != unfold.len() {
        log::error!(
            "outputNames length {} not equal to unfold length {}",
            output_names.len(),
            unfold.len()
        );
        return None;
    }
    let mut res = Vec::new();
    for (i, name) in output_names.iter().enumerate() {
        let auth_err = if !unfold[i] {
            ErrColumnaccessDenied.fast_gen_by_args(vec![
                gs("SELECT"),
                gs(user),
                gs(host),
                gs(&name.orig_col_name.l),
                gs(tbl),
            ])
        } else {
            ErrTableaccessDenied.fast_gen_by_args(vec![gs("SELECT"), gs(user), gs(host), gs(tbl)])
        };
        res.push(VisitInfo {
            db: db.to_owned(),
            table: tbl.to_owned(),
            column: name.orig_col_name.l.clone(),
            privilege: SelectPriv.0,
            err: Some(auth_err),
            ..Default::default()
        });
    }
    for col in cols_in_where {
        let err = ErrColumnaccessDenied.fast_gen_by_args(vec![
            gs("SELECT"),
            gs(user),
            gs(host),
            gs(col),
            gs(tbl),
        ]);
        res.push(VisitInfo {
            db: db.to_owned(),
            table: tbl.to_owned(),
            column: col.clone(),
            privilege: SelectPriv.0,
            err: Some(err),
            ..Default::default()
        });
    }
    Some(res)
}

/// A Go string argument of an error message.
// glue: fmt argument
fn gs(s: &str) -> tidb_base::Arg {
    tidb_base::Arg::go_string(s.as_bytes().to_vec())
}

// go: getLockWaitTime L1 [M-CTX]
fn get_lock_wait_time(
    sess_vars: &SessionVars,
    lock_info: Option<&SelectLockInfo>,
) -> Result<(bool, i64), Error> {
    let mut lock = false;
    let mut wait_time = 0;
    if let Some(lock_info) = lock_info
        && is_supported_select_lock_type(lock_info.lock_type)
    {
        // Locking of rows for update using SELECT FOR UPDATE only applies when autocommit
        // is disabled (either by beginning transaction with START TRANSACTION or by setting
        // autocommit to 0. If autocommit is enabled, the rows matching the specification are not locked.
        // See https://dev.mysql.com/doc/refman/5.7/en/innodb-locking-reads.html
        if pessimistic_lock_eligible(sess_vars)? {
            lock = true;
            wait_time = sess_vars.lock_wait_timeout;
            if lock_info.lock_type == SelectLockType::ForUpdateWaitN {
                wait_time = (lock_info.wait_sec * 1000) as i64;
            } else if lock_info.lock_type == SelectLockType::ForUpdateNoWait
                || lock_info.lock_type == SelectLockType::ForShareNoWait
            {
                wait_time = LockNoWait;
            }
        }
    }
    Ok((lock, wait_time))
}

/// `tryWhereIn2BatchPointGet`: a `WHERE (cols) IN (...)` on a unique key becomes a
/// `BatchPointGetPlan`. The batch point get is not ported (VS1-11 excludes it): the checks Go
/// makes before building one are ported, and a statement that passes them stops here (Go
/// returns a batch point get or, when no key matches, nil; the point get path cannot match an
/// `IN` condition either).
// glue: stub of tryWhereIn2BatchPointGet, blocked-by:D10.8 (BatchPointGetPlan, not in VS1)
fn try_where_in2_batch_point_get(
    ctx: &PlanCtx<'_>,
    sel_stmt: &SelectStmt,
    resolve_ctx: &ContextRef,
) -> Result<(), Error> {
    if sel_stmt.order_by.is_some()
        || sel_stmt.group_by.is_some()
        || sel_stmt.limit.is_some()
        || sel_stmt.having.is_some()
        || sel_stmt.distinct
        || !sel_stmt.window_specs.is_empty()
    {
        return Ok(());
    }
    // `expr1 in (1, 2) and expr2 in (1, 2)` isn't PatternInExpr, so it can't use tryWhereIn2BatchPointGet.
    // (expr1, expr2) in ((1, 1), (2, 2)) can hit it.
    let Some(Expr::PatternIn(in_)) = &sel_stmt.where_ else {
        return Ok(());
    };
    if in_.not || in_.list.is_empty() {
        return Ok(());
    }
    let (Some(tbl_name), tbl_alias) = get_single_table_name_and_alias(sel_stmt.from.as_deref())
    else {
        return Ok(());
    };
    // tnW might be nil, in some ut, query is directly 'optimized' without pre-process
    let Some(tn_w) = resolve_ctx.lock().get_table_name(tbl_name).cloned() else {
        return Ok(());
    };
    let tbl = &tn_w.table_info;
    // Skip the optimization with partition selection.
    if !tbl_name.partition_names.is_empty() {
        return Ok(());
    }
    for col in sl(&tbl.columns) {
        if col.is_generated() || col.state != StatePublic {
            return Ok(());
        }
    }
    if build_schema_from_fields(
        &ci(&tbl_name.schema),
        tbl,
        &tbl_alias,
        &sel_stmt.fields.fields,
    )
    .is_none()
    {
        return Ok(());
    }
    // SELECT * FROM t WHERE (key) in ((1), (2))
    let mut col_expr = &in_.expr;
    if let Expr::Paren(p) = col_expr {
        col_expr = p;
    }
    match col_expr {
        Expr::Column(col_name) => {
            if !col_name.table.o.is_empty() && col_name.table.l() != tbl_alias.l {
                return Ok(());
            }
        }
        Expr::Row(row) => {
            for col in &row.values {
                let Expr::Column(c) = col else {
                    return Ok(());
                };
                if !c.table.o.is_empty() && c.table.l() != tbl_alias.l {
                    return Ok(());
                }
            }
        }
        _ => return Ok(()),
    }
    let _ = ctx;
    // newBatchPointGetPlan(...)
    Err(blocked(
        "D10.8",
        "BatchPointGetPlan (WHERE ... IN (...) on a unique key)",
    ))
}

/// tryPointGetPlan determine if the SelectStmt can use a PointGetPlan.
/// Returns nil if not applicable.
/// To use the PointGetPlan the following rules must be satisfied:
/// 1. For the limit clause, the count should at least 1 and the offset is 0.
/// 2. It must be a single table select.
/// 3. All the columns must be public and not generated.
/// 4. The condition is an access path that the range is a unique key.
/// 5. The condition is an equal condition on a unique key or the handle.
// go: tryPointGetPlan L1
pub fn try_point_get_plan(
    ctx: &mut PlanCtx<'_>,
    sel_stmt: &SelectStmt,
    resolve_ctx: &ContextRef,
    check: bool,
) -> Result<Option<PointGetPlan>, Error> {
    if sel_stmt.having.is_some() || sel_stmt.order_by.is_some() {
        return Ok(None);
    } else if let Some(limit) = &sel_stmt.limit {
        match extract_limit_count_offset(&*ctx.expr_ctx, limit) {
            Ok((count, offset)) => {
                if count == 0 || offset > 0 {
                    return Ok(None);
                }
            }
            Err(err) => {
                if tidb_base::blocked::blocked_by(&err).is_some() {
                    return Err(err);
                }
                return Ok(None);
            }
        }
    }
    let (Some(tbl_name), tbl_alias) = get_single_table_name_and_alias(sel_stmt.from.as_deref())
    else {
        return Ok(None);
    };
    // tnW might be nil, in some ut, query is directly 'optimized' without pre-process
    let Some(tn_w) = resolve_ctx.lock().get_table_name(tbl_name).cloned() else {
        return Ok(None);
    };
    let tbl = Arc::clone(&tn_w.table_info);

    let mut pk_col_offset = 0;
    for (i, col) in sl(&tbl.columns).iter().enumerate() {
        // Do not handle generated columns.
        if col.is_generated() {
            return Ok(None);
        }
        // Only handle tables that all columns are public.
        if col.state != StatePublic {
            return Ok(None);
        }
        if has_pri_key_flag(col.get_flag()) {
            pk_col_offset = i;
        }
    }
    let Some((schema, names, unfold)) = build_schema_from_fields(
        &ci(&tbl_name.schema),
        &tbl,
        &tbl_alias,
        &sel_stmt.fields.fields,
    ) else {
        return Ok(None);
    };
    let tbl_name_schema = ci(&tbl_name.schema);
    let tbl_name_partition_names: Vec<CIStr> = tbl_name.partition_names.iter().map(ci).collect();
    let mut db_name = tbl_name_schema.l.clone();
    if db_name.is_empty() {
        db_name = ctx.vars.current_db.clone();
    }

    let pairs: Vec<NameValuePair> = Vec::with_capacity(4);
    let (pairs, is_table_dual) =
        get_name_value_pairs(ctx, &tbl, &tbl_alias, pairs, sel_stmt.where_.as_ref())?;
    let Some(pairs) = (if pairs.is_none() && !is_table_dual {
        None
    } else {
        Some(pairs.unwrap_or_default())
    }) else {
        return Ok(None);
    };
    // The deferred function of Go: colsInWhereClause and unfoldFromWildCard of the result.
    let finish = |mut p: PointGetPlan| {
        p.cols_in_where_clause = pairs_to_column_names(&pairs);
        p.unfold_from_wild_card = unfold.clone();
        Some(p)
    };

    let (handle_pair, field_type) = find_pk_handle(&tbl, &pairs);
    if !handle_pair.value.is_null()
        && pairs.len() == 1
        && index_is_available_by_hints(
            &ctx.vars.current_db,
            &db_name,
            &tbl_alias.l,
            None,
            &sel_stmt.table_hints,
            &tbl_name.index_hints,
        )?
    {
        if is_table_dual {
            let mut p = new_point_get_plan(ctx, &tbl_name_schema.o, schema, &tbl, names);
            p.is_table_dual = true;
            return Ok(finish(p));
        }

        let mut p = new_point_get_plan(ctx, &db_name, schema, &tbl, names);
        p.handle = Some(Handle::Int(IntHandle(handle_pair.value.get_int64())));
        let field_type = field_type.expect("the handle pair has a field type");
        p.unsigned_handle = has_unsigned_flag(field_type.get_flag());
        p.handle_field_type = Some(field_type);
        p.handle_constant = handle_pair.con.clone();
        p.handle_col_offset = pk_col_offset;
        p.partition_names = tbl_name_partition_names.clone();
        return Ok(finish(p));
    } else if !handle_pair.value.is_null() {
        return Ok(None);
    }

    let p = check_tbl_index_for_point_plan(
        ctx,
        &tn_w,
        &tbl_name_schema,
        &tbl_name_partition_names,
        &tbl_name.index_hints,
        schema,
        &tbl_alias.l,
        &sel_stmt.table_hints,
        names,
        &pairs,
        is_table_dual,
        check,
    )?;
    Ok(p.and_then(finish))
}

// go: pairsToColumnNames L1
fn pairs_to_column_names(pairs: &[NameValuePair]) -> Vec<String> {
    pairs.iter().map(|pair| pair.col_name.clone()).collect()
}

/// `tblName *resolve.TableNameW` of Go is the resolved name plus the AST fields Go reads
/// through the embedded `*ast.TableName` (`Schema`, `PartitionNames`, `IndexHints`).
// go: checkTblIndexForPointPlan L2 (the latest-schema check of FOR UPDATE / read committed needs the domain: blocked-by:D06.5)
#[allow(clippy::too_many_arguments)]
fn check_tbl_index_for_point_plan<T>(
    ctx: &mut PlanCtx<'_>,
    tbl_name: &TableNameW,
    tbl_name_schema: &CIStr,
    tbl_name_partition_names: &[CIStr],
    tbl_name_index_hints: &[IndexHint],
    schema: Schema,
    tbl_alias: &str,
    tbl_hints: &[T],
    names: NameSlice,
    pairs: &[NameValuePair],
    is_table_dual: bool,
    check: bool,
) -> Result<Option<PointGetPlan>, Error> {
    let mut check = check || ctx.vars.is_isolation(ReadCommitted);
    check = check && ctx.vars.connection_id > 0;

    let tbl = &tbl_name.table_info;
    let mut db_name = tbl_name_schema.l.clone();
    if db_name.is_empty() {
        db_name = ctx.vars.current_db.clone();
    }
    for idx_info in sl(&tbl.indices) {
        if !idx_info.unique
            || idx_info.state != StatePublic
            || (idx_info.invisible && !ctx.vars.optimizer_use_invisible_indexes)
            || idx_info.mv_index
            || idx_info.has_condition()
            || !index_is_available_by_hints(
                &ctx.vars.current_db,
                &db_name,
                tbl_alias,
                Some(idx_info),
                tbl_hints,
                tbl_name_index_hints,
            )?
        {
            continue;
        }
        if idx_info.global {
            let pi = tbl.get_partition_info();
            // Go reads the partition info of a table with a global index without a nil check.
            let pi = pi.expect("invalid memory address or nil pointer dereference");
            if !sl(&pi.adding_definitions).is_empty()
                || !sl(&pi.new_partition_ids).is_empty()
                || !sl(&pi.dropping_definitions).is_empty()
            {
                continue;
            }
        }
        if is_table_dual {
            if check {
                // domainmisc.GetLatestIndexInfo: the index in the latest schema.
                // glue: blocked-by:D06.5 (domainmisc.GetLatestIndexInfo)
                return Err(blocked(
                    "D06.5",
                    "GetLatestIndexInfo (the latest schema of a point get in FOR UPDATE / READ COMMITTED)",
                ));
            }
            let mut p = new_point_get_plan(ctx, &tbl_name_schema.o, schema, tbl, names);
            p.is_table_dual = true;
            return Ok(Some(p));
        }
        let Some((idx_values, idx_constant, cols_field_type)) = get_index_values(idx_info, pairs)
        else {
            continue;
        };
        if check {
            // glue: blocked-by:D06.5 (domainmisc.GetLatestIndexInfo)
            return Err(blocked(
                "D06.5",
                "GetLatestIndexInfo (the latest schema of a point get in FOR UPDATE / READ COMMITTED)",
            ));
        }
        let mut p = new_point_get_plan(ctx, &db_name, schema, tbl, names);
        p.index_info = Some(Arc::clone(idx_info));
        p.index_values = idx_values;
        p.index_constants = idx_constant;
        p.cols_field_type = cols_field_type;
        p.partition_names = tbl_name_partition_names.to_vec();
        return Ok(Some(p));
    }
    Ok(None)
}

/// `hint.HintUseIndex` & co.
const HintUseIndex: &str = "use_index";
const HintIgnoreIndex: &str = "ignore_index";
const HintForceIndex: &str = "force_index";

/// indexIsAvailableByHints checks whether this index is filtered by these specified index hints.
/// idxInfo is PK if it's nil
///
/// The comment-style hints (`/*+ USE_INDEX(...) */`, `*ast.TableOptimizerHint`) are not in
/// the Rust AST yet (the parser keeps them as an unported node, D02.2): a statement with
/// any is a boundary stub. An index hint the AST does not model (`unported`) is too.
// go: indexIsAvailableByHints L2 (comment-style hints are not in the Rust AST: blocked-by:D02.2)
fn index_is_available_by_hints<T>(
    current_db: &str,
    db_name: &str,
    tbl_alias: &str,
    idx_info: Option<&Arc<IndexInfo>>,
    tbl_hints: &[T],
    idx_hints: &[IndexHint],
) -> Result<bool, Error> {
    let combined_hints = idx_hints;

    // Handle *ast.TableOptimizerHint, which is from the comment style hint in the SQL.
    // In normal planner code path, it's processed by ParsePlanHints() + getPossibleAccessPaths().
    // ParsePlanHints() converts the use/force/ignore index hints in []*ast.TableOptimizerHint into *ast.IndexHint, and
    // put it into *hint.PlanHints.
    // getPossibleAccessPaths() receives the *hint.PlanHints, matches the table names, and combines the hints with the
    // USE/IGNORE/FORCE INDEX () syntax hint.
    // Here we try to remove the logic that is not needed in the fast path, and implement a faster and simpler version
    // to fit the fast path.
    if !tbl_hints.is_empty() {
        // for _, h := range tblHints: h.HintName, h.Tables, h.Indexes of USE_INDEX /
        // IGNORE_INDEX / FORCE_INDEX (the parser's TableOptimizerHint is unported).
        let _ = (
            current_db,
            db_name,
            tbl_alias,
            HintUseIndex,
            HintIgnoreIndex,
            HintForceIndex,
        );
        return Err(blocked(
            "D02.2",
            "optimizer hints in comments (ast.TableOptimizerHint)",
        ));
    }
    if combined_hints.iter().any(|h| h.unported.is_some()) {
        return Err(blocked("D02.2", "an index hint the AST does not model"));
    }

    if combined_hints.is_empty() {
        return Ok(true);
    }
    let matches = |name: &ast::CIStr| -> bool {
        match idx_info {
            None => name.l() == "primary",
            Some(idx_info) => idx_info.name.l == name.l(),
        }
    };
    // NOTICE: it's supposed that ignore hints and use/force hints will not be applied together since the effect of
    // the former will be eliminated by the latter.
    let mut is_ignore = false;
    for hint in combined_hints {
        if hint.hint_scope != IndexHintScope::ForScan {
            continue;
        }
        // `hint.IndexNames != nil`: the parser's list is nil for `USE INDEX ()`.
        let has_names = !hint.index_names.is_empty();
        if hint.hint_type == IndexHintType::Ignore && has_names {
            is_ignore = true;
            for name in &hint.index_names {
                if matches(name) {
                    return Ok(false);
                }
            }
        }
        if (hint.hint_type == IndexHintType::Force || hint.hint_type == IndexHintType::Use)
            && has_names
        {
            for name in &hint.index_names {
                if matches(name) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(is_ignore)
}

/// A `BasePhysicalPlan` holding Go's embedded `baseimpl.Plan` (see the module doc).
// glue: M-SELF
fn base_with_plan(plan: crate::baseimpl::Plan) -> BasePhysicalPlan {
    let mut base = BasePhysicalPlan::default();
    base.plan = plan;
    base
}

// go: newPointGetPlan L1
fn new_point_get_plan(
    ctx: &mut PlanCtx<'_>,
    db_name: &str,
    schema: Schema,
    tbl: &Arc<TableInfo>,
    names: NameSlice,
) -> PointGetPlan {
    let mut p = PointGetPlan {
        base: base_with_plan(new_base_plan(ctx, TypePointGet, 0)),
        db_name: db_name.to_owned(),
        schema,
        tbl_info: Arc::clone(tbl),
        output_names: names,
        lock_wait_time: ctx.vars.lock_wait_timeout,
        ..Default::default()
    };
    p.base.plan.set_stats(Some(Arc::new(StatsInfo {
        row_count: 1.0,
        ..Default::default()
    })));
    ctx.vars.stmt_ctx_mut().tables = vec![TableEntry {
        db: db_name.to_owned(),
        table: tbl.name.l.clone(),
    }];
    p
}

/// checkFastPlanPrivilege checks two types of privilege:
/// 1. user meets all visitInfos requirement
/// 2. all tables can pass the check in CheckTableLock
///
/// tidb-rs has no privilege manager until D04.1 (VS1 plan §3.2, the PoC's single-user
/// model): Go's `pm == nil` branch, every visit info passes the first check.
// go: checkFastPlanPrivilege L1 [M-CTX]
fn check_fast_plan_privilege(visit_infos: &[VisitInfo]) -> Result<(), Error> {
    // pm := privilege.GetPrivilegeManager(ctx): nil.
    let mut lock_visit_infos = Vec::with_capacity(visit_infos.len());
    for vs in visit_infos {
        // This visitInfo is only for table lock check, so we do not need to set column field.
        lock_visit_infos.push(VisitInfo {
            privilege: vs.privilege,
            db: vs.db.clone(),
            table: vs.table.clone(),
            ..Default::default()
        });
    }
    check_table_lock(&lock_visit_infos)
}

/// The schema, output names and `unfold` flags of the select fields (Go's nil schema is
/// `None`).
// go: buildSchemaFromFields L1
pub fn build_schema_from_fields(
    db_name: &CIStr,
    tbl: &TableInfo,
    tbl_name: &CIStr,
    fields: &[SelectField],
) -> Option<(Schema, NameSlice, Vec<bool>)> {
    let n = sl(&tbl.columns).len() + 1;
    let mut columns: Vec<Arc<Column>> = Vec::with_capacity(n);
    let mut names: NameSlice = Vec::with_capacity(n);
    let mut unfold: Vec<bool> = Vec::with_capacity(n);
    if !fields.is_empty() {
        for field in fields {
            if let Some(wild_card) = &field.wild_card {
                if !wild_card.table.o.is_empty() && wild_card.table.l() != tbl_name.l {
                    return None;
                }
                for col in sl(&tbl.columns) {
                    names.push(Arc::new(FieldName {
                        db_name: db_name.clone(),
                        orig_tbl_name: tbl.name.clone(),
                        tbl_name: tbl_name.clone(),
                        orig_col_name: col.name.clone(),
                        col_name: col.name.clone(),
                        ..Default::default()
                    }));
                    unfold.push(true);
                    columns.push(Arc::new(col_info_to_column(col, columns.len())));
                }
                continue;
            }
            if let Some((name, column)) = try_extract_row_checksum_column(field, columns.len()) {
                names.push(Arc::new(name));
                unfold.push(false);
                columns.push(Arc::new(column));
                continue;
            }
            let Some(Expr::Column(col_name_expr)) = &field.expr else {
                return None;
            };
            if !col_name_expr.table.o.is_empty() && col_name_expr.table.l() != tbl_name.l {
                return None;
            }
            let col = find_col(tbl, col_name_expr)?;
            let mut as_name = ci(&col_name_expr.name);
            if !field.as_name.o.is_empty() {
                as_name = ci(&field.as_name);
            }
            names.push(Arc::new(FieldName {
                db_name: db_name.clone(),
                orig_tbl_name: tbl.name.clone(),
                tbl_name: tbl_name.clone(),
                orig_col_name: col.name.clone(),
                col_name: as_name,
                ..Default::default()
            }));
            unfold.push(false);
            columns.push(Arc::new(col_info_to_column(&col, columns.len())));
        }
        return Some((new_schema(columns), names, unfold));
    }
    // fields len is 0 for update and delete.
    for col in sl(&tbl.columns) {
        names.push(Arc::new(FieldName {
            db_name: db_name.clone(),
            orig_tbl_name: tbl.name.clone(),
            tbl_name: tbl_name.clone(),
            orig_col_name: col.name.clone(),
            col_name: col.name.clone(),
            ..Default::default()
        }));
        unfold.push(false);
        let column = col_info_to_column(col, columns.len());
        columns.push(Arc::new(column));
    }
    Some((new_schema(columns), names, unfold))
}

// go: tryExtractRowChecksumColumn L1
fn try_extract_row_checksum_column(field: &SelectField, idx: usize) -> Option<(FieldName, Column)> {
    let Some(Expr::FuncCall(f)) = &field.expr else {
        return None;
    };
    if f.fn_name.l() != ast::funcs::TiDBRowChecksum || !f.args.is_empty() {
        return None;
    }
    let mut orig_name = ci(&f.fn_name);
    orig_name.l.push_str("()");
    orig_name.o.push_str("()");
    let mut as_name = orig_name.clone();
    if !field.as_name.o.is_empty() {
        as_name = ci(&field.as_name);
    }
    let (cs, cl) = default_charset_for_type(TypeString);
    let mut ftype = new_field_type(TypeString);
    ftype.set_charset(cs);
    ftype.set_collate(cl);
    ftype.set_flen(mysql::MaxBlobWidth);
    ftype.set_decimal(0);
    let name = FieldName {
        orig_col_name: orig_name.clone(),
        col_name: as_name,
        ..Default::default()
    };
    let column = Column {
        ret_type: Arc::new(ftype),
        id: ExtraRowChecksumID as i64,
        unique_id: ExtraRowChecksumID as i64,
        index: idx as isize,
        orig_name: orig_name.l,
        ..Default::default()
    };
    Some((name, column))
}

/// getSingleTableNameAndAlias return the ast node of queried table name and the alias string.
/// `tblName` is `nil` if there are multiple tables in the query.
/// `tblAlias` will be the real table name if there is no table alias in the query.
// go: getSingleTableNameAndAlias L1
pub fn get_single_table_name_and_alias(
    table_refs: Option<&TableRefsClause>,
) -> (Option<&ast::TableName>, CIStr) {
    let tbl_alias = CIStr::default();
    let Some(table_refs) = table_refs else {
        return (None, tbl_alias);
    };
    if table_refs.table_refs.right.is_some() {
        return (None, tbl_alias);
    }
    let Some(ResultSetNode::TableSource(tbl_src)) = &table_refs.table_refs.left else {
        return (None, tbl_alias);
    };
    let ResultSetNode::TableName(tbl_name) = &tbl_src.source else {
        return (None, tbl_alias);
    };
    let mut tbl_alias = ci(&tbl_src.as_name);
    if tbl_src.as_name.o.is_empty() {
        tbl_alias = ci(&tbl_name.name);
    }
    (Some(tbl_name), tbl_alias)
}

/// The `Datum` of a `driver.ValueExpr`: the Rust AST keeps the literal (`ast::Datum`), Go's
/// node holds the datum `SetValue` made of it (strings take the expression's collation,
/// hexadecimal and bit literals are binary literals, `TRUE` / `FALSE` are int64).
// glue: driver.ValueExpr.Datum (the Rust AST keeps the literal)
pub fn value_expr_datum(v: &ast::ValueExpr) -> Datum {
    let mut d = Datum::default();
    match &v.datum {
        ast::Datum::Null => {}
        ast::Datum::Bool(b) => d.set_int64(i64::from(*b)),
        ast::Datum::Int(i) => d.set_int64(*i),
        ast::Datum::Uint(u) => d.set_uint64(*u),
        ast::Datum::Float(f) => d.set_float64(*f),
        ast::Datum::Decimal(s) => {
            let mut dec = MyDecimal::default();
            let _ = dec.from_string(s.as_bytes());
            d.set_mysql_decimal(dec);
        }
        ast::Datum::Str(s) => d.set_string(s.as_bytes().to_vec(), v.collate),
        ast::Datum::Hex(b) | ast::Datum::Bit(b) => d.set_binary_literal(BinaryLiteral(b.clone())),
    }
    d
}

/// getNameValuePairs extracts `column = constant/paramMarker` conditions from expr as name value pairs.
///
/// Go's nil pairs are `None`. A parameter marker reads the value `EXECUTE` bound to it,
/// which the Rust AST does not keep (D13.1): a boundary stub.
// go: getNameValuePairs L2 (a parameter marker is blocked-by:D13.1)
pub fn get_name_value_pairs(
    ctx: &PlanCtx<'_>,
    tbl: &TableInfo,
    tbl_name: &CIStr,
    mut nv_pairs: Vec<NameValuePair>,
    expr: Option<&Expr>,
) -> Result<(Option<Vec<NameValuePair>>, bool), Error> {
    let eval_ctx = ctx.eval_ctx();
    let Some(Expr::Binary(bin_op)) = expr else {
        return Ok((None, false));
    };
    let bin_op: &BinaryOperationExpr = bin_op;
    match bin_op.op {
        Op::LogicAnd => {
            let (pairs, is_table_dual) =
                get_name_value_pairs(ctx, tbl, tbl_name, nv_pairs, Some(&bin_op.l))?;
            let Some(pairs) = pairs.filter(|_| !is_table_dual) else {
                return Ok((None, is_table_dual));
            };
            let (pairs, is_table_dual) =
                get_name_value_pairs(ctx, tbl, tbl_name, pairs, Some(&bin_op.r))?;
            let Some(pairs) = pairs.filter(|_| !is_table_dual) else {
                return Ok((None, is_table_dual));
            };
            Ok((Some(pairs), is_table_dual))
        }
        Op::EQ => {
            // Views' columns have no FieldType.
            if tbl.is_view() {
                return Ok((None, false));
            }
            let mut d: Datum;
            let con: Option<Arc<Constant>> = None;
            let col_name: &ColumnName;
            let constant_of = |e: &Expr| -> Result<Datum, Error> {
                match e {
                    Expr::Value(x) => Ok(value_expr_datum(x)),
                    Expr::ParamMarker(_) => {
                        // con, err = expression.ParamMarkerExpression(ctx, x, false); d = con.Eval(...)
                        // glue: blocked-by:D13.1 (the value of a parameter marker)
                        Err(blocked(
                            "D13.1",
                            "a parameter marker in a point get condition (the value bound by EXECUTE)",
                        ))
                    }
                    _ => Ok(Datum::default()),
                }
            };
            if let Expr::Column(c) = &bin_op.l {
                col_name = c;
                d = constant_of(&bin_op.r)?;
            } else if let Expr::Column(c) = &bin_op.r {
                col_name = c;
                d = constant_of(&bin_op.l)?;
            } else {
                return Ok((None, false));
            }
            if d.is_null() {
                return Ok((None, false));
            }
            if !col_name.table.o.is_empty() && col_name.table.l() != tbl_name.l {
                return Ok((None, false));
            }
            // tbl.Cols(): every column is public here (tryPointGetPlan returned otherwise).
            let Some(col) = find_column_info(sl(&tbl.columns), &col_name.name.l()) else {
                // Partition table can't use `_tidb_rowid` to generate PointGet Plan.
                if tbl.get_partition_info().is_some() && col_name.name.l() == ExtraHandleName.l {
                    return Ok((None, false));
                }
                nv_pairs.push(NameValuePair {
                    col_name: col_name.name.l(),
                    col_field_type: Some(Arc::new(new_field_type(TypeLonglong))),
                    value: d,
                    con,
                });
                return Ok((Some(nv_pairs), false));
            };

            // As in buildFromBinOp in util/ranger, when we build key from the expression to do range scan or point get on
            // a string column, we should set the collation of the string datum to collation of the column.
            if col.field_type.eval_type() == ETString
                && (d.kind() == KindString || d.kind() == KindBinaryLiteral)
            {
                let s = d.get_string().to_vec();
                d.set_string(s, col.field_type.get_collate());
            }

            if !check_can_convert_in_point_get(col, &d) {
                return Ok((None, false));
            }
            let col_ft = Arc::new(col.field_type.clone());
            if col.get_type() == TypeString && col.get_collate() == CollationBin {
                // This type we needn't to pad `\0` in here.
                nv_pairs.push(NameValuePair {
                    col_name: col_name.name.l(),
                    col_field_type: Some(col_ft),
                    value: d,
                    con,
                });
                return Ok((Some(nv_pairs), false));
            }
            let tc = eval_ctx.type_ctx();
            let (d_val, err) = d.convert_to(&tc, &col.field_type);
            if let Err(err) = err {
                if terr::ErrOverflow.equal(&err) {
                    nv_pairs.push(NameValuePair {
                        col_name: col_name.name.l(),
                        col_field_type: Some(col_ft),
                        value: d,
                        con,
                    });
                    return Ok((Some(nv_pairs), true));
                }
                // Some scenarios cast to int with error, but we may use this value in point get.
                if !terr::ErrTruncatedWrongVal.equal(&err) {
                    return Ok((None, false));
                }
            }
            // The converted result must be same as original datum.
            let (cmp, err) = d_val.compare(&tc, &d, collate::get_collator(col.get_collate()));
            if err.is_err() || cmp != 0 {
                return Ok((None, false));
            }
            nv_pairs.push(NameValuePair {
                col_name: col_name.name.l(),
                col_field_type: Some(col_ft),
                value: d_val,
                con,
            });
            Ok((Some(nv_pairs), false))
        }
        _ => Ok((None, false)),
    }
}

/// The value of `col = d` for a batch point get (Go's nil is `None`).
// go: getPointGetValue L1
pub fn get_point_get_value(
    stmt_ctx: &StatementContext,
    col: &ColumnInfo,
    d: &mut Datum,
) -> Option<Datum> {
    if !check_can_convert_in_point_get(col, d) {
        return None;
    }
    // As in buildFromBinOp in util/ranger, when we build key from the expression to do range scan or point get on
    // a string column, we should set the collation of the string datum to collation of the column.
    if col.field_type.eval_type() == ETString
        && (d.kind() == KindString || d.kind() == KindBinaryLiteral)
    {
        let s = d.get_string().to_vec();
        d.set_string(s, col.field_type.get_collate());
    }
    let tc = stmt_ctx.type_ctx();
    let (d_val, err) = d.convert_to(&tc, &col.field_type);
    if err.is_err() {
        return None;
    }
    // The converted result must be same as original datum.
    let (cmp, err) = d_val.compare(&tc, d, collate::get_collator(col.get_collate()));
    if err.is_err() || cmp != 0 {
        return None;
    }
    Some(d_val)
}

// go: checkCanConvertInPointGet L1
fn check_can_convert_in_point_get(col: &ColumnInfo, d: &Datum) -> bool {
    let kind = d.kind();
    if col.field_type.eval_type() == ETString {
        match kind {
            KindInt64 | KindUint64 | KindFloat32 | KindFloat64 | KindMysqlDecimal => {
                // column type is String and constant type is numeric
                return false;
            }
            _ => {}
        }
    }
    if col.field_type.get_type() == TypeBit && kind == KindString {
        // column type is Bit and constant type is string
        return false;
    }
    true
}

/// The handle pair and its field type (Go's zero pair and nil type when there is none).
// go: findPKHandle L1
fn find_pk_handle(
    tbl_info: &TableInfo,
    pairs: &[NameValuePair],
) -> (NameValuePair, Option<Arc<FieldType>>) {
    if !tbl_info.pk_is_handle {
        let row_id_idx = find_in_pairs("_tidb_rowid", pairs);
        if row_id_idx != -1 {
            return (
                pairs[row_id_idx as usize].clone(),
                Some(Arc::new(new_field_type(TypeLonglong))),
            );
        }
        return (NameValuePair::default(), None);
    }
    for col in sl(&tbl_info.columns) {
        if has_pri_key_flag(col.get_flag()) {
            let i = find_in_pairs(&col.name.l, pairs);
            if i == -1 {
                return (NameValuePair::default(), None);
            }
            return (
                pairs[i as usize].clone(),
                Some(Arc::new(col.field_type.clone())),
            );
        }
    }
    (NameValuePair::default(), None)
}

/// The index values, their constants and field types (Go's nil slices are `None`).
#[allow(clippy::type_complexity)]
// go: getIndexValues L1
fn get_index_values(
    idx_info: &IndexInfo,
    pairs: &[NameValuePair],
) -> Option<(Vec<Datum>, Vec<Option<Arc<Constant>>>, Vec<Arc<FieldType>>)> {
    let mut idx_values = Vec::with_capacity(4);
    let mut idx_constants = Vec::with_capacity(4);
    let mut cols_field_type = Vec::with_capacity(4);
    let idx_columns = sl(&idx_info.columns);
    if idx_columns.len() != pairs.len() {
        return None;
    }
    if idx_info.has_prefix_index() {
        return None;
    }
    for idx_col in idx_columns {
        let i = find_in_pairs(&idx_col.name.l, pairs);
        if i == -1 {
            return None;
        }
        let pair = &pairs[i as usize];
        idx_values.push(pair.value.clone());
        idx_constants.push(pair.con.clone());
        cols_field_type.push(Arc::clone(
            pair.col_field_type
                .as_ref()
                .expect("a pair has a field type"),
        ));
    }
    if !idx_values.is_empty() {
        return Some((idx_values, idx_constants, cols_field_type));
    }
    None
}

// go: findInPairs L1
fn find_in_pairs(col_name: &str, pairs: &[NameValuePair]) -> isize {
    for (i, pair) in pairs.iter().enumerate() {
        if pair.col_name == col_name {
            return i as isize;
        }
    }
    -1
}

/// `subQueryChecker` / `isExprHasSubQuery`: whether an expression of a point UPDATE / DELETE
/// has a subquery. Only the write path calls them (`tryUpdatePointPlan`,
/// `checkIfAssignmentListHasSubQuery`); the Rust AST has no visitor yet.
// glue: stub of subQueryChecker.Enter, blocked-by:X (point UPDATE / DELETE)
// glue: stub of subQueryChecker.Leave, blocked-by:X (point UPDATE / DELETE)
// glue: stub of isExprHasSubQuery, blocked-by:X (point UPDATE / DELETE)
pub fn is_expr_has_sub_query(expr: &Expr) -> Result<bool, Error> {
    let _ = expr;
    Err(blocked(
        "X",
        "the subquery check of a point UPDATE / DELETE",
    ))
}

// go: findCol L1
fn find_col(tbl: &TableInfo, col_name: &ColumnName) -> Option<Arc<ColumnInfo>> {
    if col_name.name.l() == ExtraHandleName.l && !tbl.pk_is_handle {
        let mut col_info = new_extra_handle_col_info();
        col_info.offset = sl(&tbl.columns).len() as isize - 1;
        return Some(Arc::new(col_info));
    }
    for col in sl(&tbl.columns) {
        if col.name.l == col_name.name.l() {
            return Some(Arc::clone(col));
        }
    }
    None
}

/// `colInfoToColumn`: `UniqueID` is the column's offset (the fast plan allocates no column
/// IDs; `TryFastPlan` reset `PlanColumnID`), `RetType` a copy of the column's type.
// go: colInfoToColumn L1
pub fn col_info_to_column(col: &ColumnInfo, idx: usize) -> Column {
    Column {
        ret_type: Arc::new(col.field_type.clone()),
        id: col.id,
        unique_id: col.offset as i64,
        index: idx as isize,
        orig_name: col.name.l.clone(),
        ..Default::default()
    }
}

/// The handle columns of a point UPDATE / DELETE (`fields len is 0`): the int handle, the
/// clustered index, or `_tidb_rowid` appended to the schema and the output names.
// go: buildHandleCols L1
pub fn build_handle_cols(
    db_name: &str,
    tbl: &Arc<TableInfo>,
    pointget: &mut PointGetPlan,
) -> HandleCols {
    // fields len is 0 for update and delete.
    if tbl.pk_is_handle {
        for (i, col) in sl(&tbl.columns).iter().enumerate() {
            if has_pri_key_flag(col.get_flag()) {
                return new_int_handle_cols(Arc::clone(&pointget.schema.columns[i]));
            }
        }
    }

    if tbl.is_common_handle {
        let pk_idx = tidb_infoschema::tables::find_primary_index(tbl)
            .cloned()
            .expect("a clustered table has a primary index");
        return HandleCols::Common(new_common_handle_cols(
            Arc::clone(tbl),
            pk_idx,
            &pointget.schema.columns,
        ));
    }

    let handle_col = Arc::new(col_info_to_column(
        &new_extra_handle_col_info(),
        pointget.schema.len(),
    ));
    pointget.schema.append([Arc::clone(&handle_col)]);
    let mut new_output_names = tidb_types::field_name::shallow(&pointget.output_names);
    let mut table_alias_name = tbl.name.clone();
    if !pointget.schema.is_empty() {
        table_alias_name = pointget.output_names[0].tbl_name.clone();
    }
    new_output_names.push(Arc::new(FieldName {
        db_name: CIStr::new(db_name),
        tbl_name: table_alias_name,
        orig_tbl_name: tbl.name.clone(),
        col_name: ExtraHandleName.clone(),
        ..Default::default()
    }));
    pointget.output_names = new_output_names;
    new_int_handle_cols(handle_col)
}

/// The partition column of a hash / key partitioned table for the batch point get. Reading
/// the partition expression needs the partitioned table (D06.4 part 2).
// glue: stub of getHashOrKeyPartitionColumnName, blocked-by:D06.4 (the partition expression of a partitioned table)
pub fn get_hash_or_key_partition_column_name(tbl: &TableInfo) -> Result<Option<CIStr>, Error> {
    if tbl.get_partition_info().is_none() {
        return Ok(None);
    }
    Err(blocked(
        "D06.4",
        "the partition column of a hash / key partitioned table",
    ))
}

#[cfg(test)]
mod tests;
