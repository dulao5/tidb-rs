//! go-file: pkg/distsql/distsql.go
//!
//! Port of Go TiDB v8.5.8 `pkg/distsql` (block D07.3, Parts 1 & 2: 97 functions covering
//! `RequestBuilder`, handle/common-handle/index range encoding, `Select` /
//! `SelectWithRuntimeStats`, `SetEncodeType`, `selectResult`, `sortedSelectResults`,
//! `selectResultIter` / `selRespChannelIter`, and `selectResultRuntimeStats`).
//!
//! Formal code lives in `tidb-store::distsql` (N0 rank 11) so the `--new-stack=off` PoC
//! crate `tidb-distsql` (`crates/distsql/`) stays untouched (D07.3 spec §1.4).

pub mod request_builder;
pub mod select_result;
#[cfg(test)]
mod tests;

use std::mem::size_of;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::kv::TiFlash;
use tidb_kv::kv::{ClientSendOption, Request};
use tidb_parser::types::FieldType;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_types::mydecimal::MyDecimal;

pub use request_builder::{
    RequestBuilder, common_handle_ranges_to_kv_ranges, encode_index_key, index_ranges_to_kv_ranges,
    index_ranges_to_kv_ranges_with_interrupt_signal, partition_handles_to_kv_ranges,
    split_ranges_across_int64_boundary, table_handle_ranges_to_kv_ranges,
    table_handles_to_kv_ranges, table_ranges_to_kv_ranges, verify_txn_scope,
};
pub use select_result::{
    SelectResult, SelectResultImpl, SelectResultIter, SelectResultRow, SelectResultRuntimeStats,
    get_select_result_concurrency, new_sorted_select_results,
};

/// Select sends a DAG request, returns SelectResult.
/// In `kv_req`, `key_ranges` is required; `concurrency`/`keep_order`/`desc`/
/// `isolation_level`/`priority` are optional.
///
/// Async in Rust so `CopClient::send_async` builds and opens `CopIterator` eagerly before
/// returning, matching Go's `GetSelectResultConcurrency` before the first `Next` (D07.3
/// spec §4.3, §4.6). `TiFlash` requests stop defensively with `blocked("DEF.MPP", ...)`.
// go: Select L2 (async eager CopIterator build; TiFlash returns blocked DEF.MPP; no failpoints/tracing/eventCb)
pub async fn select(
    ctx: &tidb_kv::Context,
    dctx: Arc<DistSQLContext>,
    kv_req: &Request,
    field_types: Vec<Arc<FieldType>>,
) -> Result<Box<dyn SelectResult>, Error> {
    let sr = select_impl(ctx, dctx, kv_req, field_types, Vec::new(), 0).await?;
    Ok(Box::new(sr))
}

/// SelectWithRuntimeStats sends a DAG request, returns SelectResult.
/// The difference from [`select`] is that it sets `cop_plan_ids` and `root_plan_id` into
/// `selectResult`, which helps `selectResult` collect runtime stats.
// go: SelectWithRuntimeStats L2 (async eager CopIterator build, spec §4.3/§4.6)
pub async fn select_with_runtime_stats(
    ctx: &tidb_kv::Context,
    dctx: Arc<DistSQLContext>,
    kv_req: &Request,
    field_types: Vec<Arc<FieldType>>,
    cop_plan_ids: Vec<isize>,
    root_plan_id: isize,
) -> Result<Box<dyn SelectResult>, Error> {
    let sr = select_impl(ctx, dctx, kv_req, field_types, cop_plan_ids, root_plan_id).await?;
    Ok(Box::new(sr))
}

