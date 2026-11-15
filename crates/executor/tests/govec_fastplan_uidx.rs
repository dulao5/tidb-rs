//! Replays the govec `fastplan` vectors, op `uidx` (tidb-rs-tracker
//! `govec/domains/fastplan.go`): the unique-index path of the point get (VS1-11, with the
//! executor of VS1-12) against Go TiDB v8.5.8.
//!
//! Each record is a table with a unique index (int handle, clustered key or `_tidb_rowid`;
//! sometimes partitioned with a global index), the values of a point get on it (of the
//! column types or not: the executor casts them again) and the session's statement context
//! (type flags, error levels, time zone, SQL mode; new collations on or off). It is replayed
//! twice:
//!
//! 1. `EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` directly: the key, the error
//!    (`ErrNotExist` for values the index cannot hold), the values cast in place, warnings;
//! 2. the point get executor over a snapshot holding Go's index entry (the handle, restored
//!    data of new-collation columns, the partition ID of a global index) and Go's stored row:
//!    the executor must read Go's index key and then Go's row key (the handle decoded from the
//!    index value, `decode_handle_in_index_value`, in the partition of a global index) and
//!    return the row Go decodes; without the row, it must fail with Go's inconsistency
//!    error; a global index entry in a partition the statement does not select gives no row.
//!    For the clustered primary key (`isCommonHandleRead`) the row key is the encoded values.
//!
//! The new-collation switch is process-wide: this file holds one test.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{SQLWarn, new_static_warn_handler};
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_base::mysql::SQLMode;
use tidb_executor::adapter::TelemetryInfo;
use tidb_executor::builder::new_executor_builder;
use tidb_executor::exec::ExecCtx;
use tidb_executor::glue::{InfoSchemaRef, Plan, SessionContext};
use tidb_executor::internal::exec::executor as exec;
use tidb_expression::Column;
use tidb_expression::schema::new_schema;
use tidb_kv::context::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, Iterator as KvIterator, KvFuture, OptionValue,
    Retriever, Snapshot, ValueEntry,
};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_model::TableInfo;
use tidb_planner::PhysicalPlan;
use tidb_planner::core::point_get_plan::PointGetPlan;
use tidb_planner::core::util::{encode_unique_index_key, encode_unique_index_values_for_key};
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::stmtctx::new_stmt_ctx_with_time_zone;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::context::Flags;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::time::{Duration, Time};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors(crate_dir: &str, domain: &str) -> String {
    format!(
        "{}/../{crate_dir}/tests/vectors/v8.5.8/{GO_TARGET}/{domain}.jsonl.gz",
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

// ---------------------------------------------------------------- values (as govec_table.rs)

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// vec.GoString's JSON form.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn dec_out(d: &MyDecimal) -> Value {
    let j: Value = serde_json::from_slice(&d.marshal_json().unwrap()).unwrap();
    let mut st = vec![
        j["DigitsInt"].clone(),
        j["DigitsFrac"].clone(),
        j["ResultFrac"].clone(),
        json!(i64::from(j["Negative"].as_bool().unwrap())),
    ];
    st.extend(j["WordBuf"].as_array().unwrap().iter().cloned());
    json!({ "s": String::from_utf8(d.to_string()).unwrap(), "str": d.string(), "st": st })
}

fn datum_out(d: &Datum) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("k".into(), json!(d.kind()));
    m.insert("frac".into(), json!(d.frac()));
    m.insert("len".into(), json!(d.length()));
    m.insert("i".into(), json!(d.get_int64()));
    m.insert("coll".into(), gs(d.collation().as_bytes()));
    m.insert("b".into(), json!(hex(d.get_bytes())));
    if d.kind() == dm::KindRaw && d.raw_b().is_none() {
        m.insert("raw_nil".into(), json!(true));
    }
    match d.kind() {
        dm::KindMysqlDecimal => {
            m.insert("dec".into(), dec_out(d.get_mysql_decimal()));
        }
        dm::KindMysqlTime => {
            let t = d.get_mysql_time();
            m.insert(
                "time".into(),
                json!({ "raw": t.raw().0, "s": gs(&t.string()) }),
            );
        }
        _ => {}
    }
    Value::Object(m)
}

