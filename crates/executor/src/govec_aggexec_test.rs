//! Replays the govec `aggexec` vectors (tidb-rs-tracker `govec/domains/aggexec.go`): the
//! stream aggregation executor of part A of the aggregation executors block (D12.3) and the
//! vectorized group checker (D12.0), built by the executor builder and run Open-Next-Close
//! over a source of fixed chunks, against Go TiDB v8.5.8.
//!
//! A record carries the source's column types and chunks, the GROUP BY columns, the complete
//! aggregate function descriptors (built here without type inference, as in the `aggfuncs`
//! replay), the output types, the chunk sizes, the required rows of each `Next`, the
//! vectorized-expression switch and the `Next` of the source that fails. Go builds the
//! executor from a `PhysicalStreamAgg` (`buildStreamAgg`); the physical plan is not ported
//! (the planner falls back before it), so the replay calls `buildStreamAggFromChildExec` with
//! the plan's fields and the source as the child, which is what `buildStreamAgg` does after
//! building the child. The result chunks (`Row.GetRaw` and the null flag of each cell), the
//! error and the step that returned it are compared.
//!
//! `GOVEC_AGGEXEC_FILES` (comma-separated paths) replays these files instead of the
//! committed one (other seeds, a fresh artifact).

use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::chunk::{self, Chunk};
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, BaseFuncDesc};
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Expr, ExprRef, Schema};
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::core_time::CoreTime;
use tidb_types::enum_type::Enum;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, new_time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::new_executor_builder;
use crate::exec::ExecCtx;
use crate::glue::SessionContext;
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor_v2,
};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

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

// ---------------------------------------------------------------- values (as govec_aggfuncs.rs)

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The domain's afType.ft: [tp, flag, flen, decimal, charset, collate, elems].
fn ft_of(spec: &Value) -> FieldType {
    let a = spec.as_array().unwrap();
    let mut ft = FieldType::new(a[0].as_u64().unwrap() as u8);
    ft.set_flag(a[1].as_u64().unwrap() as usize);
    ft.set_flen(a[2].as_i64().unwrap() as isize);
    ft.set_decimal(a[3].as_i64().unwrap() as isize);
    ft.set_charset(a[4].as_str().unwrap());
    ft.set_collate(a[5].as_str().unwrap());
    let elems: Vec<Vec<u8>> = a[6]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().as_bytes().to_vec())
        .collect();
    ft.set_elems(Some(elems));
    ft
}

fn kind(ft: &FieldType) -> &'static str {
    match ft.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            if mysql::has_unsigned_flag(ft.get_flag()) {
                "u64"
            } else {
                "i64"
            }
        }
        mysql::TypeFloat => "f32",
        mysql::TypeDouble => "f64",
        mysql::TypeNewDecimal => "dec",
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => "time",
        mysql::TypeDuration => "dur",
        mysql::TypeJSON => "json",
        mysql::TypeTiDBVectorFloat32 => "vec",
        mysql::TypeEnum | mysql::TypeSet => "nv",
        mysql::TypeNull => "null",
        _ => "bytes",
    }
}

fn s_u64(v: &Value) -> u64 {
    v.as_str().unwrap().parse().unwrap()
}

fn s_i64(v: &Value) -> i64 {
    v.as_str().unwrap().parse().unwrap()
}

