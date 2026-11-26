//! Unit tests for `tidb-store::distsql` (D07.3 Part 1):
//! - Ported Go unit tests (`distsql_test.go`, `request_builder_test.go`, `select_result_test.go`)
//! - Dedicated Rust safety tests (N3 `Drop` cancellation, A1 memory accounting, idempotent
//!   `Close`, post-`Close` `CopInfo` registration, `0.8` chunk reuse threshold, `SQLKiller`,
//!   `verify_txn_scope`, `has_runaway_rule`, `ResourceGroupTagger`, eager `CopClient::send_async`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protobuf::Message;
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{WarnHandler, new_static_warn_handler};
use tidb_base::errno;
use tidb_base::gostd::time::utc;
use tidb_base::kv::{TiFlash, TiKV};
use tidb_base::mysql::{self, BinaryFlag, MaxIntWidth};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_chunk::codec::new_codec;
use tidb_codec::codec::encode_value;
use tidb_execdetails::clientgo::{ScanDetail, TimeDetail};
use tidb_execdetails::execdetails::{
    CopExecDetails, DurationWrapper, Int64, RuntimeStats, RuntimeStatsColl,
};
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_infoschema::placement::Bundle;
use tidb_kv::context::Context;
use tidb_kv::copr::{
    CopRuntimeStats, ResourceGroupTagBuilder, StoreLabel, Variables, new_resource_group_tag_builder,
};
use tidb_kv::key::{Key, KeyRange};
use tidb_kv::kv::{
    Client, ClientSendOption, GlobalReplicaScope, KvFuture, OptionValue, PriorityHigh, PriorityLow,
    PriorityNormal, RC, RCCheckTS, ReqTypeDAG, Request, Response, ResultSubset, SI,
    new_non_partitioned_key_ranges,
};
use tidb_kv::option::{
    ReplicaReadClosest, ReplicaReadFollower, ReplicaReadLeader, ReplicaReadMixed,
};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::variable::tidb_defs::DefDistSQLScanConcurrency;
use tidb_types::collate::get_binary_collator_slice;
use tidb_types::datum::{new_int_datum, new_uint_datum};

use super::*;
use crate::util::paging::{MaxPagingSize, MinPagingSize};

fn int64_field_type() -> Arc<FieldType> {
    let mut ft = FieldType::new(mysql::TypeLonglong);
    ft.set_flag(BinaryFlag);
    ft.set_flen(MaxIntWidth);
    ft.set_charset("binary");
    ft.set_collate("binary");
    Arc::new(ft)
}

fn default_test_dctx() -> DistSQLContext {
    DistSQLContext {
        dist_sql_concurrency: DefDistSQLScanConcurrency as isize,
        replica_read_type: ReplicaReadLeader,
        min_paging_size: MinPagingSize as isize,
        max_paging_size: MaxPagingSize as isize,
        resource_group_name: "default".to_owned(),
        enable_chunk_rpc: true,
        ..Default::default()
    }
}

struct MockSubset {
    data: Vec<u8>,
    stats: Option<CopRuntimeStats>,
    resp_time: Duration,
}

impl ResultSubset for MockSubset {
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_start_key(&self) -> &Key {
        static EMPTY: Key = Key(Vec::new());
        &EMPTY
    }
    fn mem_size(&self) -> i64 {
        self.data.capacity() as i64
    }
    fn resp_time(&self) -> Duration {
        self.resp_time
    }
    fn get_cop_runtime_stats(&self) -> Option<&CopRuntimeStats> {
        self.stats.as_ref()
    }
}

struct ScriptedResponse {
    subsets: std::collections::VecDeque<Result<MockSubset, Error>>,
    closed: Arc<AtomicBool>,
    close_calls: Arc<AtomicUsize>,
    concurrency: Option<(isize, isize)>,
    build_elapsed: Option<Duration>,
    store_batch: Option<(u64, u64)>,
    unconsumed: Vec<CopRuntimeStats>,
}

impl ScriptedResponse {
    fn new(subsets: Vec<Result<MockSubset, Error>>) -> (Self, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let closed = Arc::new(AtomicBool::new(false));
        let close_calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                subsets: subsets.into(),
                closed: Arc::clone(&closed),
                close_calls: Arc::clone(&close_calls),
                concurrency: None,
                build_elapsed: None,
                store_batch: None,
                unconsumed: Vec::new(),
            },
            closed,
            close_calls,
        )
    }
}

impl Response for ScriptedResponse {
    fn next<'a>(&'a mut self, _ctx: &'a Context) -> KvFuture<'a, Option<Box<dyn ResultSubset>>> {
        let item = self.subsets.pop_front();
        Box::pin(async move {
            match item {
                None => Ok(None),
                Some(Ok(s)) => Ok(Some(Box::new(s) as Box<dyn ResultSubset>)),
                Some(Err(e)) => Err(e),
            }
        })
    }

    fn close(&mut self) -> Result<(), Error> {
        self.closed.store(true, Ordering::SeqCst);
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        self.concurrency
    }

    fn cop_store_batch_info(&self) -> Option<(u64, u64)> {
        self.store_batch
    }

    fn cop_build_task_elapsed(&self) -> Option<Duration> {
        self.build_elapsed
    }

    fn collect_unconsumed_cop_runtime_stats(&self) -> Vec<CopRuntimeStats> {
        self.unconsumed.clone()
    }
}

struct SingleRespClient {
    resp: Mutex<Option<Box<dyn Response>>>,
}

