//! Go package `pkg/planner/core/operator/logicalop`: the embedded structures of N1 §3.7.
//!
//! The operators themselves are ported by VS1-21 / VS1-23 (D09.14 and the rule blocks); this
//! module holds the two embedded structures every operator reuses and the macro that writes
//! the per-type boilerplate (`M-SELF`).

use std::cell::OnceCell;

use tidb_expression::{Schema, new_schema};

use crate::arena::{PlanArena, PlanId};

/// Go `base_logical_plan.go`, the fields only.
///
/// BaseLogicalPlan is the common structure that used in logical plan. Go's `self` field is
/// gone (`M-SELF`); `ctx` is the `PlanCtx` passed to every method (N2); `children` are arena
/// ids (`M-PLAN`). `taskMap` / `taskMapBak` (D10.2) are side tables keyed by `PlanId`
/// (N1 §3.2).
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

/// Go `logical_schema_producer.go`, the fields only.
///
/// LogicalSchemaProducer stores the schema for the logical plans who can produce schema
/// directly. Go's `Schema()` initialises a nil schema on first read (a getter with a side
/// effect); the schema is a `OnceCell` so that the read path stays `&self` (N1 §3.7).
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

    /// `Schema()`: the own schema, initialised on first read from the only child's (Go's
    /// default for single-child plans) or empty.
    // glue: accessor (Go LogicalSchemaProducer.Schema, ported with D09.14 / VS1-23)
    pub fn schema<'a>(&'a self, children: &[PlanId], arena: &'a PlanArena) -> &'a Schema {
        self.schema.get_or_init(|| {
            if children.len() == 1 {
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

    /// `SetSchema` (`None` is Go's nil: the next read initialises it again).
    // glue: accessor (Go LogicalSchemaProducer.SetSchema, ported with D09.14 / VS1-23)
    pub fn set_schema(&mut self, schema: Option<Schema>) {
        self.schema = match schema {
            Some(s) => OnceCell::from(s),
            None => OnceCell::new(),
        };
    }
}

/// The per-type methods of [`crate::LogicalPlan`] (`M-IFACE`, `M-SELF`): `kind`, the type
/// assertion hooks and the accessors of the embedded structures. The operator has a field
/// `base: BaseLogicalPlan`, and with `producer` also `sp: LogicalSchemaProducer`.
///
/// ```ignore
/// impl LogicalPlan for LogicalProjection {
///     logical_plan_boilerplate!(Projection, producer);
///     // go: ... the Go methods of the operator
/// }
/// ```
#[macro_export]
macro_rules! logical_plan_boilerplate {
    ($kind:ident) => {
        // glue: M-SELF (generated per operator)
        fn kind(&self) -> $crate::LogicalKind {
            $crate::LogicalKind::$kind
        }
        // glue: M-SELF (generated per operator)
        fn as_any(&self) -> &dyn ::std::any::Any {
            self
        }
        // glue: M-SELF (generated per operator)
        fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
            self
        }
        // glue: M-SELF (generated per operator)
        fn base(&self) -> &$crate::logicalop::BaseLogicalPlan {
            &self.base
        }
        // glue: M-SELF (generated per operator)
        fn base_mut(&mut self) -> &mut $crate::logicalop::BaseLogicalPlan {
            &mut self.base
        }
    };
    ($kind:ident, producer) => {
        $crate::logical_plan_boilerplate!($kind);
        // glue: M-SELF (generated per operator)
        fn schema_producer(&self) -> Option<&$crate::logicalop::LogicalSchemaProducer> {
            Some(&self.sp)
        }
        // glue: M-SELF (generated per operator)
        fn schema_producer_mut(&mut self) -> Option<&mut $crate::logicalop::LogicalSchemaProducer> {
            Some(&mut self.sp)
        }
    };
}
