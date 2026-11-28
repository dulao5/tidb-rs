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

    // SelectResult::into_iter now returns a live SelectResultIter (D07.3 Part 2).
    let (resp, closed, _) = ScriptedResponse::new(vec![]);
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
    let mut iter = sr.into_iter(vec![]).unwrap();
    assert!(iter.next(&Context::background()).await.unwrap().is_none());
    iter.close().unwrap();
    assert!(closed.load(Ordering::SeqCst));
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

// -----------------------------------------------------------------------------
// D07.3 Part 2 tests: ported Go unit tests + dedicated Rust safety tests
// -----------------------------------------------------------------------------

fn expected_handle_ranges(tid: i64, hrs: &[(i64, i64)]) -> Vec<KeyRange> {
    hrs.iter()
        .map(|&(start, end)| {
            let mut low = Vec::with_capacity(8);
            tidb_codec::number::encode_int(&mut low, start);
            let mut high = Vec::with_capacity(8);
            tidb_codec::number::encode_int(&mut high, end);
            high = Key(high).prefix_next().0;
            KeyRange {
                start_key: tidb_tablecodec::tablecodec::encode_row_key(tid, &low),
                end_key: tidb_tablecodec::tablecodec::encode_row_key(tid, &high),
            }
        })
        .collect()
}

// Port of Go TestTableHandlesToKVRanges (request_builder_test.go:46)
#[test]
fn test_table_handles_to_kv_ranges() {
    use tidb_kv::key::{Handle, IntHandle};

    let handles: Vec<Handle> = [
        0,
        2,
        3,
        4,
        5,
        10,
        11,
        100,
        9223372036854775806,
        9223372036854775807,
    ]
    .into_iter()
    .map(|v| Handle::Int(IntHandle(v)))
    .collect();

    let expect = expected_handle_ranges(
        1,
        &[
            (0, 0),
            (2, 5),
            (10, 11),
            (100, 100),
            (9223372036854775806, 9223372036854775807),
        ],
    );
    let (actual, hints) = table_handles_to_kv_ranges(1, &handles);
    assert_eq!(actual, expect);
    assert_eq!(hints, vec![1, 4, 2, 1, 2]);
}

// Port of Go TestTablePartitionHandlesToKVRanges (request_builder_test.go:79)
#[test]
fn test_table_partition_handles_to_kv_ranges() {
    use tidb_kv::key::{Handle, IntHandle, new_partition_handle};

    let handles: Vec<Handle> = [
        (1, 0),
        (2, 2),
        (2, 3),
        (2, 4),
        (3, 5),
        (1, 10),
        (2, 11),
        (3, 100),
        (1, 9223372036854775806),
        (1, 9223372036854775807),
    ]
    .into_iter()
    .map(|(pid, v)| Handle::Partition(new_partition_handle(pid, Handle::Int(IntHandle(v)))))
    .collect();

    let mut expect = Vec::new();
    expect.extend(expected_handle_ranges(1, &[(0, 0)]));
    expect.extend(expected_handle_ranges(2, &[(2, 4)]));
    expect.extend(expected_handle_ranges(3, &[(5, 5)]));
    expect.extend(expected_handle_ranges(1, &[(10, 10)]));
    expect.extend(expected_handle_ranges(2, &[(11, 11)]));
    expect.extend(expected_handle_ranges(3, &[(100, 100)]));
    expect.extend(expected_handle_ranges(
        1,
        &[(9223372036854775806, 9223372036854775807)],
    ));

    let (actual, hints) = table_handles_to_kv_ranges(0, &handles);
    assert_eq!(actual, expect);
    assert_eq!(hints, vec![1, 3, 1, 1, 1, 1, 2]);

    // PartitionHandlesToKVRanges produces identical ranges and hints on PartitionHandles.
    let (actual_p, hints_p) = partition_handles_to_kv_ranges(&handles);
    assert_eq!(actual_p, expect);
    assert_eq!(hints_p, vec![1, 3, 1, 1, 1, 1, 2]);
}

// Common handles in TableHandlesToKVRanges & PartitionHandlesToKVRanges (including spec §8 item 6).
#[test]
fn test_common_handles_to_kv_ranges() {
    use tidb_kv::key::{Handle, new_common_handle, new_partition_handle};

    let mut enc1 = Vec::new();
    tidb_codec::codec::encode_key(&utc(), &mut enc1, &[new_int_datum(7)]).unwrap();
    let mut enc2 = Vec::new();
    tidb_codec::codec::encode_key(&utc(), &mut enc2, &[new_int_datum(99)]).unwrap();

    let ch1 = Arc::new(new_common_handle(enc1).unwrap());
    let ch2 = Arc::new(new_common_handle(enc2).unwrap());

    let (krs, hints) = table_handles_to_kv_ranges(
        11,
        &[
            Handle::Common(Arc::clone(&ch1)),
            Handle::Common(Arc::clone(&ch2)),
        ],
    );
    assert_eq!(hints, vec![1, 1]);
    assert_eq!(krs.len(), 2);
    assert_eq!(
        krs[0].start_key,
        tidb_tablecodec::tablecodec::encode_row_key(11, ch1.encoded())
    );
    assert_eq!(
        krs[0].end_key,
        tidb_tablecodec::tablecodec::encode_row_key(11, &Key::from(ch1.encoded()).next())
    );

    let phs = vec![
        Handle::Partition(new_partition_handle(5, Handle::Common(Arc::clone(&ch1)))),
        Handle::Partition(new_partition_handle(6, Handle::Common(Arc::clone(&ch2)))),
    ];
    let (pkrs, phints) = partition_handles_to_kv_ranges(&phs);
    assert_eq!(phints, vec![1, 1]);
    assert_eq!(
        pkrs[0].start_key,
        tidb_tablecodec::tablecodec::encode_row_key(5, ch1.encoded())
    );
    assert_eq!(
        pkrs[0].end_key,
        tidb_tablecodec::tablecodec::encode_row_key(5, &Key::from(ch1.encoded()).next())
    );
    assert_eq!(
        pkrs[1].start_key,
        tidb_tablecodec::tablecodec::encode_row_key(6, ch2.encoded())
    );

    let req = RequestBuilder::default()
        .set_partitions_and_handles(&phs)
        .build()
        .unwrap();
    let req_krs = req.key_ranges.unwrap();
    assert!(!req_krs.is_partitioned());
    assert_eq!(req_krs.first_partition_range(), pkrs.as_slice());
}