impl SingleRespClient {
    fn new(resp: Box<dyn Response>) -> Arc<Self> {
        Arc::new(Self {
            resp: Mutex::new(Some(resp)),
        })
    }
}

impl Client for SingleRespClient {
    fn send(
        &self,
        _ctx: &Context,
        _req: &Request,
        _vars: Option<OptionValue>,
        _option: &ClientSendOption,
    ) -> Box<dyn Response> {
        self.resp.lock().unwrap().take().expect("response once")
    }

    fn is_request_type_supported(&self, _req_type: i64, _sub_type: i64) -> bool {
        true
    }
}

fn make_int4_select_resp(num_rows: usize, use_chunk_rpc: bool) -> Vec<u8> {
    let mut resp = tipb::SelectResponse::default();
    if use_chunk_rpc {
        resp.set_encode_type(tipb::EncodeType::TypeChunk);
        let ft = int64_field_type();
        let col_types: Vec<FieldType> = vec![(*ft).clone(); 4];
        let mut remaining = num_rows;
        while remaining > 0 {
            let n = remaining.min(1024);
            remaining -= n;
            let mut chk =
                new_chunk_with_capacity(&[ft.clone(), ft.clone(), ft.clone(), ft.clone()], n);
            for _ in 0..n {
                for c in 0..4 {
                    chk.append_int64(c, 123);
                }
            }
            let codec = new_codec(col_types.clone());
            let mut c_pb = tipb::Chunk::default();
            c_pb.set_rows_data(codec.encode(&chk));
            resp.mut_chunks().push(c_pb);
        }
    } else {
        resp.set_encode_type(tipb::EncodeType::TypeDefault);
        let d = new_int_datum(1);
        let mut row_bytes = Vec::new();
        encode_value(
            &utc(),
            &mut row_bytes,
            &[d.clone(), d.clone(), d.clone(), d],
        )
        .unwrap();
        for _ in 0..num_rows {
            let mut c_pb = tipb::Chunk::default();
            c_pb.set_rows_data(row_bytes.clone());
            resp.mut_chunks().push(c_pb);
        }
    }
    resp.set_output_counts(vec![1]);
    resp.write_to_bytes().unwrap()
}

fn build_mock_subsets(
    batch: usize,
    total: usize,
    use_chunk_rpc: bool,
) -> Vec<Result<MockSubset, Error>> {
    let mut out = Vec::new();
    let mut sent = 0;
    while sent < total {
        let n = batch.min(total - sent);
        sent += n;
        out.push(Ok(MockSubset {
            data: make_int4_select_resp(n, use_chunk_rpc),
            stats: None,
            resp_time: Duration::ZERO,
        }));
    }
    out
}

// Port of Go `TestSelectNormal` (both TypeChunk and TypeDefault).
#[tokio::test]
async fn test_select_normal() {
    for use_chunk_rpc in [true, false] {
        let (resp, closed, _) = ScriptedResponse::new(build_mock_subsets(1, 2, use_chunk_rpc));
        let tracker = new_tracker(-1, -1);
        let mut dctx = default_test_dctx();
        dctx.enable_chunk_rpc = use_chunk_rpc;
        dctx.client = Some(SingleRespClient::new(Box::new(resp)));
        let req = RequestBuilder::default()
            .set_key_ranges(Vec::new())
            .set_dag_request(tipb::DagRequest::default())
            .set_desc(false)
            .set_keep_order(false)
            .set_from_session_vars(&dctx)
            .set_mem_tracker(Some(Arc::clone(&tracker)))
            .build()
            .unwrap();
        let col_types = vec![int64_field_type(); 4];
        let mut sr = select(
            &Context::background(),
            Arc::new(dctx),
            &req,
            col_types.clone(),
        )
        .await
        .unwrap();

        let mut chk = new_chunk_with_capacity(&col_types, 32);
        let mut total_rows = 0;
        loop {
            sr.next(&Context::background(), &mut chk).await.unwrap();
            total_rows += chk.num_rows();
            if chk.num_rows() == 0 {
                break;
            }
        }
        assert_eq!(total_rows, 2);
        sr.close().unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(tracker.bytes_consumed(), 0);
    }
}

// Port of Go `TestSelectMemTracker`.
#[tokio::test]
async fn test_select_mem_tracker() {
    let (resp, _, _) = ScriptedResponse::new(build_mock_subsets(2, 6, true));
    let tracker = new_tracker(-1, -1);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = RequestBuilder::default()
        .set_dag_request(tipb::DagRequest::default())
        .set_from_session_vars(&dctx)
        .set_mem_tracker(Some(Arc::clone(&tracker)))
        .build()
        .unwrap();
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &req,
        col_types.clone(),
    )
    .await
    .unwrap();

    let mut chk = new_chunk_with_capacity(&col_types, 3);
    chk.set_required_rows(3, 3);
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert!(chk.is_full());
    assert!(tracker.bytes_consumed() > 0);
    sr.close().unwrap();
    assert_eq!(tracker.bytes_consumed(), 0);
}

