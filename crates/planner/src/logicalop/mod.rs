//! Go package `pkg/planner/core/operator/logicalop`: the logical operators (block D09.14) and
//! their rule methods (column pruning D09.7, predicate push down D09.8, TopN push down
//! D09.11), for the operators of the first vertical slice (VS1-23,
//! `specs/D09.14-logical-operators/spec.md`).
//!
//! Each operator is a struct with a `base: BaseLogicalPlan` field (and `sp:
//! LogicalSchemaProducer` when Go embeds one); its Go methods are the trait methods of
//! [`crate::LogicalPlan`] it overrides (`M-SELF`, N1 §3.7).

pub mod base_logical_plan;
pub mod derive_stats;
pub mod logical_datasource;
pub mod logical_join;
pub mod logical_limit;
pub mod logical_plans_misc;
pub mod logical_projection;
pub mod logical_schema_producer;
pub mod logical_selection;
pub mod logical_sort;
pub mod logical_table_dual;
pub mod logical_table_scan;
pub mod logical_top_n;

pub use base_logical_plan::{BaseLogicalPlan, new_base_logical_plan};
pub use logical_datasource::DataSource;
pub use logical_limit::LogicalLimit;
pub use logical_projection::LogicalProjection;
pub use logical_schema_producer::LogicalSchemaProducer;
pub use logical_selection::LogicalSelection;
pub use logical_sort::LogicalSort;
pub use logical_table_dual::LogicalTableDual;
pub use logical_top_n::LogicalTopN;

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
