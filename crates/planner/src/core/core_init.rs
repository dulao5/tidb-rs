//! go-file: pkg/planner/core/core_init.go
//!
//! Go's `init` stores functions of package `core` into function variables of lower
//! packages (`utilfuncp`, `logicalop`, `property`, `cardinality`, ...) to break import
//! cycles. One Rust crate holds all of them, so the callers call the functions directly
//! (e.g. `BasePhysicalPlan::get_est_row_count_for_display` calls
//! `core::plan::get_estimated_probe_cnt_from_probe_parents`).

// go: init L2 (cross-package function variables; the crate calls the functions directly)