// Port of Go TestIndexRangesToKVRanges & TestIndexRangesToKVRangesWithFbs (request_builder_test.go:185, 702)
#[test]
fn test_index_ranges_to_kv_ranges() {
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
            high_exclude: true,
            collators: get_binary_collator_slice(1),
            ..Default::default()
        },
        Range {
            low_val: vec![new_int_datum(19)],
            high_val: vec![new_int_datum(32)],
            low_exclude: true,
            collators: get_binary_collator_slice(1),
            ..Default::default()
        },
        Range {
            low_val: vec![new_int_datum(34)],
            high_val: vec![new_int_datum(34)],
            low_exclude: true,
            collators: get_binary_collator_slice(1),
            ..Default::default()
        },
    ];
    let expect = vec![
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x1,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x3,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x3,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x4,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x4,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x13,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x14,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x21,
            ]),
        },
        KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x23,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xc, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x3,
                0x80, 0, 0, 0, 0, 0, 0, 0x23,
            ]),
        },
    ];
    let dctx = default_test_dctx();
    let actual = index_ranges_to_kv_ranges(Some(&dctx), 12, 15, &ranges).unwrap();
    assert!(!actual.is_partitioned());
    assert_eq!(actual.first_partition_range(), expect.as_slice());

    // TestIndexRangesToKVRangesWithFbs (tid=0, idxID=0)
    let fbs_ranges = vec![Range {
        low_val: vec![new_int_datum(1)],
        high_val: vec![new_int_datum(4)],
        collators: get_binary_collator_slice(1),
        ..Default::default()
    }];
    let actual_fbs = index_ranges_to_kv_ranges(Some(&dctx), 0, 0, &fbs_ranges).unwrap();
    assert_eq!(
        actual_fbs.first_partition_range(),
        &[KeyRange {
            start_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x3, 0x80,
                0, 0, 0, 0, 0, 0, 0x1,
            ]),
            end_key: Key(vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x5f, 0x69, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x3, 0x80,
                0, 0, 0, 0, 0, 0, 0x5,
            ]),
        }]
    );
}

// Interrupt signal & mem_tracker accounting / kill signal in IndexRangesToKVRangesWithInterruptSignal.
#[test]
fn test_index_ranges_interrupt_signal_and_mem_tracker() {
    let dctx = default_test_dctx();
    let ranges: Vec<Range> = (0..12)
        .map(|i| Range {
            low_val: vec![new_int_datum(i)],
            high_val: vec![new_int_datum(i + 1)],
            collators: get_binary_collator_slice(1),
            ..Default::default()
        })
        .collect();

    // Interrupt signal set -> returns empty non-partitioned KeyRanges.
    let sig = AtomicBool::new(true);
    let krs = index_ranges_to_kv_ranges_with_interrupt_signal(
        Some(&dctx),
        12,
        15,
        &ranges,
        None,
        Some(&sig),
    )
    .unwrap();
    assert!(!krs.is_partitioned());
    assert_eq!(krs.ranges().iter().map(Vec::len).sum::<usize>(), 0);

    // MemTracker accounts actual built KeyRange capacities.
    let tracker = new_tracker(-1, -1);
    let krs = index_ranges_to_kv_ranges_with_interrupt_signal(
        Some(&dctx),
        12,
        15,
        &ranges,
        Some(&tracker),
        None,
    )
    .unwrap();
    assert_eq!(krs.ranges().iter().map(Vec::len).sum::<usize>(), 12);
    assert!(tracker.bytes_consumed() > 0);

    // Killed tracker returns ErrQueryInterrupted.
    let killer = Arc::new(SqlKiller::default());
    killer.send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    let killed_tracker = new_tracker(-1, -1);
    killed_tracker
        .is_root_tracker_of_sess
        .store(true, Ordering::SeqCst);
    killed_tracker.set_killer(Some(killer));
    let err = index_ranges_to_kv_ranges_with_interrupt_signal(
        Some(&dctx),
        12,
        15,
        &ranges,
        Some(&killed_tracker),
        None,
    )
    .unwrap_err();
    assert_eq!(err.code(), errno::ErrQueryInterrupted as isize);
}

