//! Replays the govec `distsql` vectors (tidb-rs-tracker `govec/domains/distsql.go`):
//! block D07.3 parts 1 & 2 (`SplitRangesAcrossInt64Boundary`, table/common-handle/index-key
//! range encoding, `TableHandlesToKVRanges`, `PartitionHandlesToKVRanges`,
//! `IndexRangesToKVRanges*`, `RequestBuilder` + `SetEncodeType` + `ResourceGroupTagger`
//! TableId/Label, `Select` / `SelectWithRuntimeStats` + `TypeChunk`/`TypeDefault` decoding,
//! `NewSortedSelectResults`, `SelectResultIter` / `selRespChannelIter`, and
//! `selectResultRuntimeStats`) against Go TiDB v8.5.8.
//!
//! `GOVEC_DISTSQL_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{WarnAppender, WarnAppenderRef, ignore_warn_appender};
use tidb_base::gostd::time::{fixed_zone, utc};
use tidb_base::kv::StoreType;
use tidb_base::mysql::{DelayedPriority, HighPriority, LowPriority, NoPriority};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_execdetails::execdetails::{
    CopExecDetails, RuntimeStats, RuntimeStatsColl, SyncExecDetails,
};
use tidb_execdetails::ppcpuusage::SQLCPUUsages;
use tidb_expression::byitem::ByItems;
use tidb_expression::column::Column;
use tidb_expression::expression::Expr;
use tidb_expression::schema::new_schema;
use tidb_infoschema::context::MetaOnlyInfoSchema;
use tidb_kv::copr::{CopRuntimeStats, ResourceGroupTagBuilder, Variables};
use tidb_kv::key::{Handle, IntHandle, Key, KeyRange, new_common_handle, new_partition_handle};
use tidb_kv::kv::{
    Client, ClientSendOption, KeyRanges, KvFuture, RC, RCCheckTS, ReqTypeDAG, Request, Response,
    ResultSubset, SI,
};
use tidb_kv::option::ReplicaReadType;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::errctx::{
    self, ErrGroupBadNull, ErrGroupDupKey, ErrGroupNoDefault, ErrGroupTruncate, LevelIgnore,
    LevelMap,
};
use tidb_sessionctx::exprstatic::evalctx::new_eval_context;
use tidb_sessionctx::ranger::Range;
use tidb_store::distsql::{
    RequestBuilder, SelectResult, SelectResultRuntimeStats, common_handle_ranges_to_kv_ranges,
    encode_index_key, get_select_result_concurrency, get_system_endian, index_ranges_to_kv_ranges,
    index_ranges_to_kv_ranges_with_interrupt_signal, new_sorted_select_results,
    partition_handles_to_kv_ranges, select, select_with_runtime_stats, set_encode_type,
    split_ranges_across_int64_boundary, table_handle_ranges_to_kv_ranges,
    table_handles_to_kv_ranges, table_ranges_to_kv_ranges, verify_txn_scope,
};
use tidb_store::util::resourcegrouptag::{encode_tag_with_key, get_resource_group_label_by_key};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration as MysqlDuration, Time};
use tidb_types::vector::parse_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/distsql.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- value codecs

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn ungs(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => unhex(m["hex"].as_str().unwrap()),
        _ => panic!("not a GoString: {v}"),
    }
}

