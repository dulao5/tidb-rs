//! Replays the govec `pointget` vectors (tidb-rs-tracker `govec/domains/pointget.go`):
//! block D12.1 (VS1-12), `NewRowDecoder` and `DecodeRowValToChunk` of the point get, against
//! Go TiDB v8.5.8: row formats v1 and v2 (small, large, with a checksum), int and common
//! handles (prefix and restored-data columns), columns missing from the row (origin
//! defaults of added columns), stored NULLs, virtual columns and `_tidb_rowid`.
//!
//! Each record is replayed twice: through `new_row_decoder` / `decode_row_val_to_chunk`
//! (compared as JSON with Go's output), and, when Go decodes it without error and the
//! schema has no virtual column, through the whole point get executor (built from the
//! plan carrier over a snapshot holding the row) whose row must be the same.
//!
//! The record keys of a point get are not repeated in `pointget`: the `tablecodec` domain's
//! `keys` records (`EncodeRowKeyWithHandle`, `rowh`) are replayed through the executor here,
//! which must read exactly that key (VS1-12 review: one vector set for handle encoding,
//! shared with the snapshot `Get` of VS1-18).
//!
//! `GOVEC_POINTGET_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::SystemTime;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{SQLWarn, WarnHandler, new_static_warn_handler};
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_base::mysql::{SQLMode, TypeLonglong};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_executor::adapter::TelemetryInfo;
use tidb_executor::builder::{new_executor_builder, new_row_decoder};
use tidb_executor::exec::ExecCtx;
use tidb_executor::glue::{InfoSchemaRef, Plan, PointGetPlan, SessionContext};
use tidb_executor::internal::exec::executor as exec;
use tidb_executor::point_get::{decode_row_val_to_chunk, fill_row_checksum};
use tidb_expression::Column;
use tidb_expression::constant::new_zero;
use tidb_expression::expression::Expr;
use tidb_expression::schema::new_schema;
use tidb_kv::context::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::{Handle, IntHandle, Key, new_common_handle, new_partition_handle};
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, Iterator as KvIterator, KvFuture, OptionValue,
    Retriever, Snapshot, ValueEntry,
};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_model::{ExtraHandleID, ExtraRowChecksumID, TableInfo};
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_tablecodec::tablecodec;
use tidb_types::MyDecimal;
use tidb_types::context::Flags;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::field_type::new_field_type;

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