// Port of Go TestRequestBuilder3 & TestRequestBuilderHandle (request_builder_test.go:417, 898)
#[test]
fn test_request_builder_3_and_handle_tag() {
    use tidb_kv::key::{Handle, IntHandle};

    let handles: Vec<Handle> = [0, 2, 3, 4, 5, 10, 11, 100]
        .into_iter()
        .map(|v| Handle::Int(IntHandle(v)))
        .collect();
    let dctx = default_test_dctx();
    let tagger = Arc::new(new_resource_group_tag_builder());

    let req = RequestBuilder::default()
        .set_table_handles(15, &handles)
        .set_dag_request(tipb::DagRequest::default())
        .set_desc(false)
        .set_keep_order(false)
        .set_from_session_vars(&dctx)
        .set_resource_group_tagger(Some(tagger))
        .build()
        .unwrap();

    assert_eq!(req.tp, ReqTypeDAG);
    assert_eq!(req.data, vec![0x18, 0x0, 0x20, 0x0, 0x40, 0x0, 0x5a, 0x0]);
    let krs = req.key_ranges.as_ref().unwrap();
    assert!(!krs.is_partitioned());
    let mut got_hints: Vec<isize> = Vec::new();
    krs.for_each_partition_with_err(|ranges, hints| {
        assert_eq!(ranges.len(), 4);
        assert_eq!(
            ranges[0].start_key.0,
            vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x0
            ]
        );
        assert_eq!(
            ranges[3].end_key.0,
            vec![
                0x74, 0x80, 0, 0, 0, 0, 0, 0, 0xf, 0x5f, 0x72, 0x80, 0, 0, 0, 0, 0, 0, 0x65
            ]
        );
        got_hints = hints.unwrap().to_vec();
        Ok(())
    })
    .unwrap();
    assert_eq!(got_hints, vec![1, 4, 2, 1]);

    // ResourceGroupTagger encodes TableID = 15 from the first handle key.
    let first_key = &krs.first_partition_range()[0].start_key.0;
    let raw_tag = crate::util::resourcegrouptag::encode_tag_with_key(
        req.resource_group_tagger.as_ref().unwrap(),
        first_key,
    );
    let mut tag = tipb::ResourceGroupTag::default();
    tag.merge_from_bytes(&raw_tag).unwrap();
    assert_eq!(tag.get_table_id(), 15);
}

fn string_field_type() -> Arc<FieldType> {
    let mut ft = FieldType::new(mysql::TypeString);
    ft.set_flen(32);
    ft.set_charset("utf8mb4");
    ft.set_collate("utf8mb4_bin");
    Arc::new(ft)
}

fn timestamp_field_type() -> Arc<FieldType> {
    let mut ft = FieldType::new(mysql::TypeTimestamp);
    ft.set_flen(19);
    ft.set_decimal(0);
    ft.set_charset("binary");
    ft.set_collate("binary");
    Arc::new(ft)
}

fn encode_tipb_chunk(
    loc: &Arc<tidb_base::gostd::time::Location>,
    encode_type: tipb::EncodeType,
    fts: &[Arc<FieldType>],
    rows: &[Vec<tidb_types::datum::Datum>],
) -> tipb::Chunk {
    let mut c = tipb::Chunk::default();
    if rows.is_empty() {
        return c;
    }
    match encode_type {
        tipb::EncodeType::TypeChunk => {
            let mut chk = new_chunk_with_capacity(fts, rows.len());
            for row in rows {
                for (ci, d) in row.iter().enumerate() {
                    chk.append_datum(ci, d);
                }
            }
            let raw_fts: Vec<FieldType> = fts.iter().map(|ft| (**ft).clone()).collect();
            c.set_rows_data(new_codec(raw_fts).encode(&chk));
        }
        tipb::EncodeType::TypeDefault => {
            let mut buf = Vec::new();
            for row in rows {
                encode_value(loc, &mut buf, row).unwrap();
            }
            c.set_rows_data(buf);
        }
        other => panic!("unexpected encode type {other:?}"),
    }
    c
}