fn hex_opt(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        None
    } else {
        Some(unhex(v.as_str().unwrap()))
    }
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    let st = |i: usize| p[i].as_str().unwrap().to_owned();
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindMinNotNull => d.set_min_not_null(),
        dm::KindMaxValue => d = dm::max_value_datum(),
        dm::KindInt64 => d.set_int64(i(1)),
        dm::KindUint64 => d.set_uint64(u(1)),
        dm::KindFloat32 => d.set_float32(f64::from_bits(u(1)) as f32),
        dm::KindFloat64 => d.set_float64(f64::from_bits(u(1))),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => d.set_mysql_decimal(dec_of(p[1].as_str().unwrap())),
        dm::KindMysqlDuration => d.set_mysql_duration(MysqlDuration {
            duration: i(1),
            fsp: i(2) as isize,
        }),
        dm::KindMysqlEnum => d.set_mysql_enum(
            Enum {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlSet => d.set_mysql_set(
            Set {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlTime => d.set_mysql_time(Time::from_raw(CoreTime(u(1)))),
        dm::KindRaw => d.set_raw(hex_opt(&p[1])),
        dm::KindMysqlJSON => d.set_mysql_json(BinaryJSON::owned(
            u(1) as u8,
            hex_opt(&p[2]).unwrap_or_default(),
        )),
        dm::KindVectorFloat32 => {
            d.set_vector_float32(parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap())
        }
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

fn datum_compact(d: &Datum) -> Value {
    match d.kind() {
        dm::KindNull | dm::KindMinNotNull | dm::KindMaxValue => json!([d.kind()]),
        dm::KindInt64 => json!([d.kind(), d.get_int64()]),
        dm::KindUint64 => json!([d.kind(), d.get_uint64()]),
        dm::KindFloat32 => json!([d.kind(), (d.get_float32() as f64).to_bits()]),
        dm::KindFloat64 => json!([d.kind(), d.get_float64().to_bits()]),
        dm::KindString => json!([d.kind(), gs(d.get_string()), d.collation()]),
        dm::KindBytes => json!([d.kind(), hex(d.get_bytes())]),
        dm::KindMysqlDecimal => json!([d.kind(), d.get_mysql_decimal().to_string()]),
        dm::KindMysqlTime => json!([d.kind(), d.get_mysql_time().core_time().0]),
        _ => json!([d.kind(), d.string()]),
    }
}

fn range_in(v: &Value) -> Range {
    let list = |k: &str| -> Vec<Datum> {
        match &v[k] {
            Value::Array(a) => a.iter().map(datum_in).collect(),
            _ => Vec::new(),
        }
    };
    let colls = match &v["c"] {
        Value::Array(a) => a
            .iter()
            .map(|c| collate::get_collator(c.as_str().unwrap()))
            .collect(),
        _ => Vec::new(),
    };
    Range {
        low_val: list("l"),
        high_val: list("h"),
        low_exclude: v["le"].as_bool().unwrap_or(false),
        high_exclude: v["he"].as_bool().unwrap_or(false),
        collators: colls,
    }
}

fn ranges_in(v: &Value) -> Vec<Range> {
    v.as_array()
        .map(|a| a.iter().map(range_in).collect())
        .unwrap_or_default()
}

fn ranges_out(rs: &[Range]) -> Value {
    Value::Array(
        rs.iter()
            .map(|r| {
                json!({
                    "l": r.low_val.iter().map(datum_compact).collect::<Vec<_>>(),
                    "h": r.high_val.iter().map(datum_compact).collect::<Vec<_>>(),
                    "le": r.low_exclude,
                    "he": r.high_exclude,
                })
            })
            .collect(),
    )
}

fn kv_range_in(v: &Value) -> KeyRange {
    KeyRange {
        start_key: Key(unhex(v[0].as_str().unwrap())),
        end_key: Key(unhex(v[1].as_str().unwrap())),
    }
}

fn kv_ranges_in(v: &Value) -> Vec<KeyRange> {
    v.as_array()
        .map(|a| a.iter().map(kv_range_in).collect())
        .unwrap_or_default()
}

fn kv_ranges_out(rs: &[KeyRange]) -> Value {
    Value::Array(
        rs.iter()
            .map(|r| json!([hex(&r.start_key.0), hex(&r.end_key.0)]))
            .collect(),
    )
}

fn key_ranges_out(krs: Option<&KeyRanges>) -> Value {
    let Some(krs) = krs else {
        return json!({ "partNum": 0, "ranges": [] });
    };
    let mut flat = Vec::new();
    let _ = krs.for_each_partition_with_err(|rs, _hints| {
        for r in rs {
            flat.push(json!([hex(&r.start_key.0), hex(&r.end_key.0)]));
        }
        Ok(())
    });
    json!({
        "partNum": krs.partition_num(),
        "ranges": flat,
    })
}

fn loc_of(offset: i32) -> Arc<tidb_base::gostd::time::Location> {
    if offset == 0 {
        utc()
    } else {
        fixed_zone("Z", offset)
    }
}

fn err_ctx_of(strict: bool) -> errctx::Context {
    if strict {
        errctx::StrictNoWarningContext.clone()
    } else {
        let mut lm = LevelMap::default();
        lm[ErrGroupBadNull.0 as usize] = LevelIgnore;
        lm[ErrGroupTruncate.0 as usize] = LevelIgnore;
        lm[ErrGroupNoDefault.0 as usize] = LevelIgnore;
        lm[ErrGroupDupKey.0 as usize] = LevelIgnore;
        errctx::new_context_with_levels(lm, ignore_warn_appender())
    }
}

fn field_type_in(v: &Value) -> FieldType {
    let mut ft = FieldType::new(v["tp"].as_u64().unwrap() as u8);
    ft.set_flag(v["flag"].as_u64().unwrap_or(0) as usize);
    ft.set_flen(v["flen"].as_i64().unwrap_or(0) as isize);
    ft.set_decimal(v["dec"].as_i64().unwrap_or(0) as isize);
    ft.set_charset(v["cs"].as_str().unwrap_or(""));
    ft.set_collate(v["coll"].as_str().unwrap_or(""));
    ft
}

// ---------------------------------------------------------------- mock helpers

struct NullMetaIS;
impl tidb_sessionctx::expropt::MetaOnlyInfoSchema for NullMetaIS {
    fn schema_meta_version(&self) -> i64 {
        1
    }
}
impl MetaOnlyInfoSchema for NullMetaIS {
    fn placement_bundle_by_physical_table_id(
        &self,
        _id: i64,
    ) -> Option<Arc<tidb_infoschema::placement::Bundle>> {
        None
    }
}

struct WarnRecorder(Mutex<Vec<String>>);
impl WarnRecorder {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Vec::new())))
    }
    fn take(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}
impl WarnAppender for WarnRecorder {
    fn append_warning(&self, err: Error) {
        self.0.lock().unwrap().push(err.to_string());
    }
    fn append_note(&self, err: Error) {
        self.0.lock().unwrap().push(err.to_string());
    }
}

fn cop_stats_in(v: &Value) -> Option<CopRuntimeStats> {
    if v.is_null() {
        return None;
    }
    let mut cs = CopRuntimeStats {
        cop_exec_details: CopExecDetails {
            callee_address: v["callee"].as_str().unwrap_or("").to_owned(),
            ..Default::default()
        },
        copr_cache_hit: v["cacheHit"].as_bool().unwrap_or(false),
    };
    cs.cop_exec_details.scan_detail.processed_keys = v["procKeys"].as_i64().unwrap_or(0);
    cs.cop_exec_details.time_detail.process_time = v["procNs"].as_i64().unwrap_or(0);
    cs.cop_exec_details.time_detail.wait_time = v["waitNs"].as_i64().unwrap_or(0);
    if let Some(k) = v["boKey"].as_str()
        && !k.is_empty()
    {
        let mut m = BTreeMap::new();
        m.insert(k.to_owned(), v["boNs"].as_i64().unwrap_or(0));
        cs.cop_exec_details.backoff_sleep = m;
    } else if let Some(obj) = v["boMap"].as_object() {
        for (k, val) in obj {
            cs.cop_exec_details
                .backoff_sleep
                .insert(k.clone(), val.as_i64().unwrap_or(0));
        }
    }
    Some(cs)
}

struct MockSubset {
    data: Vec<u8>,
    start_key: Key,
    resp_time: Duration,
    stats: Option<CopRuntimeStats>,
}

impl ResultSubset for MockSubset {
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_start_key(&self) -> &Key {
        &self.start_key
    }
    fn mem_size(&self) -> i64 {
        self.data.len() as i64
    }
    fn resp_time(&self) -> Duration {
        self.resp_time
    }
    fn get_cop_runtime_stats(&self) -> Option<&CopRuntimeStats> {
        self.stats.as_ref()
    }
}

struct MockPacket {
    data: Vec<u8>,
    resp_time: Duration,
    stats: Option<CopRuntimeStats>,
    next_err: Option<String>,
}

struct MockResp {
    packets: Vec<MockPacket>,
    idx: usize,
    conc: isize,
    extra_conc: isize,
    batch_num: u64,
    batch_fallback: u64,
    build_elapsed: Duration,
    unconsumed: Vec<CopRuntimeStats>,
}

impl Response for MockResp {
    fn next<'a>(
        &'a mut self,
        _ctx: &'a tidb_kv::Context,
    ) -> KvFuture<'a, Option<Box<dyn ResultSubset>>> {
        let item = if self.idx < self.packets.len() {
            let p = &self.packets[self.idx];
            self.idx += 1;
            if let Some(e) = &p.next_err {
                Err(Error::plain(e.clone()))
            } else {
                Ok(Some(Box::new(MockSubset {
                    data: p.data.clone(),
                    start_key: Key(Vec::new()),
                    resp_time: p.resp_time,
                    stats: p.stats.clone(),
                }) as Box<dyn ResultSubset>))
            }
        } else {
            Ok(None)
        };
        Box::pin(async move { item })
    }

    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        Some((self.conc, self.extra_conc))
    }

    fn cop_store_batch_info(&self) -> Option<(u64, u64)> {
        Some((self.batch_num, self.batch_fallback))
    }

    fn cop_build_task_elapsed(&self) -> Option<Duration> {
        Some(self.build_elapsed)
    }

    fn collect_unconsumed_cop_runtime_stats(&self) -> Vec<CopRuntimeStats> {
        self.unconsumed.clone()
    }
}

