//! go-file: pkg/planner/core/operator/logicalop/logical_schema_producer.go
//!
//! `LogicalSchemaProducer`, embedded by the operators that own a schema. Go's `Schema()` and
//! `OutputNames()` initialise a nil field on first read (a getter with a side effect); the
//! fields are `OnceCell`s so that the read path stays `&self` (N1 §3.7). Go embeds
//! `BaseLogicalPlan` in it; here the operator holds both side by side (`base`, `sp`).

use std::cell::OnceCell;
use std::sync::Arc;

use tidb_expression::{Column, Schema, get_used_list, new_schema};
use tidb_types::field_name::NameSlice;

use crate::arena::{PlanArena, PlanId};
use crate::base::LogicalPlan;
use crate::planctx::PlanCtx;
use crate::util::logicaltrace;
use crate::util::optimizetrace::LogicalOptimizeOp;

/// LogicalSchemaProducer stores the schema for the logical plans who can produce schema
/// directly.
#[derive(Debug, Default)]
pub struct LogicalSchemaProducer {
    schema: OnceCell<Schema>,
    names: OnceCell<NameSlice>,
}

impl LogicalSchemaProducer {
    /// A producer with `schema` (Go: `SetSchema` right after `Init`), or a nil schema.
    // glue: N1 (struct literal)
    pub fn new(schema: Option<Schema>) -> LogicalSchemaProducer {
        let mut sp = LogicalSchemaProducer::default();
        sp.set_schema(schema);
        sp
    }

    /// Schema implements the Plan.Schema interface.
    // go: LogicalSchemaProducer.Schema L1 [M-PLAN]
    pub fn schema<'a>(&'a self, children: &[PlanId], arena: &'a PlanArena) -> &'a Schema {
        self.schema.get_or_init(|| {
            if children.len() == 1 {
                // default implementation for plans has only one child: proprgate child schema.
                // multi-children plans are likely to have particular implementation.
                arena.get(children[0]).schema(arena).clone_schema()
            } else {
                new_schema(Vec::new())
            }
        })
    }

    /// The schema for writing; [`Self::schema`] must have run (callers go through
    /// `LogicalPlan::schema_mut` / `PlanArena::schema_mut`, which ensure it).
    // glue: N1
    pub fn schema_cell_mut(&mut self) -> &mut Schema {
        self.schema.get_mut().expect("schema initialised")
    }

    /// OutputNames implements the Plan.OutputNames interface. Go leaves `names` nil when
    /// the plan has no single child; that nil is the empty slice here and is not cached.
    // go: LogicalSchemaProducer.OutputNames L1 [M-PLAN]
    pub fn output_names<'a>(&'a self, children: &[PlanId], arena: &'a PlanArena) -> &'a NameSlice {
        if self.names.get().is_none() && children.len() == 1 {
            // default implementation for plans has only one child: proprgate child `OutputNames`.
            // multi-children plans are likely to have particular implementation.
            let names = arena.get(children[0]).output_names(arena).clone();
            let _ = self.names.set(names);
        }
        static EMPTY: NameSlice = Vec::new();
        self.names.get().unwrap_or(&EMPTY)
    }

    /// SetOutputNames sets the output names for the plan.
    // go: LogicalSchemaProducer.SetOutputNames L1
    pub fn set_output_names(&mut self, names: NameSlice) {
        self.names = OnceCell::from(names);
    }

    /// SetSchema sets the logical schema producer's schema (`None` is Go's nil: the next read
    /// initialises it again).
    // go: LogicalSchemaProducer.SetSchema L1
    pub fn set_schema(&mut self, schema: Option<Schema>) {
        self.schema = match schema {
            Some(s) => OnceCell::from(s),
            None => OnceCell::new(),
        };
    }

    /// SetSchemaAndNames sets the schema and names for the plan.
    // go: LogicalSchemaProducer.SetSchemaAndNames L1
    pub fn set_schema_and_names(&mut self, schema: Schema, names: NameSlice) {
        self.schema = OnceCell::from(schema);
        self.names = OnceCell::from(names);
    }
}

/// InlineProjection prunes unneeded columns inline an executor. `p` is the operator that
/// embeds the producer (Go's `s.Self()` for the trace).
// go: LogicalSchemaProducer.InlineProjection L1 [M-SELF]
pub fn inline_projection<P: LogicalPlan + ?Sized>(
    p: &mut P,
    ctx: &mut PlanCtx<'_>,
    parent_used_cols: &[Arc<Column>],
    opt: &LogicalOptimizeOp,
) {
    let mut pruned_columns = Vec::new();
    let ectx = ctx.eval_ctx();
    let mut used = get_used_list(&*ectx, parent_used_cols, p.schema(&ctx.arena));
    if parent_used_cols.is_empty() {
        // When this operator output no columns, we return its smallest column for safety.
        let mut min_col_len = isize::MAX;
        let mut chosen_pos = 0;
        for (i, col) in p.schema(&ctx.arena).columns.iter().enumerate() {
            let flen = col.get_type().get_flen() as isize;
            if flen < min_col_len {
                chosen_pos = i;
                min_col_len = flen;
            }
        }
        // It should be always true.
        if !used.is_empty() {
            used[chosen_pos] = true;
        }
    }
    for i in (0..used.len()).rev() {
        if !used[i] {
            pruned_columns.push(Arc::clone(&p.schema(&ctx.arena).columns[i]));
            p.schema_mut(&mut ctx.arena).columns.remove(i);
        }
    }
    logicaltrace::append_column_prune_trace_step(&*p, ctx, &pruned_columns, opt);
}

/// BuildKeyInfo implements LogicalPlan.BuildKeyInfo interface: the default for operators
/// with one child (propagate the child's keys). `p` is the operator; its own schema is the
/// `selfSchema` Go passes (the rule calls `p.BuildKeyInfo(p.Schema(), childSchema)`).
// go: LogicalSchemaProducer.BuildKeyInfo L1 [M-SELF, M-PLAN]
pub fn build_key_info<P: LogicalPlan + ?Sized>(p: &mut P, arena: &mut PlanArena) {
    p.schema_mut(arena).keys = Vec::new();
    crate::logicalop::base_logical_plan::build_key_info(p, arena);

    // default implementation for plans has only one child: proprgate child keys
    // multi-children plans are likely to have particular implementation.
    if p.children().len() == 1 {
        let child_keys = arena.get(p.children()[0]).schema(arena).keys.clone();
        let self_schema = p.schema_mut(arena);
        for key in &child_keys {
            let Some(indices) = self_schema.columns_indices(key) else {
                continue;
            };
            let new_key: Vec<Arc<Column>> = indices
                .iter()
                .map(|&i| Arc::clone(&self_schema.columns[i as usize]))
                .collect();
            self_schema.keys.push(new_key);
        }
    }
}