/// govec's chunkAppend (through the Chunk API).
fn append(c: &mut Chunk, ft: &FieldType, col: usize, v: &Value) {
    if v.is_null() {
        c.append_null(col);
        return;
    }
    match kind(ft) {
        "i64" => c.append_int64(col, s_i64(v)),
        "u64" => c.append_uint64(col, s_u64(v)),
        "f32" => c.append_float32(col, f32::from_bits(s_u64(v) as u32)),
        "f64" => c.append_float64(col, f64::from_bits(s_u64(v))),
        "dec" => {
            let mut d = MyDecimal::default();
            let _ = d.from_string(v.as_str().unwrap().as_bytes());
            c.append_my_decimal(col, &d)
        }
        "time" => {
            let a = v.as_array().unwrap();
            let t = new_time(
                CoreTime(s_u64(&a[0])),
                a[1].as_u64().unwrap() as u8,
                a[2].as_i64().unwrap() as isize,
            );
            c.append_time(col, t)
        }
        "dur" => {
            let a = v.as_array().unwrap();
            let d = Duration {
                duration: s_i64(&a[0]),
                fsp: a[1].as_i64().unwrap() as isize,
            };
            c.append_duration(col, d)
        }
        "json" => {
            let j = parse_binary_json_from_string(v.as_str().unwrap().as_bytes()).unwrap();
            c.append_json(col, &j)
        }
        "vec" => {
            let bits = v.as_array().unwrap();
            let mut b = (bits.len() as u32).to_le_bytes().to_vec();
            for x in bits {
                b.extend_from_slice(&(x.as_u64().unwrap() as u32).to_le_bytes());
            }
            let (vv, _) = zero_copy_deserialize_vector_float32(&b).unwrap();
            c.append_vector_float32(col, &vv)
        }
        "nv" => {
            let a = v.as_array().unwrap();
            let name = unhex(a[0].as_str().unwrap());
            let value = a[1].as_u64().unwrap();
            if ft.get_type() == mysql::TypeEnum {
                c.append_enum(col, &Enum { name, value })
            } else {
                c.append_set(col, &Set { name, value })
            }
        }
        _ => {
            let b = unhex(v.as_str().unwrap());
            if ft.get_type() == mysql::TypeVarchar {
                c.append_string(col, &b)
            } else {
                c.append_bytes(col, &b)
            }
        }
    }
}

// ---------------------------------------------------------------- the executor

fn mode_of(m: i64) -> AggFunctionMode {
    match m {
        0 => AggFunctionMode::CompleteMode,
        1 => AggFunctionMode::FinalMode,
        2 => AggFunctionMode::Partial1Mode,
        3 => AggFunctionMode::Partial2Mode,
        4 => AggFunctionMode::DedupMode,
        _ => panic!("mode {m}"),
    }
}

/// The govec `aeSource`: one batch per `Next`, then empty chunks; its `err_at`-th `Next`
/// (counted from 0, also after the batches) fails.
struct Source {
    base: BaseExecutorV2,
    batches: Vec<Chunk>,
    ptr: usize,
    nexts: i64,
    err_at: i64,
}