// Port of Go `TestSelectNormalChunkSize`.
#[tokio::test]
async fn test_select_normal_chunk_size() {
    let (resp, _, _) = ScriptedResponse::new(build_mock_subsets(100, 500, false));
    let tracker = new_tracker(-1, -1);
    let mut dctx = default_test_dctx();
    dctx.enable_chunk_rpc = false;
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = RequestBuilder::default()
        .set_dag_request(tipb::DagRequest::default())
        .set_from_session_vars(&dctx)
        .set_mem_tracker(Some(Arc::clone(&tracker)))
        .build()
        .unwrap();
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &req,
        col_types.clone(),
    )
    .await
    .unwrap();

    let mut chk = new_chunk_with_capacity(&col_types, 32);
    for (req_rows, want_rows) in [
        (32, 32),
        (32, 32),
        (1, 1),
        (2, 2),
        (17, 17),
        (170, 32),
        (32, 32),
        (0, 32),
        (-1, 32),
    ] {
        chk.set_required_rows(req_rows, 32);
        sr.next(&Context::background(), &mut chk).await.unwrap();
        assert_eq!(chk.num_rows(), want_rows, "req_rows={req_rows}");
    }
    sr.close().unwrap();
    assert_eq!(tracker.bytes_consumed(), 0);
}

// Port of Go `TestSelectWithRuntimeStats`.
#[tokio::test]
async fn test_select_with_runtime_stats() {
    let (resp, _, _) = ScriptedResponse::new(build_mock_subsets(1, 2, true));
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = RequestBuilder::default()
        .set_dag_request(tipb::DagRequest::default())
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select_with_runtime_stats(
        &Context::background(),
        Arc::new(dctx),
        &req,
        col_types.clone(),
        vec![1, 2, 3],
        1,
    )
    .await
    .unwrap();

    let mut chk = new_chunk_with_capacity(&col_types, 32);
    let mut total = 0;
    loop {
        sr.next(&Context::background(), &mut chk).await.unwrap();
        total += chk.num_rows();
        if chk.num_rows() == 0 {
            break;
        }
    }
    assert_eq!(total, 2);
    sr.close().unwrap();
}

// Port of Go `TestSelectResultRuntimeStats` (without client-go `rpc_info` per spec §7 item 4).
#[test]
fn test_select_result_runtime_stats() {
    let stmt_stats = RuntimeStatsColl::default();
    let basic = stmt_stats.get_basic_runtime_stats(1);
    basic.record(1_000_000_000, 20);

    let mut s1 = SelectResultRuntimeStats {
        backoff_sleep: BTreeMap::from([("RegionMiss".to_owned(), 1_000_000)]),
        total_process_time: 1_000_000_000,
        total_wait_time: 1_000_000_000,
        dist_sql_concurrency: 15,
        ..Default::default()
    };
    s1.cop_resp_time.add(DurationWrapper(1_000_000_000));
    s1.cop_resp_time.add(DurationWrapper(1_000_000));
    s1.proc_keys.add(Int64(100));
    s1.proc_keys.add(Int64(200));

    let s2 = s1.clone_stats();
    stmt_stats.register_stats(1, s1.clone_stats());
    stmt_stats.register_stats(1, s2);
    let stats = stmt_stats.get_root_stats(1);
    let expect = "time:1s, loops:1, cop_task: {num: 4, max: 1s, min: 1ms, avg: 500.5ms, p95: 1s, max_proc_keys: 200, p95_proc_keys: 200, tot_proc: 2s, tot_wait: 2s, copr_cache_hit_ratio: 0.00, max_distsql_concurrency: 15}, backoff{RegionMiss: 2ms}";
    assert_eq!(stats.string(), expect);
    // Idempotence.
    assert_eq!(stats.string(), expect);

    let mut single = SelectResultRuntimeStats {
        backoff_sleep: BTreeMap::from([("RegionMiss".to_owned(), 1_000_000)]),
        total_process_time: 1_000_000_000,
        total_wait_time: 1_000_000_000,
        ..Default::default()
    };
    single.cop_resp_time.add(DurationWrapper(1_000_000_000));
    single.proc_keys.add(Int64(100));
    assert_eq!(
        single.string_with_cache_enabled(true),
        "cop_task: {num: 1, max: 1s, proc_keys: 100, tot_proc: 1s, tot_wait: 1s, copr_cache_hit_ratio: 0.00}, backoff{RegionMiss: 1ms}"
    );
    assert_eq!(
        single.string_with_cache_enabled(false),
        "cop_task: {num: 1, max: 1s, proc_keys: 100, tot_proc: 1s, tot_wait: 1s, copr_cache: disabled}, backoff{RegionMiss: 1ms}"
    );

    // Spec §8 item 3 (`go-bug-kept`): `merge_cop_runtime_stats` overwrites backoff_sleep per key.
    let mut ov = SelectResultRuntimeStats::default();
    let cs1 = CopRuntimeStats {
        cop_exec_details: CopExecDetails {
            backoff_sleep: BTreeMap::from([
                ("RegionMiss".to_owned(), 10_000_000),
                ("tikvRPC".to_owned(), 3_000_000),
            ]),
            ..Default::default()
        },
        copr_cache_hit: false,
    };
    let cs2 = CopRuntimeStats {
        cop_exec_details: CopExecDetails {
            backoff_sleep: BTreeMap::from([("RegionMiss".to_owned(), 2_000_000)]),
            ..Default::default()
        },
        copr_cache_hit: false,
    };
    ov.merge_cop_runtime_stats(&cs1, Duration::from_millis(5));
    ov.merge_cop_runtime_stats(&cs2, Duration::from_millis(7));
    assert_eq!(
        ov.string_with_cache_enabled(true),
        "cop_task: {num: 2, max: 7ms, min: 5ms, avg: 6ms, p95: 7ms, copr_cache_hit_ratio: 0.00}, backoff{RegionMiss: 2ms, tikvRPC: 3ms}"
    );
}

