//! go-file: pkg/planner/core/resolve_indices.go
//!
//! `ResolveIndices` of the operators of the first vertical slice (block D10.9): every column
//! reference of an operator gets the position of that column in its child's schema
//! (`Column.Index`), so the executors evaluate expressions by position. Also
//! `refine4NeighbourProj`, which `InjectExtraProjection` (the optimizer driver, block
//! D09.13) calls too.
//!
//! - Go writes the plan in place; Rust writes it through `make_mut_plan` (`M-PLAN`), which is
//!   in place while the statement owns the tree. The only caller is `physicalOptimize`, right
//!   after the best plan leaves FindBestTask's memo and before the plan can enter a plan
//!   cache (N1 §3 item 9.3, A3 spec §3.1): nothing is copied.
//! - Go's `p.SCtx()` is the `ctx` argument (`M-CTX`).
//! - Go writes `Column.Index` / `Column.VirtualExpr` into column objects that several
//!   schemas share (the table reader's schema is its table plan's schema object). In Rust
//!   every node owns its schema (N1 §3.3, D10.1 spec §5 item 2), so such a write reaches only
//!   the node that makes it. The table reader copies the resolved `Index` values of its table
//!   plan's schema back into its own; spec §5 item 1 lists what the other holders do not see.
//! - The operators' `ResolveIndices` are inherent methods here; the `PhysicalPlan` trait
//!   methods in `core/physical_plans.rs` dispatch to them.

use std::collections::BTreeMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::disjointset::new_int_set;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tidb_expression::{Column, Expr, Schema, new_schema};

use crate::base::{PhysicalKind, PhysicalPlan};
use crate::core::initialize::flatten_list_push_down_plan;
use crate::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
};
use crate::core::util::{BaseSchemaProducer, PhysicalSchemaProducer};
use crate::physicalop::make_mut_plan;
use crate::planctx::PlanCtx;
use crate::util::ByItems;

impl PhysicalProjection {
    /// ResolveIndicesItself resolve indices for PhysicalPlan itself
    // go: PhysicalProjection.ResolveIndicesItself L1 [M-EXPR]
    pub fn resolve_indices_itself(&mut self) -> Result<(), Error> {
        let child = Arc::clone(&self.sp.base.children[0]);
        for i in 0..self.exprs.len() {
            self.exprs[i] = self.exprs[i].resolve_indices(child.schema())?;
        }
        let Some(child_proj) = child.as_any().downcast_ref::<PhysicalProjection>() else {
            return Ok(());
        };
        refine4_neighbour_proj(self, child_proj);
        Ok(())
    }

    /// ResolveIndices implements Plan interface.
    // go: PhysicalProjection.ResolveIndices L1 [M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.sp.resolve_indices(ctx)?;
        self.resolve_indices_itself()
    }
}

/// refine4NeighbourProj refines the index for p.Exprs whose type is *Column when
/// there is two neighbouring Projections.
/// This function is introduced because that different childProj.Expr may refer
/// to the same index of childProj.Schema, so we need to keep this relation
/// between the specified expressions in the parent Projection.
///
/// Go ranges over a map; the groups of different input columns are disjoint, so the order
/// does not matter (a `BTreeMap` here). `p`'s expressions are written through `make_mut`
/// (they are fresh copies when `InjectExtraProjection` and `ResolveIndicesItself` call this).
// go: refine4NeighbourProj L1 [M-EXPR]
pub fn refine4_neighbour_proj(p: &mut PhysicalProjection, child_proj: &PhysicalProjection) {
    let mut input_idx2_output_idxes: BTreeMap<isize, Vec<usize>> = BTreeMap::new();
    for (i, expr) in child_proj.exprs.iter().enumerate() {
        let Expr::Column(col) = &**expr else {
            continue;
        };
        input_idx2_output_idxes
            .entry(col.index)
            .or_default()
            .push(i);
    }
    let mut child_schema_union_set = new_int_set(child_proj.schema().len());
    for output_idxes in input_idx2_output_idxes.values() {
        if output_idxes.len() <= 1 {
            continue;
        }
        for i in 1..output_idxes.len() {
            child_schema_union_set.union(output_idxes[0], output_idxes[i]);
        }
    }

    for expr in p.exprs.iter_mut() {
        if !matches!(&**expr, Expr::Column(_)) {
            continue;
        }
        let Expr::Column(col) = Arc::make_mut(expr) else {
            unreachable!()
        };
        col.index = child_schema_union_set.find_root(col.index as usize) as isize;
    }
}

