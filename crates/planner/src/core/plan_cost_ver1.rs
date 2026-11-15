//! go-file: pkg/planner/core/plan_cost_ver1.go
//!
//! The cost model v1 (D10.5, VS1-30). So far the stub the point get executor calls.

use tidb_base::Error;
use tidb_base::blocked::blocked;

use crate::core::point_get_plan::PointGetPlan;

impl PointGetPlan {
    /// `GetAvgRowSize` (`plan_cost_ver1.go`, D10.5): the row size estimate of the
    /// closest-adaptive replica read adjuster (cardinality of the accessed columns).
    // glue: stub of PointGetPlan.GetAvgRowSize, blocked-by:D10.5 (row size of a point get for tidb_replica_read=closest-adaptive)
    pub fn get_avg_row_size(&self) -> Result<f64, Error> {
        Err(blocked(
            "D10.5",
            "the row size of a point get (tidb_replica_read=closest-adaptive)",
        ))
    }
}