// go-part: Select (shared implementation for select and select_with_runtime_stats)
async fn select_impl(
    ctx: &tidb_kv::Context,
    dctx: Arc<DistSQLContext>,
    kv_req: &Request,
    field_types: Vec<Arc<FieldType>>,
    cop_plan_ids: Vec<isize>,
    root_plan_id: isize,
) -> Result<SelectResultImpl, Error> {
    if kv_req.store_type == TiFlash {
        return Err(blocked("DEF.MPP", "distsql.Select with StoreType::TiFlash"));
    }

    let ctx = with_sql_kv_exec_counter_interceptor(ctx);
    let enable_collect_execution_info = tidb_base::config::get_global_config()
        .instance
        .enable_collect_execution_info
        .load();
    let option = ClientSendOption {
        session_mem_tracker: dctx.session_mem_tracker.clone(),
        enabled_rate_limit_action: dctx.enabled_rate_limit_action,
        enable_collect_execution_info,
        append_warning: None,
        try_cop_lite_worker: Some(Arc::clone(&dctx.try_cop_lite_worker)),
    };

    let Some(client) = &dctx.client else {
        return Err(Error::plain("client returns nil response"));
    };
    let resp = client
        .send_async(&ctx, kv_req, Some(Box::new(dctx.kv_vars.clone())), &option)
        .await?;

    let sql_type = if dctx.in_restricted_sql {
        "internal"
    } else {
        tidb_metrics::GENERAL
    };

    // kvReq.MemTracker is used to trace and control memory usage in DistSQL layer;
    // for selectResult, we just use the kvReq.MemTracker prepared for co-processor
    // instead of creating a new one for simplification.
    Ok(SelectResultImpl::new(
        "dag",
        resp,
        field_types,
        dctx,
        sql_type,
        kv_req.mem_tracker.clone(),
        kv_req.store_type,
        kv_req.paging.enable,
        kv_req.concurrency,
        cop_plan_ids,
        root_plan_id,
    ))
}

/// SetEncodeType sets the encoding method for the DAGRequest. The supported encoding
/// methods are:
/// 1. `TypeChunk`: the result is encoded using the Chunk format
/// 2. `TypeDefault`: the result is encoded row by row
// go: SetEncodeType L1
pub fn set_encode_type(ctx: &DistSQLContext, dag_req: &mut tipb::DagRequest) {
    if can_use_chunk_rpc(ctx) {
        dag_req.set_encode_type(tipb::EncodeType::TypeChunk);
        set_chunk_memory_layout(dag_req);
    } else {
        dag_req.set_encode_type(tipb::EncodeType::TypeDefault);
    }
}

// go: canUseChunkRPC L1
fn can_use_chunk_rpc(ctx: &DistSQLContext) -> bool {
    if !ctx.enable_chunk_rpc {
        return false;
    }
    if !check_alignment() {
        return false;
    }
    true
}

/// checkAlignment checks the alignment in current system environment.
/// Rust `MyDecimal` is `#[repr(C)]` with `size_of::<MyDecimal>() == 40`.
// go: checkAlignment L2 (compile-time size_of::<MyDecimal>() == 40 instead of unsafe.Sizeof)
fn check_alignment() -> bool {
    const { size_of::<MyDecimal>() == 40 }
}

/// setChunkMemoryLayout sets the chunk memory layout for the DAGRequest.
// go: setChunkMemoryLayout L1
fn set_chunk_memory_layout(dag_req: &mut tipb::DagRequest) {
    let mut layout = tipb::ChunkMemoryLayout::default();
    layout.set_endian(get_system_endian());
    dag_req.set_chunk_memory_layout(layout);
}

/// GetSystemEndian gets the system endian.
// go: GetSystemEndian L2 (compile-time cfg(target_endian) instead of runtime unsafe init)
pub fn get_system_endian() -> tipb::Endian {
    #[cfg(target_endian = "big")]
    {
        tipb::Endian::BigEndian
    }
    #[cfg(not(target_endian = "big"))]
    {
        tipb::Endian::LittleEndian
    }
}

/// WithSQLKvExecCounterInterceptor binds an interceptor for client-go to count the number of
/// SQL executions of each TiKV (if any).
///
/// Top SQL (D14.2) is not recorded on tidb-rs (ROADMAP §3) and client-rust has no RPC
/// interceptor, so this is a no-op returning the context unchanged.
// go: WithSQLKvExecCounterInterceptor L2 (no-op: Top SQL not ported, ROADMAP §3)
pub fn with_sql_kv_exec_counter_interceptor(ctx: &tidb_kv::Context) -> tidb_kv::Context {
    ctx.clone()
}