/// resolveIndicesForVirtualColumn resolves dependent columns's indices for virtual columns.
///
/// Go writes `col.VirtualExpr` into the column objects of `result`; here `result` is the
/// owner's column list and each changed column is written through `Arc::make_mut`. Callers
/// whose `result` and `schema` are one schema pass a shallow copy as `schema` (the lookup
/// reads only unique ids).
// go: resolveIndicesForVirtualColumn L1 [M-EXPR]
pub fn resolve_indices_for_virtual_column(
    result: &mut [Arc<Column>],
    schema: &Schema,
) -> Result<(), Error> {
    for col in result.iter_mut() {
        if let Some(virtual_expr) = &col.virtual_expr {
            let new_expr = virtual_expr.resolve_indices(schema)?;
            Arc::make_mut(col).virtual_expr = Some(new_expr);
        }
    }
    Ok(())
}

impl PhysicalTableReader {
    /// ResolveIndices implements Plan interface.
    ///
    /// The nodes of the table plan are also held by `TablePlans` (Go: the same pointers);
    /// the list is cleared so that the chain is owned alone and resolved in place, and is
    /// flattened again afterwards, as `rewrite_table_plan` does (N1 §3.4). A nil table plan
    /// panics as in Go.
    ///
    /// In Go the reader's schema is the table plan's schema object (`Init`:
    /// `p.schema = p.tablePlan.Schema()`), so resolving the table plan also sets `Index` on
    /// the reader's columns. Here the reader owns a copy, and the resolved `Index` values of
    /// the table plan's schema are copied back into it afterwards, so the reader's schema
    /// reads as Go's (spec §5 item 1).
    // go: PhysicalTableReader.ResolveIndices L1 [M-PLAN, M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        let schema = self.sp.schema().clone();
        resolve_indices_for_virtual_column(&mut self.sp.schema_mut().columns, &schema)?;
        self.table_plans.clear();
        let table_plan = self.table_plan.as_mut().expect("tablePlan");
        let res = make_mut_plan(table_plan).resolve_indices(ctx);
        self.table_plans = flatten_list_push_down_plan(table_plan);
        copy_resolved_indices(&mut self.sp.schema_mut().columns, table_plan.schema());
        res
    }
}

/// Copies the `Index` of each column of `resolved` (the table plan's schema) into the column
/// of `reader` (the table reader's own copy of that schema) at the same position. Go's two
/// schemas are one object, so a write to one is a write to the other; a column is copied only
/// when both positions hold the same column (equal unique ids), which is every position
/// while the reader's copy comes from the table plan's schema at `Init`. The copy also runs
/// when resolving failed: Go's writes made before the error are visible through the reader.
// glue: N1 §3.3 (the table reader and its table plan share one schema object in Go)
fn copy_resolved_indices(reader: &mut [Arc<Column>], resolved: &Schema) {
    for (col, resolved_col) in reader.iter_mut().zip(&resolved.columns) {
        if col.unique_id == resolved_col.unique_id && col.index != resolved_col.index {
            Arc::make_mut(col).index = resolved_col.index;
        }
    }
}

impl PhysicalSelection {
    /// ResolveIndices implements Plan interface.
    // go: PhysicalSelection.ResolveIndices L1 [M-CTX, M-EXPR]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.base.resolve_indices(ctx)?;
        let child = Arc::clone(&self.base.children[0]);
        for i in 0..self.conditions.len() {
            let expr = Arc::clone(&self.conditions[i]);
            match expr.resolve_indices(child.schema()) {
                Ok(e) => self.conditions[i] = e,
                Err(err) => {
                    // Check if there is duplicate virtual expression column matched.
                    let eval_ctx = ctx.eval_ctx();
                    let (new_cond, is_ok) =
                        expr.resolve_indices_by_virtual_expr(&*eval_ctx, child.schema());
                    if is_ok {
                        self.conditions[i] = new_cond;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }
}

/// resolveIndicesForSort: the `ResolveIndices` of `PhysicalSort` and `NominalSort`.
///
/// Go passes the embedded `BasePhysicalPlan` and switches on its `Self`; here the operator
/// itself is passed (`M-SELF`) and its kind decides (`M-IFACE`).
// go: resolveIndicesForSort L1 [M-SELF, M-IFACE, M-CTX]
pub fn resolve_indices_for_sort(p: &mut dyn PhysicalPlan, ctx: &PlanCtx<'_>) -> Result<(), Error> {
    p.base_physical_mut().resolve_indices(ctx)?;

    let child = Arc::clone(&p.children()[0]);
    let by_items: &mut Vec<ByItems> = match p.kind() {
        PhysicalKind::Sort => {
            &mut p
                .as_any_mut()
                .downcast_mut::<PhysicalSort>()
                .expect("PhysicalSort")
                .by_items
        }
        PhysicalKind::NominalSort => {
            &mut p
                .as_any_mut()
                .downcast_mut::<NominalSort>()
                .expect("NominalSort")
                .by_items
        }
        _ => {
            return Err(errors::errorf(
                "expect PhysicalSort or NominalSort, but got %s",
                &[Arg::from(p.tp())],
            ));
        }
    };
    for item in by_items.iter_mut() {
        item.expr = item.expr.resolve_indices(child.schema())?;
    }
    Ok(())
}

impl PhysicalSort {
    /// ResolveIndices implements Plan interface.
    // go: PhysicalSort.ResolveIndices L1 [M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        resolve_indices_for_sort(self, ctx)
    }
}

impl NominalSort {
    /// ResolveIndices implements Plan interface.
    // go: NominalSort.ResolveIndices L1 [M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        resolve_indices_for_sort(self, ctx)
    }
}

impl PhysicalTopN {
    /// ResolveIndices implements Plan interface.
    // go: PhysicalTopN.ResolveIndices L1 [M-CTX, M-EXPR]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.base.resolve_indices(ctx)?;
        let child = Arc::clone(&self.base.children[0]);
        for item in self.by_items.iter_mut() {
            item.expr = item.expr.resolve_indices(child.schema())?;
        }
        for i in 0..self.partition_by.len() {
            let new_col = self.partition_by[i].col.resolve_indices(child.schema())?;
            self.partition_by[i].col = Arc::new(new_col);
        }
        if let Some(prefix_col) = &self.prefix_col {
            let new_col = prefix_col.resolve_indices(child.schema())?;
            self.prefix_col = Some(Arc::new(new_col));
        }
        Ok(())
    }
}