// Port of Go TestNewSelRespChannelIter (select_result_test.go:69)
#[test]
fn test_new_sel_resp_channel_iter() {
    use select_result::new_sel_resp_channel_iter;
    use tidb_base::gostd::time::fixed_zone;
    use tidb_sessionctx::variable::tidb_defs::DefInitChunkSize;

    let loc = fixed_zone("-02:00", -2 * 3600);
    let mut dctx = default_test_dctx();
    dctx.location = Arc::clone(&loc);

    let (resp, _, _) = ScriptedResponse::new(vec![]);
    let fts = vec![int64_field_type(), string_field_type(), int64_field_type()];
    let mut sr = SelectResultImpl::new(
        "dag",
        Box::new(resp),
        fts.clone(),
        Arc::new(dctx),
        "general",
        None,
        TiKV,
        false,
        1,
        vec![],
        0,
    );
    let inner = sr.inner_mut_for_test();
    inner.intermediate_output_types = vec![
        vec![string_field_type(), int64_field_type()],
        vec![int64_field_type()],
    ];
    let mut pb = tipb::SelectResponse::default();
    pb.set_encode_type(tipb::EncodeType::TypeChunk);
    for b in [&b"123"[..], &b"456"[..]] {
        let mut c = tipb::Chunk::default();
        c.set_rows_data(b.to_vec());
        pb.mut_chunks().push(c);
    }
    let mut io0 = tipb::IntermediateOutput::default();
    io0.set_encode_type(tipb::EncodeType::TypeDefault);
    for b in [&b"111"[..], &b"789"[..], &b"101112"[..]] {
        let mut c = tipb::Chunk::default();
        c.set_rows_data(b.to_vec());
        io0.mut_chunks().push(c);
    }
    let mut io1 = tipb::IntermediateOutput::default();
    io1.set_encode_type(tipb::EncodeType::TypeChunk);
    for b in [&b"1098"[..], &b"765"[..]] {
        let mut c = tipb::Chunk::default();
        c.set_rows_data(b.to_vec());
        io1.mut_chunks().push(c);
    }
    pb.mut_intermediate_outputs().push(io0);
    pb.mut_intermediate_outputs().push(io1);
    inner.select_resp = Some(pb);

    for enc in [tipb::EncodeType::TypeChunk, tipb::EncodeType::TypeDefault] {
        inner.select_resp.as_mut().unwrap().set_encode_type(enc);
        let iter = new_sel_resp_channel_iter(inner, 2).unwrap();
        assert_eq!(iter.channel(), 2);
        assert_eq!(iter.row_len, 3);
        assert_eq!(iter.encode_type, enc);
        assert_eq!(iter.chk_data.len(), 2);
        assert_eq!(iter.reserve_chk_size, DefInitChunkSize as usize);
    }

    let iter0 = new_sel_resp_channel_iter(inner, 0).unwrap();
    assert_eq!(iter0.channel(), 0);
    assert_eq!(iter0.row_len, 2);
    assert_eq!(iter0.encode_type, tipb::EncodeType::TypeDefault);
    assert_eq!(iter0.chk_data.len(), 3);

    let iter1 = new_sel_resp_channel_iter(inner, 1).unwrap();
    assert_eq!(iter1.channel(), 1);
    assert_eq!(iter1.row_len, 1);
    assert_eq!(iter1.encode_type, tipb::EncodeType::TypeChunk);
    assert_eq!(iter1.chk_data.len(), 2);

    let err = new_sel_resp_channel_iter(inner, 3).err().unwrap();
    assert!(err.to_string().contains("invalid channel 3"), "{err}");
    let err_neg = new_sel_resp_channel_iter(inner, -1).err().unwrap();
    assert!(
        err_neg.to_string().contains("invalid channel -1"),
        "{err_neg}"
    );
}

// Port of Go TestSelRespChannelIterRead (select_result_test.go:175)
#[test]
fn test_sel_resp_channel_iter_read() {
    use select_result::new_sel_resp_channel_iter;
    use tidb_base::gostd::time::fixed_zone;
    use tidb_types::datum::{new_string_datum, new_time_datum};
    use tidb_types::time::{from_date, new_time};

    let loc = fixed_zone("+01:00", 3600);
    let col_types = vec![
        string_field_type(),
        int64_field_type(),
        timestamp_field_type(),
    ];

    let mk_row = |s: &str, v: i64, sec_add: isize| {
        let total = 15 + sec_add;
        let min = 14 + total / 60;
        let sec = total % 60;
        let t = new_time(
            from_date(2024, 1, 12, 13, min, sec, 0),
            mysql::TypeTimestamp,
            0,
        );
        vec![
            new_string_datum(s.as_bytes()),
            new_int_datum(v),
            new_time_datum(t),
        ]
    };

    let rows0 = vec![
        mk_row("hello", 1, 0),
        mk_row("hello2", 2, 1),
        mk_row("hello3", 3, 2),
        mk_row("hello4", 4, 3),
        mk_row("hello5", 5, 4),
        mk_row("hello6", 6, 4),
    ];
    let rows1 = vec![mk_row("hello30", 30, 30)];
    let rows3 = vec![
        mk_row("hello1000", 1000, 40),
        mk_row("hello1001", 1001, 41),
        mk_row("hello1002", 1002, 42),
    ];
    let mut all_rows = Vec::new();
    all_rows.extend(rows0.clone());
    all_rows.extend(rows1.clone());
    all_rows.extend(rows3.clone());

    for enc in [tipb::EncodeType::TypeDefault, tipb::EncodeType::TypeChunk] {
        let mut dctx = default_test_dctx();
        dctx.location = Arc::clone(&loc);
        let (resp, _, _) = ScriptedResponse::new(vec![]);
        let mut sr = SelectResultImpl::new(
            "dag",
            Box::new(resp),
            vec![],
            Arc::new(dctx),
            "general",
            None,
            TiKV,
            false,
            1,
            vec![],
            0,
        );
        let inner = sr.inner_mut_for_test();
        inner.intermediate_output_types = vec![
            vec![string_field_type()],
            col_types.clone(),
            vec![int64_field_type()],
        ];
        let mut io0 = tipb::IntermediateOutput::default();
        io0.set_encode_type(enc);
        let mut io1 = tipb::IntermediateOutput::default();
        io1.set_encode_type(enc);
        io1.mut_chunks()
            .push(encode_tipb_chunk(&loc, enc, &col_types, &rows0));
        io1.mut_chunks()
            .push(encode_tipb_chunk(&loc, enc, &col_types, &rows1));
        io1.mut_chunks()
            .push(encode_tipb_chunk(&loc, enc, &col_types, &[]));
        io1.mut_chunks().push(tipb::Chunk::default());
        io1.mut_chunks()
            .push(encode_tipb_chunk(&loc, enc, &col_types, &rows3));
        let mut io2 = tipb::IntermediateOutput::default();
        io2.set_encode_type(enc);
        io2.mut_chunks().push(tipb::Chunk::default());

        let mut pb = tipb::SelectResponse::default();
        pb.mut_intermediate_outputs().push(io0);
        pb.mut_intermediate_outputs().push(io1);
        pb.mut_intermediate_outputs().push(io2);
        inner.select_resp = Some(pb);

        let mut iter = new_sel_resp_channel_iter(inner, 1).unwrap();
        iter.reserve_chk_size = 4;
        for (i, expected) in all_rows.iter().enumerate() {
            let row = iter.next().unwrap().expect("expected row");
            assert_eq!(row.channel_index, 1);
            let r = row.row.row();
            assert_eq!(r.get_string(0), expected[0].get_bytes(), "row {i}");
            assert_eq!(r.get_int64(1), expected[1].get_int64(), "row {i}");
            assert_eq!(r.get_time(2), expected[2].get_mysql_time(), "row {i}");
        }
        assert!(iter.next().unwrap().is_none());

        // Channel 0: no chunks
        let mut iter0 = new_sel_resp_channel_iter(inner, 0).unwrap();
        assert!(iter0.next().unwrap().is_none());

        // Channel 2: one empty chunk
        let mut iter2 = new_sel_resp_channel_iter(inner, 2).unwrap();
        assert!(iter2.next().unwrap().is_none());
    }
}