fn loc_of(v: &Value) -> Arc<Location> {
    if v.is_null() {
        utc()
    } else {
        fixed_zone("Z", v.as_i64().unwrap() as i32)
    }
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

type Ctxs = (Arc<es::ExprContext>, Arc<es::EvalContext>);

/// tblCtx: [castCtx, loc]; castCtx is [typeFlags, [7]errLevels, maxAllowedPacket, sqlMode,
/// now (unix ns)].
fn ctx_of(v: &Value) -> Ctxs {
    let outer = v.as_array().unwrap();
    let a = outer[0].as_array().unwrap();
    let mut lm: LevelMap = Default::default();
    for (i, l) in a[1].as_array().unwrap().iter().enumerate() {
        lm[i] = Level(l.as_u64().unwrap() as _);
    }
    let ns = a[4].as_i64().unwrap();
    let opts = vec![
        es::with_type_flags(Flags(a[0].as_u64().unwrap() as u16)),
        es::with_err_level_map(lm),
        es::with_warn_handler(Some(Arc::new(new_static_warn_handler(0)))),
        es::with_max_allowed_packet(a[2].as_u64().unwrap()),
        es::with_sql_mode(SQLMode(a[3].as_u64().unwrap() as i64)),
        es::with_current_time(Arc::new(move || Ok(tidb_base::gostd::time::unix(0, ns)))),
        es::with_location(Some(loc_of(&outer[1]))),
    ];
    let ev = es::new_eval_context(opts);
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(Arc::clone(&ev))]);
    (ctx, ev)
}

fn tbl_in(v: &Value) -> Arc<TableInfo> {
    let mut t = TableInfo::default();
    t.unmarshal_json(v.as_str().unwrap().as_bytes()).unwrap();
    Arc::new(t)
}

// ---------------------------------------------------------------- a snapshot and a session

/// A snapshot over a map; records the keys read.
#[derive(Default)]
struct MemSnapshot {
    data: HashMap<Vec<u8>, Vec<u8>>,
    gets: Mutex<Vec<Vec<u8>>>,
}

impl Getter for MemSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        self.gets.lock().push(k.0.clone());
        let v = self.data.get(&k.0).cloned();
        Box::pin(async move {
            match v {
                Some(value) => Ok(ValueEntry {
                    value,
                    commit_ts: 0,
                }),
                None => Err(ErrNotExist.clone()),
            }
        })
    }
}
impl Retriever for MemSnapshot {
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MemSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Snapshot for MemSnapshot {
    fn set_option(&self, _: isize, _: OptionValue) {}
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

/// The record's session: its expression context and time zone (as the Go mock context).
struct Session {
    vars: SessionVars,
    killer: Arc<SqlKiller>,
    expr_ctx: ExprContextRef,
    snapshot: Arc<MemSnapshot>,
}

impl SessionContext for Session {
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
        Box::pin(async { Ok(u64::MAX) })
    }
    fn get_txn_info_schema(&mut self) -> Option<InfoSchemaRef> {
        None
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
        Ok(None)
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
        let s: Arc<dyn Snapshot> = Arc::clone(&self.snapshot) as Arc<dyn Snapshot>;
        Box::pin(async move { Ok(s) })
    }
}

fn session(
    expr_ctx: ExprContextRef,
    loc: Option<Arc<Location>>,
    snap: Arc<MemSnapshot>,
) -> Session {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.time_zone = loc;
    Session {
        vars,
        killer: SqlKiller::new(),
        expr_ctx,
        snapshot: snap,
    }
}

/// The point get of `plan` over `snap`: the rows' datums, or the error.
fn run_point_get(s: &mut Session, plan: PointGetPlan) -> Result<Vec<Vec<Datum>>, Error> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut b = new_executor_builder(s, None, Arc::new(TelemetryInfo::default()));
        let plan: Plan = Arc::new(plan);
        let e = b.build(s, Some(&plan)).await;
        if let Some(err) = b.err {
            return Err(err);
        }
        let mut e = e.expect("built");
        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        exec::open(&ctx, e.as_mut()).await?;
        let mut chk = exec::new_first_chunk(e.as_ref());
        let fts: Vec<_> = e.ret_field_types().iter().map(|f| (**f).clone()).collect();
        let mut out = Vec::new();
        loop {
            exec::next(&ctx, e.as_mut(), &mut chk).await?;
            if chk.num_rows() == 0 {
                break;
            }
            for r in 0..chk.num_rows() {
                out.push(chk.get_row(r).get_datum_row(&fts));
            }
        }
        exec::close(e.as_mut()).await?;
        Ok(out)
    })
}

// ---------------------------------------------------------------- uidx

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