impl PhysicalLimit {
    /// ResolveIndices implements Plan interface.
    // go: PhysicalLimit.ResolveIndices L1 [M-CTX, M-EXPR]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.sp.base.resolve_indices(ctx)?;
        let child = Arc::clone(&self.sp.base.children[0]);
        for i in 0..self.partition_by.len() {
            let new_col = self.partition_by[i].col.resolve_indices(child.schema())?;
            self.partition_by[i].col = Arc::new(new_col);
        }
        // To avoid that two plan shares the same column slice.
        let shallow_col_slice: Vec<Arc<Column>> = self.sp.schema().columns.clone();
        self.sp.set_schema(Some(new_schema(shallow_col_slice)));
        let mut found_cnt = 0;
        // The two column sets are all ordered. And the colsNeedResolving is the subset of the mergedSchema.
        // So we can just move forward j if there's no matching is found.
        // We don't use the normal ResolvIndices here since there might be duplicate columns in the schema.
        //   e.g. The schema of child_0 is [col0, col0, col1]
        //        ResolveIndices will only resolve all col0 reference of the current plan to the first col0.
        let child_schema = child.schema();
        let schema = self.sp.schema_mut();
        let (mut i, mut j) = (0, 0);
        while i < schema.len() && j < child_schema.len() {
            if !schema.columns[i].equal_col(&child_schema.columns[j]) {
                j += 1;
                continue;
            }
            let mut col = Column::clone(&schema.columns[i]);
            col.index = j as isize;
            schema.columns[i] = Arc::new(col);
            i += 1;
            j += 1;
            found_cnt += 1;
        }
        let schema_len = schema.len();
        if found_cnt < schema_len {
            return Err(errors::errorf(
                "Some columns of %v cannot find the reference from its child(ren)",
                &[Arg::from(self.explain_id(Some(ctx.vars.stmt_ctx())))],
            ));
        }
        if let Some(prefix_col) = &self.prefix_col {
            let new_col = prefix_col.resolve_indices(child.schema())?;
            self.prefix_col = Some(Arc::new(new_col));
        }
        Ok(())
    }
}

impl PhysicalTableScan {
    /// ResolveIndices implements Plan interface.
    // go: PhysicalTableScan.ResolveIndices L1 [M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.sp.resolve_indices(ctx)?;
        self.resolve_indices_itself()
    }

    /// ResolveIndicesItself implements PhysicalTableScan interface.
    ///
    /// Go sets the index on the column objects of the scan's schema, which other schemas may
    /// hold too (spec §5 item 1); each column is written through `Arc::make_mut`.
    // go: PhysicalTableScan.ResolveIndicesItself L1 [M-EXPR]
    pub fn resolve_indices_itself(&mut self) -> Result<(), Error> {
        for (i, column) in self.sp.schema_mut().columns.iter_mut().enumerate() {
            Arc::make_mut(column).index = i as isize;
        }
        Ok(())
    }
}

impl PhysicalSchemaProducer {
    // go: physicalSchemaProducer.ResolveIndices L1 [M-CTX]
    pub fn resolve_indices(&mut self, ctx: &PlanCtx<'_>) -> Result<(), Error> {
        self.base.resolve_indices(ctx)
    }
}

impl BaseSchemaProducer {
    // go: baseSchemaProducer.ResolveIndices L1
    pub fn resolve_indices(&mut self) -> Result<(), Error> {
        Ok(())
    }
}