fn handle_of(v: &Value) -> Handle {
    match v["t"].as_str().unwrap() {
        "int" => Handle::Int(IntHandle(v["v"].as_i64().unwrap_or(0))),
        "common" => Handle::Common(Arc::new(
            new_common_handle(unhex(v["enc"].as_str().unwrap_or(""))).unwrap(),
        )),
        "part" => Handle::Partition(new_partition_handle(
            v["pid"].as_i64().unwrap_or(0),
            handle_of(&v["h"]),
        )),
        t => panic!("bad spec {t}"),
    }
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

fn try_(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
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
    fn get_txn_info_schema(&self) -> Option<InfoSchemaRef> {
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

// ---------------------------------------------------------------- decode_row_val

/// pgSchema: the plan schema (`colInfoToColumn`: `RetType` a copy of the column's type,
/// shared by the decoder from then on).
fn schema_of(tbl: &TableInfo, spec: &Value) -> tidb_expression::Schema {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let spec = spec.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    new_schema(
        spec.iter()
            .enumerate()
            .map(|(i, c)| {
                let off = c["off"].as_i64().unwrap();
                if off == -2 {
                    // _tidb_row_checksum: CHAR(MaxBlobWidth) in the default charset.
                    let mut ft = new_field_type(tidb_base::mysql::TypeString);
                    ft.set_charset("utf8mb4");
                    ft.set_collate("utf8mb4_bin");
                    ft.set_flen(tidb_base::mysql::MaxBlobWidth);
                    ft.set_decimal(0);
                    return Arc::new(Column {
                        ret_type: Arc::new(ft),
                        id: ExtraRowChecksumID as i64,
                        unique_id: ExtraRowChecksumID as i64,
                        index: i as isize,
                        ..Column::default()
                    });
                }
                if off < 0 {
                    return Arc::new(Column {
                        ret_type: Arc::new(new_field_type(TypeLonglong)),
                        id: ExtraHandleID as i64,
                        unique_id: 1000 + i as i64,
                        index: i as isize,
                        ..Column::default()
                    });
                }
                let ci = &cols[off as usize];
                let mut col = Column {
                    ret_type: Arc::new(ci.field_type.clone()),
                    id: ci.id,
                    unique_id: 1000 + i as i64,
                    index: i as isize,
                    orig_name: ci.name.o.clone(),
                    ..Column::default()
                };
                if c["virtual"].as_bool().unwrap_or(false) {
                    col.virtual_expr = Some(Arc::new(Expr::Constant(new_zero())));
                }
                Arc::new(col)
            })
            .collect(),
    )
}

struct Decoded {
    out: Value,
    /// The row Go and Rust decoded, for the executor replay (`None`: an error, a panic or
    /// an incomplete row).
    row: Option<Vec<Datum>>,
}

fn decode_row_val(a: &[Value]) -> Decoded {
    let (ctx, ev) = ctx_of(&a[0]);
    let loc = loc_of(&a[0].as_array().unwrap()[1]);
    let tbl = tbl_in(&a[1]);
    let schema = schema_of(&tbl, &a[2]);
    let h = handle_of(&a[3]);
    let val = unhex(a[4].as_str().unwrap());
    let expr_ctx: ExprContextRef = ctx;
    let mut row = None;
    let r = try_(|| {
        let rd = new_row_decoder(&expr_ctx, Arc::clone(&loc), &schema, &tbl);
        let fts: Vec<Arc<_>> = schema
            .columns
            .iter()
            .map(|c| Arc::clone(&c.ret_type))
            .collect();
        let mut chk = new_chunk_with_capacity(&fts, 1);
        let e = decode_row_val_to_chunk(&*expr_ctx, &loc, &schema, &tbl, &h, &val, &mut chk, &rd);
        let lens: Vec<usize> = (0..chk.num_cols()).map(|i| chk.column(i).rows()).collect();
        let mut res = serde_json::Map::new();
        res.insert("lens".into(), json!(lens));
        if let Err(e) = &e {
            res.insert("err".into(), gs(&e.error()));
        }
        if lens.iter().all(|&l| l == 1) {
            let r = chk.get_row(0);
            let ds: Vec<Datum> = fts
                .iter()
                .enumerate()
                .map(|(i, ft)| r.get_datum(i, ft))
                .collect();
            // Go's nil slice when the schema is empty.
            let out = if ds.is_empty() {
                Value::Null
            } else {
                Value::Array(ds.iter().map(datum_out).collect())
            };
            res.insert("row".into(), out);
            if e.is_ok() {
                row = Some(ds);
            }
        }
        Value::Object(res)
    });
    let w = warns(&ev.truncate_warnings(0));
    Decoded {
        out: json!({ "r": r, "w": w }),
        row,
    }
}

/// The same record through the executor: same row (or no row when the row value is empty).
fn executor_replay(a: &[Value], want: &[Datum]) -> Option<String> {
    let (ctx, _ev) = ctx_of(&a[0]);
    let loc = loc_of(&a[0].as_array().unwrap()[1]);
    // Partitioned and cached tables stop at their stubs before the read (unit tests cover
    // that); the row decoding does not depend on either, so the replay reads them as plain
    // tables.
    let mut t = TableInfo::clone(&tbl_in(&a[1]));
    t.partition = None;
    t.table_cache_status_type = tidb_model::TableCacheStatusDisable;
    let tbl = Arc::new(t);
    let schema = schema_of(&tbl, &a[2]);
    if schema.is_empty()
        || schema
            .columns
            .iter()
            .any(|c| c.virtual_expr.is_some() || c.id == ExtraRowChecksumID as i64)
    {
        // No column: the chunk has no rows to compare. A virtual column:
        // FillVirtualColumnValue would evaluate the placeholder expression. The checksum
        // column: Next then fills it (fillRowChecksum, the row_checksum records), which
        // the decode-only record does not show.
        return None;
    }
    let h = handle_of(&a[3]);
    let val = unhex(a[4].as_str().unwrap());
    let mut snap = MemSnapshot::default();
    let key = tablecodec::encode_row_key_with_handle(tbl.id, &h);
    snap.data.insert(key.0.clone(), val.clone());
    let snap = Arc::new(snap);
    let mut s = session(ctx, Some(loc), Arc::clone(&snap));
    let plan = PointGetPlan {
        id: 1,
        schema,
        tbl_info: Arc::clone(&tbl),
        handle: Some(h),
        columns: tbl.columns.clone().unwrap_or_default(),
        ..PointGetPlan::default()
    };
    let got = match catch_unwind(AssertUnwindSafe(|| run_point_get(&mut s, plan))) {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return Some(format!("executor: {e}")),
        Err(_) => return Some("executor panicked".to_owned()),
    };
    if *snap.gets.lock() != vec![key.0] {
        return Some(format!("executor read {:?}", snap.gets.lock()));
    }
    let want_rows: Vec<&[Datum]> = if val.is_empty() { vec![] } else { vec![want] };
    let got_v: Vec<Value> = got
        .iter()
        .map(|r| Value::Array(r.iter().map(datum_out).collect()))
        .collect();
    let want_v: Vec<Value> = want_rows
        .iter()
        .map(|r| Value::Array(r.iter().map(datum_out).collect()))
        .collect();
    (got_v != want_v).then(|| format!("executor rows: got {got_v:?}\n    want {want_v:?}"))
}

/// row_checksum: `fillRowChecksum` of one row into a fresh chunk, in the session zone
/// `a[1]`. Each column: `"-"` (no row: untouched, or an error before `SetCol`), null, or
/// the checksum string.
fn row_checksum(a: &[Value]) -> Value {
    let (ctx, ev) = ctx_of(&a[0]);
    let tz = fixed_zone("Z", a[1].as_i64().unwrap() as i32);
    let tbl = tbl_in(&a[2]);
    let schema = schema_of(&tbl, &a[3]);
    let h = handle_of(&a[4]);
    let val = unhex(a[5].as_str().unwrap());
    let expr_ctx: ExprContextRef = ctx;
    let r = try_(|| {
        let fts: Vec<Arc<_>> = schema
            .columns
            .iter()
            .map(|c| Arc::clone(&c.ret_type))
            .collect();
        let mut chk = new_chunk_with_capacity(&fts, 1);
        let e = fill_row_checksum(
            &*expr_ctx,
            &tz,
            0,
            1,
            &schema,
            &tbl,
            &[&val],
            &[&h],
            &mut chk,
            Vec::new(),
        );
        let mut res = serde_json::Map::new();
        if let Err(e) = &e {
            res.insert("err".into(), gs(&e.error()));
        }
        let cols: Vec<Value> = (0..chk.num_cols())
            .map(|i| {
                let c = chk.column(i);
                if c.rows() == 0 {
                    json!("-")
                } else if c.is_null(0) {
                    Value::Null
                } else {
                    gs(c.get_string(0))
                }
            })
            .collect();
        res.insert(
            "cols".into(),
            if cols.is_empty() {
                Value::Null
            } else {
                Value::Array(cols)
            },
        );
        Value::Object(res)
    });
    let w = warns(&ev.truncate_warnings(0));
    json!({ "r": r, "w": w })
}

fn check_pointget(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    if op == "row_checksum" {
        let got = catch_unwind(AssertUnwindSafe(|| row_checksum(args)))
            .unwrap_or_else(|_| json!("Rust panicked outside the call"));
        return (got != rec["out"]).then(|| format!("out: got {got}\n    want {}", rec["out"]));
    }
    assert_eq!(op, "decode_row_val");
    let got = catch_unwind(AssertUnwindSafe(|| decode_row_val(args)));
    let d = match got {
        Ok(d) => d,
        Err(_) => return Some("Rust panicked outside the decode".to_owned()),
    };
    if d.out != rec["out"] {
        return Some(format!("out: got {}\n    want {}", d.out, rec["out"]));
    }
    // The decoders borrow the schema's field types for each decode (N13): decoding the
    // same row again gives the same result.
    let again = decode_row_val(args);
    if again.out != d.out {
        return Some("decoding twice differs".to_owned());
    }
    let row = d.row?;
    executor_replay(args, &row)
}

fn report(what: &str, n: usize, fails: Vec<String>) {
    let show = std::env::var("GOVEC_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    for f in fails.iter().take(show) {
        eprintln!("FAIL {f}");
    }
    assert!(
        fails.is_empty(),
        "{what}: {} of {n} records differ",
        fails.len()
    );
}

fn replay_pointget(path: &str) {
    let lines = read_records(path, "pointget");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let prev = std::panic::take_hook();
    if std::env::var("GOVEC_PANIC").is_err() {
        std::panic::set_hook(Box::new(|_| {}));
    }
    let mut fails = vec![];
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check_pointget(&rec) {
            let cut = line
                .char_indices()
                .nth(600)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            fails.push(format!("{}\n    {why}", &line[..cut]));
        }
    }
    std::panic::set_hook(prev);
    report(path, lines.len(), fails);
}

#[test]
fn govec_pointget() {
    replay_pointget(&vectors("executor", "pointget"));
    if let Ok(files) = std::env::var("GOVEC_POINTGET_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay_pointget(f);
        }
    }
}

// ---------------------------------------------------------------- the record keys

/// The tablecodec domain's `keys` records with a handle: a point get of that handle in
/// table `tid` reads exactly Go's `EncodeRowKeyWithHandle(tid, h)` (`rowh`).
#[test]
fn govec_tablecodec_record_keys_through_the_point_get() {
    let lines = read_records(&vectors("tablecodec", "tablecodec"), "tablecodec");
    let mut fails = vec![];
    let mut n = 0;
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        if rec["op"] != "keys" || rec["args"][2].is_null() {
            continue;
        }
        n += 1;
        let a = rec["args"].as_array().unwrap();
        let tid = a[0].as_i64().unwrap();
        let h = handle_of(&a[2]);
        let want = rec["out"]["rowh"].as_str().unwrap();
        let tbl = Arc::new(TableInfo {
            id: tid,
            ..TableInfo::default()
        });
        let snap = Arc::new(MemSnapshot::default());
        let mut s = session(es::new_expr_context(Vec::new()), None, Arc::clone(&snap));
        let plan = PointGetPlan {
            id: 1,
            schema: new_schema(vec![Arc::new(Column {
                ret_type: Arc::new(new_field_type(TypeLonglong)),
                id: ExtraHandleID as i64,
                ..Column::default()
            })]),
            tbl_info: tbl,
            handle: Some(h),
            ..PointGetPlan::default()
        };
        match run_point_get(&mut s, plan) {
            Ok(rows) if rows.is_empty() => {}
            other => fails.push(format!("{line}\n    {other:?}")),
        }
        let gets = snap.gets.lock().clone();
        if gets.len() != 1 || hex(&gets[0]) != want {
            fails.push(format!("{line}\n    read {gets:?}, want [{want}]"));
        }
    }
    assert!(n >= 300, "only {n} keys records with a handle");
    report("tablecodec keys", n, fails);
}

// Keep the datum kinds of the helpers in use.
const _: u8 = dm::KindNull;