struct MockClient(Mutex<Option<Box<dyn Response>>>);

impl Client for MockClient {
    fn send(
        &self,
        _ctx: &tidb_kv::Context,
        _req: &Request,
        _vars: Option<Box<dyn std::any::Any + Send + Sync>>,
        _option: &ClientSendOption,
    ) -> Box<dyn Response> {
        self.0
            .lock()
            .unwrap()
            .take()
            .expect("MockClient::send called once")
    }
    fn is_request_type_supported(&self, _req_type: i64, _sub_type: i64) -> bool {
        true
    }
}

// ---------------------------------------------------------------- exec ops

fn exec_request_builder(spec: &Value) -> Value {
    let mut dctx = DistSQLContext {
        location: utc(),
        err_ctx: errctx::StrictNoWarningContext.clone(),
        enable_chunk_rpc: spec["enableChunkRPC"].as_bool().unwrap_or(false),
        dist_sql_concurrency: spec["distSQLConc"].as_i64().unwrap_or(0) as isize,
        weak_consistency: spec["weak"].as_bool().unwrap_or(false),
        rc_check_ts: spec["rcCheckTS"].as_bool().unwrap_or(false),
        not_fill_cache: spec["notFillCache"].as_bool().unwrap_or(false),
        task_id: spec["taskID"].as_u64().unwrap_or(0),
        priority: match spec["priority"].as_i64().unwrap_or(0) {
            1 => LowPriority,
            2 => HighPriority,
            3 => DelayedPriority,
            _ => NoPriority,
        },
        replica_read_type: ReplicaReadType(spec["replicaRead"].as_u64().unwrap_or(0) as u8),
        enable_paging: spec["enablePaging"].as_bool().unwrap_or(false),
        min_paging_size: spec["minPaging"].as_i64().unwrap_or(0) as isize,
        max_paging_size: spec["maxPaging"].as_i64().unwrap_or(0) as isize,
        store_batch_size: spec["storeBatchSize"].as_i64().unwrap_or(0) as isize,
        resource_group_name: spec["rgName"].as_str().unwrap_or("").to_owned(),
        load_based_replica_read_threshold: Duration::from_millis(
            spec["busyMs"].as_u64().unwrap_or(0),
        ),
        tikv_client_read_timeout: spec["readTimeout"].as_u64().unwrap_or(0),
        in_restricted_sql: spec["restricted"].as_bool().unwrap_or(false),
        request_source_type: spec["reqSrc"].as_str().unwrap_or("").to_owned(),
        explicit_request_source_type: spec["explicitReqSrc"].as_str().unwrap_or("").to_owned(),
        connection_id: spec["connID"].as_u64().unwrap_or(0),
        session_alias: spec["connAlias"].as_str().unwrap_or("").to_owned(),
        ..Default::default()
    };
    let set_tagger = spec["setTagger"].as_bool().unwrap_or(false);
    if set_tagger {
        dctx.resource_group_tagger = Some(Arc::new(ResourceGroupTagBuilder::default()));
    }

    let mut b = RequestBuilder::default();
    if spec["hasMemTracker"].as_bool().unwrap_or(false) {
        b.set_mem_tracker(Some(new_tracker(-1, -1)));
    }
    match spec["rangeMode"].as_str().unwrap_or("none") {
        "table" => {
            let rs = ranges_in(&spec["ranges"]);
            b.set_table_ranges(spec["tid"].as_i64().unwrap_or(0), &rs);
        }
        "handle" => {
            let rs = ranges_in(&spec["ranges"]);
            b.set_handle_ranges(
                Some(&dctx),
                spec["tid"].as_i64().unwrap_or(0),
                spec["commonHandle"].as_bool().unwrap_or(false),
                &rs,
            );
        }
        "handle_multi" => {
            let tids: Vec<i64> = spec["tids"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_i64().unwrap()).collect())
                .unwrap_or_default();
            let rs = ranges_in(&spec["ranges"]);
            b.set_handle_ranges_for_tables(
                Some(&dctx),
                &tids,
                spec["commonHandle"].as_bool().unwrap_or(false),
                &rs,
            );
        }
        "key" => {
            b.set_key_ranges(kv_ranges_in(&spec["rawKeys"]));
        }
        "key_hints" => {
            let hints: Vec<isize> = spec["hints"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_i64().unwrap() as isize).collect())
                .unwrap_or_default();
            b.set_key_ranges_with_hints(kv_ranges_in(&spec["rawKeys"]), Some(hints));
        }
        "part_key" => {
            let parts: Vec<Vec<KeyRange>> = spec["partKeys"]
                .as_array()
                .map(|a| a.iter().map(kv_ranges_in).collect())
                .unwrap_or_default();
            b.set_partition_key_ranges(parts);
        }
        _ => {}
    }

    if !spec["dag"].is_null() {
        let d = &spec["dag"];
        let mut dag = tipb::DagRequest::default();
        dag.set_time_zone_offset(d["tzOffset"].as_i64().unwrap_or(0));
        dag.set_flags(d["flags"].as_u64().unwrap_or(0));
        dag.set_time_zone_name(d["tzName"].as_str().unwrap_or("").to_owned());
        if let Some(execs) = d["execs"].as_array() {
            for es in execs {
                let mut e = tipb::Executor::default();
                let tp_num = es["tp"].as_i64().unwrap() as i32;
                let tp = protobuf::ProtobufEnum::from_i32(tp_num).expect("valid ExecType");
                e.set_tp(tp);
                if let Some(lim) = es["limit"].as_u64() {
                    let mut l = tipb::Limit::default();
                    l.set_limit(lim);
                    e.set_limit(l);
                }
                if let Some(pidx) = es["parentIdx"].as_u64() {
                    e.set_parent_idx(pidx as u32);
                }
                dag.mut_executors().push(e);
            }
        }
        if d["useSetEnc"].as_bool().unwrap_or(false) {
            set_encode_type(&dctx, &mut dag);
        }
        b.set_dag_request(dag);
    }

    b.set_start_ts(spec["startTS"].as_u64().unwrap_or(0));
    b.set_desc(spec["desc"].as_bool().unwrap_or(false));
    b.set_keep_order(spec["keepOrder"].as_bool().unwrap_or(false));
    b.set_store_type(StoreType(spec["storeType"].as_u64().unwrap_or(0) as u8));
    b.set_allow_batch_cop(spec["allowBatchCop"].as_bool().unwrap_or(false));
    let set_from_session = spec["setFromSession"].as_bool().unwrap_or(false);
    if set_from_session {
        b.set_from_session_vars(&dctx);
    }
    if let Some(v) = spec["overridePaging"].as_bool() {
        b.set_paging(v);
    }
    if let Some(v) = spec["overrideConc"].as_i64() {
        b.set_concurrency(v as isize);
    }
    if spec["hasRateLimit"].as_bool().unwrap_or(false) {
        b.set_copr_request_rate_limit(None);
    }
    let sid = spec["serverID"].as_u64().unwrap_or(0);
    if sid != 0 {
        b.set_tidb_server_id(sid);
    }
    if let Some(s) = spec["overrideRGName"].as_str() {
        b.set_resource_group_name(s.to_owned());
    }
    if let Some(s) = spec["overrideExplicitSrc"].as_str() {
        b.set_explicit_request_source_type(s.to_owned());
    }
    if set_tagger && !set_from_session {
        b.set_resource_group_tagger(dctx.resource_group_tagger.clone());
    }
    if let Some(s) = spec["txnScope"].as_str()
        && !s.is_empty()
    {
        b.set_txn_scope(s.to_owned());
    }
    if let Some(s) = spec["readReplicaScope"].as_str()
        && !s.is_empty()
    {
        b.set_read_replica_scope(s.to_owned());
    }
    if spec["isStaleness"].as_bool().unwrap_or(false) {
        b.set_is_staleness(true);
    }
    if spec["hasAdjuster"].as_bool().unwrap_or(false) {
        b.set_closest_replica_read_adjuster(Some(Arc::new(|_: &mut Request, _: isize| false)));
    }
    if spec["setConnID"].as_bool().unwrap_or(false) {
        b.set_conn_id_and_conn_alias(
            spec["connID"].as_u64().unwrap_or(0),
            spec["connAlias"].as_str().unwrap_or("").to_owned(),
        );
    }
    b.set_from_info_schema(Arc::new(NullMetaIS));

    let req = match b.build() {
        Ok(r) => r,
        Err(e) => return json!({ "err": e.to_string() }),
    };

    let labels: Vec<Value> = req
        .match_store_labels
        .iter()
        .map(|l| json!([l.key, l.value]))
        .collect();
    let tag_key_hex = spec["tagKeyHex"].as_str().unwrap_or("");
    let tag_out = if let Some(tagger) = &req.resource_group_tagger
        && !tag_key_hex.is_empty()
    {
        let tk = unhex(tag_key_hex);
        let raw_tag = encode_tag_with_key(tagger, &tk);
        let mut pb_tag = tipb::ResourceGroupTag::default();
        pb_tag.merge_from_bytes(&raw_tag).unwrap();
        json!([pb_tag.get_table_id(), pb_tag.get_label() as i32])
    } else {
        Value::Null
    };
    let label_by_key = get_resource_group_label_by_key(&unhex(tag_key_hex)) as i32;
    let verify_global = verify_txn_scope(
        tidb_kv::txn_scope_var::GlobalTxnScope,
        spec["tid"].as_i64().unwrap_or(0),
        &NullMetaIS,
    )
    .unwrap();
    let iso_level = match req.isolation_level {
        x if x == SI => 0,
        x if x == RC => 1,
        x if x == RCCheckTS => 2,
        other => other.0 as i32,
    };

    json!({
        "tp": req.tp,
        "startTS": req.start_ts,
        "dataHex": hex(&req.data),
        "conc": req.concurrency,
        "keepOrder": req.keep_order,
        "desc": req.desc,
        "notFillCache": req.not_fill_cache,
        "cacheable": req.cacheable,
        "isoLevel": iso_level,
        "priority": req.priority,
        "replicaRead": req.replica_read.0,
        "storeType": req.store_type.0,
        "allowBatchCop": req.batch_cop,
        "paging": [req.paging.enable, req.paging.min_paging_size, req.paging.max_paging_size],
        "storeBatchSize": req.store_batch_size,
        "busyMs": req.store_busy_threshold.as_millis() as i64,
        "readTimeout": req.tikv_client_read_timeout,
        "rgName": req.resource_group_name,
        "txnScope": req.txn_scope,
        "readReplicaScope": req.read_replica_scope,
        "isStaleness": req.is_staleness,
        "matchLabels": labels,
        "kr": key_ranges_out(req.key_ranges.as_ref()),
        "limitSize": req.limit_size,
        "serverID": req.tidb_server_id,
        "connID": req.conn_id,
        "connAlias": req.conn_alias,
        "reqSrc": [
            req.request_source.request_source_internal,
            req.request_source.request_source_type,
            req.request_source.explicit_request_source_type,
        ],
        "tag": tag_out,
        "labelByKey": label_by_key,
        "verifyGlobal": verify_global,
        "endian": get_system_endian() as i32,
    })
}

