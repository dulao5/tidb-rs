//! go-file: pkg/executor/internal/builder/builder_utils.go
//!
//! The DAG request of a reader executor from its pushed-down physical plans (block D12.0).

use std::collections::HashMap;

use tidb_base::Error;
use tidb_base::kv::{StoreType, TiFlash, TiKV};
use tidb_base::timeutil;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::variable::tidb_defs::DefDivPrecisionIncrement;
use tidb_store::distsql;

use crate::glue::{Plan, SessionContext};

/// ConstructTreeBasedDistExec constructs tree based DAGRequest
// go: ConstructTreeBasedDistExec L1
pub fn construct_tree_based_dist_exec(
    pctx: &BuildPBContext,
    p: &Plan,
) -> Result<Vec<tipb::Executor>, Error> {
    let exec_pb = p.to_pb(pctx, TiFlash)?;
    Ok(vec![exec_pb])
}

/// ConstructListBasedDistExec constructs list based DAGRequest
// go: ConstructListBasedDistExec L1
pub fn construct_list_based_dist_exec(
    pctx: &BuildPBContext,
    plans: &[Plan],
) -> Result<Vec<tipb::Executor>, Error> {
    let mut executors = Vec::with_capacity(plans.len());
    for p in plans {
        let exec_pb = p.to_pb(pctx, TiKV)?;
        executors.push(exec_pb);
    }
    Ok(executors)
}

/// ConstructListBasedDistExecForUnNatureOrderPlans constructs list based executors and sets
/// ParentIdx for those executors whose parent is not the next one in the list.
// go: ConstructListBasedDistExecForUnNatureOrderPlans L1 [M-MAPORDER]
pub fn construct_list_based_dist_exec_for_un_nature_order_plans(
    pctx: &BuildPBContext,
    plans: &[Plan],
    un_nature_orders: &HashMap<isize, isize>,
) -> Result<Vec<tipb::Executor>, Error> {
    let mut executors = construct_list_based_dist_exec(pctx, plans)?;
    for (&i, &j) in un_nature_orders {
        let parent_idx = j as u32;
        executors[i as usize].set_parent_idx(parent_idx);
    }
    Ok(executors)
}

/// ConstructDAGReq constructs DAGRequest for physical plans
// go: ConstructDAGReq L1
pub fn construct_dag_req(
    ctx: &dyn SessionContext,
    plans: &[Plan],
    store_type: StoreType,
) -> Result<tipb::DagRequest, Error> {
    let mut dag_req = tipb::DagRequest::default();
    let vars = ctx.get_session_vars();
    let (tz_name, tz_offset) = timeutil::zone(&vars.location());
    dag_req.set_time_zone_name(tz_name);
    dag_req.set_time_zone_offset(tz_offset);
    let sc = vars.stmt_ctx();
    if sc.runtime_stats_coll.is_some() {
        dag_req.set_collect_execution_summaries(true);
    }
    dag_req.set_flags(sc.push_down_flags());
    let div_prec_incr = vars.get_div_precision_increment();
    if div_prec_incr != DefDivPrecisionIncrement {
        dag_req.set_div_precision_increment(div_prec_incr as u32);
    }
    let build_pb_ctx = ctx.get_build_pb_ctx()?;
    let res = if store_type == TiFlash {
        construct_tree_based_dist_exec(&build_pb_ctx, &plans[0]).map(|mut executors| {
            dag_req.set_root_executor(executors.remove(0));
        })
    } else {
        construct_list_based_dist_exec(&build_pb_ctx, plans).map(|executors| {
            dag_req.set_executors(executors.into());
        })
    };

    let dctx = ctx.get_dist_sql_ctx();
    distsql::set_encode_type(&dctx, &mut dag_req);
    res?;
    Ok(dag_req)
}

/// ConstructDAGReqForUnNatureOrderPlans constructs DAGRequest for physical plans.
/// The unNatureOrders map is used to set the ParentIdx of executors in DAGRequest.
/// `unNatureOrders` is a map with layout {childIndex => parentIndex} and contains the children
/// indexes whose parent is not the next one.
// go: ConstructDAGReqForUnNatureOrderPlans L1 [M-MAPORDER]
pub fn construct_dag_req_for_un_nature_order_plans(
    ctx: &dyn SessionContext,
    plans: &[Plan],
    un_nature_orders: &HashMap<isize, isize>,
    store_type: StoreType,
) -> Result<tipb::DagRequest, Error> {
    let mut dag_req = construct_dag_req(ctx, plans, store_type)?;

    for (&i, &j) in un_nature_orders {
        let parent_idx = j as u32;
        dag_req.mut_executors()[i as usize].set_parent_idx(parent_idx);
    }

    Ok(dag_req)
}
