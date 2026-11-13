//! go-package: pkg/planner/core/operator/logicalop (spike subset)
//!
//! Each operator keeps only the fields that D09.7 reads; its `PruneColumns` (and
//! `ReplaceExprColumns`, used by projection elimination) are ported line by line.

use std::cell::OnceCell;

use tidb_expression::{Schema, new_schema};

use crate::arena::{PlanArena, PlanId};
use crate::planctx::PlanCtx;

pub mod logical_aggregation;
pub mod logical_datasource;
pub mod logical_join;
pub mod logical_limit;
pub mod logical_plans_misc;
pub mod logical_projection;
pub mod logical_selection;
pub mod logical_sort;
pub mod logical_table_dual;
pub mod logical_top_n;

pub use logical_aggregation::LogicalAggregation;
pub use logical_datasource::DataSource;
pub use logical_join::{JoinType, LogicalJoin};
pub use logical_limit::LogicalLimit;
pub use logical_projection::LogicalProjection;
pub use logical_selection::LogicalSelection;
pub use logical_sort::LogicalSort;
pub use logical_table_dual::LogicalTableDual;
pub use logical_top_n::LogicalTopN;

/// go-file: pkg/planner/core/operator/logicalop/base_logical_plan.go
///
/// BaseLogicalPlan is the common structure that used in logical plan. Go's `self` field is
/// gone (`M-SELF`); `ctx` is the `PlanCtx` passed to every method (N2); `children` are arena
/// ids (`M-PLAN`). `taskMap` / `taskMapBak` (D10.2) would be a side table keyed by `PlanId`
/// in the arena (N1 §3.2) — not exercised by D09.7.
#[derive(Debug)]
pub struct BaseLogicalPlan {
    /// Go's plan id (`baseimpl.Plan.id`), N1 §3.6.
    pub id: i32,
    /// `baseimpl.Plan.tp`.
    pub tp: &'static str,
    /// `baseimpl.Plan.qbBlock`.
    pub qb_offset: i32,
    /// children.
    pub children: Vec<PlanId>,
}

impl BaseLogicalPlan {
    /// NewBaseLogicalPlan is the basic constructor of BaseLogicalPlan.
    // go: pkg/planner/core/operator/logicalop/base_logical_plan.go NewBaseLogicalPlan L1 [M-SELF]
    pub fn new(ctx: &PlanCtx<'_>, tp: &'static str, qb_offset: i32) -> BaseLogicalPlan {
        BaseLogicalPlan {
            id: ctx.alloc_plan_id(),
            tp,
            qb_offset,
            children: Vec::new(),
        }
    }
}

/// go-file: pkg/planner/core/operator/logicalop/logical_schema_producer.go
///
/// LogicalSchemaProducer stores the schema for the logical plans who can produce schema
/// directly. Go's `Schema()` initialises a nil schema on first read (a getter with a side
/// effect); the schema is a `OnceCell` so that the read path stays `&self`.
#[derive(Debug, Default)]
pub struct LogicalSchemaProducer {
    schema: OnceCell<Schema>,
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
    // go: pkg/planner/core/operator/logicalop/logical_schema_producer.go (*LogicalSchemaProducer).Schema L1 [M-PLAN]
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

    /// SetSchema sets the logical schema producer's schema (`None` is Go's nil).
    // go: pkg/planner/core/operator/logicalop/logical_schema_producer.go (*LogicalSchemaProducer).SetSchema L1
    pub fn set_schema(&mut self, schema: Option<Schema>) {
        self.schema = match schema {
            Some(s) => OnceCell::from(s),
            None => OnceCell::new(),
        };
    }
}

/// The per-type methods of [`crate::base::LogicalPlan`] (`M-IFACE`, `M-SELF`).
macro_rules! logical_plan_boilerplate {
    ($kind:ident) => {
        fn kind(&self) -> $crate::base::LogicalKind {
            $crate::base::LogicalKind::$kind
        }
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
        fn base(&self) -> &$crate::logicalop::BaseLogicalPlan {
            &self.base
        }
        fn base_mut(&mut self) -> &mut $crate::logicalop::BaseLogicalPlan {
            &mut self.base
        }
    };
    ($kind:ident, producer) => {
        $crate::logicalop::logical_plan_boilerplate!($kind);
        fn schema_producer(&self) -> Option<&$crate::logicalop::LogicalSchemaProducer> {
            Some(&self.sp)
        }
        fn schema_producer_mut(&mut self) -> Option<&mut $crate::logicalop::LogicalSchemaProducer> {
            Some(&mut self.sp)
        }
    };
}
pub(crate) use logical_plan_boilerplate;