// Port of Go TestSelectResultIter (select_result_test.go:280)
#[tokio::test]
async fn test_select_result_iter() {
    use tidb_types::datum::new_string_datum;

    let main_fts = vec![
        int64_field_type(),
        int64_field_type(),
        int64_field_type(),
        int64_field_type(),
    ];
    let inter_fts = vec![vec![int64_field_type()], vec![string_field_type()]];
    let loc = utc();

    let mk_io_ints = |vals: &[i64]| {
        let mut out = tipb::IntermediateOutput::default();
        out.set_encode_type(tipb::EncodeType::TypeChunk);
        for chunk_vals in vals.chunks(2) {
            let rows: Vec<_> = chunk_vals.iter().map(|&v| vec![new_int_datum(v)]).collect();
            out.mut_chunks().push(encode_tipb_chunk(
                &loc,
                tipb::EncodeType::TypeChunk,
                &inter_fts[0],
                &rows,
            ));
        }
        out
    };
    let mk_io_strs = |vals: &[&str]| {
        let mut out = tipb::IntermediateOutput::default();
        out.set_encode_type(tipb::EncodeType::TypeChunk);
        for chunk_vals in vals.chunks(2) {
            let rows: Vec<_> = chunk_vals
                .iter()
                .map(|s| vec![new_string_datum(s.as_bytes())])
                .collect();
            out.mut_chunks().push(encode_tipb_chunk(
                &loc,
                tipb::EncodeType::TypeChunk,
                &inter_fts[1],
                &rows,
            ));
        }
        out
    };
    let mk_pkt = |main_cnt: usize, io0: tipb::IntermediateOutput, io1: tipb::IntermediateOutput| {
        let mut pb = tipb::SelectResponse::default();
        pb.set_encode_type(tipb::EncodeType::TypeChunk);
        if main_cnt > 0 {
            let rows: Vec<_> = (0..main_cnt)
                .map(|_| {
                    vec![
                        new_int_datum(123),
                        new_int_datum(123),
                        new_int_datum(123),
                        new_int_datum(123),
                    ]
                })
                .collect();
            pb.mut_chunks().push(encode_tipb_chunk(
                &loc,
                tipb::EncodeType::TypeChunk,
                &main_fts,
                &rows,
            ));
        }
        pb.mut_intermediate_outputs().push(io0);
        pb.mut_intermediate_outputs().push(io1);
        Ok(MockSubset {
            data: pb.write_to_bytes().unwrap(),
            stats: None,
            resp_time: Duration::ZERO,
        })
    };

    // Normal case: 4 packets (2 main + 2 inter; 1 main + 2 inter; 0 main + 2 inter; empty)
    let packets = vec![
        mk_pkt(2, mk_io_ints(&[1, 2, 3, 4, 5]), mk_io_strs(&["aa", "bb"])),
        mk_pkt(1, mk_io_ints(&[11]), mk_io_strs(&["1aa", "1bb", "1cc"])),
        mk_pkt(
            0,
            mk_io_ints(&[21, 22]),
            mk_io_strs(&["2aa", "2bb", "2cc", "2dd"]),
        ),
        mk_pkt(0, mk_io_ints(&[]), mk_io_strs(&[])),
    ];
    let (resp, closed, _) = ScriptedResponse::new(packets);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let mut sr = select(
        &Context::background(),
        Arc::new(dctx),
        &Request::default(),
        main_fts.clone(),
    )
    .await
    .unwrap();
    let mut iter = sr.into_iter(inter_fts.clone()).unwrap();

    // All methods on original SelectResult fail with exact Go error after IntoIter().
    assert_eq!(
        sr.next_raw(&Context::background())
            .await
            .unwrap_err()
            .to_string(),
        "selectResult is invalid after IntoIter()"
    );
    let mut dummy_chk = new_chunk_with_capacity(&main_fts, 1);
    assert_eq!(
        sr.next(&Context::background(), &mut dummy_chk)
            .await
            .unwrap_err()
            .to_string(),
        "selectResult is invalid after IntoIter()"
    );
    assert_eq!(
        sr.into_iter(inter_fts.clone()).err().unwrap().to_string(),
        "selectResult is invalid after IntoIter()"
    );
    assert_eq!(
        sr.close().unwrap_err().to_string(),
        "selectResult is invalid after IntoIter()"
    );

    let mut channels = Vec::new();
    let mut vals = Vec::new();
    while let Some(row) = iter.next(&Context::background()).await.unwrap() {
        channels.push(row.channel_index);
        let r = row.row.row();
        match row.channel_index {
            2 => vals.push(format!(
                "{}_{}_{}_{}",
                r.get_int64(0),
                r.get_int64(1),
                r.get_int64(2),
                r.get_int64(3)
            )),
            0 => vals.push(r.get_int64(0).to_string()),
            1 => vals.push(String::from_utf8(r.get_string(0).to_vec()).unwrap()),
            c => panic!("unexpected channel {c}"),
        }
    }
    assert_eq!(
        channels,
        vec![2, 2, 1, 1, 0, 0, 0, 0, 0, 2, 1, 1, 1, 0, 1, 1, 1, 1, 0, 0]
    );
    assert_eq!(
        vals,
        vec![
            "123_123_123_123",
            "123_123_123_123",
            "aa",
            "bb",
            "1",
            "2",
            "3",
            "4",
            "5",
            "123_123_123_123",
            "1aa",
            "1bb",
            "1cc",
            "11",
            "2aa",
            "2bb",
            "2cc",
            "2dd",
            "21",
            "22",
        ]
    );
    assert!(!closed.load(Ordering::SeqCst));
    iter.close().unwrap();
    assert!(closed.load(Ordering::SeqCst));

    // Mismatched intermediateOutputTypes length returns exact Go error.
    let (resp2, _, _) =
        ScriptedResponse::new(vec![mk_pkt(1, mk_io_ints(&[1]), mk_io_strs(&["x"]))]);
    let mut dctx2 = default_test_dctx();
    dctx2.client = Some(SingleRespClient::new(Box::new(resp2)));
    let mut sr2 = select(
        &Context::background(),
        Arc::new(dctx2),
        &Request::default(),
        main_fts.clone(),
    )
    .await
    .unwrap();
    let mut iter2 = sr2.into_iter(inter_fts[1..].to_vec()).unwrap();
    let err = iter2.next(&Context::background()).await.unwrap_err();
    assert!(
        err.to_string().contains(
            "The length of intermediate output types 1 mismatches the length of got intermediate outputs 2"
        ),
        "{err}"
    );
    iter2.close().unwrap();

    // selectResult.Next() fails when response contains intermediate outputs.
    let (resp3, _, _) = ScriptedResponse::new(vec![mk_pkt(1, mk_io_ints(&[]), mk_io_strs(&[]))]);
    let mut dctx3 = default_test_dctx();
    dctx3.client = Some(SingleRespClient::new(Box::new(resp3)));
    let mut sr3 = select(
        &Context::background(),
        Arc::new(dctx3),
        &Request::default(),
        main_fts.clone(),
    )
    .await
    .unwrap();
    let mut chk3 = new_chunk_with_capacity(&main_fts, 1);
    let err3 = sr3
        .next(&Context::background(), &mut chk3)
        .await
        .unwrap_err();
    assert!(
        err3.to_string().contains(
            "If a response contains intermediate outputs, you should use the SelectResultIter to read the data"
        ),
        "{err3}"
    );
    sr3.close().unwrap();
}

