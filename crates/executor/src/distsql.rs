//! go-file: pkg/executor/distsql.go
//!
//! Shared DistSQL reader helpers (`closeAll`, `getPhysicalPlanIDs`, block D12.1 Part 2A).
//! `IndexReaderExecutor` (Part 2B) and `IndexLookUpExecutor` (Part 2C) will be added here.

use tidb_base::Error;
use tidb_store::distsql::SelectResult;

use crate::glue::Plan;

/// Go's `Closeable` interface (`distsql.go:134`).
pub trait Closeable {
    // glue: Go interface method Closeable.Close
    fn close(&mut self) -> Result<(), Error>;
}

impl Closeable for Box<dyn SelectResult> {
    // glue: Closeable for Box<dyn SelectResult>
    fn close(&mut self) -> Result<(), Error> {
        SelectResult::close(self.as_mut())
    }
}

/// closeAll closes all objects even if an object returns an error.
/// If multiple objects return an error, the first error will be returned.
// go: closeAll L1
pub fn close_all(objs: &mut [Option<&mut dyn Closeable>]) -> Result<(), Error> {
    let mut err: Option<Error> = None;
    for obj in objs {
        if let Some(obj) = obj.as_deref_mut()
            && let Err(err1) = obj.close()
            && err.is_none()
        {
            err = Some(err1);
        }
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// getPhysicalPlanIDs returns the plan IDs of all pushed-down physical plans.
// go: getPhysicalPlanIDs L1
pub fn get_physical_plan_ids(plans: &[Plan]) -> Vec<isize> {
    let mut plan_ids = Vec::with_capacity(plans.len());
    for p in plans {
        plan_ids.push(p.id() as isize);
    }
    plan_ids
}