fn exec_select(spec: &Value) -> Value {
    let fts_owned: Vec<FieldType> = spec["cols"]
        .as_array()
        .map(|a| a.iter().map(field_type_in).collect())
        .unwrap_or_default();
    let fts_arc: Vec<Arc<FieldType>> = fts_owned.iter().cloned().map(Arc::new).collect();

    let packets: Vec<MockPacket> = spec["packets"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    let stats = cop_stats_in(&p["stats"]);
                    let resp_ns = p["stats"]["respNs"].as_i64().unwrap_or(0);
                    MockPacket {
                        data: unhex(p["dataHex"].as_str().unwrap_or("")),
                        resp_time: Duration::from_nanos(resp_ns.max(0) as u64),
                        stats,
                        next_err: p["nextErr"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let unconsumed: Vec<CopRuntimeStats> = spec["unconsumed"]
        .as_array()
        .map(|a| a.iter().filter_map(cop_stats_in).collect())
        .unwrap_or_default();

    let conc = spec["conc"].as_i64().unwrap_or(0) as isize;
    let extra_conc = spec["extraConc"].as_i64().unwrap_or(0) as isize;
    let resp = MockResp {
        packets,
        idx: 0,
        conc,
        extra_conc,
        batch_num: spec["batchNum"].as_u64().unwrap_or(0),
        batch_fallback: spec["batchFallback"].as_u64().unwrap_or(0),
        build_elapsed: Duration::from_nanos(
            spec["buildElapsedNs"].as_i64().unwrap_or(0).max(0) as u64
        ),
        unconsumed,
    };
    let client: Arc<dyn Client> = Arc::new(MockClient(Mutex::new(Some(Box::new(resp)))));
    let warns = WarnRecorder::new();
    let warn_ref: WarnAppenderRef = warns.clone();
    let tracker = spec["useMemTracker"]
        .as_bool()
        .unwrap_or(false)
        .then(|| new_tracker(-1, -1));
    let with_stats = spec["withStats"].as_bool().unwrap_or(false);
    let coll = with_stats.then(|| Arc::new(RuntimeStatsColl::default()));
    let cpu_usage = Arc::new(SQLCPUUsages::default());
    let dctx = Arc::new(DistSQLContext {
        client: Some(client),
        warn_handler: Some(warn_ref),
        location: loc_of(spec["tzOffset"].as_i64().unwrap_or(0) as i32),
        err_ctx: errctx::StrictNoWarningContext.clone(),
        session_mem_tracker: tracker.clone(),
        runtime_stats_coll: coll.clone(),
        sql_killer: Some(Arc::new(SqlKiller::default())),
        cpu_usage: Arc::clone(&cpu_usage),
        exec_details: Arc::new(SyncExecDetails::default()),
        kv_vars: Variables::default(),
        ..Default::default()
    });
    let kv_req = Request {
        tp: ReqTypeDAG,
        store_type: tidb_base::kv::TiKV,
        concurrency: conc,
        mem_tracker: tracker.clone(),
        ..Default::default()
    };

    let cop_plan_ids: Vec<isize> = spec["copPlanIDs"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_i64().unwrap() as isize).collect())
        .unwrap_or_default();
    let root_plan_id = spec["rootPlanID"].as_i64().unwrap_or(0) as isize;
    let ctx = tidb_kv::Context::background();

    let sr = futures::executor::block_on(async {
        if with_stats {
            select_with_runtime_stats(
                &ctx,
                Arc::clone(&dctx),
                &kv_req,
                fts_arc.clone(),
                cop_plan_ids.clone(),
                root_plan_id,
            )
            .await
        } else {
            select(&ctx, Arc::clone(&dctx), &kv_req, fts_arc.clone()).await
        }
    });
    let sr = match sr {
        Ok(s) => s,
        Err(e) => return json!({ "err": e.to_string() }),
    };
    let mut sr = sr;

    let conc_out = match get_select_result_concurrency(sr.as_ref()) {
        Some((c1, c2)) => json!([c1, c2, true]),
        None => json!([0, 0, false]),
    };

    if spec["rawMode"].as_bool().unwrap_or(false) {
        let mut raw_chunks: Option<Vec<String>> = None;
        let mut raw_err = String::new();
        loop {
            match futures::executor::block_on(sr.next_raw(&ctx)) {
                Ok(Some(b)) => raw_chunks.get_or_insert_with(Vec::new).push(hex(&b)),
                Ok(None) => break,
                Err(e) => {
                    raw_err = e.to_string();
                    break;
                }
            }
        }
        let _ = sr.close();
        let mem_after = tracker.as_ref().map_or(0, |t| t.bytes_consumed());
        let w = warns.take();
        return json!({
            "conc": conc_out,
            "raw": raw_chunks,
            "err": raw_err,
            "warns": if w.is_empty() { Value::Null } else { json!(w) },
            "memAfter": mem_after,
        });
    }

    let req_rows_spec: Vec<usize> = spec["reqRows"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_u64().unwrap_or(32) as usize)
                .collect()
        })
        .unwrap_or_default();
    let mut batches: Option<Vec<Vec<String>>> = None;
    let mut next_err = String::new();
    let mut step = 0usize;
    loop {
        let rr = if req_rows_spec.is_empty() {
            32
        } else if step < req_rows_spec.len() {
            req_rows_spec[step]
        } else {
            *req_rows_spec.last().unwrap()
        };
        step += 1;
        let mut chk = new_chunk_with_capacity(&fts_owned, 32);
        chk.set_required_rows(rr as isize, 1024);
        match futures::executor::block_on(sr.next(&ctx, &mut chk)) {
            Ok(()) => {
                if chk.num_rows() == 0 {
                    break;
                }
                let mut rows = Vec::with_capacity(chk.num_rows());
                for r in 0..chk.num_rows() {
                    rows.push(
                        String::from_utf8_lossy(&chk.get_row(r).to_string(&fts_owned)).into_owned(),
                    );
                }
                batches.get_or_insert_with(Vec::new).push(rows);
            }
            Err(e) => {
                next_err = e.to_string();
                break;
            }
        }
        assert!(step <= 200, "select_decode: runaway Next loop");
    }
    let _ = sr.close();
    let mem_after = tracker.as_ref().map_or(0, |t| t.bytes_consumed());
    let mut root_stats_str = String::new();
    let mut cop_stats_strs: Option<Vec<String>> = None;
    if let Some(c) = &coll
        && root_plan_id > 0
    {
        if c.exists_root_stats(root_plan_id as i64) {
            root_stats_str = c.get_root_stats(root_plan_id as i64).string();
        }
        if !cop_plan_ids.is_empty() {
            let mut v = Vec::with_capacity(cop_plan_ids.len());
            for &pid in &cop_plan_ids {
                v.push(c.get_cop_stats(pid as i64).map_or_else(String::new, |cs| {
                    cs.lock().unwrap_or_else(|e| e.into_inner()).string()
                }));
            }
            cop_stats_strs = Some(v);
        }
    }
    let w = warns.take();
    json!({
        "conc": conc_out,
        "batches": batches,
        "err": next_err,
        "warns": if w.is_empty() { Value::Null } else { json!(w) },
        "memAfter": mem_after,
        "rootStats": root_stats_str,
        "copStats": cop_stats_strs,
        "tikvCPU": cpu_usage.get_cpu_usages().tikv_cpu_time.as_nanos() as i64,
    })
}