// Dedicated Rust test (spec §6.1 & §8 item 1 / #415): SelectResultIter zeros mem_tracker across
// multiple responses and on close / mid-batch drop.
#[tokio::test]
async fn select_result_iter_mem_tracker_zeros_across_multiple_responses() {
    let fts = vec![int64_field_type()];
    let loc = utc();
    let mk_pkt = |vals: &[i64]| {
        let mut pb = tipb::SelectResponse::default();
        pb.set_encode_type(tipb::EncodeType::TypeChunk);
        let rows: Vec<_> = vals.iter().map(|&v| vec![new_int_datum(v)]).collect();
        pb.mut_chunks().push(encode_tipb_chunk(
            &loc,
            tipb::EncodeType::TypeChunk,
            &fts,
            &rows,
        ));
        Ok(MockSubset {
            data: pb.write_to_bytes().unwrap(),
            stats: None,
            resp_time: Duration::ZERO,
        })
    };

    let tracker = new_tracker(-1, -1);
    let (resp, _, _) =
        ScriptedResponse::new(vec![mk_pkt(&[10, 20]), mk_pkt(&[30, 40]), mk_pkt(&[50])]);
    let mut dctx = default_test_dctx();
    dctx.client = Some(SingleRespClient::new(Box::new(resp)));
    let req = Request {
        mem_tracker: Some(Arc::clone(&tracker)),
        ..Default::default()
    };
    let mut sr = select(&Context::background(), Arc::new(dctx), &req, fts.clone())
        .await
        .unwrap();
    let mut iter = sr.into_iter(vec![]).unwrap();
    let mut got = Vec::new();
    while let Some(row) = iter.next(&Context::background()).await.unwrap() {
        got.push(row.row.row().get_int64(0));
    }
    assert_eq!(got, vec![10, 20, 30, 40, 50]);
    assert_eq!(tracker.bytes_consumed(), 0);
    iter.close().unwrap();
    assert_eq!(tracker.bytes_consumed(), 0);

    // Mid-batch drop without close also zeros mem_tracker and closes response.
    let (resp2, closed2, _) = ScriptedResponse::new(vec![mk_pkt(&[1, 2, 3])]);
    let mut dctx2 = default_test_dctx();
    dctx2.client = Some(SingleRespClient::new(Box::new(resp2)));
    let mut sr2 = select(&Context::background(), Arc::new(dctx2), &req, fts)
        .await
        .unwrap();
    let mut iter2 = sr2.into_iter(vec![]).unwrap();
    let first = iter2.next(&Context::background()).await.unwrap().unwrap();
    assert_eq!(first.row.row().get_int64(0), 1);
    assert!(tracker.bytes_consumed() > 0);
    drop(iter2);
    assert!(closed2.load(Ordering::SeqCst));
    assert_eq!(tracker.bytes_consumed(), 0);
}