#[async_trait]
impl Executor for Source {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        let n = self.nexts;
        self.nexts += 1;
        if n == self.err_at {
            return Err(Error::plain("mock child error"));
        }
        if self.ptr >= self.batches.len() {
            return Ok(());
        }
        let b = &self.batches[self.ptr];
        req.append(b, 0, b.num_rows());
        self.ptr += 1;
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// The govec `sortexecSchema`: a column per type (UniqueID i+1, Index i).
fn schema_of(fts: &[FieldType]) -> Schema {
    new_schema(
        fts.iter()
            .enumerate()
            .map(|(i, ft)| {
                Arc::new(Column {
                    unique_id: i as i64 + 1,
                    index: i as isize,
                    ret_type: Arc::new(ft.clone()),
                    ..Column::default()
                })
            })
            .collect(),
    )
}

fn column(fts: &[FieldType], i: usize) -> ExprRef {
    Expr::Column(Column {
        unique_id: i as i64 + 1,
        index: i as isize,
        ret_type: Arc::new(fts[i].clone()),
        ..Column::default()
    })
    .into_ref()
}

fn encode(c: &Chunk) -> Value {
    Value::Array(
        (0..c.num_rows())
            .map(|r| {
                let row = c.get_row(r);
                Value::Array(
                    (0..c.num_cols())
                        .map(|col| json!({"null": row.is_null(col), "raw": hex(row.get_raw(col))}))
                        .collect(),
                )
            })
            .collect(),
    )
}

/// The govec `aggexecExec`: (out, error).
async fn run(cfg: &Value) -> (Value, Option<Error>) {
    let fts: Vec<FieldType> = cfg["types"].as_array().unwrap().iter().map(ft_of).collect();
    let out_fts: Vec<FieldType> = cfg["out"].as_array().unwrap().iter().map(ft_of).collect();
    let sizes = cfg["sizes"].as_array().unwrap();
    let max = sizes[1].as_i64().unwrap();

    let mut sctx = MockSession::new(1);
    let vars = &mut sctx.vars;
    vars.init_chunk_size = sizes[0].as_i64().unwrap();
    vars.max_chunk_size = max;
    vars.enable_vectorized_expression = cfg["vec"].as_bool().unwrap();
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, -1));

    let aggs: Vec<AggFuncDesc> = cfg["aggs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let args = a["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| column(&fts, c.as_u64().unwrap() as usize))
                .collect();
            let base = BaseFuncDesc::from_parts(
                a["name"].as_str().unwrap().to_owned(),
                args,
                Some(Arc::new(ft_of(&a["ret"]))),
            );
            let mut d = AggFuncDesc::from_base(base, a["distinct"].as_bool().unwrap());
            d.mode = mode_of(a["mode"].as_i64().unwrap());
            d
        })
        .collect();
    let group_by: Vec<ExprRef> = cfg["group_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| column(&fts, c.as_u64().unwrap() as usize))
        .collect();
    let batches: Vec<Chunk> = cfg["batches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rows| {
            let rows = rows.as_array().unwrap();
            let mut c = chunk::new_chunk_with_capacity(&fts, rows.len());
            for row in rows {
                for (col, v) in row.as_array().unwrap().iter().enumerate() {
                    append(&mut c, &fts[col], col, v);
                }
            }
            c
        })
        .collect();
    let src = Source {
        base: new_base_executor_v2(
            sctx.get_session_vars(),
            &sctx.sql_killer(),
            Some(schema_of(&fts)),
            1,
            Vec::new(),
        ),
        batches,
        ptr: 0,
        nexts: 0,
        err_at: cfg["err_at"].as_i64().unwrap(),
    };

    let mut out = vec![];
    let result = |out: Vec<Value>, step: &str| json!({"chunks": out, "step": step});
    let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
    let Some(mut e) = b.build_stream_agg_from_child_exec(
        &sctx,
        Some(Box::new(src)),
        &schema_of(&out_fts),
        2,
        &group_by,
        &aggs,
    ) else {
        // Go's Exec returns no output when Build returns nil.
        return (Value::Null, b.err.take());
    };
    let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    if let Err(err) = e.open(&ctx).await {
        return (result(out, "open"), Some(err));
    }
    let mut chk = exec::new_first_chunk(&e);
    let reqs = cfg["reqs"].as_array().unwrap();
    let mut i = 0;
    loop {
        if let Some(r) = reqs.get(i).and_then(Value::as_i64)
            && r > 0
        {
            chk.set_required_rows(r as isize, max as usize);
        }
        i += 1;
        if let Err(err) = e.next(&ctx, &mut chk).await {
            let _ = e.close().await;
            return (result(out, "next"), Some(err));
        }
        if chk.num_rows() == 0 {
            break;
        }
        out.push(encode(&chk));
    }
    if let Err(err) = e.close().await {
        return (result(out, "close"), Some(err));
    }
    (result(out, ""), None)
}

/// Compares one record; Err(why) on a difference.
async fn check(rec: &Value) -> Result<(), String> {
    assert_eq!(rec["op"], "stream_agg");
    let (out, err) = run(&rec["args"][0]).await;
    let got_err = err.map(|e| e.to_string());
    let want_err = rec["err"]["msg"].as_str().map(str::to_owned);
    if got_err != want_err {
        return Err(format!("error: got {got_err:?}, want {want_err:?}"));
    }
    if out != rec["out"] {
        return Err(format!(
            "out differs: got {}",
            serde_json::to_string(&out).unwrap()
        ));
    }
    Ok(())
}

async fn replay(path: &str) {
    let lines = read_records(path, "aggexec");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let mut fails = vec![];
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Err(why) = check(&rec).await {
            let cut = line
                .char_indices()
                .nth(600)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            fails.push(format!("{}\n    {why}", &line[..cut]));
        }
    }
    let show = std::env::var("GOVEC_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    for f in fails.iter().take(show) {
        eprintln!("FAIL {f}");
    }
    assert!(
        fails.is_empty(),
        "{path}: {} of {} records differ",
        fails.len(),
        lines.len()
    );
    eprintln!("{path}: {} records", lines.len());
}

#[tokio::test]
async fn govec_aggexec() {
    match std::env::var("GOVEC_AGGEXEC_FILES") {
        Ok(files) => {
            for f in files.split(',').filter(|f| !f.is_empty()) {
                replay(f).await;
            }
        }
        Err(_) => replay(&vectors("aggexec")).await,
    }
}