fn make_stats(
    entries: &Value,
    conc: isize,
    extra: isize,
    batch: u64,
    fallback: u64,
    build_ns: i64,
) -> SelectResultRuntimeStats {
    let mut s = SelectResultRuntimeStats {
        dist_sql_concurrency: conc,
        extra_concurrency: extra,
        store_batched_num: batch,
        store_batched_fallback_num: fallback,
        build_task_duration: build_ns,
        ..Default::default()
    };
    if let Some(arr) = entries.as_array() {
        for e in arr {
            if let Some(cs) = cop_stats_in(&e["stats"]) {
                let ns = e["respNs"].as_i64().unwrap_or(0).max(0) as u64;
                s.merge_cop_runtime_stats(&cs, Duration::from_nanos(ns));
            }
        }
    }
    s
}

fn exec_stats(spec: &Value) -> Value {
    let cache_enabled = spec["cacheEnabled"].as_bool().unwrap_or(true);
    let mut a = make_stats(
        &spec["a"],
        spec["concA"].as_i64().unwrap_or(0) as isize,
        spec["extraA"].as_i64().unwrap_or(0) as isize,
        spec["batchA"].as_u64().unwrap_or(0),
        spec["fallbackA"].as_u64().unwrap_or(0),
        spec["buildNsA"].as_i64().unwrap_or(0),
    );
    let b = make_stats(
        &spec["b"],
        spec["concB"].as_i64().unwrap_or(0) as isize,
        spec["extraB"].as_i64().unwrap_or(0) as isize,
        spec["batchB"].as_u64().unwrap_or(0),
        spec["fallbackB"].as_u64().unwrap_or(0),
        spec["buildNsB"].as_i64().unwrap_or(0),
    );
    let a_str = a.string_with_cache_enabled(cache_enabled);
    let a_tp = a.tp();
    let c_box = a.clone_stats();
    let c_ref = c_box
        .as_any()
        .downcast_ref::<SelectResultRuntimeStats>()
        .unwrap();
    let c_str_before = c_ref.string_with_cache_enabled(cache_enabled);
    a.merge(&b);
    let a_str_after = a.string_with_cache_enabled(cache_enabled);
    let c_str_after = c_ref.string_with_cache_enabled(cache_enabled);
    json!({
        "a": a_str,
        "tp": a_tp,
        "clone0": c_str_before,
        "merged": a_str_after,
        "clone1": c_str_after,
    })
}

