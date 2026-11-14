//! go-file: pkg/executor/internal/builder/builder_utils.go
//!
//! The DAG request of a reader executor from its pushed-down physical plans. The plans'
//! `ToPB` is D10.9 (VS1-31): it is a boundary stub, so the list builders fail with
//! `blocked-by:D10.9` until then; `ConstructDAGReq` (time zone, flags, encode type: D00.3
//! `timeutil.Zone`, D07.3 `distsql.SetEncodeType`) comes with the table reader (VS1-34).

use std::collections::HashMap;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv::StoreType;

use crate::glue::Plan;

/// `p.ToPB(pctx, storeType)`.
// glue: stub, blocked-by:D10.9 (PhysicalPlan.ToPB, VS1-31)
fn to_pb(p: &Plan, store_type: StoreType) -> Result<tipb::Executor, Error> {
    let _ = (p, store_type);
    Err(blocked("D10.9", "PhysicalPlan.ToPB"))
}

/// ConstructTreeBasedDistExec constructs tree based DAGRequest
// go: ConstructTreeBasedDistExec L1
pub fn construct_tree_based_dist_exec(p: &Plan) -> Result<Vec<tipb::Executor>, Error> {
    let exec_pb = to_pb(p, tidb_base::kv::TiFlash)?;
    Ok(vec![exec_pb])
}

/// ConstructListBasedDistExec constructs list based DAGRequest
// go: ConstructListBasedDistExec L1
pub fn construct_list_based_dist_exec(plans: &[Plan]) -> Result<Vec<tipb::Executor>, Error> {
    let mut executors = Vec::with_capacity(plans.len());
    for p in plans {
        let exec_pb = to_pb(p, tidb_base::kv::TiKV)?;
        executors.push(exec_pb);
    }
    Ok(executors)
}

/// ConstructListBasedDistExecForUnNatureOrderPlans constructs list based executors and sets
/// ParentIdx for those executors whose parent is not the next one in the list.
// go: ConstructListBasedDistExecForUnNatureOrderPlans L1 [M-MAPORDER]
pub fn construct_list_based_dist_exec_for_un_nature_order_plans(
    plans: &[Plan],
    un_nature_orders: &HashMap<isize, isize>,
) -> Result<Vec<tipb::Executor>, Error> {
    let mut executors = construct_list_based_dist_exec(plans)?;
    for (&i, &j) in un_nature_orders {
        let parent_idx = j as u32;
        executors[i as usize].set_parent_idx(parent_idx);
    }
    Ok(executors)
}

/// ConstructDAGReq constructs DAGRequest for physical plans
// go: ConstructDAGReq L2 (boundary stub, blocked-by:D10.9)
pub fn construct_dag_req(plans: &[Plan], store_type: StoreType) -> Result<tipb::DagRequest, Error> {
    let _ = (plans, store_type);
    Err(blocked("D10.9", "the DAG request of a reader executor"))
}

/// ConstructDAGReqForUnNatureOrderPlans constructs DAGRequest for physical plans.
/// The unNatureOrders map is used to set the ParentIdx of executors in DAGRequest.
/// `unNatureOrders` is a map with layout {childIndex => parentIndex} and contains the children
/// indexes whose parent is not the next one.
// go: ConstructDAGReqForUnNatureOrderPlans L1 [M-MAPORDER]
pub fn construct_dag_req_for_un_nature_order_plans(
    plans: &[Plan],
    un_nature_orders: &HashMap<isize, isize>,
    store_type: StoreType,
) -> Result<tipb::DagRequest, Error> {
    let mut dag_req = construct_dag_req(plans, store_type)?;

    for (&i, &j) in un_nature_orders {
        let parent_idx = j as u32;
        dag_req.mut_executors()[i as usize].set_parent_idx(parent_idx);
    }

    Ok(dag_req)
}
