//! go-file: pkg/planner/core/operator/logicalop/logical_show.go
//! go-file: pkg/planner/core/operator/logicalop/logical_show_ddl_jobs.go
//!
//! `LogicalShow` and `LogicalShowDDLJobs` are built only for SHOW and ADMIN SHOW DDL JOBS,
//! which the new stack does not route (D12.12): their FindBestTask comes with them.

// glue: stub of LogicalShow.FindBestTask, blocked-by:D12.12 (LogicalShow is built only for SHOW)
// glue: stub of LogicalShowDDLJobs.FindBestTask, blocked-by:D12.12 (LogicalShowDDLJobs is built only for ADMIN SHOW DDL JOBS)
/// The FindBestTask of the SHOW operators: a boundary.
pub fn show_find_best_task_boundary() -> tidb_base::Error {
    tidb_base::blocked::blocked("D12.12", "the FindBestTask of SHOW (LogicalShow)")
}