/// d2DatumIn (the kinds the records use).
fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    let st = |i: usize| p[i].as_str().unwrap().to_owned();
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindInt64 => d.set_int64(i(1)),
        dm::KindUint64 => d.set_uint64(u(1)),
        dm::KindFloat64 => d.set_float64(f64::from_bits(u(1))),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => {
            let mut x = MyDecimal::default();
            let _ = x.from_string(p[1].as_str().unwrap().as_bytes());
            d.set_mysql_decimal(x)
        }
        dm::KindMysqlDuration => d.set_mysql_duration(Duration {
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
        dm::KindMysqlTime => d.set_mysql_time(Time::from_raw(CoreTime(u(1)))),
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

/// Go's error output: ErrNotExist by name, otherwise the message.
fn err_out(e: &Result<(), Error>) -> Value {
    match e {
        Ok(()) => Value::Null,
        Err(e) if ErrNotExist.equal(e) => json!("ErrNotExist"),
        Err(e) => gs(&e.error()),
    }
}

/// The plan schema of all public columns (pgSchema of the pointget domain).
fn full_schema(tbl: &TableInfo) -> tidb_expression::Schema {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    new_schema(
        cols.iter()
            .filter(|c| c.state == tidb_model::StatePublic)
            .enumerate()
            .map(|(i, c)| {
                Arc::new(Column {
                    ret_type: Arc::new(c.field_type.clone()),
                    id: c.id,
                    unique_id: 1000 + i as i64,
                    index: i as isize,
                    orig_name: c.name.o.clone(),
                    ..Column::default()
                })
            })
            .collect(),
    )
}

/// The executor's rows or its error.
type Rows = Result<Vec<Vec<Datum>>, Error>;

/// Replays one record; `None` when it matches Go.
fn check_uidx(rec: &Value) -> Option<String> {
    let a = rec["args"].as_array().unwrap();
    let out = &rec["out"];
    collate::set_new_collation_enabled_for_test(a[0].as_i64().unwrap() == 1);
    let ca = a[1].as_array().unwrap();
    let mut lm: LevelMap = Default::default();
    for (i, l) in ca[1].as_array().unwrap().iter().enumerate() {
        lm[i] = Level(l.as_u64().unwrap() as _);
    }
    let loc = fixed_zone("Z", a[2].as_i64().unwrap() as i32);
    let mut sc = new_stmt_ctx_with_time_zone(Arc::clone(&loc));
    sc.set_type_flags(Flags(ca[0].as_u64().unwrap() as u16));
    sc.set_err_levels(lm);
    let sql_mode = SQLMode(ca[2].as_u64().unwrap() as i64);
    let tbl = tbl_in(&a[3]);
    let idx_name = a[4].as_str().unwrap();
    let idx = Arc::clone(
        tbl.indices
            .as_deref()
            .unwrap()
            .iter()
            .find(|i| i.name.l == idx_name)
            .unwrap(),
    );
    let vals: Vec<Datum> = a[5].as_array().unwrap().iter().map(datum_in).collect();
    let tid = a[6].as_i64().unwrap();

    // 1. The key encoding.
    let mut v1 = vals.clone();
    let key = catch_unwind(AssertUnwindSafe(|| {
        encode_unique_index_key(&sc, sql_mode, 0, &tbl, &idx, &mut v1, tid)
    }));
    if out.get("key_panic").is_some() {
        return key
            .is_ok()
            .then(|| "Go panics in EncodeUniqueIndexKey".to_owned());
    }
    let Ok(key) = key else {
        return Some("EncodeUniqueIndexKey panicked".to_owned());
    };
    let (key, kerr) = match key {
        Ok(k) => (k, Ok(())),
        Err(e) => (Vec::new(), Err(e)),
    };
    let mut v2 = vals.clone();
    let vfk = catch_unwind(AssertUnwindSafe(|| {
        encode_unique_index_values_for_key(&sc, sql_mode, 0, &tbl, &idx, &mut v2)
    }));
    if out.get("vfk_panic").is_some() {
        return vfk
            .is_ok()
            .then(|| "Go panics in EncodeUniqueIndexValuesForKey".to_owned());
    }
    let Ok(vfk) = vfk else {
        return Some("EncodeUniqueIndexValuesForKey panicked".to_owned());
    };
    let (vfk, verr) = match vfk {
        Ok(k) => (k, Ok(())),
        Err(e) => (Vec::new(), Err(e)),
    };
    let got = json!({
        "key": hex(&key), "key_err": err_out(&kerr),
        "key_vals": Value::Array(v1.iter().map(datum_out).collect()),
        "vfk": hex(&vfk), "vfk_err": err_out(&verr), "w": warns(&sc.get_warnings()),
    });
    for k in ["key", "key_err", "key_vals", "vfk", "vfk_err", "w"] {
        if got[k] != out[k] {
            return Some(format!("{k}: got {}", got[k]));
        }
    }
    if kerr.is_err() || verr.is_err() {
        return None;
    }
    // A record whose index entry or row Go could not build has nothing more to replay.
    let row = out["row"].as_str()?;
    let want = out["dec"].as_array()?;

    // 2. The point get executor.
    let common_read = tbl.is_common_handle && idx.primary;
    let row_key = unhex(out["rowkey"].as_str().unwrap());
    let mut data = HashMap::new();
    if !common_read {
        data.insert(key.clone(), unhex(out["ival"].as_str().unwrap()));
    }
    let pnames: Vec<tidb_parser::model::CIStr> = a[9]
        .as_array()
        .map(|v| {
            v.iter()
                .map(|n| tidb_parser::model::CIStr::new(n.as_str().unwrap()))
                .collect()
        })
        .unwrap_or_default();
    // A global index entry in a partition the statement does not select (matchPartitionNames).
    let skipped = out.get("pid").is_some_and(|pid| {
        !pnames.is_empty() && {
            let pi = tbl.get_partition_info().unwrap();
            let def = pi
                .definitions
                .as_deref()
                .unwrap()
                .iter()
                .find(|d| d.id == pid.as_i64().unwrap());
            !def.is_some_and(|d| pnames.iter().any(|n| n.l == d.name.l))
        }
    });
    let run = |with_row: bool| -> (Rows, Vec<Vec<u8>>) {
        let mut d = data.clone();
        if with_row {
            d.insert(row_key.clone(), unhex(row));
        }
        let snap = Arc::new(MemSnapshot {
            data: d,
            gets: Mutex::new(Vec::new()),
        });
        let (ectx, _) = ctx_of(&json!([
            [Flags::default().0, [0, 0, 0, 0, 0, 0, 0], 67108864, 0, 0],
            null
        ]));
        let mut s = session(ectx, Some(utc()), Arc::clone(&snap));
        s.vars.sql_mode = sql_mode.0;
        let mut sc2 = new_stmt_ctx_with_time_zone(Arc::clone(&loc));
        sc2.set_type_flags(Flags(ca[0].as_u64().unwrap() as u16));
        sc2.set_err_levels(lm);
        s.vars.set_stmt_ctx(Arc::new(sc2));
        let mut plan = PointGetPlan {
            schema: full_schema(&tbl),
            tbl_info: Arc::clone(&tbl),
            index_info: Some(Arc::clone(&idx)),
            index_values: vals.clone(),
            partition_names: pnames.clone(),
            columns: tbl.columns.clone().unwrap_or_default(),
            ..PointGetPlan::default()
        };
        plan.set_id(1);
        let res = run_point_get(&mut s, plan);
        let gets = snap.gets.lock().clone();
        (res, gets)
    };
    let want_reads: Vec<Vec<u8>> = if common_read {
        vec![row_key.clone()]
    } else if skipped {
        vec![key.clone()]
    } else {
        vec![key.clone(), row_key.clone()]
    };
    let (res, gets) = run(true);
    if gets != want_reads {
        return Some(format!(
            "executor read {:?}, want {:?}",
            gets.iter().map(|k| hex(k)).collect::<Vec<_>>(),
            want_reads.iter().map(|k| hex(k)).collect::<Vec<_>>()
        ));
    }
    match res {
        Err(e) => return Some(format!("executor: {e}")),
        Ok(rows) if skipped => {
            if !rows.is_empty() {
                return Some("a row of a partition the statement does not select".to_owned());
            }
        }
        Ok(rows) => {
            let got: Vec<Value> = rows
                .first()
                .map(|r| r.iter().map(datum_out).collect())
                .unwrap_or_default();
            if rows.len() != 1 || got != *want {
                return Some(format!("executor row {got:?}"));
            }
        }
    }
    if common_read || skipped {
        return None;
    }
    // Without the row: the inconsistency error.
    let (res, gets) = run(false);
    if gets != want_reads {
        return Some(format!("executor without the row read {} keys", gets.len()));
    }
    match res {
        Err(e) if gs(&e.error()) == out["inconsistent"] => None,
        Err(e) => Some(format!("inconsistency error: {e}")),
        Ok(_) => Some("no inconsistency error".to_owned()),
    }
}

#[test]
fn govec_fastplan_uidx() {
    let recs = read_records(&vectors("planner", "fastplan"), "fastplan");
    let (mut n, mut executed, mut fails) = (0, 0, Vec::new());
    for line in &recs {
        let rec: Value = serde_json::from_str(line).unwrap();
        if rec["op"] != "uidx" {
            continue;
        }
        n += 1;
        if rec["out"].get("dec").is_some() {
            executed += 1;
        }
        if let Some(f) = check_uidx(&rec) {
            fails.push(format!("{line}\n    {f}"));
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    eprintln!("fastplan uidx: {n} records, {executed} through the executor");
    assert!(n >= 1500);
    assert!(
        fails.is_empty(),
        "{} of {n} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