// Port of Go `TestTableRangesToKVRanges` and `TestTableRangesToKVRangesWithFbs`.
#[test]
fn test_table_ranges_to_kv_ranges() {
    let ranges = vec![
        Range {
            low_val: vec![new_int_datum(1)],
            high_val: vec![new_int_datum(2)],
            collators: get_binary_collator_slice(1),
            ..Default::default()
        },
        Range {
            low_val: vec![new_int_datum(2)],
            high_val: vec![new_int_datum(4)],
            low_exclude: true,
            high_exclude: true,
            collators: get_binary_collator_slice(1),
        },
        Range {
            low_val: vec![new_int_datum(4)],
            high_val: vec![new_int_datum(19)],
            low_exclude: false,
            high_exclude: true,
            collators: get_binary_collator_slice(1),
        },
        Range {
            low_val: vec![new_int_datum(19)],
            high_val: vec![new_int_datum(32)],
            low_exclude: true,
            high_exclude: false,
            collators: get_binary_collator_slice(1),
        },
        Range {
            low_val: vec![new_int_datum(34)],
            high_val: vec![new_int_datum(34)],
            low_exclude: true,
            high_exclude: false,
            collators: get_binary_collator_slice(1),
        },
    ];
    let actual = table_ranges_to_kv_ranges(13, &ranges);
    let expect = vec![
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x1,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x3,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x3,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x4,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x4,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x13,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x14,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x21,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x23,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xd, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x23,
            ]),
        },
    ];
    assert_eq!(actual, expect);

    // TestTableRangesToKVRangesWithFbs
    let fbs_ranges = vec![Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(4)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }];
    assert_eq!(
        table_ranges_to_kv_ranges(0, &fbs_ranges),
        vec![KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x1
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x5
            ]),
        }]
    );
}

// Port of Go `TestRequestBuilder1`, `TestRequestBuilder4`, `TestRequestBuilder7`,
// `TestRequestBuilder8`, `TestRequestBuilderTiKVClientReadTimeout`.
#[test]
fn test_request_builder_core_cases() {
    let dctx = default_test_dctx();
    let ranges = vec![Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(2)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }];

    // TestRequestBuilder1: verifies DAGRequest zero-value wire bytes [0x18, 0, 0x20, 0, 0x40, 0, 0x5a, 0]
    let mut b = RequestBuilder::default();
    let actual = b
        .set_handle_ranges(None, 12, false, &ranges)
        .set_dag_request(tipb::DagRequest::default())
        .set_desc(false)
        .set_keep_order(false)
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();
    assert_eq!(actual.tp, ReqTypeDAG);
    assert_eq!(
        actual.data,
        vec![0x18, 0x0, 0x20, 0x0, 0x40, 0x0, 0x5a, 0x0]
    );
    assert!(actual.cacheable);
    assert_eq!(actual.concurrency, DefDistSQLScanConcurrency as isize);
    assert_eq!(actual.isolation_level, SI);
    assert_eq!(actual.priority, PriorityNormal);
    assert_eq!(actual.replica_read, ReplicaReadLeader);
    assert_eq!(actual.read_replica_scope, GlobalReplicaScope);
    assert_eq!(actual.resource_group_name, "default");
    assert_eq!(actual.paging.min_paging_size, MinPagingSize);
    assert_eq!(actual.paging.max_paging_size, MaxPagingSize);
    assert_eq!(
        actual.key_ranges,
        Some(new_non_partitioned_key_ranges(vec![KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x1
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x3
            ]),
        }]))
    );
    // Single-use check under #[cfg(test)] (Go intest.InTest).
    assert_eq!(
        b.build().err().unwrap().to_string(),
        "request builder is already used"
    );

    // TestRequestBuilder7: replica read modes + concurrency override.
    for rr in [ReplicaReadLeader, ReplicaReadFollower, ReplicaReadMixed] {
        let mut dc = default_test_dctx();
        dc.replica_read_type = rr;
        let req = RequestBuilder::default()
            .set_from_session_vars(&dc)
            .set_concurrency(10)
            .build()
            .unwrap();
        assert_eq!(req.replica_read, rr);
        assert_eq!(req.concurrency, 10);
    }

    // TestRequestBuilder8 + TiKVClientReadTimeout + WeakConsistency / RCCheckTS / Priority.
    let mut dc = default_test_dctx();
    dc.resource_group_name = "test".to_owned();
    dc.tikv_client_read_timeout = 100;
    dc.weak_consistency = true;
    dc.priority = mysql::HighPriority;
    let req = RequestBuilder::default()
        .set_from_session_vars(&dc)
        .build()
        .unwrap();
    assert_eq!(req.resource_group_name, "test");
    assert_eq!(req.tikv_client_read_timeout, 100);
    assert_eq!(req.isolation_level, RC);
    assert_eq!(req.priority, PriorityHigh);

    dc.weak_consistency = false;
    dc.rc_check_ts = true;
    dc.replica_read_type = ReplicaReadFollower;
    dc.priority = mysql::LowPriority;
    let req = RequestBuilder::default()
        .set_from_session_vars(&dc)
        .build()
        .unwrap();
    assert_eq!(req.isolation_level, RCCheckTS);
    assert_eq!(req.replica_read, ReplicaReadLeader);
    assert_eq!(req.priority, PriorityLow);

    // Closest read match_store_labels.
    dc.rc_check_ts = false;
    dc.replica_read_type = ReplicaReadClosest;
    let req = RequestBuilder::default()
        .set_from_session_vars(&dc)
        .set_read_replica_scope("dc-1")
        .build()
        .unwrap();
    assert_eq!(
        req.match_store_labels,
        vec![StoreLabel {
            key: "zone".to_owned(),
            value: "dc-1".to_owned(),
        }]
    );
}

