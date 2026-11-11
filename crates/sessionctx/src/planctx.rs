//! Go: pkg/planner/planctx/context.go (no `go-file` header until part 2b ports it).
//!
//! Interfaces of the planner context (block D00.1). Part 2a declares only the trait that
//! `ranger.MutableRanges.Rebuild` takes; the method set comes with part 2b (task T7).

/// PlanContext is the context for building plan.
// glue: interface skeleton, methods follow in D00.1 part 2b
pub trait PlanContext: Send + Sync {}