// Helper mock SelectResult for SortedSelectResults unit tests.
struct MockChildResult {
    chunks: std::collections::VecDeque<Vec<Vec<tidb_types::datum::Datum>>>,
    close_err: Option<String>,
    closed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl SelectResult for MockChildResult {
    async fn next_raw(&mut self, _ctx: &Context) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }

    async fn next(&mut self, _ctx: &Context, chk: &mut tidb_chunk::Chunk) -> Result<(), Error> {
        chk.reset();
        if let Some(rows) = self.chunks.pop_front() {
            for row in rows {
                for (ci, d) in row.iter().enumerate() {
                    chk.append_datum(ci, d);
                }
            }
        }
        Ok(())
    }

    fn into_iter(
        &mut self,
        _intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        Err(Error::plain("not implemented"))
    }

    fn close(&mut self) -> Result<(), Error> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(msg) = &self.close_err {
            return Err(Error::plain(msg.clone()));
        }
        Ok(())
    }
}

// Dedicated Rust test: NewSortedSelectResults merge sort, ASC/DESC, NULLs, schema=None/Some,
// IntoIter error, NextRaw panic.
#[tokio::test]
async fn sorted_select_results_merge_sort_and_tie_breaking() {
    use tidb_expression::byitem::ByItems;
    use tidb_expression::column::Column;
    use tidb_expression::expression::Expr;
    use tidb_expression::schema::new_schema;
    use tidb_sessionctx::exprstatic::evalctx::new_eval_context;
    use tidb_types::datum::{Datum, new_string_datum};

    let ectx = new_eval_context(vec![]);
    let fts = vec![int64_field_type(), string_field_type()];
    let c0 = Arc::new(Column {
        id: 1,
        unique_id: 1,
        ret_type: Arc::clone(&fts[0]),
        ..Default::default()
    });
    let c1 = Arc::new(Column {
        id: 2,
        unique_id: 2,
        ret_type: Arc::clone(&fts[1]),
        ..Default::default()
    });
    let schema = new_schema(vec![Arc::clone(&c0), Arc::clone(&c1)]);

    let null_d = || Datum::default();
    let child0: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![
            vec![
                vec![null_d(), new_string_datum(b"p0_n")],
                vec![new_int_datum(1), new_string_datum(b"b")],
                vec![new_int_datum(5), new_string_datum(b"a")],
            ],
            vec![vec![new_int_datum(10), new_string_datum(b"z")]],
        ]
        .into(),
        close_err: None,
        closed: Arc::new(AtomicBool::new(false)),
    });
    let child1: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![vec![
            vec![new_int_datum(1), new_string_datum(b"c")],
            vec![new_int_datum(5), new_string_datum(b"d")],
        ]]
        .into(),
        close_err: None,
        closed: Arc::new(AtomicBool::new(false)),
    });
    let child2: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![].into(),
        close_err: None,
        closed: Arc::new(AtomicBool::new(false)),
    });

    // Sort by (col0 ASC, col1 DESC) with explicit schema.
    let by_items = vec![
        ByItems {
            expr: Expr::Column((*c0).clone()).into_ref(),
            desc: false,
        },
        ByItems {
            expr: Expr::Column((*c1).clone()).into_ref(),
            desc: true,
        },
    ];
    let tracker = new_tracker(-1, -1);
    let mut ssr = new_sorted_select_results(
        ectx.as_ref(),
        vec![child0, child1, child2],
        Some(&schema),
        by_items,
        Some(Arc::clone(&tracker)),
        None,
        None,
    )
    .unwrap();

    // IntoIter returns "not implemented" without breaking subsequent Next/Close.
    assert_eq!(
        ssr.into_iter(vec![]).err().unwrap().to_string(),
        "not implemented"
    );

    let mut out = Vec::new();
    let mut chk = new_chunk_with_capacity(&fts, 2);
    loop {
        ssr.next(&Context::background(), &mut chk).await.unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        for i in 0..chk.num_rows() {
            let r = chk.get_row(i);
            let k = if r.is_null(0) {
                None
            } else {
                Some(r.get_int64(0))
            };
            let s = String::from_utf8(r.get_string(1).to_vec()).unwrap();
            out.push((k, s));
        }
    }
    assert_eq!(
        out,
        vec![
            (None, "p0_n".to_owned()),
            (Some(1), "c".to_owned()),
            (Some(1), "b".to_owned()),
            (Some(5), "d".to_owned()),
            (Some(5), "a".to_owned()),
            (Some(10), "z".to_owned()),
        ]
    );
    ssr.close().unwrap();
    assert_eq!(tracker.bytes_consumed(), 0);
}