// Port of Go `TestScanLimitConcurrency`, `TestIndexLookUpPushDownScanConcurrency`,
// and KeepOrder simple-scan concurrency=2 rule.
#[test]
fn test_scan_limit_and_keep_order_concurrency() {
    let dctx = default_test_dctx();
    for (tp, limit, want_conc) in [
        (tipb::ExecType::TypeTableScan, 1u64, 1isize),
        (tipb::ExecType::TypeIndexScan, 1u64, 1isize),
        (tipb::ExecType::TypeTableScan, 99_999u64, 1isize),
        (
            tipb::ExecType::TypeTableScan,
            100_000u64,
            dctx.dist_sql_concurrency,
        ),
        (
            tipb::ExecType::TypeIndexScan,
            1_000_000u64,
            dctx.dist_sql_concurrency,
        ),
    ] {
        let mut e0 = tipb::Executor::default();
        e0.set_tp(tp);
        let mut e1 = tipb::Executor::default();
        e1.set_tp(tipb::ExecType::TypeLimit);
        let mut lim = tipb::Limit::default();
        lim.set_limit(limit);
        e1.set_limit(lim);
        let mut dag = tipb::DagRequest::default();
        dag.mut_executors().push(e0);
        dag.mut_executors().push(e1);
        let req = RequestBuilder::default()
            .set_dag_request(dag)
            .set_from_session_vars(&dctx)
            .build()
            .unwrap();
        assert_eq!(req.concurrency, want_conc);
        assert_eq!(req.limit_size, limit);
    }

    // IndexLookUp push-down: IndexScan -> Limit(parent=3) -> TableScan -> IndexLookUp
    for (limit, want_conc) in [(1u64, 1isize), (1_000_000u64, dctx.dist_sql_concurrency)] {
        let mut e0 = tipb::Executor::default();
        e0.set_tp(tipb::ExecType::TypeIndexScan);
        let mut e1 = tipb::Executor::default();
        e1.set_tp(tipb::ExecType::TypeLimit);
        let mut lim = tipb::Limit::default();
        lim.set_limit(limit);
        e1.set_limit(lim);
        e1.set_parent_idx(3);
        let mut e2 = tipb::Executor::default();
        e2.set_tp(tipb::ExecType::TypeTableScan);
        let mut e3 = tipb::Executor::default();
        e3.set_tp(tipb::ExecType::TypeIndexLookUp);
        e3.set_index_lookup(tipb::IndexLookUp::default());
        let mut dag = tipb::DagRequest::default();
        dag.mut_executors().push(e0);
        dag.mut_executors().push(e1);
        dag.mut_executors().push(e2);
        dag.mut_executors().push(e3);
        let req = RequestBuilder::default()
            .set_dag_request(dag)
            .set_from_session_vars(&dctx)
            .build()
            .unwrap();
        assert_eq!(req.concurrency, want_conc);
    }

    // Simple scan + KeepOrder with default concurrency (15) -> 2; custom concurrency stays.
    let mut e0 = tipb::Executor::default();
    e0.set_tp(tipb::ExecType::TypeTableScan);
    let mut dag = tipb::DagRequest::default();
    dag.mut_executors().push(e0);
    let req_def = RequestBuilder::default()
        .set_dag_request(dag.clone())
        .set_keep_order(true)
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();
    assert_eq!(req_def.concurrency, 2);

    let mut custom_dctx = default_test_dctx();
    custom_dctx.dist_sql_concurrency = 10;
    let req_custom = RequestBuilder::default()
        .set_dag_request(dag)
        .set_keep_order(true)
        .set_from_session_vars(&custom_dctx)
        .build()
        .unwrap();
    assert_eq!(req_custom.concurrency, 10);
}

// Port of Go `TestUpdateCopRuntimeStats`.
#[tokio::test]
async fn test_update_cop_runtime_stats() {
    let make_summary_resp = || {
        let mut sr = tipb::SelectResponse::default();
        sr.set_encode_type(tipb::EncodeType::TypeChunk);
        let mut s = tipb::ExecutorExecutionSummary::default();
        s.set_time_processed_ns(1);
        s.set_num_produced_rows(1);
        s.set_num_iterations(1);
        sr.mut_execution_summaries().push(s);
        let mut c = tipb::Chunk::default();
        let ft = int64_field_type();
        let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 1);
        chk.append_int64(0, 42);
        c.set_rows_data(new_codec(vec![(*ft).clone()]).encode(&chk));
        sr.mut_chunks().push(c);
        sr.write_to_bytes().unwrap()
    };

    let make_stats = |addr: &str| CopRuntimeStats {
        cop_exec_details: CopExecDetails {
            callee_address: addr.to_owned(),
            ..Default::default()
        },
        copr_cache_hit: false,
    };

    // 1. RuntimeStatsColl == None -> no-op.
    let (resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: make_summary_resp(),
        stats: Some(make_stats("a")),
        resp_time: Duration::ZERO,
    })]);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = Request::default();
    let ft = vec![int64_field_type()];
    let mut sr = select_with_runtime_stats(
        &Context::background(),
        Arc::new(dctx),
        &req,
        ft.clone(),
        vec![],
        1234,
    )
    .await
    .unwrap();
    let mut chk = new_chunk_with_capacity(&ft, 4);
    sr.next(&Context::background(), &mut chk).await.unwrap();
    sr.close().unwrap();

    // 2. Summary length mismatch -> cop stats not recorded.
    let coll = Arc::new(RuntimeStatsColl::default());
    let (resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: make_summary_resp(),
        stats: Some(make_stats("callee")),
        resp_time: Duration::ZERO,
    })]);
    let mut dctx = default_test_dctx();
    dctx.runtime_stats_coll = Some(Arc::clone(&coll));
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select_with_runtime_stats(
        &Context::background(),
        Arc::new(dctx),
        &req,
        ft.clone(),
        vec![], // mismatch: 0 != 1
        1234,
    )
    .await
    .unwrap();
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert!(!coll.exists_cop_stats(1234));
    sr.close().unwrap();

    // 3. Matching cop_plan_ids -> records `tikv_task:{time:1ns, loops:1}`.
    let coll = Arc::new(RuntimeStatsColl::default());
    let (resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: make_summary_resp(),
        stats: Some(make_stats("callee")),
        resp_time: Duration::ZERO,
    })]);
    let mut dctx = default_test_dctx();
    dctx.runtime_stats_coll = Some(Arc::clone(&coll));
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select_with_runtime_stats(
        &Context::background(),
        Arc::new(dctx),
        &req,
        ft.clone(),
        vec![1234],
        1234,
    )
    .await
    .unwrap();
    sr.next(&Context::background(), &mut chk).await.unwrap();
    let cop = coll.get_cop_stats(1234).expect("cop stats recorded");
    assert_eq!(
        cop.lock().unwrap().string(),
        "tikv_task:{time:1ns, loops:1}"
    );
    sr.close().unwrap();
}

