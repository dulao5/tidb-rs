//! Replays the govec `readexec` vectors (tidb-rs-tracker `govec/domains/readexec.go`):
//! block D12.1 Part 2A (`TableReaderExecutor`, `buildTableReader`, `buildNoRangeTableReader`,
//! `newClosestReadAdjuster`, `tableResultHandler`, `closeAll`, `getPhysicalPlanIDs`) and
//! Part 2B (`IndexReaderExecutor`, `buildIndexReader`, `buildNoRangeIndexReader`,
//! `buildKeyRanges`, `needMergeSort`, `newIndexReaderExecutorContext`) against Go TiDB v8.5.8.
//!
//! `GOVEC_READEXEC_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::io::Read;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::config;
use tidb_base::kv::TiKV;
use tidb_base::mysql::{
    BinaryFlag, NotNullFlag, PriKeyFlag, TypeLonglong, TypeVarchar, UnsignedFlag,
};
use tidb_chunk::chunk::{self, Chunk};
use tidb_codec::codec::encode_key;
use tidb_execdetails::execdetails::{RuntimeStats, RuntimeStatsColl};
use tidb_executor::adapter::TelemetryInfo;
use tidb_executor::builder::{
    build_index_scan_output_offsets, new_closest_read_adjuster, new_executor_builder,
};
use tidb_executor::distsql::{
    IndexLookUpExecutor, IndexLookUpRunTimeStats, calculate_batch_size,
    get_index_scan_max_in_flight, get_lack_handles, get_merge_sort_index_scan_concurrency,
};
use tidb_executor::exec::ExecCtx;
use tidb_executor::glue::{InfoSchemaRef, Plan, SessionContext};
use tidb_executor::internal::exec::executor::{self as exec, Executor};
use tidb_executor::table_reader::SelectResultHook;
use tidb_expression::Column;
use tidb_expression::constant::Constant;
use tidb_expression::expression::Expr;
use tidb_expression::scalar_function::new_function;
use tidb_expression::schema::new_schema;
use tidb_kv::key::{Handle, IntHandle, new_common_handle, new_handle_map};
use tidb_kv::kv::{
    Client as KvClient, ClientSendOption, KvFuture, OptionValue, Request, Response, ResultSubset,
    Snapshot,
};
use tidb_kv::option::{
    ReplicaReadClosest, ReplicaReadClosestAdaptive, ReplicaReadFollower, ReplicaReadLeader,
    ReplicaReadMixed, ReplicaReadType,
};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;
use tidb_model::{
    ColumnInfo, DBInfo, IndexColumn, IndexInfo, PartitionInfo, StatePublic, TableInfo,
    TempTableLocal,
};
use tidb_parser::ast::funcs::{Mul, Plus};
use tidb_parser::gocharset::{CharsetUTF8MB4, CollationBin};
use tidb_parser::model::CIStr;
use tidb_parser::types::{ETString, FieldType, UnspecifiedLength};
use tidb_planner::core::physical_plans::{
    Cop, PhysicalIndexLookUpReader, PhysicalIndexReader, PhysicalIndexScan, PhysicalLimit,
    PhysicalLocalIndexLookUp, PhysicalTableReader, PhysicalTableScan, PushedDownLimit,
};
use tidb_planner::property::StatsInfo;
use tidb_planner::util::byitem::ByItems;
use tidb_planner::{PhysicalPlan, PhysicalPlanRef};
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::{BuildContextRef, ExprContextRef};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::Range;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_statistics::HistColl;
use tidb_store::distsql::{SelectResult, SelectResultIter};
use tidb_tablecodec::encode_row_key_with_handle;
use tidb_types::datum::{
    Datum, new_collation_string_datum, new_int_datum, new_string_datum, new_uint_datum,
};
use tidb_types::field_type::new_field_type;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

const EXPECTED_BLOCKED: usize = 0;

fn vectors(domain: &str) -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/{domain}.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn read_records(path: &str, domain: &str) -> Vec<String> {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], domain);
    lines.map(str::to_owned).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_replica_read(s: &str) -> ReplicaReadType {
    match s {
        "leader" => ReplicaReadLeader,
        "follower" => ReplicaReadFollower,
        "mixed" => ReplicaReadMixed,
        "closest-adaptive" => ReplicaReadClosestAdaptive,
        "closest" => ReplicaReadClosest,
        other => panic!("unknown replica read: {other}"),
    }
}

fn format_replica_read(t: ReplicaReadType) -> &'static str {
    if t == ReplicaReadLeader {
        "leader"
    } else if t == ReplicaReadFollower {
        "follower"
    } else if t == ReplicaReadMixed {
        "mixed"
    } else if t == ReplicaReadClosestAdaptive {
        "closest-adaptive"
    } else if t == ReplicaReadClosest {
        "closest"
    } else {
        panic!("unexpected replica read type: {t:?}")
    }
}

// ---------------------------------------------------------------- closest_read_adjuster

fn replay_closest_read_adjuster(args: &[Value]) -> Value {
    let rr = args[0].as_str().unwrap();
    let threshold = args[1].as_i64().unwrap();
    let net_data_size = args[2].as_f64().unwrap();
    let cop_task_count = args[3].as_i64().unwrap() as isize;
    let zone = args[4].as_str().unwrap();

    let restore = config::restore_func();
    config::update_global(|cfg| {
        let labels = cfg.labels.get_or_insert_with(Default::default);
        if zone.is_empty() {
            labels.remove("zone");
        } else {
            labels.insert("zone".to_owned(), zone.to_owned());
        }
    });

    let dctx = Arc::new(DistSQLContext {
        replica_closest_read_threshold: threshold,
        ..Default::default()
    });

    let mut req = Request {
        replica_read: parse_replica_read(rr),
        ..Default::default()
    };

    let out = match new_closest_read_adjuster(&dctx, &req, net_data_size) {
        None => json!({
            "nil_adjuster": true,
            "adjusted": false,
            "replica_read": format_replica_read(req.replica_read),
            "match_store_labels": [],
        }),
        Some(adj) => {
            let adjusted = adj(&mut req, cop_task_count);
            let labels: Vec<Value> = req
                .match_store_labels
                .iter()
                .map(|l| json!({ "key": l.key, "value": l.value }))
                .collect();
            json!({
                "nil_adjuster": false,
                "adjusted": adjusted,
                "replica_read": format_replica_read(req.replica_read),
                "match_store_labels": labels,
            })
        }
    };

    restore();
    out
}

// ---------------------------------------------------------------- table_reader

struct RangeSpec {
    low: Vec<Value>,
    high: Vec<Value>,
    low_exclude: bool,
    high_exclude: bool,
}

#[derive(Clone, Default)]
struct StreamSpec {
    open_err: String,
    rows: Vec<Vec<Option<i64>>>,
    next_err: String,
    err_after: usize,
    close_err: String,
}

struct TableReaderSpec {
    table_id: i64,
    handle_kind: String,
    virtual_cols: bool,
    temp_table: bool,
    set_dummy_call: bool,
    keep_order: bool,
    desc: bool,
    paging: bool,
    concurrency: usize,
    replica_read: String,
    threshold: i64,
    max_chunk_size: usize,
    ranges: Vec<RangeSpec>,
    streams: Vec<StreamSpec>,
    required_rows: Vec<isize>,
}

