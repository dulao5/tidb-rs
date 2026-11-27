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
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::config;
use tidb_base::kv::TiKV;
use tidb_base::mysql::{NotNullFlag, PriKeyFlag, TypeLonglong, TypeVarchar, UnsignedFlag};
use tidb_chunk::chunk::Chunk;
use tidb_executor::adapter::TelemetryInfo;
use tidb_executor::builder::{new_closest_read_adjuster, new_executor_builder};
use tidb_executor::exec::ExecCtx;
use tidb_executor::glue::{InfoSchemaRef, Plan, SessionContext};
use tidb_executor::internal::exec::executor::{self as exec, Executor};
use tidb_executor::table_reader::SelectResultHook;
use tidb_expression::Column;
use tidb_expression::constant::Constant;
use tidb_expression::expression::Expr;
use tidb_expression::scalar_function::new_function;
use tidb_expression::schema::new_schema;
use tidb_kv::kv::{Client as KvClient, ClientSendOption, OptionValue, Request, Response, Snapshot};
use tidb_kv::option::{
    ReplicaReadClosest, ReplicaReadClosestAdaptive, ReplicaReadFollower, ReplicaReadLeader,
    ReplicaReadMixed, ReplicaReadType,
};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;
use tidb_model::{
    ColumnInfo, DBInfo, IndexColumn, IndexInfo, StatePublic, TableInfo, TempTableLocal,
};
use tidb_parser::ast::funcs::{Mul, Plus};
use tidb_parser::model::CIStr;
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_planner::core::physical_plans::{
    Cop, PhysicalIndexReader, PhysicalIndexScan, PhysicalTableReader, PhysicalTableScan,
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
use tidb_types::datum::{Datum, new_int_datum, new_string_datum, new_uint_datum};
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
        Arc::new(DistSQLContext {
            client: Some(Arc::new(DummyClient)),
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