// ---------------------------------------------------------------- Part 2 replay

fn handle_in(v: &Value) -> Handle {
    match v["k"].as_str().unwrap() {
        "int" => Handle::Int(IntHandle(v["v"].as_i64().unwrap_or(0))),
        "common" => {
            let raw = unhex(v["hex"].as_str().unwrap_or(""));
            Handle::Common(Arc::new(new_common_handle(raw).unwrap()))
        }
        "part" => {
            let pid = v["pid"].as_i64().unwrap_or(0);
            let inner = handle_in(&v["h"]);
            Handle::Partition(new_partition_handle(pid, inner))
        }
        other => panic!("handle_in: unknown kind {other}"),
    }
}

fn handles_in(v: &Value) -> Vec<Handle> {
    v.as_array()
        .map(|arr| arr.iter().map(handle_in).collect())
        .unwrap_or_default()
}

fn exec_index_and_handles(args: &[Value]) -> Value {
    match args[0].as_str().unwrap() {
        "table_handles" => {
            let tid = args[1].as_i64().unwrap();
            let hs = handles_in(&args[2]);
            let (krs, hints) = table_handles_to_kv_ranges(tid, &hs);
            let req = RequestBuilder::default()
                .set_table_handles(tid, &hs)
                .build()
                .unwrap();
            json!({
                "ranges": kv_ranges_out(&krs),
                "hints": hints,
                "reqKR": key_ranges_out(req.key_ranges.as_ref()),
            })
        }
        "part_handles" => {
            let hs = handles_in(&args[1]);
            let (krs, hints) = partition_handles_to_kv_ranges(&hs);
            let req = RequestBuilder::default()
                .set_partitions_and_handles(&hs)
                .build()
                .unwrap();
            json!({
                "ranges": kv_ranges_out(&krs),
                "hints": hints,
                "reqKR": key_ranges_out(req.key_ranges.as_ref()),
            })
        }
        "index_ranges" => {
            let tid = args[1].as_i64().unwrap();
            let idx_id = args[2].as_i64().unwrap();
            let rs = ranges_in(&args[3]);
            let tz_offset = args[4].as_i64().unwrap() as i32;
            let strict = args[5].as_bool().unwrap();
            let interrupt = args[6].as_bool().unwrap();
            let dctx = DistSQLContext {
                location: loc_of(tz_offset),
                err_ctx: err_ctx_of(strict),
                ..Default::default()
            };
            let sig = AtomicBool::new(interrupt);
            let tracker = new_tracker(-1, -1);
            if interrupt {
                return match index_ranges_to_kv_ranges_with_interrupt_signal(
                    Some(&dctx),
                    tid,
                    idx_id,
                    &rs,
                    Some(&tracker),
                    Some(&sig),
                ) {
                    Ok(kr_sig) => json!({ "krSig": key_ranges_out(Some(&kr_sig)) }),
                    Err(e) => json!({ "err": e.to_string() }),
                };
            }
            let kr = match index_ranges_to_kv_ranges(Some(&dctx), tid, idx_id, &rs) {
                Ok(k) => k,
                Err(e) => return json!({ "err": e.to_string() }),
            };
            let kr_sig = match index_ranges_to_kv_ranges_with_interrupt_signal(
                Some(&dctx),
                tid,
                idx_id,
                &rs,
                Some(&tracker),
                Some(&sig),
            ) {
                Ok(k) => k,
                Err(e) => return json!({ "err": e.to_string() }),
            };
            json!({
                "kr": key_ranges_out(Some(&kr)),
                "krSig": key_ranges_out(Some(&kr_sig)),
            })
        }
        other => panic!("index_and_handles: unknown sub {other}"),
    }
}

fn col_spec_to_ft(c: &Value) -> FieldType {
    let mut ft = FieldType::new(c["tp"].as_u64().unwrap() as u8);
    ft.set_flag(c["flag"].as_u64().unwrap_or(0) as usize);
    ft.set_flen(c["flen"].as_i64().unwrap_or(-1) as isize);
    ft.set_decimal(c["dec"].as_i64().unwrap_or(0) as isize);
    ft.set_charset(c["cs"].as_str().unwrap_or(""));
    ft.set_collate(c["coll"].as_str().unwrap_or(""));
    ft
}

