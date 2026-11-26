//! go-file: pkg/executor/internal/util/partition_table.go
//!
//! Partition table DAG request helper (D12.0, carried with D12.1 Part 2A).

use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gofmt::Arg;
use tipb::ExecType;

/// UpdateExecutorTableID updates executor's TableId attribute.
///
/// The Go test-only context hook (`ctx.Value("nextPartitionUpdateDAGReq")`) is omitted under
/// mapping rule `M-GOCTX`.
// go: UpdateExecutorTableID L2 (M-GOCTX: omits test-only ctx.Value hook)
pub fn update_executor_table_id(
    _ctx: &tidb_kv::Context,
    exec: Option<&mut tipb::Executor>,
    recursive: bool,
    partition_ids: &[i64],
) -> Result<(), Error> {
    let Some(exec) = exec else {
        return Ok(());
    };
    let tp = exec.get_tp();
    macro_rules! opt_child {
        ($msg:expr) => {{
            let m = $msg;
            if m.has_child() {
                Some(m.mut_child())
            } else {
                None
            }
        }};
    }
    let child: Option<&mut tipb::Executor> = match tp {
        ExecType::TypeTableScan => {
            exec.mut_tbl_scan().set_table_id(partition_ids[0]);
            None
        }
        ExecType::TypePartitionTableScan => {
            exec.mut_partition_table_scan()
                .set_partition_ids(partition_ids.to_vec());
            None
        }
        ExecType::TypeIndexScan => {
            exec.mut_idx_scan().set_table_id(partition_ids[0]);
            None
        }
        ExecType::TypeSelection => opt_child!(exec.mut_selection()),
        ExecType::TypeAggregation | ExecType::TypeStreamAgg => opt_child!(exec.mut_aggregation()),
        ExecType::TypeTopN => opt_child!(exec.mut_top_n()),
        ExecType::TypeLimit => opt_child!(exec.mut_limit()),
        ExecType::TypeExchangeSender => opt_child!(exec.mut_exchange_sender()),
        ExecType::TypeExchangeReceiver => None,
        ExecType::TypeJoin => {
            let join = exec.mut_join();
            let probe_idx = (1 - join.get_inner_idx()) as usize;
            join.mut_children().get_mut(probe_idx)
        }
        ExecType::TypeProjection => opt_child!(exec.mut_projection()),
        ExecType::TypeWindow => opt_child!(exec.mut_window()),
        ExecType::TypeSort => opt_child!(exec.mut_sort()),
        ExecType::TypeExpand => opt_child!(exec.mut_expand()),
        ExecType::TypeExpand2 => opt_child!(exec.mut_expand2()),
        _ => {
            return Err(errors::errorf(
                "unknown new tipb protocol %d",
                &[Arg::from(tp as i32)],
            ));
        }
    };
    if let Some(child) = child
        && recursive
    {
        return update_executor_table_id(_ctx, Some(child), recursive, partition_ids);
    }
    Ok(())
}