fn parse_table_reader_spec(v: &Value) -> TableReaderSpec {
    let ranges = v["ranges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| RangeSpec {
                    low: r["low"].as_array().cloned().unwrap_or_default(),
                    high: r["high"].as_array().cloned().unwrap_or_default(),
                    low_exclude: r["low_exclude"].as_bool().unwrap_or(false),
                    high_exclude: r["high_exclude"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    let streams = v["streams"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| StreamSpec {
                    open_err: s["open_err"].as_str().unwrap_or("").to_owned(),
                    rows: s["rows"]
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .map(|row| {
                                    row.as_array()
                                        .unwrap()
                                        .iter()
                                        .map(|cell| cell.as_i64())
                                        .collect()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    next_err: s["next_err"].as_str().unwrap_or("").to_owned(),
                    err_after: s["err_after"].as_u64().unwrap_or(0) as usize,
                    close_err: s["close_err"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let required_rows = v["required_rows"]
        .as_array()
        .map(|arr| arr.iter().map(|x| x.as_i64().unwrap() as isize).collect())
        .unwrap_or_default();
    TableReaderSpec {
        table_id: v["table_id"].as_i64().unwrap(),
        handle_kind: v["handle_kind"].as_str().unwrap().to_owned(),
        virtual_cols: v["virtual_cols"].as_bool().unwrap_or(false),
        temp_table: v["temp_table"].as_bool().unwrap_or(false),
        set_dummy_call: v["set_dummy_call"].as_bool().unwrap_or(false),
        keep_order: v["keep_order"].as_bool().unwrap_or(false),
        desc: v["desc"].as_bool().unwrap_or(false),
        paging: v["paging"].as_bool().unwrap_or(false),
        concurrency: v["concurrency"].as_u64().unwrap_or(15) as usize,
        replica_read: v["replica_read"].as_str().unwrap_or("leader").to_owned(),
        threshold: v["threshold"].as_i64().unwrap_or(0),
        max_chunk_size: v["max_chunk_size"].as_u64().unwrap() as usize,
        ranges,
        streams,
        required_rows,
    }
}

struct MockSelectResult {
    spec: StreamSpec,
    num_cols: usize,
    pos: usize,
    next_calls: usize,
}

#[async_trait]
impl SelectResult for MockSelectResult {
    async fn next_raw(&mut self, _ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }

    async fn next(&mut self, _ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error> {
        chk.reset();
        if !self.spec.next_err.is_empty() && self.next_calls >= self.spec.err_after {
            self.next_calls += 1;
            return Err(Error::plain(self.spec.next_err.clone()));
        }
        self.next_calls += 1;
        if self.pos >= self.spec.rows.len() {
            return Ok(());
        }
        let take = chk.required_rows().min(self.spec.rows.len() - self.pos);
        for i in 0..take {
            let r = &self.spec.rows[self.pos + i];
            for col_idx in 0..self.num_cols {
                match r.get(col_idx).and_then(|v| *v) {
                    Some(v) => chk.append_int64(col_idx, v),
                    None => chk.append_null(col_idx),
                }
            }
        }
        self.pos += take;
        Ok(())
    }

    fn into_iter(
        &mut self,
        _col_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error> {
        Err(Error::plain("not implemented"))
    }

    fn close(&mut self) -> Result<(), Error> {
        if !self.spec.close_err.is_empty() {
            return Err(Error::plain(self.spec.close_err.clone()));
        }
        Ok(())
    }
}

struct Acc;
impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

struct DummyClient;
impl KvClient for DummyClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("DummyClient.send should not be called when select_result_hook is installed")
    }
    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

struct ReplaySession {
    vars: SessionVars,
    killer: Arc<SqlKiller>,
    expr_ctx: ExprContextRef,
    info_schema: InfoSchemaRef,
    client: Option<Arc<dyn KvClient>>,
}

impl SessionContext for ReplaySession {
    fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }
    fn sql_killer(&self) -> Arc<SqlKiller> {
        Arc::clone(&self.killer)
    }
    fn get_stmt_read_ts(&mut self) -> BoxFuture<'_, Result<u64, Error>> {
        Box::pin(async { Ok(100) })
    }
    fn get_txn_info_schema(&mut self) -> Option<InfoSchemaRef> {
        Some(Arc::clone(&self.info_schema))
    }
    fn get_txn_scope(&self) -> String {
        "global".to_owned()
    }
    fn get_read_replica_scope(&self) -> String {
        "global".to_owned()
    }
    fn is_stmt_staleness(&self) -> bool {
        false
    }
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error> {
        Ok(Some(100))
    }
    fn set_process_info(&mut self, _: &str, _: SystemTime, _: u8, _: u64) {}
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, _: &Plan) -> Result<(), Error> {
        Ok(())
    }
    fn get_expr_ctx(&self) -> ExprContextRef {
        Arc::clone(&self.expr_ctx)
    }
    fn get_snapshot_with_stmt_read_ts(
        &mut self,
    ) -> BoxFuture<'_, Result<Arc<dyn Snapshot>, Error>> {
        Box::pin(async { Err(Error::plain("no snapshot")) })
    }
    fn get_dist_sql_ctx(&self) -> Arc<DistSQLContext> {
        let sc = self.vars.stmt_ctx();
        let client: Arc<dyn KvClient> = self
            .client
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::new(DummyClient));
        Arc::new(DistSQLContext {
            client: Some(client),
            enable_chunk_rpc: self.vars.enable_chunk_rpc,
            dist_sql_concurrency: self.vars.dist_sql_scan_concurrency() as isize,
            replica_read_type: self.vars.get_replica_read(),
            enable_paging: self.vars.enable_paging,
            min_paging_size: self.vars.min_paging_size as isize,
            max_paging_size: self.vars.max_paging_size as isize,
            replica_closest_read_threshold: self.vars.replica_closest_read_threshold,
            runtime_stats_coll: sc.runtime_stats_coll.clone(),
            sql_killer: Some(Arc::clone(&self.killer)),
            location: self.vars.location(),
            err_ctx: sc.err_ctx(),
            ..Default::default()
        })
    }
    fn get_build_pb_ctx(&self) -> Result<Arc<BuildPBContext>, Error> {
        let sc = self.vars.stmt_ctx();
        Ok(Arc::new(BuildPBContext {
            expr_ctx: Arc::clone(&self.expr_ctx) as BuildContextRef,
            client: Arc::new(DummyClient),
            tiflash_fast_scan: self.vars.tiflash_fast_scan,
            tiflash_fine_grained_shuffle_batch_size: self
                .vars
                .tiflash_fine_grained_shuffle_batch_size,
            group_concat_max_len: self.vars.group_concat_max_len,
            in_explain_stmt: sc.in_explain_stmt,
            warn_handler: Some(sc.warn_handler() as tidb_base::contextutil::WarnAppenderRef),
            extra_warnghandler: Some(
                Arc::clone(&sc.extra_warn_handler) as tidb_base::contextutil::WarnAppenderRef
            ),
        }))
    }
    fn get_ranger_ctx(&self) -> Arc<RangerContext> {
        let sc = self.vars.stmt_ctx();
        Arc::new(RangerContext {
            type_ctx: sc.type_ctx(),
            err_ctx: sc.err_ctx(),
            expr_ctx: Arc::clone(&self.expr_ctx) as BuildContextRef,
            range_fallback_handler: Arc::clone(&sc.range_fallback_handler),
            plan_cache_tracker: Arc::clone(&sc.plan_cache_tracker),
            optimizer_fix_control: self.vars.get_optimizer_fix_control_map().clone(),
            use_cache: false,
            in_prepared_plan_building: sc.in_prepared_plan_building,
            regard_null_as_point: self.vars.regard_null_as_point,
            opt_prefix_index_single_scan: self.vars.opt_prefix_index_single_scan,
        })
    }
    fn get_info_schema(&self) -> Option<InfoSchemaRef> {
        Some(Arc::clone(&self.info_schema))
    }
    fn get_plan_ctx(&self) -> tidb_planner::planctx::SessionPlanCtx {
        tidb_planner::planctx::SessionPlanCtx {
            expr_ctx: Arc::clone(&self.expr_ctx),
            client: None,
            has_dirty_content: None,
            advise_txn_warmup: None,
            domain_info_schema: None,
            instance_plan_cache: None,
        }
    }
}

fn decode_range_datum(handle_kind: &str, col_idx: usize, v: &Value) -> Datum {
    if handle_kind == "uint" {
        let u: u64 = v.as_str().unwrap().parse().unwrap();
        return new_uint_datum(u);
    }
    if handle_kind == "common" && col_idx == 1 {
        return new_string_datum(v.as_str().unwrap().to_owned());
    }
    new_int_datum(v.as_i64().unwrap_or_else(|| v.as_f64().unwrap() as i64))
}

fn build_ranges(handle_kind: &str, specs: &[RangeSpec]) -> Vec<Range> {
    specs
        .iter()
        .map(|rs| Range {
            low_val: rs
                .low
                .iter()
                .enumerate()
                .map(|(i, v)| decode_range_datum(handle_kind, i, v))
                .collect(),
            high_val: rs
                .high
                .iter()
                .enumerate()
                .map(|(i, v)| decode_range_datum(handle_kind, i, v))
                .collect(),
            low_exclude: rs.low_exclude,
            high_exclude: rs.high_exclude,
            ..Default::default()
        })
        .collect()
}

fn int_ft(unsigned: bool) -> FieldType {
    let mut ft = new_field_type(TypeLonglong);
    if unsigned {
        ft.add_flag(UnsignedFlag);
    }
    ft
}

fn str_ft() -> FieldType {
    let mut ft = new_field_type(TypeVarchar);
    ft.set_flen(64);
    ft.set_charset("utf8mb4");
    ft.set_collate("utf8mb4_bin");
    ft
}

fn build_table_and_plan(
    expr_ctx: &ExprContextRef,
    spec: &TableReaderSpec,
) -> (Arc<TableInfo>, PhysicalTableReader) {
    let (cols, schema_cols, indices) = match spec.handle_kind.as_str() {
        "int" | "uint" => {
            let unsigned = spec.handle_kind == "uint";
            let mut pk_ft = int_ft(unsigned);
            pk_ft.add_flag(PriKeyFlag | NotNullFlag);
            let c0 = Arc::new(ColumnInfo {
                id: 1,
                name: CIStr::new("id"),
                offset: 0,
                field_type: pk_ft.clone(),
                state: StatePublic,
                ..Default::default()
            });
            let c1 = Arc::new(ColumnInfo {
                id: 2,
                name: CIStr::new("a"),
                offset: 1,
                field_type: int_ft(false),
                state: StatePublic,
                ..Default::default()
            });
            let mut cols = vec![c0, c1];
            let ec0 = Column {
                id: 1,
                unique_id: 1,
                index: 0,
                ret_type: Arc::new(pk_ft),
                ..Default::default()
            };
            let mut ec1 = Column {
                id: 2,
                unique_id: 2,
                index: 1,
                ret_type: Arc::new(int_ft(false)),
                ..Default::default()
            };
            let schema_cols = if spec.virtual_cols {
                let c2 = Arc::new(ColumnInfo {
                    id: 3,
                    name: CIStr::new("v1"),
                    offset: 2,
                    field_type: int_ft(false),
                    state: StatePublic,
                    generated_expr_string: "a + 10".to_owned(),
                    generated_stored: false,
                    ..Default::default()
                });
                let c3 = Arc::new(ColumnInfo {
                    id: 4,
                    name: CIStr::new("v2"),
                    offset: 3,
                    field_type: int_ft(false),
                    state: StatePublic,
                    generated_expr_string: "v1 * 2".to_owned(),
                    generated_stored: false,
                    ..Default::default()
                });
                cols.push(c2);
                cols.push(c3);

                let a_ref = Expr::Column(Column {
                    id: 2,
                    unique_id: 2,
                    index: 2,
                    ret_type: Arc::new(int_ft(false)),
                    ..Default::default()
                })
                .into_ref();
                let v1_ref = Expr::Column(Column {
                    id: 3,
                    unique_id: 3,
                    index: 3,
                    ret_type: Arc::new(int_ft(false)),
                    ..Default::default()
                })
                .into_ref();
                let ten =
                    Expr::Constant(Constant::new(new_int_datum(10), int_ft(false))).into_ref();
                let two = Expr::Constant(Constant::new(new_int_datum(2), int_ft(false))).into_ref();
                let v1_expr =
                    new_function(expr_ctx.as_ref(), Plus, int_ft(false), vec![a_ref, ten]).unwrap();
                let v2_expr =
                    new_function(expr_ctx.as_ref(), Mul, int_ft(false), vec![v1_ref, two]).unwrap();
                ec1.index = 2;
                let ec_v2 = Column {
                    id: 4,
                    unique_id: 4,
                    index: 1,
                    ret_type: Arc::new(int_ft(false)),
                    virtual_expr: Some(v2_expr),
                    ..Default::default()
                };
                let ec_v1 = Column {
                    id: 3,
                    unique_id: 3,
                    index: 3,
                    ret_type: Arc::new(int_ft(false)),
                    virtual_expr: Some(v1_expr),
                    ..Default::default()
                };
                vec![
                    Arc::new(ec0),
                    Arc::new(ec_v2),
                    Arc::new(ec1),
                    Arc::new(ec_v1),
                ]
            } else {
                vec![Arc::new(ec0), Arc::new(ec1)]
            };
            (cols, schema_cols, Vec::new())
        }
        "common" => {
            let c0 = Arc::new(ColumnInfo {
                id: 1,
                name: CIStr::new("k1"),
                offset: 0,
                field_type: int_ft(false),
                state: StatePublic,
                ..Default::default()
            });
            let c1 = Arc::new(ColumnInfo {
                id: 2,
                name: CIStr::new("k2"),
                offset: 1,
                field_type: str_ft(),
                state: StatePublic,
                ..Default::default()
            });
            let c2 = Arc::new(ColumnInfo {
                id: 3,
                name: CIStr::new("v"),
                offset: 2,
                field_type: int_ft(false),
                state: StatePublic,
                ..Default::default()
            });
            let idx = Arc::new(IndexInfo {
                id: 1,
                name: CIStr::new("PRIMARY"),
                primary: true,
                unique: true,
                state: StatePublic,
                columns: Some(vec![
                    IndexColumn {
                        name: CIStr::new("k1"),
                        offset: 0,
                        length: UnspecifiedLength,
                        use_changing_type: false,
                    },
                    IndexColumn {
                        name: CIStr::new("k2"),
                        offset: 1,
                        length: UnspecifiedLength,
                        use_changing_type: false,
                    },
                ]),
                ..Default::default()
            });
            let schema_cols = vec![
                Arc::new(Column {
                    id: 1,
                    unique_id: 1,
                    index: 0,
                    ret_type: Arc::new(int_ft(false)),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 3,
                    unique_id: 3,
                    index: 1,
                    ret_type: Arc::new(int_ft(false)),
                    ..Default::default()
                }),
            ];
            (vec![c0, c1, c2], schema_cols, vec![idx])
        }
        other => panic!("unknown handle_kind: {other}"),
    };

    let mut tbl = TableInfo {
        id: spec.table_id,
        name: CIStr::new("t"),
        columns: Some(cols.clone()),
        indices: if indices.is_empty() {
            None
        } else {
            Some(indices)
        },
        pk_is_handle: spec.handle_kind == "int" || spec.handle_kind == "uint",
        is_common_handle: spec.handle_kind == "common",
        state: StatePublic,
        ..Default::default()
    };
    if spec.temp_table {
        tbl.temp_table_type = TempTableLocal;
    }
    let tbl = Arc::new(tbl);

    let schema = new_schema(schema_cols);
    let stats = Arc::new(StatsInfo {
        row_count: 50.0,
        ..Default::default()
    });
    let mut scan = PhysicalTableScan {
        table: Arc::clone(&tbl),
        columns: cols,
        ranges: build_ranges(&spec.handle_kind, &spec.ranges),
        keep_order: spec.keep_order,
        desc: spec.desc,
        store_type: TiKV,
        tbl_col_hists: Some(Arc::new(HistColl::default())),
        ..Default::default()
    };
    scan.set_id(1);
    scan.set_schema(schema.clone());
    scan.set_stats(Some(Arc::clone(&stats)));
    let scan_ref: PhysicalPlanRef = Arc::new(scan);

    let mut reader = PhysicalTableReader {
        table_plan: Some(Arc::clone(&scan_ref)),
        table_plans: vec![scan_ref],
        store_type: TiKV,
        read_req_type: Cop,
        is_common_handle: tbl.is_common_handle,
        ..Default::default()
    };
    reader.set_id(2);
    reader.set_schema(schema);
    reader.set_stats(Some(stats));
    (tbl, reader)
}

async fn info_schema_with_table(tbl: Arc<TableInfo>) -> InfoSchemaRef {
    let mut db = DBInfo {
        id: 1,
        name: CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![tbl]);
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    b.init_with_db_infos(vec![db], vec![], vec![], 1)
        .await
        .unwrap();
    Arc::new(b.build(1).await)
}

async fn replay_table_reader(args: &[Value]) -> Result<Value, Error> {
    let spec = parse_table_reader_spec(&args[0]);

    let expr_ctx: ExprContextRef = es::new_expr_context(Vec::new());
    let (tbl_info, reader_plan) = build_table_and_plan(&expr_ctx, &spec);
    let is = info_schema_with_table(Arc::clone(&tbl_info)).await;

    let mut vars = SessionVars::new(Box::new(Acc));
    vars.init_chunk_size = 32;
    vars.max_chunk_size = spec.max_chunk_size as i64;
    vars.enable_chunk_rpc = true;
    vars.enable_paging = spec.paging;
    vars.set_dist_sql_scan_concurrency(spec.concurrency as i64);
    vars.set_replica_read(parse_replica_read(&spec.replica_read));
    vars.replica_closest_read_threshold = spec.threshold;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, -1));

    let mut sctx = ReplaySession {
        vars,
        killer: SqlKiller::new(),
        expr_ctx,
        info_schema: Arc::clone(&is),
        client: None,
    };

    let mut b = new_executor_builder(
        &sctx,
        Some(Arc::clone(&is)),
        Arc::new(TelemetryInfo::default()),
    );
    let mut tr = match b.build_table_reader(&mut sctx, &reader_plan).await {
        Some(tr) => tr,
        None => return Err(b.err.take().unwrap_or_else(|| Error::plain("build failed"))),
    };

    if spec.set_dummy_call {
        tr.set_dummy();
    }
    let tbl_id_check = tr.table().unwrap().meta().id;
    let output_offsets = tr.dag_pb.get_output_offsets().to_vec();

    let reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_idx = Arc::new(Mutex::new(0usize));
    let streams = Arc::new(spec.streams);

    let reqs_cb = Arc::clone(&reqs);
    let idx_cb = Arc::clone(&stream_idx);
    let streams_cb = Arc::clone(&streams);
    let offsets_cb = output_offsets.clone();
    tr.select_result_hook = SelectResultHook {
        select_result_func: Some(Arc::new(
            move |_ctx, _dctx, kv_req, field_types, cop_plan_ids| {
                let krs: Vec<Value> = kv_req
                    .key_ranges
                    .as_ref()
                    .map(|kr| kr.append_self_to(Vec::new()))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|kr| {
                        json!({
                            "start": hex(&kr.start_key),
                            "end": hex(&kr.end_key),
                        })
                    })
                    .collect();
                reqs_cb.lock().push(json!({
                    "key_ranges": krs,
                    "desc": kv_req.desc,
                    "keep_order": kv_req.keep_order,
                    "concurrency": kv_req.concurrency,
                    "paging": kv_req.paging.enable,
                    "start_ts": kv_req.start_ts,
                    "replica_read": format_replica_read(kv_req.replica_read),
                    "output_offsets": offsets_cb,
                    "cop_plan_ids": cop_plan_ids,
                }));
                let mut i = idx_cb.lock();
                let st = streams_cb.get(*i).cloned().unwrap_or_default();
                *i += 1;
                let num_cols = field_types.len();
                Box::pin(async move {
                    if !st.open_err.is_empty() {
                        return Err(Error::plain(st.open_err));
                    }
                    Ok(Box::new(MockSelectResult {
                        spec: st,
                        num_cols,
                        pos: 0,
                        next_calls: 0,
                    }) as Box<dyn SelectResult>)
                })
            },
        )),
    };

    let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    let mut out = serde_json::Map::new();
    let open_res = tr.open(&ctx).await;

    let captured_reqs = reqs.lock().clone();
    let captured_kv_ranges: Vec<Value> = tr
        .kv_ranges
        .iter()
        .map(|kr| json!({ "start": hex(&kr.start_key), "end": hex(&kr.end_key) }))
        .collect();

    if let Err(err) = open_res {
        let _ = tr.close().await;
        out.insert("reqs".into(), Value::Array(captured_reqs));
        out.insert("kv_ranges".into(), Value::Array(captured_kv_ranges));
        out.insert("open_err".into(), json!(err.to_string()));
        out.insert("nexts".into(), Value::Array(Vec::new()));
        out.insert("table_id_check".into(), json!(tbl_id_check));
        return Ok(Value::Object(out));
    }

    let mut chk = exec::new_first_chunk(&tr);
    let num_cols = chk.num_cols();
    let mut nexts = Vec::new();
    for &req_rows in &spec.required_rows {
        chk.set_required_rows(req_rows, spec.max_chunk_size);
        match tr.next(&ctx, &mut chk).await {
            Err(err) => {
                nexts.push(json!({ "err": err.to_string() }));
                break;
            }
            Ok(()) => {
                if chk.num_rows() == 0 {
                    nexts.push(json!({}));
                } else {
                    let mut rows = Vec::with_capacity(chk.num_rows());
                    for r in 0..chk.num_rows() {
                        let row = chk.get_row(r);
                        let mut vals = Vec::with_capacity(num_cols);
                        for c in 0..num_cols {
                            if row.is_null(c) {
                                vals.push(Value::Null);
                            } else {
                                vals.push(json!(row.get_int64(c)));
                            }
                        }
                        rows.push(Value::Array(vals));
                    }
                    nexts.push(json!({ "rows": rows }));
                }
            }
        }
    }

    out.insert("reqs".into(), Value::Array(captured_reqs));
    out.insert("kv_ranges".into(), Value::Array(captured_kv_ranges));
    out.insert("nexts".into(), Value::Array(nexts));
    if let Err(err) = tr.close().await {
        out.insert("close_err".into(), json!(err.to_string()));
    }
    out.insert("table_id_check".into(), json!(tbl_id_check));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------- index_reader

struct IndexReaderSpec {
    table_id: i64,
    index_id: i64,
    temp_table: bool,
    set_dummy_call: bool,
    keep_order: bool,
    desc: bool,
    paging: bool,
    concurrency: usize,
    replica_read: String,
    threshold: i64,
    max_chunk_size: usize,
    output_offsets: Vec<usize>,
    by_items_desc: Vec<bool>,
    ranges: Vec<RangeSpec>,
    grouped_ranges: Vec<Vec<RangeSpec>>,
    streams: Vec<StreamSpec>,
    required_rows: Vec<isize>,
}

fn parse_range_specs(v: &Value) -> Vec<RangeSpec> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| RangeSpec {
                    low: r["low"].as_array().cloned().unwrap_or_default(),
                    high: r["high"].as_array().cloned().unwrap_or_default(),
                    low_exclude: r["low_exclude"].as_bool().unwrap_or(false),
                    high_exclude: r["high_exclude"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_index_reader_spec(v: &Value) -> IndexReaderSpec {
    let ranges = parse_range_specs(&v["ranges"]);
    let grouped_ranges = v["grouped_ranges"]
        .as_array()
        .map(|groups| groups.iter().map(parse_range_specs).collect())
        .unwrap_or_default();
    let streams = v["streams"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| StreamSpec {
                    open_err: s["open_err"].as_str().unwrap_or("").to_owned(),
                    rows: s["rows"]
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .map(|row| {
                                    row.as_array()
                                        .unwrap()
                                        .iter()
                                        .map(|cell| cell.as_i64())
                                        .collect()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    next_err: s["next_err"].as_str().unwrap_or("").to_owned(),
                    err_after: s["err_after"].as_u64().unwrap_or(0) as usize,
                    close_err: s["close_err"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let required_rows = v["required_rows"]
        .as_array()
        .map(|arr| arr.iter().map(|x| x.as_i64().unwrap() as isize).collect())
        .unwrap_or_default();
    let output_offsets = v["output_offsets"]
        .as_array()
        .map(|arr| arr.iter().map(|x| x.as_u64().unwrap() as usize).collect())
        .unwrap_or_default();
    let by_items_desc = v["by_items_desc"]
        .as_array()
        .map(|arr| arr.iter().map(|x| x.as_bool().unwrap()).collect())
        .unwrap_or_default();
    IndexReaderSpec {
        table_id: v["table_id"].as_i64().unwrap(),
        index_id: v["index_id"].as_i64().unwrap(),
        temp_table: v["temp_table"].as_bool().unwrap_or(false),
        set_dummy_call: v["set_dummy_call"].as_bool().unwrap_or(false),
        keep_order: v["keep_order"].as_bool().unwrap_or(false),
        desc: v["desc"].as_bool().unwrap_or(false),
        paging: v["paging"].as_bool().unwrap_or(false),
        concurrency: v["concurrency"].as_u64().unwrap_or(15) as usize,
        replica_read: v["replica_read"].as_str().unwrap_or("leader").to_owned(),
        threshold: v["threshold"].as_i64().unwrap_or(0),
        max_chunk_size: v["max_chunk_size"].as_u64().unwrap() as usize,
        output_offsets,
        by_items_desc,
        ranges,
        grouped_ranges,
        streams,
        required_rows,
    }
}

fn build_index_table_and_plan(spec: &IndexReaderSpec) -> (Arc<TableInfo>, PhysicalIndexReader) {
    let mut pk_ft = int_ft(false);
    pk_ft.add_flag(PriKeyFlag | NotNullFlag);
    let c0 = Arc::new(ColumnInfo {
        id: 1,
        name: CIStr::new("id"),
        offset: 0,
        field_type: pk_ft.clone(),
        state: StatePublic,
        ..Default::default()
    });
    let c1 = Arc::new(ColumnInfo {
        id: 2,
        name: CIStr::new("k1"),
        offset: 1,
        field_type: int_ft(false),
        state: StatePublic,
        ..Default::default()
    });
    let c2 = Arc::new(ColumnInfo {
        id: 3,
        name: CIStr::new("k2"),
        offset: 2,
        field_type: int_ft(false),
        state: StatePublic,
        ..Default::default()
    });
    let cols = vec![Arc::clone(&c0), Arc::clone(&c1), Arc::clone(&c2)];

    let is_common = spec.index_id == -1;
    let idx_name = if is_common { "PRIMARY" } else { "idx_k" };
    let idx_info = Arc::new(IndexInfo {
        id: spec.index_id,
        name: CIStr::new(idx_name),
        table: CIStr::new("t"),
        primary: is_common,
        unique: is_common,
        state: StatePublic,
        columns: Some(vec![
            IndexColumn {
                name: CIStr::new("k1"),
                offset: 1,
                length: UnspecifiedLength,
                use_changing_type: false,
            },
            IndexColumn {
                name: CIStr::new("k2"),
                offset: 2,
                length: UnspecifiedLength,
                use_changing_type: false,
            },
        ]),
        ..Default::default()
    });

    let mut tbl = TableInfo {
        id: spec.table_id,
        name: CIStr::new("t"),
        columns: Some(cols),
        indices: Some(vec![Arc::clone(&idx_info)]),
        pk_is_handle: !is_common,
        is_common_handle: is_common,
        state: StatePublic,
        ..Default::default()
    };
    if spec.temp_table {
        tbl.temp_table_type = TempTableLocal;
    }
    let tbl = Arc::new(tbl);

    let scan_cols = [
        Arc::new(Column {
            id: 2,
            unique_id: 2,
            index: 0,
            ret_type: Arc::new(int_ft(false)),
            ..Default::default()
        }),
        Arc::new(Column {
            id: 3,
            unique_id: 3,
            index: 1,
            ret_type: Arc::new(int_ft(false)),
            ..Default::default()
        }),
        Arc::new(Column {
            id: 1,
            unique_id: 1,
            index: 2,
            ret_type: Arc::new(pk_ft),
            ..Default::default()
        }),
    ];
    let scan_schema = new_schema(scan_cols.to_vec());

    let mut out_cols = Vec::with_capacity(spec.output_offsets.len());
    let mut reader_cols = Vec::with_capacity(spec.output_offsets.len());
    for (i, &off) in spec.output_offsets.iter().enumerate() {
        let sc = &scan_cols[off];
        out_cols.push(Arc::new(Column {
            id: sc.id,
            unique_id: sc.unique_id,
            index: off as isize,
            ret_type: Arc::clone(&sc.ret_type),
            ..Default::default()
        }));
        reader_cols.push(Arc::new(Column {
            id: sc.id,
            unique_id: sc.unique_id,
            index: i as isize,
            ret_type: Arc::clone(&sc.ret_type),
            ..Default::default()
        }));
    }
    let reader_schema = new_schema(reader_cols.clone());

    let by_items: Vec<ByItems> = spec
        .by_items_desc
        .iter()
        .enumerate()
        .map(|(i, &desc)| ByItems {
            expr: Expr::Column((*reader_cols[i]).clone()).into_ref(),
            desc,
        })
        .collect();

    let grouped_ranges: Vec<Vec<Range>> = spec
        .grouped_ranges
        .iter()
        .map(|g| build_ranges("int", g))
        .collect();

    let stats = Arc::new(StatsInfo {
        row_count: 50.0,
        ..Default::default()
    });
    let mut scan = PhysicalIndexScan {
        table: Arc::clone(&tbl),
        index: idx_info,
        columns: vec![c1, c2, c0],
        ranges: build_ranges("int", &spec.ranges),
        grouped_ranges,
        keep_order: spec.keep_order,
        desc: spec.desc,
        by_items,
        data_source_schema: Some(scan_schema.clone()),
        tbl_col_hists: Some(Arc::new(HistColl::default())),
        ..Default::default()
    };
    scan.set_id(1);
    scan.set_schema(scan_schema);
    scan.set_stats(Some(Arc::clone(&stats)));
    let scan_ref: PhysicalPlanRef = Arc::new(scan);

    let mut reader = PhysicalIndexReader {
        index_plan: Some(Arc::clone(&scan_ref)),
        index_plans: vec![scan_ref],
        output_columns: out_cols,
        ..Default::default()
    };
    reader.set_id(2);
    reader.sp.set_schema(Some(reader_schema));
    reader.set_stats(Some(stats));
    (tbl, reader)
}

async fn replay_index_reader(args: &[Value]) -> Result<Value, Error> {
    let spec = parse_index_reader_spec(&args[0]);

    let expr_ctx: ExprContextRef = es::new_expr_context(Vec::new());
    let (tbl_info, reader_plan) = build_index_table_and_plan(&spec);
    let is = info_schema_with_table(Arc::clone(&tbl_info)).await;

    let mut vars = SessionVars::new(Box::new(Acc));
    vars.init_chunk_size = 32;
    vars.max_chunk_size = spec.max_chunk_size as i64;
    vars.enable_chunk_rpc = true;
    vars.enable_paging = spec.paging;
    vars.set_dist_sql_scan_concurrency(spec.concurrency as i64);
    vars.set_replica_read(parse_replica_read(&spec.replica_read));
    vars.replica_closest_read_threshold = spec.threshold;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, -1));

    let mut sctx = ReplaySession {
        vars,
        killer: SqlKiller::new(),
        expr_ctx,
        info_schema: Arc::clone(&is),
        client: None,
    };

    let mut b = new_executor_builder(
        &sctx,
        Some(Arc::clone(&is)),
        Arc::new(TelemetryInfo::default()),
    );
    let mut ir = match b.build_index_reader(&mut sctx, &reader_plan).await {
        Some(ir) => ir,
        None => return Err(b.err.take().unwrap_or_else(|| Error::plain("build failed"))),
    };

    if spec.set_dummy_call {
        ir.set_dummy();
    }
    let tbl_id_check = ir.table().unwrap().meta().id;
    let index_names = sctx.vars.stmt_ctx().index_names.clone();
    let output_offsets = ir.dag_pb.get_output_offsets().to_vec();

    let reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let stream_idx = Arc::new(Mutex::new(0usize));
    let streams = Arc::new(spec.streams);

    let reqs_cb = Arc::clone(&reqs);
    let idx_cb = Arc::clone(&stream_idx);
    let streams_cb = Arc::clone(&streams);
    let offsets_cb = output_offsets.clone();
    ir.select_result_hook = SelectResultHook {
        select_result_func: Some(Arc::new(
            move |_ctx, _dctx, kv_req, field_types, cop_plan_ids| {
                let krs: Vec<Value> = kv_req
                    .key_ranges
                    .as_ref()
                    .map(|kr| kr.append_self_to(Vec::new()))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|kr| {
                        json!({
                            "start": hex(&kr.start_key),
                            "end": hex(&kr.end_key),
                        })
                    })
                    .collect();
                reqs_cb.lock().push(json!({
                    "key_ranges": krs,
                    "desc": kv_req.desc,
                    "keep_order": kv_req.keep_order,
                    "concurrency": kv_req.concurrency,
                    "paging": kv_req.paging.enable,
                    "start_ts": kv_req.start_ts,
                    "replica_read": format_replica_read(kv_req.replica_read),
                    "output_offsets": offsets_cb,
                    "cop_plan_ids": cop_plan_ids,
                }));
                let mut i = idx_cb.lock();
                let st = streams_cb.get(*i).cloned().unwrap_or_default();
                *i += 1;
                let num_cols = field_types.len();
                Box::pin(async move {
                    if !st.open_err.is_empty() {
                        return Err(Error::plain(st.open_err));
                    }
                    Ok(Box::new(MockSelectResult {
                        spec: st,
                        num_cols,
                        pos: 0,
                        next_calls: 0,
                    }) as Box<dyn SelectResult>)
                })
            },
        )),
    };

    let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    let mut out = serde_json::Map::new();
    let open_res = ir.open(&ctx).await;

    let captured_reqs = reqs.lock().clone();
    let captured_kv_ranges: Vec<Value> = ir
        .kv_ranges
        .iter()
        .map(|kr| json!({ "start": hex(&kr.start_key), "end": hex(&kr.end_key) }))
        .collect();

    if let Err(err) = open_res {
        let _ = ir.close().await;
        out.insert("reqs".into(), Value::Array(captured_reqs));
        out.insert("kv_ranges".into(), Value::Array(captured_kv_ranges));
        out.insert("open_err".into(), json!(err.to_string()));
        out.insert("nexts".into(), Value::Array(Vec::new()));
        out.insert("table_id_check".into(), json!(tbl_id_check));
        out.insert("index_names".into(), json!(index_names));
        return Ok(Value::Object(out));
    }

    let mut chk = exec::new_first_chunk(&ir);
    let num_cols = chk.num_cols();
    let mut nexts = Vec::new();
    for &req_rows in &spec.required_rows {
        chk.set_required_rows(req_rows, spec.max_chunk_size);
        match ir.next(&ctx, &mut chk).await {
            Err(err) => {
                nexts.push(json!({ "err": err.to_string() }));
                break;
            }
            Ok(()) => {
                if chk.num_rows() == 0 {
                    nexts.push(json!({}));
                } else {
                    let mut rows = Vec::with_capacity(chk.num_rows());
                    for r in 0..chk.num_rows() {
                        let row = chk.get_row(r);
                        let mut vals = Vec::with_capacity(num_cols);
                        for c in 0..num_cols {
                            if row.is_null(c) {
                                vals.push(Value::Null);
                            } else {
                                vals.push(json!(row.get_int64(c)));
                            }
                        }
                        rows.push(Value::Array(vals));
                    }
                    nexts.push(json!({ "rows": rows }));
                }
            }
        }
    }

    out.insert("reqs".into(), Value::Array(captured_reqs));
    out.insert("kv_ranges".into(), Value::Array(captured_kv_ranges));
    out.insert("nexts".into(), Value::Array(nexts));
    if let Err(err) = ir.close().await {
        out.insert("close_err".into(), json!(err.to_string()));
    }
    out.insert("table_id_check".into(), json!(tbl_id_check));
    out.insert("index_names".into(), json!(index_names));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------- index_lookup_helpers

fn replay_index_lookup_helpers(args: &[Value]) -> Value {
    let kind = args[0].as_str().unwrap();
    match kind {
        "calc_batch_size" => {
            let est = args[1].as_i64().unwrap() as isize;
            let init_b = args[2].as_i64().unwrap() as isize;
            let max_b = args[3].as_i64().unwrap() as isize;
            json!(calculate_batch_size(est, init_b, max_b))
        }
        "max_in_flight" => {
            let c = args[1].as_i64().unwrap() as isize;
            json!(get_index_scan_max_in_flight(c))
        }
        "merge_sort_concurrency" => {
            let need_merge = args[1].as_bool().unwrap();
            let n_ranges = args[2].as_i64().unwrap().max(0) as usize;
            let c = args[3].as_i64().unwrap() as isize;
            json!(get_merge_sort_index_scan_concurrency(
                need_merge, n_ranges, c
            ))
        }
        "lack_handles" => {
            let is_common = args[1].as_bool().unwrap();
            let exp_vals: Vec<i64> = args[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_i64().unwrap())
                .collect();
            let obt_vals: Vec<i64> = args[3]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_i64().unwrap())
                .collect();
            let mk_h = |v: i64| -> Handle {
                if !is_common {
                    return Handle::Int(IntHandle(v));
                }
                let mut enc = Vec::new();
                encode_key(
                    &tidb_base::gostd::time::utc(),
                    &mut enc,
                    &[new_int_datum(v)],
                )
                .unwrap();
                Handle::Common(Arc::new(new_common_handle(enc).unwrap()))
            };
            let expected: Vec<Handle> = exp_vals.into_iter().map(mk_h).collect();
            let mut hm = new_handle_map();
            for v in obt_vals {
                hm.set(&mk_h(v), true);
            }
            let lack = get_lack_handles(&expected, &mut hm);
            let out: Vec<String> = lack.iter().map(ToString::to_string).collect();
            json!({ "lack": out, "rem_count": hm.len() })
        }
        "index_scan_offsets" => {
            let num_idx_cols = args[1].as_u64().unwrap() as usize;
            let handle_len = args[2].as_u64().unwrap() as usize;
            let by_ids: Vec<i64> = args[3]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_i64().unwrap())
                .collect();
            let non_col_by_item = args[4].as_bool().unwrap();
            let is_partitioned = args[5].as_bool().unwrap();
            let is_global = args[6].as_bool().unwrap();
            let dynamic_prune = args[7].as_bool().unwrap();

            let mut vars = SessionVars::new(Box::new(Acc));
            vars.stmt_ctx_mut().use_dynamic_prune_mode = dynamic_prune;

            let mut tbl_info = TableInfo {
                id: 10,
                name: CIStr::new("t"),
                ..Default::default()
            };
            if is_partitioned {
                tbl_info.partition = Some(Box::new(PartitionInfo {
                    enable: true,
                    ..Default::default()
                }));
            }
            let mut idx_cols = Vec::with_capacity(num_idx_cols);
            let mut schema_cols = Vec::with_capacity(num_idx_cols + handle_len);
            for i in 0..num_idx_cols {
                idx_cols.push(IndexColumn {
                    name: CIStr::new(&format!("c{i}")),
                    offset: i as isize,
                    ..Default::default()
                });
                schema_cols.push(Arc::new(Column {
                    id: (i + 1) as i64,
                    index: i as isize,
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    ..Default::default()
                }));
            }
            for i in 0..handle_len {
                schema_cols.push(Arc::new(Column {
                    id: (100 + i) as i64,
                    index: (num_idx_cols + i) as isize,
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    ..Default::default()
                }));
            }
            let idx_info = Arc::new(IndexInfo {
                id: 1,
                name: CIStr::new("idx"),
                global: is_global,
                columns: Some(idx_cols.clone()),
                ..Default::default()
            });
            let mut by_items = Vec::new();
            if non_col_by_item {
                by_items.push(ByItems {
                    expr: Expr::Constant(Constant::new(
                        new_int_datum(1),
                        new_field_type(TypeLonglong),
                    ))
                    .into_ref(),
                    desc: false,
                });
            }
            for cid in by_ids {
                by_items.push(ByItems {
                    expr: Expr::Column(Column {
                        id: cid,
                        ret_type: Arc::new(new_field_type(TypeLonglong)),
                        ..Default::default()
                    })
                    .into_ref(),
                    desc: false,
                });
            }
            let mut scan = PhysicalIndexScan {
                table: Arc::new(tbl_info),
                index: idx_info,
                by_items,
                ..Default::default()
            };
            scan.sp.set_schema(Some(new_schema(schema_cols)));
            match build_index_scan_output_offsets(&scan, &vars, &idx_cols, handle_len) {
                Ok(offs) => json!({ "offsets": offs }),
                Err(err) => json!({ "err": err.to_string() }),
            }
        }
        "runtime_stats" => {
            let mk = |v: &Value| -> IndexLookUpRunTimeStats {
                let s = IndexLookUpRunTimeStats::new(v["concurrency"].as_i64().unwrap() as isize);
                s.fetch_handle_total.store(
                    v["fetch_handle_total"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.fetch_handle.store(
                    v["fetch_handle"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.task_wait.store(
                    v["task_wait"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.table_row_scan.store(
                    v["table_row_scan"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.table_task_num.store(
                    v["table_task_num"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.next_wait_index_scan.store(
                    v["next_wait_index_scan"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.next_wait_table_lookup_build.store(
                    v["next_wait_table_lookup_build"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s.next_wait_table_lookup_resp.store(
                    v["next_wait_table_lookup_resp"].as_i64().unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                s
            };
            let mut a = mk(&args[1]);
            let b = mk(&args[2]);
            let a_str = a.string();
            let clone_str = a.clone_stats().string();
            a.merge(&b);
            json!({
                "a_str": a_str,
                "clone_str": clone_str,
                "merged_str": a.string(),
                "tp": a.tp(),
            })
        }
        other => panic!("unknown index_lookup_helpers kind: {other}"),
    }
}

// ---------------------------------------------------------------- index_lookup

struct LookUpResultSubset {
    data: Vec<u8>,
    start_key: tidb_kv::key::Key,
}

impl ResultSubset for LookUpResultSubset {
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_start_key(&self) -> &tidb_kv::key::Key {
        &self.start_key
    }
    fn mem_size(&self) -> i64 {
        self.data.len() as i64
    }
    fn resp_time(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

struct LookUpMockResp {
    packets: Vec<Vec<u8>>,
    idx: usize,
    err_msg: Option<String>,
    conc: isize,
}

impl Response for LookUpMockResp {
    fn next<'a>(
        &'a mut self,
        _ctx: &'a tidb_kv::Context,
    ) -> KvFuture<'a, Option<Box<dyn ResultSubset>>> {
        let res = if let Some(msg) = self.err_msg.take() {
            Err(Error::plain(msg))
        } else if self.idx >= self.packets.len() {
            Ok(None)
        } else {
            let pkt = self.packets[self.idx].clone();
            self.idx += 1;
            Ok(Some(Box::new(LookUpResultSubset {
                data: pkt,
                start_key: tidb_kv::key::Key::default(),
            }) as Box<dyn ResultSubset>))
        };
        Box::pin(async move { res })
    }

    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        if self.conc > 0 {
            Some((self.conc, 0))
        } else {
            Some((1, 0))
        }
    }
}

fn encode_chunk_bytes(fts: &[Arc<FieldType>], rows: &[Vec<Value>], v0_sort_key: bool) -> Vec<u8> {
    let mut chk = chunk::new_chunk_with_capacity(fts, rows.len());
    let coll = if v0_sort_key {
        Some(tidb_types::collate::get_collator("utf8mb4_general_ci"))
    } else {
        None
    };
    for row in rows {
        for (col_idx, ft) in fts.iter().enumerate() {
            let cell = row.get(col_idx).unwrap_or(&Value::Null);
            if cell.is_null() {
                chk.append_null(col_idx);
            } else if ft.eval_type() == ETString {
                let s = cell.as_str().unwrap();
                if let Some(c) = coll
                    && !tidb_base::mysql::has_binary_flag(ft.get_flag())
                {
                    chk.append_bytes(col_idx, &c.key(s.as_bytes()));
                } else {
                    chk.append_string(col_idx, s.as_bytes());
                }
            } else {
                chk.append_int64(col_idx, cell.as_i64().unwrap());
            }
        }
    }
    let owned_fts: Vec<FieldType> = fts.iter().map(|ft| (**ft).clone()).collect();
    tidb_chunk::codec::new_codec(owned_fts).encode(&chk)
}

fn encode_select_resp(
    fts: &[Arc<FieldType>],
    rows: &[Vec<Value>],
    rows_sort_key: bool,
    inter_fts: Option<&[Arc<FieldType>]>,
    inter_rows: &[Vec<Value>],
    inter_sort_key: bool,
) -> Vec<u8> {
    let mut resp = tipb::SelectResponse::default();
    resp.set_encode_type(tipb::EncodeType::TypeChunk);
    if !rows.is_empty() {
        let mut c = tipb::Chunk::default();
        c.set_rows_data(encode_chunk_bytes(fts, rows, rows_sort_key));
        resp.set_chunks(vec![c].into());
    }
    if let Some(ifts) = inter_fts {
        let mut inter = tipb::IntermediateOutput::default();
        inter.set_encode_type(tipb::EncodeType::TypeChunk);
        if !inter_rows.is_empty() {
            let mut c = tipb::Chunk::default();
            c.set_rows_data(encode_chunk_bytes(ifts, inter_rows, inter_sort_key));
            inter.set_chunks(vec![c].into());
        }
        resp.set_intermediate_outputs(vec![inter].into());
    }
    resp.write_to_bytes().unwrap()
}

struct LookUpBatchSpec {
    rows: Vec<Vec<Value>>,
    hit_rows: Vec<Vec<Value>>,
    miss_handles: Vec<Vec<Value>>,
}

struct LookUpMockClient {
    index_look_up_push_down: bool,
    common_handle_v0_str: bool,
    index_err: String,
    table_err: String,
    cop_conc: isize,
    index_streams: Vec<Vec<LookUpBatchSpec>>,
    index_fts: Vec<Arc<FieldType>>,
    table_fts: Vec<Arc<FieldType>>,
    table_kvs: Vec<(Vec<u8>, Vec<Value>)>,
    state: Mutex<(usize, Vec<Value>)>,
}

impl KvClient for LookUpMockClient {
    fn send(
        &self,
        _ctx: &tidb_kv::Context,
        req: &Request,
        _vars: Option<OptionValue>,
        _opt: &ClientSendOption,
    ) -> Box<dyn Response> {
        let mut dag = tipb::DagRequest::default();
        if let Err(e) = dag.merge_from_bytes(&req.data) {
            return Box::new(LookUpMockResp {
                packets: Vec::new(),
                idx: 0,
                err_msg: Some(e.to_string()),
                conc: self.cop_conc,
            });
        }
        if dag.get_executors().is_empty() {
            return Box::new(LookUpMockResp {
                packets: Vec::new(),
                idx: 0,
                err_msg: None,
                conc: self.cop_conc,
            });
        }
        if dag.get_executors()[0].get_tp() == tipb::ExecType::TypeIndexScan {
            let krs = req
                .key_ranges
                .as_ref()
                .map(|k| k.append_self_to(Vec::new()))
                .unwrap_or_default();
            let kr_out: Vec<Value> = krs
                .iter()
                .map(|kr| json!({ "start": hex(&kr.start_key.0), "end": hex(&kr.end_key.0) }))
                .collect();
            let offs: Vec<u32> = dag.get_output_offsets().to_vec();
            let stream_idx = {
                let mut guard = self.state.lock();
                guard.1.push(json!({
                    "key_ranges": kr_out,
                    "desc": req.desc,
                    "keep_order": req.keep_order,
                    "concurrency": req.concurrency,
                    "paging": req.paging.enable,
                    "min_paging_size": req.paging.min_paging_size,
                    "cacheable": req.cacheable,
                    "output_offsets": offs,
                }));
                let idx = guard.0;
                guard.0 += 1;
                idx
            };
            if !self.index_err.is_empty() {
                return Box::new(LookUpMockResp {
                    packets: Vec::new(),
                    idx: 0,
                    err_msg: Some(self.index_err.clone()),
                    conc: self.cop_conc,
                });
            }
            let pkts = self
                .index_streams
                .get(stream_idx)
                .map(|batches| {
                    batches
                        .iter()
                        .map(|b| {
                            if self.index_look_up_push_down {
                                encode_select_resp(
                                    &self.table_fts,
                                    &b.hit_rows,
                                    false,
                                    Some(&self.index_fts),
                                    &b.miss_handles,
                                    self.common_handle_v0_str,
                                )
                            } else {
                                encode_select_resp(
                                    &self.index_fts,
                                    &b.rows,
                                    self.common_handle_v0_str,
                                    None,
                                    &[],
                                    false,
                                )
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Box::new(LookUpMockResp {
                packets: pkts,
                idx: 0,
                err_msg: None,
                conc: self.cop_conc,
            });
        }

        if !self.table_err.is_empty() {
            return Box::new(LookUpMockResp {
                packets: Vec::new(),
                idx: 0,
                err_msg: Some(self.table_err.clone()),
                conc: 1,
            });
        }
        let krs = req
            .key_ranges
            .as_ref()
            .map(|k| k.append_self_to(Vec::new()))
            .unwrap_or_default();
        let mut matched: Vec<Vec<Value>> = Vec::new();
        for kr in &krs {
            for (k, row) in &self.table_kvs {
                if k.as_slice() >= kr.start_key.0.as_slice()
                    && k.as_slice() < kr.end_key.0.as_slice()
                {
                    matched.push(row.clone());
                }
            }
        }
        let pkts = if matched.is_empty() {
            Vec::new()
        } else {
            vec![encode_select_resp(
                &self.table_fts,
                &matched,
                false,
                None,
                &[],
                false,
            )]
        };
        Box::new(LookUpMockResp {
            packets: pkts,
            idx: 0,
            err_msg: None,
            conc: 1,
        })
    }

    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

fn sort_json_rows(rows: &mut [Value]) {
    let fmt_cell = |v: &Value| -> String {
        match v {
            Value::Null => "<nil>".to_owned(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    };
    rows.sort_by(|a, b| {
        let aa = a.as_array().unwrap();
        let ba = b.as_array().unwrap();
        for (x, y) in aa.iter().zip(ba.iter()) {
            let cmp = fmt_cell(x).cmp(&fmt_cell(y));
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        aa.len().cmp(&ba.len())
    });
}

async fn replay_index_lookup(args: &[Value]) -> Result<Value, Error> {
    let v = &args[0];
    let table_id = v["table_id"].as_i64().unwrap();
    let index_id = v["index_id"].as_i64().unwrap();
    let is_common = v["is_common_handle"].as_bool().unwrap_or(false);
    let v0_str = v["common_handle_v0_str"].as_bool().unwrap_or(false);
    let temp_table = v["temp_table"].as_bool().unwrap_or(false);
    let set_dummy_call = v["set_dummy_call"].as_bool().unwrap_or(false);
    let keep_order = v["keep_order"].as_bool().unwrap_or(false);
    let desc = v["desc"].as_bool().unwrap_or(false);
    let index_paging = v["index_paging"].as_bool().unwrap_or(false);
    let push_down = v["index_look_up_push_down"].as_bool().unwrap_or(false);
    let weak_consistency = v["weak_consistency"].as_bool().unwrap_or(false);
    let extra_tbl_plan = v["extra_tbl_plan"].as_bool().unwrap_or(false);
    let index_lookup_size = v["index_lookup_size"].as_i64().unwrap_or(20000);
    let index_lookup_conc = v["index_lookup_concurrency"].as_i64().unwrap_or(0);
    let dist_sql_conc = v["dist_sql_concurrency"].as_i64().unwrap_or(0);
    let max_chunk_size = v["max_chunk_size"].as_u64().unwrap_or(32) as usize;
    let est_rows = v["est_rows"].as_f64().unwrap_or(50.0);
    let est_rows = if est_rows <= 0.0 { 50.0 } else { est_rows };
    let by_items_desc: Vec<bool> = v["by_items_desc"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_bool().unwrap()).collect())
        .unwrap_or_default();
    let ranges = build_ranges("int", &parse_range_specs(&v["ranges"]));
    let grouped_ranges: Vec<Vec<Range>> = v["grouped_ranges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|g| build_ranges("int", &parse_range_specs(g)))
                .collect()
        })
        .unwrap_or_default();
    let pushed_limit = v.get("pushed_limit").and_then(|pl| {
        if pl.is_null() {
            None
        } else {
            Some(PushedDownLimit {
                offset: pl["offset"].as_u64().unwrap(),
                count: pl["count"].as_u64().unwrap(),
            })
        }
    });
    let index_streams: Vec<Vec<LookUpBatchSpec>> = v["index_streams"]
        .as_array()
        .map(|streams| {
            streams
                .iter()
                .map(|batches| {
                    batches
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|b| LookUpBatchSpec {
                                    rows: b["rows"]
                                        .as_array()
                                        .map(|rs| {
                                            rs.iter()
                                                .map(|r| r.as_array().cloned().unwrap_or_default())
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                    hit_rows: b["hit_rows"]
                                        .as_array()
                                        .map(|rs| {
                                            rs.iter()
                                                .map(|r| r.as_array().cloned().unwrap_or_default())
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                    miss_handles: b["miss_handles"]
                                        .as_array()
                                        .map(|rs| {
                                            rs.iter()
                                                .map(|r| r.as_array().cloned().unwrap_or_default())
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let index_err = v["index_err"].as_str().unwrap_or("").to_owned();
    let table_err = v["table_err"].as_str().unwrap_or("").to_owned();
    let cop_conc = v["cop_conc"].as_i64().unwrap_or(0) as isize;
    let required_rows: Vec<isize> = v["required_rows"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_i64().unwrap() as isize).collect())
        .unwrap_or_default();

    let int_ft = || new_field_type(TypeLonglong);
    let str_ft = |bin: bool| {
        let mut ft = new_field_type(TypeVarchar);
        ft.set_flen(64);
        ft.set_charset(CharsetUTF8MB4);
        if bin {
            ft.set_collate(CollationBin);
            ft.add_flag(BinaryFlag);
        } else {
            ft.set_collate("utf8mb4_general_ci");
        }
        ft
    };

    let (cols, indices, idx_scan_cols, tbl_cols, extra_handle_col, common_handle_cols) =
        if !is_common {
            let mut pk_ft = int_ft();
            pk_ft.add_flag(PriKeyFlag | NotNullFlag);
            let c0 = Arc::new(ColumnInfo {
                id: 1,
                name: CIStr::new("id"),
                offset: 0,
                field_type: pk_ft.clone(),
                state: StatePublic,
                ..Default::default()
            });
            let c1 = Arc::new(ColumnInfo {
                id: 2,
                name: CIStr::new("k"),
                offset: 1,
                field_type: int_ft(),
                state: StatePublic,
                ..Default::default()
            });
            let c2 = Arc::new(ColumnInfo {
                id: 3,
                name: CIStr::new("v"),
                offset: 2,
                field_type: int_ft(),
                state: StatePublic,
                ..Default::default()
            });
            let sec_idx = Arc::new(IndexInfo {
                id: index_id,
                name: CIStr::new("idx_k"),
                table: CIStr::new("t"),
                state: StatePublic,
                columns: Some(vec![IndexColumn {
                    name: CIStr::new("k"),
                    offset: 1,
                    length: UnspecifiedLength,
                    use_changing_type: false,
                }]),
                ..Default::default()
            });
            let is_cols = vec![
                Arc::new(Column {
                    id: 2,
                    unique_id: 2,
                    index: 0,
                    ret_type: Arc::new(int_ft()),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 1,
                    unique_id: 1,
                    index: 1,
                    ret_type: Arc::new(pk_ft.clone()),
                    ..Default::default()
                }),
            ];
            let ts_cols = vec![
                Arc::new(Column {
                    id: 1,
                    unique_id: 1,
                    index: 0,
                    ret_type: Arc::new(pk_ft),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 3,
                    unique_id: 3,
                    index: 1,
                    ret_type: Arc::new(int_ft()),
                    ..Default::default()
                }),
            ];
            let extra = Some(Arc::clone(&ts_cols[0]));
            (
                vec![c0, c1, c2],
                vec![sec_idx],
                is_cols,
                ts_cols,
                extra,
                Vec::new(),
            )
        } else {
            let mut k1_ft = int_ft();
            k1_ft.add_flag(PriKeyFlag | NotNullFlag);
            let mut k2_ft = str_ft(!v0_str);
            k2_ft.add_flag(PriKeyFlag | NotNullFlag);
            let c0 = Arc::new(ColumnInfo {
                id: 1,
                name: CIStr::new("k1"),
                offset: 0,
                field_type: k1_ft.clone(),
                state: StatePublic,
                ..Default::default()
            });
            let c1 = Arc::new(ColumnInfo {
                id: 2,
                name: CIStr::new("k2"),
                offset: 1,
                field_type: k2_ft.clone(),
                state: StatePublic,
                ..Default::default()
            });
            let c2 = Arc::new(ColumnInfo {
                id: 3,
                name: CIStr::new("k"),
                offset: 2,
                field_type: int_ft(),
                state: StatePublic,
                ..Default::default()
            });
            let c3 = Arc::new(ColumnInfo {
                id: 4,
                name: CIStr::new("v"),
                offset: 3,
                field_type: int_ft(),
                state: StatePublic,
                ..Default::default()
            });
            let pk_idx = Arc::new(IndexInfo {
                id: 1,
                name: CIStr::new("PRIMARY"),
                table: CIStr::new("t"),
                primary: true,
                unique: true,
                state: StatePublic,
                columns: Some(vec![
                    IndexColumn {
                        name: CIStr::new("k1"),
                        offset: 0,
                        length: UnspecifiedLength,
                        use_changing_type: false,
                    },
                    IndexColumn {
                        name: CIStr::new("k2"),
                        offset: 1,
                        length: UnspecifiedLength,
                        use_changing_type: false,
                    },
                ]),
                ..Default::default()
            });
            let sec_idx = Arc::new(IndexInfo {
                id: index_id,
                name: CIStr::new("idx_k"),
                table: CIStr::new("t"),
                state: StatePublic,
                columns: Some(vec![IndexColumn {
                    name: CIStr::new("k"),
                    offset: 2,
                    length: UnspecifiedLength,
                    use_changing_type: false,
                }]),
                ..Default::default()
            });
            let is_cols = vec![
                Arc::new(Column {
                    id: 3,
                    unique_id: 3,
                    index: 0,
                    ret_type: Arc::new(int_ft()),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 1,
                    unique_id: 1,
                    index: 1,
                    ret_type: Arc::new(k1_ft.clone()),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 2,
                    unique_id: 2,
                    index: 2,
                    ret_type: Arc::new(k2_ft.clone()),
                    ..Default::default()
                }),
            ];
            let ts_cols = vec![
                Arc::new(Column {
                    id: 1,
                    unique_id: 1,
                    index: 0,
                    ret_type: Arc::new(k1_ft),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 2,
                    unique_id: 2,
                    index: 1,
                    ret_type: Arc::new(k2_ft),
                    ..Default::default()
                }),
                Arc::new(Column {
                    id: 4,
                    unique_id: 4,
                    index: 2,
                    ret_type: Arc::new(int_ft()),
                    ..Default::default()
                }),
            ];
            let ch_cols = vec![Arc::clone(&ts_cols[0]), Arc::clone(&ts_cols[1])];
            (
                vec![c0, c1, c2, c3],
                vec![pk_idx, sec_idx],
                is_cols,
                ts_cols,
                None,
                ch_cols,
            )
        };

    let sec_idx = Arc::clone(indices.last().unwrap());
    let tbl_info = Arc::new(TableInfo {
        id: table_id,
        name: CIStr::new("t"),
        columns: Some(cols.clone()),
        indices: Some(indices),
        pk_is_handle: !is_common,
        is_common_handle: is_common,
        common_handle_version: if v0_str { 0 } else { 1 },
        temp_table_type: if temp_table {
            TempTableLocal
        } else {
            tidb_model::TempTableNone
        },
        state: StatePublic,
        ..Default::default()
    });

    let idx_schema = new_schema(idx_scan_cols.clone());
    let tbl_schema = new_schema(tbl_cols.clone());
    let by_items: Vec<ByItems> = by_items_desc
        .iter()
        .enumerate()
        .map(|(i, &d)| ByItems {
            expr: Expr::Column((*idx_scan_cols[i]).clone()).into_ref(),
            desc: d,
        })
        .collect();

    let stats = Arc::new(StatsInfo {
        row_count: est_rows,
        ..Default::default()
    });
    let pseudo = Arc::new(HistColl::default());

    let is_col_infos = if !is_common {
        vec![Arc::clone(&cols[1]), Arc::clone(&cols[0])]
    } else {
        vec![
            Arc::clone(&cols[2]),
            Arc::clone(&cols[0]),
            Arc::clone(&cols[1]),
        ]
    };
    let mut idx_scan = PhysicalIndexScan {
        table: Arc::clone(&tbl_info),
        index: sec_idx,
        columns: is_col_infos,
        ranges,
        grouped_ranges: grouped_ranges.clone(),
        keep_order,
        desc,
        by_items: by_items.clone(),
        data_source_schema: Some(idx_schema.clone()),
        tbl_col_hists: Some(Arc::clone(&pseudo)),
        ..Default::default()
    };
    idx_scan.set_id(2);
    idx_scan.sp.set_schema(Some(idx_schema));
    idx_scan.set_stats(Some(Arc::clone(&stats)));
    let idx_scan_ref: PhysicalPlanRef = Arc::new(idx_scan);

    let ts_col_infos = if !is_common {
        vec![Arc::clone(&cols[0]), Arc::clone(&cols[2])]
    } else {
        vec![
            Arc::clone(&cols[0]),
            Arc::clone(&cols[1]),
            Arc::clone(&cols[3]),
        ]
    };
    let mut tbl_scan = PhysicalTableScan {
        table: Arc::clone(&tbl_info),
        columns: ts_col_infos,
        store_type: TiKV,
        tbl_col_hists: Some(pseudo),
        ..Default::default()
    };
    tbl_scan.set_id(3);
    tbl_scan.sp.set_schema(Some(tbl_schema.clone()));
    tbl_scan.set_stats(Some(Arc::clone(&stats)));
    let tbl_scan_ref: PhysicalPlanRef = Arc::new(tbl_scan);

    let mut table_plans: Vec<PhysicalPlanRef> = vec![Arc::clone(&tbl_scan_ref)];
    if extra_tbl_plan {
        let mut lim = PhysicalLimit {
            offset: 0,
            count: 100000,
            ..Default::default()
        };
        lim.set_id(4);
        lim.sp.set_schema(Some(tbl_schema.clone()));
        lim.set_stats(Some(Arc::clone(&stats)));
        lim.sp.base.children = vec![Arc::clone(&tbl_scan_ref)];
        table_plans.push(Arc::new(lim));
    }

    let (index_plans, un_nature) = if push_down {
        let mut local = PhysicalLocalIndexLookUp {
            index_handle_offsets: vec![(idx_scan_cols.len() - 1) as u32],
            ..Default::default()
        };
        local.set_id(5);
        local.sp.set_schema(Some(tbl_schema.clone()));
        local.sp.base.children = vec![Arc::clone(&idx_scan_ref), Arc::clone(&tbl_scan_ref)];
        let mut map = std::collections::HashMap::new();
        map.insert(0, 2);
        map.insert(1, 2);
        (
            vec![
                Arc::clone(&idx_scan_ref),
                Arc::clone(&tbl_scan_ref),
                Arc::new(local) as PhysicalPlanRef,
            ],
            map,
        )
    } else {
        (
            vec![Arc::clone(&idx_scan_ref)],
            std::collections::HashMap::new(),
        )
    };

    let mut reader = PhysicalIndexLookUpReader {
        index_plan: Some(Arc::clone(index_plans.last().unwrap())),
        table_plan: Some(Arc::clone(table_plans.last().unwrap())),
        index_plans,
        index_plans_un_nature_orders: un_nature,
        table_plans,
        paging: std::sync::atomic::AtomicBool::new(index_paging),
        extra_handle_col,
        common_handle_cols: common_handle_cols.clone(),
        pushed_limit,
        index_look_up_push_down: push_down,
        keep_order,
        ..Default::default()
    };
    reader.set_id(1);
    reader.sp.set_schema(Some(tbl_schema));
    reader.set_stats(Some(stats));

    let mut index_fts: Vec<Arc<FieldType>> = Vec::new();
    if grouped_ranges.len() > 1 && !by_items.is_empty() {
        for col in idx_scan_cols.iter().take(by_items.len()) {
            index_fts.push(Arc::clone(&col.ret_type));
        }
    }
    if !is_common {
        index_fts.push(Arc::new(new_field_type(TypeLonglong)));
    } else {
        for hc in &common_handle_cols {
            index_fts.push(Arc::clone(&hc.ret_type));
        }
    }
    let table_fts: Vec<Arc<FieldType>> = tbl_cols.iter().map(|c| Arc::clone(&c.ret_type)).collect();

    let mut table_kvs: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
    if let Some(entries) = v["table_entries"].as_array() {
        for te in entries {
            let hk = te["handle_key"].as_array().unwrap();
            let h = if !is_common {
                Handle::Int(IntHandle(hk[0].as_i64().unwrap()))
            } else {
                let d0 = new_int_datum(hk[0].as_i64().unwrap());
                let d1 = if v0_str {
                    new_collation_string_datum(
                        hk[1].as_str().unwrap().as_bytes().to_vec(),
                        "utf8mb4_general_ci",
                    )
                } else {
                    new_string_datum(hk[1].as_str().unwrap().as_bytes().to_vec())
                };
                let mut enc = Vec::new();
                encode_key(&tidb_base::gostd::time::utc(), &mut enc, &[d0, d1])?;
                Handle::Common(Arc::new(new_common_handle(enc)?))
            };
            let rk = encode_row_key_with_handle(table_id, &h);
            let row = te["row"].as_array().cloned().unwrap_or_default();
            table_kvs.push((rk.0, row));
        }
    }
    table_kvs.sort_by(|a, b| a.0.cmp(&b.0));

    let mock_client = Arc::new(LookUpMockClient {
        index_look_up_push_down: push_down,
        common_handle_v0_str: v0_str,
        index_err,
        table_err,
        cop_conc,
        index_streams,
        index_fts,
        table_fts: table_fts.clone(),
        table_kvs,
        state: Mutex::new((0, Vec::new())),
    });

    let is = info_schema_with_table(tbl_info).await;
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.max_chunk_size = max_chunk_size as i64;
    vars.enable_chunk_rpc = true;
    vars.enable_paging = index_paging;
    vars.index_lookup_size = index_lookup_size;
    if index_lookup_conc > 0 {
        vars.set_index_lookup_concurrency(index_lookup_conc);
    }
    if dist_sql_conc > 0 {
        vars.set_dist_sql_scan_concurrency(dist_sql_conc);
    }
    vars.stmt_ctx_mut().weak_consistency = weak_consistency;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, -1));
    vars.stmt_ctx_mut().runtime_stats_coll = Some(RuntimeStatsColl::new(None));
    let expr_ctx: ExprContextRef = es::new_expr_context(Vec::new());

    let mut sctx = ReplaySession {
        vars,
        killer: SqlKiller::new(),
        expr_ctx,
        info_schema: Arc::clone(&is),
        client: Some(Arc::clone(&mock_client) as Arc<dyn KvClient>),
    };
    let mut builder = new_executor_builder(
        &sctx,
        Some(Arc::clone(&is)),
        Arc::new(TelemetryInfo::default()),
    );
    builder.for_data_reader_builder = true;
    builder.data_reader_ts = 100;
    let mut ilu: IndexLookUpExecutor =
        match builder.build_index_look_up_reader(&mut sctx, &reader).await {
            Some(e) => e,
            None => return Err(builder.err.unwrap()),
        };
    if set_dummy_call {
        ilu.set_dummy();
    }
    let tbl_id_check = ilu.table().unwrap().meta().id;
    let index_names = sctx.vars.stmt_ctx().index_names.clone();
    let table_ids = sctx.vars.stmt_ctx().table_ids.clone();

    let exec_ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    let mut err_str = String::new();
    let mut out_rows: Vec<Value> = Vec::new();

    if let Err(err) = exec::open(&exec_ctx, &mut ilu).await {
        err_str = err.to_string();
        let _ = exec::close(&mut ilu).await;
    } else {
        let mut chk = exec::new_first_chunk(&ilu);
        let num_cols = chk.num_cols();
        let mut req_idx = 0usize;
        loop {
            let req_rows = required_rows
                .get(req_idx)
                .copied()
                .unwrap_or(max_chunk_size as isize);
            if req_idx < required_rows.len() {
                req_idx += 1;
            }
            chk.set_required_rows(req_rows, max_chunk_size);
            if let Err(err) = exec::next(&exec_ctx, &mut ilu, &mut chk).await {
                err_str = err.to_string();
                break;
            }
            if chk.num_rows() == 0 {
                break;
            }
            for r in 0..chk.num_rows() {
                let row = chk.get_row(r);
                let mut vals = Vec::with_capacity(num_cols);
                for (c, ft) in table_fts.iter().enumerate().take(num_cols) {
                    if row.is_null(c) {
                        vals.push(Value::Null);
                    } else if ft.eval_type() == ETString {
                        vals.push(json!(
                            String::from_utf8_lossy(row.get_string(c)).into_owned()
                        ));
                    } else {
                        vals.push(json!(row.get_int64(c)));
                    }
                }
                out_rows.push(Value::Array(vals));
            }
        }
        if let Err(err) = exec::close(&mut ilu).await
            && err_str.is_empty()
        {
            err_str = err.to_string();
        }
    }

    let index_reqs = mock_client.state.lock().1.clone();
    if !keep_order {
        sort_json_rows(&mut out_rows);
    }
    let table_task_num = ilu.stats.as_ref().map_or(0, |st| {
        st.table_task_num.load(std::sync::atomic::Ordering::Relaxed)
    });

    let mut out = serde_json::Map::new();
    out.insert("index_reqs".into(), Value::Array(index_reqs));
    out.insert("table_task_num".into(), json!(table_task_num));
    out.insert("rows".into(), Value::Array(out_rows));
    if !err_str.is_empty() {
        out.insert("err".into(), json!(err_str));
    }
    out.insert("table_id_check".into(), json!(tbl_id_check));
    out.insert("index_names".into(), json!(index_names));
    out.insert("table_ids".into(), json!(table_ids));
    Ok(Value::Object(out))
}

#[tokio::test]
async fn govec_readexec() {
    let mut paths = vec![vectors("readexec")];
    if let Ok(extra) = std::env::var("GOVEC_READEXEC_FILES") {
        for p in extra.split(',').filter(|s| !s.is_empty()) {
            paths.push(p.to_owned());
        }
    }

    let mut total = 0usize;
    let mut blocked_cnt = 0usize;
    let mut mismatches = Vec::new();

    for path in &paths {
        for (idx, line) in read_records(path, "readexec").iter().enumerate() {
            let rec: Value = serde_json::from_str(line).unwrap();
            let op = rec["op"].as_str().unwrap();
            let args = rec["args"].as_array().unwrap();
            let expected_out = &rec["out"];
            let expected_err = &rec["err"];
            total += 1;

            let actual: Result<Value, Error> = match op {
                "closest_read_adjuster" => Ok(replay_closest_read_adjuster(args)),
                "table_reader" => replay_table_reader(args).await,
                "index_reader" => replay_index_reader(args).await,
                "index_lookup_helpers" => Ok(replay_index_lookup_helpers(args)),
                "index_lookup" => replay_index_lookup(args).await,
                other => panic!("unexpected op: {other}"),
            };

            match actual {
                Ok(got) => {
                    if !expected_err.is_null() {
                        mismatches.push(format!(
                            "{path}:{idx} ({op} src={}): expected err {expected_err}, got out {got}",
                            rec["src"]
                        ));
                    } else if &got != expected_out {
                        mismatches.push(format!(
                            "{path}:{idx} ({op} src={}):\n  want: {expected_out}\n  got:  {got}",
                            rec["src"]
                        ));
                    }
                }
                Err(err) => {
                    if tidb_base::blocked::blocked_by(&err).is_some() {
                        blocked_cnt += 1;
                    } else if expected_err.is_null() {
                        mismatches.push(format!(
                            "{path}:{idx} ({op} src={}): unexpected err {err} (expected out {expected_out})",
                            rec["src"]
                        ));
                    }
                }
            }
        }
    }

    assert_eq!(blocked_cnt, EXPECTED_BLOCKED, "blocked count mismatch");
    assert!(
        mismatches.is_empty(),
        "readexec govec mismatches ({}/{}):\n{}",
        mismatches.len(),
        total,
        mismatches
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(total >= 550, "expected >= 550 records, got {total}");
}