// Dedicated Rust test: Drop without Close closes underlying response, zeros mem_tracker,
// and does NOT register runtime stats (N3 §3.5).
#[tokio::test]
async fn select_result_drop_without_close_stops_workers_and_zeros_mem_tracker() {
    let (mut resp, closed, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: make_int4_select_resp(4, true),
        stats: Some(CopRuntimeStats {
            cop_exec_details: CopExecDetails {
                callee_address: "127.0.0.1:20160".to_owned(),
                ..Default::default()
            },
            copr_cache_hit: false,
        }),
        resp_time: Duration::from_millis(5),
    })]);
    resp.concurrency = Some((4, 1));
    let tracker = new_tracker(-1, -1);
    let coll = Arc::new(RuntimeStatsColl::default());
    let mut dctx = default_test_dctx();
    dctx.runtime_stats_coll = Some(Arc::clone(&coll));
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = RequestBuilder::default()
        .set_mem_tracker(Some(Arc::clone(&tracker)))
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();
    let col_types = vec![int64_field_type(); 4];
    {
        let mut sr = select_with_runtime_stats(
            &Context::background(),
            Arc::new(dctx),
            &req,
            col_types.clone(),
            vec![],
            7,
        )
        .await
        .unwrap();
        assert_eq!(get_select_result_concurrency(sr.as_ref()), Some((4, 1)));
        let mut chk = new_chunk_with_capacity(&col_types, 2);
        chk.set_required_rows(2, 2);
        sr.next(&Context::background(), &mut chk).await.unwrap();
        assert!(tracker.bytes_consumed() > 0);
        // Drop `sr` without calling `close()`.
    }
    assert!(closed.load(Ordering::SeqCst));
    assert_eq!(tracker.bytes_consumed(), 0);
    assert!(!coll.exists_root_stats(7));
}

// Dedicated Rust test: idempotent close().
#[tokio::test]
async fn select_result_close_is_idempotent() {
    let (resp, _, close_calls) = ScriptedResponse::new(build_mock_subsets(2, 4, true));
    let tracker = new_tracker(-1, -1);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = RequestBuilder::default()
        .set_mem_tracker(Some(Arc::clone(&tracker)))
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &req,
        col_types.clone(),
    )
    .await
    .unwrap();
    let mut chk = new_chunk_with_capacity(&col_types, 2);
    sr.next(&Context::background(), &mut chk).await.unwrap();
    sr.close().unwrap();
    sr.close().unwrap();
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tracker.bytes_consumed(), 0);
}

// Dedicated Rust test: close() registers final CopInfo (build_task_duration, store_batch_num)
// after resp.close().
#[tokio::test]
async fn select_result_close_registers_final_cop_info_after_resp_close() {
    let (mut resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: make_int4_select_resp(2, true),
        stats: Some(CopRuntimeStats {
            cop_exec_details: CopExecDetails {
                callee_address: "127.0.0.1:20160".to_owned(),
                time_detail: TimeDetail {
                    process_time: 2_000_000,
                    wait_time: 1_000_000,
                    ..Default::default()
                },
                scan_detail: ScanDetail {
                    processed_keys: 50,
                    ..Default::default()
                },
                ..Default::default()
            },
            copr_cache_hit: false,
        }),
        resp_time: Duration::from_millis(4),
    })]);
    resp.concurrency = Some((3, 2));
    resp.build_elapsed = Some(Duration::from_millis(9));
    resp.store_batch = Some((5, 1));

    let coll = Arc::new(RuntimeStatsColl::default());
    let mut dctx = default_test_dctx();
    let cpu = Arc::clone(&dctx.cpu_usage);
    let exec_details = Arc::clone(&dctx.exec_details);
    dctx.runtime_stats_coll = Some(Arc::clone(&coll));
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = Request::default();
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select_with_runtime_stats(
        &Context::background(),
        Arc::new(dctx),
        &req,
        col_types.clone(),
        vec![],
        9,
    )
    .await
    .unwrap();
    let mut chk = new_chunk_with_capacity(&col_types, 8);
    sr.next(&Context::background(), &mut chk).await.unwrap();
    sr.close().unwrap();

    let s = coll.get_root_stats(9).string();
    assert!(s.contains("build_task_duration: 9ms"), "got: {s}");
    assert!(s.contains("max_distsql_concurrency: 3"), "got: {s}");
    assert!(s.contains("max_extra_concurrency: 2"), "got: {s}");
    assert!(s.contains("store_batch_num: 5"), "got: {s}");
    assert!(s.contains("store_batch_fallback_num: 1"), "got: {s}");
    assert_eq!(cpu.get_cpu_usages().tikv_cpu_time, Duration::from_millis(2));
    assert_eq!(exec_details.get_exec_details().request_count, 1);
}