fn exec_sorted(spec: &Value) -> Value {
    let fts_owned: Vec<FieldType> = spec["cols"]
        .as_array()
        .map(|a| a.iter().map(col_spec_to_ft).collect())
        .unwrap_or_default();
    let fts_arc: Vec<Arc<FieldType>> = fts_owned.iter().cloned().map(Arc::new).collect();
    let cols: Vec<Arc<Column>> = fts_arc
        .iter()
        .enumerate()
        .map(|(i, ft)| {
            Arc::new(Column {
                id: (i + 1) as i64,
                unique_id: (i + 1) as i64,
                index: i as isize,
                ret_type: Arc::clone(ft),
                ..Default::default()
            })
        })
        .collect();
    let schema = spec["useSchema"]
        .as_bool()
        .unwrap_or(false)
        .then(|| new_schema(cols.clone()));
    let by_items: Vec<ByItems> = spec["by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let col_idx = b["col"].as_u64().unwrap() as usize;
            ByItems {
                expr: Expr::Column((*cols[col_idx]).clone()).into_ref(),
                desc: b["desc"].as_bool().unwrap_or(false),
            }
        })
        .collect();

    let tz_offset = spec["tzOffset"].as_i64().unwrap_or(0) as i32;
    let ctx = tidb_kv::Context::background();
    let mut children: Vec<Box<dyn SelectResult>> = Vec::new();
    for ch in spec["children"].as_array().unwrap() {
        let pkts: Vec<MockPacket> = ch
            .as_array()
            .unwrap()
            .iter()
            .map(|h| MockPacket {
                data: unhex(h.as_str().unwrap()),
                stats: None,
                resp_time: Duration::ZERO,
                next_err: None,
            })
            .collect();
        let resp = MockResp {
            packets: pkts,
            idx: 0,
            conc: 1,
            extra_conc: 0,
            batch_num: 0,
            batch_fallback: 0,
            build_elapsed: Duration::ZERO,
            unconsumed: Vec::new(),
        };
        let dctx = Arc::new(DistSQLContext {
            client: Some(Arc::new(MockClient(Mutex::new(Some(Box::new(resp)))))),
            warn_handler: Some(ignore_warn_appender()),
            location: loc_of(tz_offset),
            err_ctx: err_ctx_of(true),
            sql_killer: Some(Arc::new(SqlKiller::default())),
            cpu_usage: Arc::new(SQLCPUUsages::default()),
            exec_details: Arc::new(SyncExecDetails::default()),
            kv_vars: Variables::default(),
            ..Default::default()
        });
        let kv_req = Request {
            tp: ReqTypeDAG,
            store_type: tidb_base::kv::TiKV,
            ..Default::default()
        };
        let sr = futures::executor::block_on(select(&ctx, dctx, &kv_req, fts_arc.clone())).unwrap();
        children.push(sr);
    }

    let tracker = new_tracker(-1, -1);
    let ectx = new_eval_context(vec![]);
    let mut ssr = new_sorted_select_results(
        ectx.as_ref(),
        children,
        schema.as_ref(),
        by_items,
        Some(Arc::clone(&tracker)),
        None,
        None,
    )
    .unwrap();

    let into_iter_err = match ssr.into_iter(vec![]) {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    };

    let mut next_raw_panic = String::new();
    if spec["testNextRaw"].as_bool().unwrap_or(false) {
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = futures::executor::block_on(ssr.next_raw(&ctx));
        }));
        if let Err(payload) = res {
            next_raw_panic = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
        }
    }

    let req_rows_spec: Vec<usize> = spec["reqRows"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_u64().unwrap_or(32) as usize)
                .collect()
        })
        .unwrap_or_default();
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut next_err = String::new();
    let mut step = 0usize;
    loop {
        let rr = if req_rows_spec.is_empty() {
            32
        } else if step < req_rows_spec.len() {
            req_rows_spec[step]
        } else {
            *req_rows_spec.last().unwrap()
        };
        step += 1;
        let mut chk = new_chunk_with_capacity(&fts_owned, 32);
        chk.set_required_rows(rr as isize, 1024);
        match futures::executor::block_on(ssr.next(&ctx, &mut chk)) {
            Ok(()) => {
                if chk.num_rows() == 0 {
                    break;
                }
                let mut rows = Vec::with_capacity(chk.num_rows());
                for r in 0..chk.num_rows() {
                    rows.push(
                        String::from_utf8_lossy(&chk.get_row(r).to_string(&fts_owned)).into_owned(),
                    );
                }
                batches.push(rows);
            }
            Err(e) => {
                next_err = e.to_string();
                break;
            }
        }
        assert!(step <= 200, "sorted_and_iter: runaway Next loop");
    }
    let _ = ssr.close();
    assert_eq!(tracker.bytes_consumed(), 0);
    json!({
        "batches": batches,
        "err": next_err,
        "intoIterErr": into_iter_err,
        "nextRawPanic": next_raw_panic,
    })
}

