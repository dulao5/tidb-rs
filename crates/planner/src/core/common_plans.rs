//! go-file: pkg/planner/core/common_plans.go
//!
//! So far the two point-get helpers of VS1-19 (the point get arm with VS1-11) and the
//! `Execute` plan of a prepared statement's execution (D13.1); the other common plans (SET,
//! SIMPLE, EXPLAIN, ...) come with their blocks.

use std::sync::Arc;

use tidb_base::Error;
use tidb_expression::{Schema, new_schema};
use tidb_infoschema::tables::find_primary_index;
use tidb_model::TableCacheStatusDisable;
use tidb_parser::ast::{self, KillStmt};
use tidb_sessionctx::variable::session::SessionVars;

use tidb_types::datum::Datum;
use tidb_types::field_name::NameSlice;

use crate::PhysicalPlanRef;
use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::physical_plans::{
    PhysicalIndexReader, PhysicalIndexScan, PhysicalProjection, PhysicalTableReader,
    PhysicalTableScan,
};
use crate::core::plan_cache_utils::PlanCacheStmtRef;
use crate::core::point_get_plan::PointGetPlan;
use crate::core::resolve::ContextRef;
use crate::physicalop::{BasePhysicalPlan, new_base_physical_plan};
use crate::planctx::PlanCtx;

/// Lowered fields of `ast.KillStmt` carried by [`Simple`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimpleKillStmt {
    pub query: bool,
    pub connection_id: u64,
    pub tidb_extension: bool,
    /// `Some(lower_name)` when a zero-arg function expression (`CONNECTION_ID()`, etc.) was supplied.
    pub expr_func_name: Option<String>,
    /// True when the `KILL` expression was not a zero-arg function call.
    pub expr_invalid: bool,
}

/// Statements handled directly by `SimpleExec`.
#[derive(Clone, Debug)]
pub enum SimpleStmt {
    Kill(SimpleKillStmt),
}

/// `plannercore.Simple`: non-relational utility statement plan (e.g. `KILL`).
// go: plannercore.Simple L2 (covers KILL; USE/BEGIN/COMMIT/ROLLBACK/SET remain on their paths)
#[derive(Clone)]
pub struct Simple {
    pub base: BasePhysicalPlan,
    pub schema: Schema,
    pub output_names: NameSlice,
    pub stmt: SimpleStmt,
    pub stale_txn_start_ts: u64,
    pub is_from_remote: bool,
    pub is_refreshed: bool,
    pub resolve_ctx: Option<ContextRef>,
}

impl Simple {
    /// Constructs a [`Simple`] plan from an `ast::KillStmt`.
    // go: PlanBuilder.buildSimple / Simple L2
    pub fn from_kill(ctx: &PlanCtx<'_>, raw: &KillStmt) -> Simple {
        let (expr_func_name, expr_invalid) = match &raw.expr {
            None => (None, false),
            Some(ast::Expr::FuncCall(fc)) if fc.args.is_empty() && fc.schema.o.is_empty() => {
                (Some(fc.fn_name.l()), false)
            }
            Some(_) => (None, true),
        };
        Simple {
            base: new_base_physical_plan(ctx, "Simple", 0),
            schema: new_schema(Vec::new()),
            output_names: NameSlice::new(),
            stmt: SimpleStmt::Kill(SimpleKillStmt {
                query: raw.query,
                connection_id: raw.connection_id,
                tidb_extension: raw.tidb_extension,
                expr_func_name,
                expr_invalid,
            }),
            stale_txn_start_ts: 0,
            is_from_remote: false,
            is_refreshed: false,
            resolve_ctx: None,
        }
    }
}

impl PhysicalPlan for Simple {
    crate::physical_plan_boilerplate!(Simple);

    // go: baseimpl.Plan.Schema L1
    fn schema(&self) -> &Schema {
        &self.schema
    }

    // glue: accessor for schema mutation
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }

    // go: baseimpl.Plan.OutputNames L1
    fn output_names(&self) -> NameSlice {
        self.output_names.clone()
    }

    // go: baseimpl.Plan.SetOutputNames L1
    fn set_output_names(&mut self, names: NameSlice) {
        self.output_names = names;
    }

    // go: BasePhysicalPlan.Clone L1
    fn clone_plan(&self) -> Result<PhysicalPlanRef, Error> {
        Ok(Arc::new(self.clone()))
    }
}