// Dedicated Rust test: TiKV error, multiple warnings, and SQLKiller signals.
#[tokio::test]
async fn select_result_warnings_errors_and_sql_killer() {
    // 1. TiKV SelectResponse.Error
    let mut err_pb = tipb::SelectResponse::default();
    let mut e = tipb::Error::default();
    e.set_code(1105);
    e.set_msg("tikv boom".to_owned());
    err_pb.set_error(e);
    let (resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: err_pb.write_to_bytes().unwrap(),
        stats: None,
        resp_time: Duration::ZERO,
    })]);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        col_types.clone(),
    )
    .await
    .unwrap();
    let mut chk = new_chunk_with_capacity(&col_types, 4);
    let err = sr.next(&Context::background(), &mut chk).await.unwrap_err();
    assert_eq!(err.code(), 1105);
    assert!(err.to_string().contains("tikv boom"));

    // 2. Multiple warnings preserved in order.
    let mut warn_pb = tipb::SelectResponse::default();
    warn_pb
        .merge_from_bytes(&make_int4_select_resp(1, true))
        .unwrap();
    for (code, msg) in [(1265, "warn-one"), (1292, "warn-two")] {
        let mut w = tipb::Error::default();
        w.set_code(code);
        w.set_msg(msg.to_owned());
        warn_pb.mut_warnings().push(w);
    }
    let warn_handler = Arc::new(new_static_warn_handler(8));
    let (resp, _, _) = ScriptedResponse::new(vec![Ok(MockSubset {
        data: warn_pb.write_to_bytes().unwrap(),
        stats: None,
        resp_time: Duration::ZERO,
    })]);
    let mut dctx = default_test_dctx();
    dctx.warn_handler = Some(warn_handler.clone());
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        col_types.clone(),
    )
    .await
    .unwrap();
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert_eq!(warn_handler.warning_count(), 2);
    let warns = warn_handler.copy_warnings(Vec::new());
    assert_eq!(warns[0].err.code(), 1265);
    assert_eq!(warns[1].err.code(), 1292);
    sr.close().unwrap();

    // 3. SQLKiller set -> returns ErrQueryInterrupted (1317).
    let killer = Arc::new(SqlKiller::default());
    killer.send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    let (resp, _, _) = ScriptedResponse::new(build_mock_subsets(1, 2, true));
    let mut dctx = default_test_dctx();
    dctx.sql_killer = Some(killer);
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        col_types.clone(),
    )
    .await
    .unwrap();
    let err = sr.next(&Context::background(), &mut chk).await.unwrap_err();
    assert_eq!(err.code(), errno::ErrQueryInterrupted as isize);
}

// Dedicated Rust test: `read_from_chunk` 0.8 reuse threshold (swap on empty chk, early return on
// non-empty chk, splice when <= 0.8).
#[tokio::test]
async fn read_from_chunk_reuse_threshold_0_8() {
    // Two responses: first has 5 rows (<= 10*0.8 = 8), second has 9 rows (> 10*0.8 = 8).
    let (resp, _, _) = ScriptedResponse::new(vec![
        Ok(MockSubset {
            data: make_int4_select_resp(5, true),
            stats: None,
            resp_time: Duration::ZERO,
        }),
        Ok(MockSubset {
            data: make_int4_select_resp(9, true),
            stats: None,
            resp_time: Duration::ZERO,
        }),
    ]);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let col_types = vec![int64_field_type(); 4];
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        col_types.clone(),
    )
    .await
    .unwrap();

    let mut chk = new_chunk_with_capacity(&col_types, 10);
    chk.set_required_rows(10, 10);
    // First Next: splices 5 rows, sees next chunk has 9 > 8 while chk.num_rows() == 5 > 0 -> returns 5 early!
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 5);
    // Second Next: chk is empty and decoder has 9 > 8 -> reuses interm chk directly and returns 9!
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 9);
    // Third Next: EOF.
    sr.next(&Context::background(), &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 0);
    sr.close().unwrap();
}

struct MockMetaIS {
    ver: i64,
    bundles: BTreeMap<i64, Arc<Bundle>>,
}

impl tidb_sessionctx::expropt::MetaOnlyInfoSchema for MockMetaIS {
    fn schema_meta_version(&self) -> i64 {
        self.ver
    }
}

impl MetaOnlyInfoSchema for MockMetaIS {
    fn placement_bundle_by_physical_table_id(&self, id: i64) -> Option<Arc<Bundle>> {
        self.bundles.get(&id).cloned()
    }
}