fn exec_iter(spec: &Value) -> Value {
    let main_owned: Vec<FieldType> = spec["mainCols"]
        .as_array()
        .map(|a| a.iter().map(col_spec_to_ft).collect())
        .unwrap_or_default();
    let main_arc: Vec<Arc<FieldType>> = main_owned.iter().cloned().map(Arc::new).collect();
    let all_inter_owned: Vec<Vec<FieldType>> = spec["interCols"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|cols| {
                    cols.as_array()
                        .unwrap()
                        .iter()
                        .map(col_spec_to_ft)
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    let pass_count = spec["passInterCount"].as_i64().unwrap_or(-1);
    let pass_inter_owned: &[Vec<FieldType>] =
        if pass_count >= 0 && (pass_count as usize) <= all_inter_owned.len() {
            &all_inter_owned[..pass_count as usize]
        } else {
            &all_inter_owned
        };
    let pass_inter_arc: Vec<Vec<Arc<FieldType>>> = pass_inter_owned
        .iter()
        .map(|v| v.iter().cloned().map(Arc::new).collect())
        .collect();

    let pkts: Vec<MockPacket> = spec["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| MockPacket {
            data: unhex(h.as_str().unwrap()),
            stats: None,
            resp_time: Duration::ZERO,
            next_err: None,
        })
        .collect();
    let resp = MockResp {
        packets: pkts,
        idx: 0,
        conc: 1,
        extra_conc: 0,
        batch_num: 0,
        batch_fallback: 0,
        build_elapsed: Duration::ZERO,
        unconsumed: Vec::new(),
    };
    let warns = WarnRecorder::new();
    let tz_offset = spec["tzOffset"].as_i64().unwrap_or(0) as i32;
    let dctx = Arc::new(DistSQLContext {
        client: Some(Arc::new(MockClient(Mutex::new(Some(Box::new(resp)))))),
        warn_handler: Some(Arc::clone(&warns) as WarnAppenderRef),
        location: loc_of(tz_offset),
        err_ctx: err_ctx_of(true),
        sql_killer: Some(Arc::new(SqlKiller::default())),
        cpu_usage: Arc::new(SQLCPUUsages::default()),
        exec_details: Arc::new(SyncExecDetails::default()),
        kv_vars: Variables::default(),
        ..Default::default()
    });
    let kv_req = Request {
        tp: ReqTypeDAG,
        store_type: tidb_base::kv::TiKV,
        ..Default::default()
    };
    let ctx = tidb_kv::Context::background();
    let mut sr = futures::executor::block_on(select(&ctx, dctx, &kv_req, main_arc)).unwrap();
    let mut iter = sr.into_iter(pass_inter_arc.clone()).unwrap();

    let post_errs: Option<Vec<String>> = if spec["checkPostIter"].as_bool().unwrap_or(false) {
        let e1 = futures::executor::block_on(sr.next_raw(&ctx))
            .unwrap_err()
            .to_string();
        let mut dummy_chk = new_chunk_with_capacity(&main_owned, 1);
        let e2 = futures::executor::block_on(sr.next(&ctx, &mut dummy_chk))
            .unwrap_err()
            .to_string();
        let e3 = sr.into_iter(pass_inter_arc).err().unwrap().to_string();
        let e4 = sr.close().unwrap_err().to_string();
        Some(vec![e1, e2, e3, e4])
    } else {
        None
    };

    let mut rows: Vec<Value> = Vec::new();
    let mut iter_err = String::new();
    loop {
        match futures::executor::block_on(iter.next(&ctx)) {
            Ok(Some(row)) => {
                let ch = row.channel_index as usize;
                let fts = if ch == pass_inter_owned.len() {
                    &main_owned
                } else {
                    &pass_inter_owned[ch]
                };
                let s = String::from_utf8_lossy(&row.row.row().to_string(fts)).into_owned();
                rows.push(json!([row.channel_index, s]));
            }
            Ok(None) => break,
            Err(e) => {
                iter_err = e.to_string();
                break;
            }
        }
    }
    let _ = iter.close();
    let w = warns.take();
    json!({
        "rows": rows,
        "err": iter_err,
        "warns": if w.is_empty() { Value::Null } else { json!(w) },
        "postErrs": post_errs,
    })
}

fn exec(op: &str, args: &[Value]) -> Value {
    collate::set_new_collation_enabled_for_test(true);
    match op {
        "split_int64" => {
            let rs = ranges_in(&args[0]);
            let keep_order = args[1].as_bool().unwrap();
            let desc = args[2].as_bool().unwrap();
            let is_common = args[3].as_bool().unwrap();
            let (r1, r2) = split_ranges_across_int64_boundary(&rs, keep_order, desc, is_common);
            json!([ranges_out(&r1), ranges_out(&r2)])
        }
        "table_ranges" => {
            let mode = args[0].as_str().unwrap();
            let tids: Vec<i64> = args[1]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_i64().unwrap()).collect())
                .unwrap_or_default();
            let is_common = args[2].as_bool().unwrap();
            let rs = ranges_in(&args[3]);
            let tz_offset = args[4].as_i64().unwrap() as i32;
            let strict = args[5].as_bool().unwrap();
            let dctx = DistSQLContext {
                location: loc_of(tz_offset),
                err_ctx: err_ctx_of(strict),
                ..Default::default()
            };
            match mode {
                "handle" => {
                    match table_handle_ranges_to_kv_ranges(Some(&dctx), &tids, is_common, &rs) {
                        Ok(krs) => key_ranges_out(Some(&krs)),
                        Err(e) => json!({ "err": e.to_string() }),
                    }
                }
                "table" => {
                    let krs = table_ranges_to_kv_ranges(tids[0], &rs);
                    json!({ "ranges": kv_ranges_out(&krs) })
                }
                "common" => match common_handle_ranges_to_kv_ranges(Some(&dctx), &tids, &rs) {
                    Ok(krs) => key_ranges_out(Some(&krs)),
                    Err(e) => json!({ "err": e.to_string() }),
                },
                "index_key" => match encode_index_key(Some(&dctx), &rs[0]) {
                    Ok((low, high)) => json!({ "low": hex(&low), "high": hex(&high) }),
                    Err(e) => json!({ "err": e.to_string() }),
                },
                other => panic!("table_ranges: unknown mode {other}"),
            }
        }
        "request_builder" => exec_request_builder(&args[0]),
        "select_decode" => match args[0].as_str().unwrap() {
            "select" => exec_select(&args[1]),
            "stats" => exec_stats(&args[1]),
            other => panic!("select_decode: unknown sub {other}"),
        },
        "index_and_handles" => exec_index_and_handles(args),
        "sorted_and_iter" => match args[0].as_str().unwrap() {
            "sorted" => exec_sorted(&args[1]),
            "iter" => exec_iter(&args[1]),
            other => panic!("sorted_and_iter: unknown sub {other}"),
        },
        other => panic!("unknown op {other}"),
    }
}

fn replay(path: &str) -> usize {
    let mut s = String::new();
    flate2::read::GzDecoder::new(
        std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}")),
    )
    .read_to_string(&mut s)
    .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "distsql");
    let mut n = 0;
    let mut fails = Vec::new();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let err = &rec["err"];
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
        n += 1;
        if !err.is_null() {
            if err["name"] == "panic" {
                if let Ok(v) = got {
                    fails.push(format!("{line}\n  Go panicked, Rust returned {v:?}"));
                }
                continue;
            }
            panic!("unexpected Go error record: {line}");
        }
        let want = &rec["out"];
        match got {
            Ok(ref got) if got == want => {}
            Ok(got) => fails.push(format!("{line}\n  got  {got}\n  want {want}")),
            Err(e) => fails.push(format!(
                "{line}\n  Rust panicked: {:?}",
                e.downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
            )),
        }
    }
    if !fails.is_empty() {
        panic!(
            "{} of {n} records differ, first ones:\n{}",
            fails.len(),
            fails[..fails.len().min(5)].join("\n")
        );
    }
    n
}

#[test]
fn govec_distsql() {
    let n = replay(&vectors());
    assert!(n >= 950, "{n} records");
    if let Ok(files) = std::env::var("GOVEC_DISTSQL_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