// Dedicated Rust test (spec §6.1 & §8 item 4): SortedSelectResults::close drains all children
// and frees cached chunk memory even when child 0 returns an error; Drop safety net works too.
#[tokio::test]
async fn sorted_select_results_close_drains_all_on_error() {
    use tidb_expression::byitem::ByItems;
    use tidb_expression::column::Column;
    use tidb_expression::expression::Expr;
    use tidb_sessionctx::exprstatic::evalctx::new_eval_context;

    let ectx = new_eval_context(vec![]);
    let fts = vec![int64_field_type()];
    let c0 = Arc::new(Column {
        id: 1,
        unique_id: 1,
        ret_type: Arc::clone(&fts[0]),
        ..Default::default()
    });
    let closed0 = Arc::new(AtomicBool::new(false));
    let closed1 = Arc::new(AtomicBool::new(false));
    let child0: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![vec![vec![new_int_datum(1)], vec![new_int_datum(3)]]].into(),
        close_err: Some("close err on sr0".to_owned()),
        closed: Arc::clone(&closed0),
    });
    let child1: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![vec![vec![new_int_datum(2)], vec![new_int_datum(4)]]].into(),
        close_err: None,
        closed: Arc::clone(&closed1),
    });

    // schema = None -> sort by first column (col 0).
    let tracker = new_tracker(-1, -1);
    let mut ssr = new_sorted_select_results(
        ectx.as_ref(),
        vec![child0, child1],
        None,
        vec![ByItems {
            expr: Expr::Column((*c0).clone()).into_ref(),
            desc: false,
        }],
        Some(Arc::clone(&tracker)),
        None,
        None,
    )
    .unwrap();

    let mut chk = new_chunk_with_capacity(&fts, 1);
    ssr.next(&Context::background(), &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 1);
    assert_eq!(chk.get_row(0).get_int64(0), 1);
    assert!(tracker.bytes_consumed() > 0);

    let err = ssr.close().unwrap_err();
    assert_eq!(err.to_string(), "close err on sr0");
    assert!(closed0.load(Ordering::SeqCst));
    assert!(closed1.load(Ordering::SeqCst));
    assert_eq!(tracker.bytes_consumed(), 0);
    // Second close is idempotent.
    ssr.close().unwrap();

    // Drop without close also closes children and frees tracked chunk memory.
    let closed_drop = Arc::new(AtomicBool::new(false));
    let child_drop: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![vec![vec![new_int_datum(7)]]].into(),
        close_err: None,
        closed: Arc::clone(&closed_drop),
    });
    let mut ssr2 = new_sorted_select_results(
        ectx.as_ref(),
        vec![child_drop],
        None,
        vec![ByItems {
            expr: Expr::Column((*c0).clone()).into_ref(),
            desc: false,
        }],
        Some(Arc::clone(&tracker)),
        None,
        None,
    )
    .unwrap();
    let mut chk2 = new_chunk_with_capacity(&fts, 1);
    ssr2.next(&Context::background(), &mut chk2).await.unwrap();
    assert!(tracker.bytes_consumed() > 0);
    drop(ssr2);
    assert!(closed_drop.load(Ordering::SeqCst));
    assert_eq!(tracker.bytes_consumed(), 0);
}

#[tokio::test]
async fn sorted_select_results_checks_sql_killer_during_merge() {
    use tidb_expression::byitem::ByItems;
    use tidb_expression::column::Column;
    use tidb_expression::expression::Expr;
    use tidb_runtime::KillCancel;
    use tidb_sessionctx::exprstatic::evalctx::new_eval_context;

    let ectx = new_eval_context(vec![]);
    let fts = vec![int64_field_type()];
    let c0 = Arc::new(Column {
        id: 1,
        unique_id: 1,
        ret_type: Arc::clone(&fts[0]),
        ..Default::default()
    });
    let closed0 = Arc::new(AtomicBool::new(false));
    let child0: Box<dyn SelectResult> = Box::new(MockChildResult {
        chunks: vec![vec![
            vec![new_int_datum(1)],
            vec![new_int_datum(2)],
            vec![new_int_datum(3)],
        ]]
        .into(),
        close_err: None,
        closed: Arc::clone(&closed0),
    });
    let killer = SqlKiller::new();
    let kc = KillCancel::new(killer.signal_handle());
    killer.send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    let mut ssr = new_sorted_select_results(
        ectx.as_ref(),
        vec![child0],
        None,
        vec![ByItems {
            expr: Expr::Column((*c0).clone()).into_ref(),
            desc: false,
        }],
        None,
        Some(Arc::clone(&killer)),
        Some(kc),
    )
    .unwrap();
    let mut chk = new_chunk_with_capacity(&fts, 4);
    let err = ssr
        .next(&Context::background(), &mut chk)
        .await
        .unwrap_err();
    assert_eq!(err.code(), errno::ErrQueryInterrupted as isize);
}