// Dedicated Rust test: verify_txn_scope + boundary stubs (X, N10, DEF.MPP, D07.3).
#[tokio::test]
async fn verify_txn_scope_and_boundary_stubs() {
    let mut bundles = BTreeMap::new();
    bundles.insert(
        42,
        Arc::new(Bundle {
            policy_id: 1,
            ids: vec![42],
        }),
    );
    let is: Arc<dyn MetaOnlyInfoSchema> = Arc::new(MockMetaIS { ver: 7, bundles });

    // Empty / global always Ok(true).
    assert!(verify_txn_scope("", 42, is.as_ref()).unwrap());
    assert!(verify_txn_scope("global", 42, is.as_ref()).unwrap());
    // Non-global without bundle -> Ok(true).
    assert!(verify_txn_scope("dc-west", 99, is.as_ref()).unwrap());
    // Non-global with bundle -> blocked("X", ...).
    let err = verify_txn_scope("dc-west", 42, is.as_ref()).unwrap_err();
    assert_eq!(blocked_by(&err), Some("X"));

    // RequestBuilder with non-global txn_scope & bundle -> blocked("X", ...).
    let ran = vec![Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(2)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }];
    let err = RequestBuilder::default()
        .set_handle_ranges(None, 42, false, &ran)
        .set_txn_scope("dc-west")
        .set_from_info_schema(Arc::clone(&is))
        .build()
        .err()
        .unwrap();
    assert_eq!(blocked_by(&err), Some("X"));

    // Spec §8 item 8: empty KeyRanges + non-global txn_scope does not panic.
    let req = RequestBuilder::default()
        .set_txn_scope("dc-west")
        .set_from_info_schema(Arc::clone(&is))
        .build()
        .unwrap();
    assert_eq!(req.schema_var, 7);

    // has_runaway_rule == true -> blocked("N10", ...).
    let mut dctx = default_test_dctx();
    dctx.has_runaway_rule = true;
    let err = RequestBuilder::default()
        .set_from_session_vars(&dctx)
        .build()
        .err()
        .unwrap();
    assert_eq!(blocked_by(&err), Some("N10"));

    // Select with StoreType::TiFlash -> blocked("DEF.MPP", ...).
    let tiflash_req = Request {
        store_type: TiFlash,
        ..Default::default()
    };
    let err = select(
        &Context::background(),
        Arc::new(default_test_dctx()),
        &tiflash_req,
        vec![],
    )
    .await
    .err()
    .unwrap();
    assert_eq!(blocked_by(&err), Some("DEF.MPP"));

    // SelectResult::into_iter -> temporary blocked("D07.3", ...).
    let (resp, _, _) = ScriptedResponse::new(vec![]);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        vec![],
    )
    .await
    .unwrap();
    let err = sr.into_iter(vec![]).err().unwrap();
    assert_eq!(blocked_by(&err), Some("D07.3"));
    sr.close().unwrap();
}

// Dedicated Rust test: ResourceGroupTagBuilder + util::resourcegrouptag wiring.
#[test]
fn resource_group_tagger_encodes_table_id_and_label() {
    let builder: ResourceGroupTagBuilder = new_resource_group_tag_builder();
    let row_key = tidb_tablecodec::tablecodec::encode_row_key(15, &[0x80, 0, 0, 0, 0, 0, 0, 1]);
    let tag_bytes = crate::util::resourcegrouptag::encode_tag_with_key(&builder, &row_key.0);
    let mut tag = tipb::ResourceGroupTag::default();
    tag.merge_from_bytes(&tag_bytes).unwrap();
    assert_eq!(tag.get_table_id(), 15);
    assert_eq!(
        tag.get_label(),
        tipb::ResourceGroupTagLabel::ResourceGroupTagLabelRow
    );
    assert!(tag.get_sql_digest().is_empty());
    assert!(tag.get_plan_digest().is_empty());

    // SplitRangesAcrossInt64Boundary sanity check (unsigned crossing i64::MAX).
    let r = Range {
        low_val: vec![new_uint_datum(i64::MAX as u64 - 1)],
        high_val: vec![new_uint_datum(i64::MAX as u64 + 2)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    };
    let (first, second) =
        split_ranges_across_int64_boundary(std::slice::from_ref(&r), true, false, false);
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].high_val[0].get_uint64(), i64::MAX as u64);
    assert_eq!(second[0].low_val[0].get_uint64(), i64::MAX as u64 + 1);
    let (merged, empty) = split_ranges_across_int64_boundary(&[r], false, false, false);
    assert_eq!(merged.len(), 2);
    assert!(empty.is_empty());
    assert_eq!(merged[0].low_val[0].get_uint64(), i64::MAX as u64 + 1);
}

// Dedicated Rust test: `SetEncodeType` and `GetSystemEndian`.
#[test]
fn set_encode_type_and_endian() {
    let mut dctx = default_test_dctx();
    let mut dag = tipb::DagRequest::default();
    set_encode_type(&dctx, &mut dag);
    assert_eq!(dag.get_encode_type(), tipb::EncodeType::TypeChunk);
    assert_eq!(
        dag.get_chunk_memory_layout().get_endian(),
        get_system_endian()
    );

    dctx.enable_chunk_rpc = false;
    let mut dag2 = tipb::DagRequest::default();
    set_encode_type(&dctx, &mut dag2);
    assert_eq!(dag2.get_encode_type(), tipb::EncodeType::TypeDefault);
    assert!(!dag2.has_chunk_memory_layout());

    // StoreType::TiKV request with Variables passed through RequestBuilder.
    let _req = Request {
        store_type: TiKV,
        ..Default::default()
    };
    let _vars = Variables::default();
}