/// Execute represents prepare plan.
///
/// - `Params`: Go's `[]expression.Expression` of the arguments; the new stack's arguments are
///   the binary protocol's constants (the text protocol's `USING @var` needs the expression
///   rewriter, D09.2), so their values are kept, as `SetParameterValuesIntoSCtx` reads them.
/// - `Stmt` (Go: the prepared statement's AST, `PrepStmt.PreparedAst.Stmt`) is reached
///   through `prep_stmt`: the AST is owned by the `PlanCacheStmt`.
/// - `baseSchemaProducer`: only the output names are kept (the schema comes with the
///   physical plan, `plan`).
pub struct Execute {
    pub name: String,
    pub params: Vec<Datum>,
    pub prep_stmt: PlanCacheStmtRef,
    pub plan: Option<PhysicalPlanRef>,
    pub output_names: NameSlice,
}

/// IsPointGetWithPKOrUniqueKeyByAutoCommit returns true when meets following conditions:
///  1. ctx is auto commit tagged
///  2. session is not InTxn
///  3. plan is point get by pk, or point get by unique index (no double read)
///
/// Keeps the `Result` return type for callers until all plan-level boundaries are retired.
// go: IsPointGetWithPKOrUniqueKeyByAutoCommit L1
pub fn is_point_get_with_pk_or_unique_key_by_auto_commit(
    vars: &SessionVars,
    mut p: &dyn PhysicalPlan,
) -> Result<bool, Error> {
    if !is_auto_commit_txn(vars) {
        return Ok(false);
    }

    // check plan
    if p.kind() == PhysicalKind::Projection
        && let Some(proj) = p.as_any().downcast_ref::<PhysicalProjection>()
    {
        p = proj.children()[0].as_ref();
    }

    match p.kind() {
        PhysicalKind::IndexReader => {
            let Some(v) = p.as_any().downcast_ref::<PhysicalIndexReader>() else {
                return Ok(false);
            };
            let index_scan = v.index_plans[0]
                .as_any()
                .downcast_ref::<PhysicalIndexScan>()
                .expect("interface conversion: base.PhysicalPlan is not *core.PhysicalIndexScan");
            Ok(index_scan.is_point_get_by_unique_key(&vars.stmt_ctx().type_ctx()))
        }
        PhysicalKind::TableReader => {
            let Some(v) = p.as_any().downcast_ref::<PhysicalTableReader>() else {
                return Ok(false);
            };
            let Some(table_scan) = v.table_plans[0]
                .as_any()
                .downcast_ref::<PhysicalTableScan>()
            else {
                return Ok(false);
            };
            let is_point_range = table_scan.ranges.len() == 1
                && table_scan.ranges[0].is_point_non_nullable(&vars.stmt_ctx().type_ctx());
            if !is_point_range {
                return Ok(false);
            }
            let mut pk_length = 1usize;
            if table_scan.table.is_common_handle {
                let pk_idx = find_primary_index(&table_scan.table)
                    .expect("runtime error: invalid memory address or nil pointer dereference");
                pk_length = pk_idx.columns.as_deref().map_or(0, <[_]>::len);
            }
            Ok(table_scan.ranges[0].low_val.len() == pk_length)
        }
        PhysicalKind::PointGet => {
            let Some(v) = p.as_any().downcast_ref::<PointGetPlan>() else {
                return Ok(false);
            };
            // If the PointGetPlan needs to read data using unique index (double read), we
            // can't use max uint64, because using math.MaxUint64 can't guarantee repeatable-read
            // and the data and index would be inconsistent!
            // If the PointGetPlan needs to read data from Cache Table, we can't use max uint64,
            // because math.MaxUint64 always make cacheData invalid.
            let no_second_read = v
                .index_info
                .as_ref()
                .is_none_or(|i| i.primary && v.tbl_info.is_common_handle);
            if !no_second_read {
                return Ok(false);
            }
            if v.tbl_info.table_cache_status_type != TableCacheStatusDisable {
                return Ok(false);
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// IsAutoCommitTxn checks if session is in autocommit mode and not InTxn
/// used for fast plan like point get
// go: IsAutoCommitTxn L1
pub fn is_auto_commit_txn(vars: &SessionVars) -> bool {
    vars.is_autocommit() && !vars.in_txn()
}
