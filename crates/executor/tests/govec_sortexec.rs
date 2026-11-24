//! Replays the govec `sortexec` vectors (tidb-rs-tracker `govec/domains/sortexec.go`):
//! the in-memory sort and top-n executors (D12.5), against Go TiDB v8.5.8.
//!
//! A record is a source of fixed chunks (one per `Next`, every chunk type), by-items
//! (columns with mixed ASC / DESC, constants, a correlated column), the chunk sizes, the
//! required rows of each result chunk, and for a top-n its offset and count, for a parallel
//! sort its worker count. Go ran `SortExec` in its unparallel mode (op `sort`, switched
//! after `Open`, as its tests do), in its parallel mode (op `psort`, with
//! `tidb_executor_concurrency` workers) or `TopNExec` without spill; the replay runs the same
//! executors here (the unparallel sort through its test hook `set_unparallel_for_test`) and
//! compares every result chunk cell by cell (`Row.GetRaw` and the null flag), and the
//! error and the step that returned it.
//!
//! In a parallel sort the order of rows whose sort keys compare equal depends on which
//! worker sorted which chunk. Go's records and this replay both put each run of such rows in
//! the order of their cells' encoding (`sortexecTieOrder` in the domain, [`tie_order`]
//! here), so the key order, the chunk sizes and the set of rows are compared, not the order
//! of ties.
//!
//! `GOVEC_SORTEXEC_FILES` (comma-separated paths) replays these files instead of the
//! committed one (other seeds, a fresh artifact).

use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::mysql;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::compare::CompareFunc;
use tidb_executor::exec::ExecCtx;
use tidb_executor::internal::exec::executor::{
    BaseExecutorV2, Executor, new_base_executor, new_base_executor_v2, new_first_chunk,
};
use tidb_executor::sortexec::sort::new_sort_exec;
use tidb_executor::sortexec::topn::new_top_n_exec;
use tidb_expression::constant::new_one;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, CorrSlot, CorrelatedColumn, Expr, ExprRef, Schema};
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::{LabelForSQLText, new_tracker};
use tidb_parser::types::FieldType;
use tidb_planner::core::physical_plans::PhysicalLimit;
use tidb_planner::util::byitem::ByItems;
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::MyDecimal;
use tidb_types::core_time::CoreTime;
use tidb_types::enum_type::Enum;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, new_time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

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

// ---------------------------------------------------------------- values (as govec_chunk.rs)

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// govec's chunkTypeSpec.ft.
fn ft_of(spec: &Value) -> FieldType {
    let a = spec.as_array().unwrap();
    let tp = a[0].as_u64().unwrap() as u8;
    let mut ft = FieldType::new(tp);
    ft.set_flag(a[1].as_u64().unwrap() as usize);
    ft.set_collate(a[2].as_str().unwrap());
    let flen = a[3].as_i64().unwrap() as isize;
    ft.set_flen(flen);
    if tp == mysql::TypeDuration {
        ft.set_decimal(flen % 7);
    }
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

// ---------------------------------------------------------------- the executors

struct NoGlobals;

impl GlobalVarAccessor for NoGlobals {
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

/// The shim's `govecSource`: one of its chunks per `Next` (whole), then empty chunks.
struct Source {
    base: BaseExecutorV2,
    batches: Vec<Chunk>,
    ptr: usize,
}

#[async_trait]
impl Executor for Source {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.ptr >= self.batches.len() {
            return Ok(());
        }
        let b = &self.batches[self.ptr];
        req.append(b, 0, b.num_rows());
        self.ptr += 1;
        Ok(())
    }
}

fn schema_of(fts: &[FieldType]) -> Schema {
    new_schema(
        fts.iter()
            .enumerate()
            .map(|(i, ft)| {
                Arc::new(Column {
                    ret_type: Arc::new(ft.clone()),
                    index: i as isize,
                    unique_id: i as i64 + 1,
                    ..Column::default()
                })
            })
            .collect(),
    )
}

fn by_items_of(v: &Value, schema: &Schema) -> Vec<ByItems> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let expr: ExprRef = if let Some(c) = b.get("col") {
                Expr::Column((*schema.columns[c.as_u64().unwrap() as usize]).clone()).into_ref()
            } else if let Some(spec) = b.get("const") {
                let mut c = new_one();
                c.ret_type = ft_of(spec);
                Expr::Constant(c).into_ref()
            } else {
                let i = b["corr"].as_u64().unwrap() as usize;
                Expr::CorrelatedColumn(CorrelatedColumn {
                    column: (*schema.columns[i]).clone(),
                    slot: CorrSlot(1),
                })
                .into_ref()
            };
            ByItems {
                expr,
                desc: b.get("desc").and_then(Value::as_bool).unwrap_or(false),
            }
        })
        .collect()
}

/// The shim's `GovecSortRun`: the result chunks (cells as govec writes them) and the step of
/// the first error.
async fn run(op: &str, a: &[Value]) -> (Vec<Value>, &'static str, Option<Error>) {
    let fts: Vec<FieldType> = a[0].as_array().unwrap().iter().map(ft_of).collect();
    let schema = schema_of(&fts);
    let batches: Vec<Chunk> = a[1]
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
    let by_items = by_items_of(&a[2], &schema);
    let sizes: Vec<i64> = a[3]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let reqs: Vec<usize> = a[4]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();

    let mut vars = SessionVars::new(Box::new(NoGlobals));
    vars.init_chunk_size = sizes[0];
    vars.max_chunk_size = sizes[1];
    if op == "psort" {
        vars.executor_concurrency = a[5].as_i64().unwrap();
    }
    let killer = SqlKiller::new();
    let src = Box::new(Source {
        base: new_base_executor_v2(&vars, &killer, Some(schema.clone()), 1, Vec::new()),
        batches,
        ptr: 0,
    });
    let mut sort = new_sort_exec(
        new_base_executor(&vars, &killer, Some(schema.clone()), 2, vec![src]),
        by_items,
        schema,
        tidb_sessionctx::exprstatic::new_expr_context(Vec::new()),
        -1,
        vars.executor_concurrency,
    );
    let max = sizes[1] as usize;
    if op == "topn" {
        let lim = a[5].as_array().unwrap();
        let mut e = new_top_n_exec(
            sort,
            PhysicalLimit {
                offset: s_u64(&lim[0]),
                count: s_u64(&lim[1]),
                ..PhysicalLimit::default()
            },
            5,
        );
        let (chunks, step, err) = drive(&mut e, &reqs, max).await;
        return (encode(&chunks), step, err);
    }
    if op != "psort" {
        sort.set_unparallel_for_test();
    }
    let (chunks, step, err) = drive(&mut sort, &reqs, max).await;
    let mut out = encode(&chunks);
    if op == "psort" {
        // The domain's sortexecTieOrder, with the keys Next built (GetSortMetaForTest).
        // A Desc flag only flips the sign of a compare, so the runs do not depend on it.
        let (key_columns, cmps, _) = sort.get_sort_meta_for_test();
        tie_order(&chunks, &mut out, &key_columns, &cmps);
    }
    (out, step, err)
}

/// The shim's `govecRun`: opens e, reads it with the required rows of `reqs` and closes it;
/// the result chunks and the step of the first error.
async fn drive(
    e: &mut dyn Executor,
    reqs: &[usize],
    max: usize,
) -> (Vec<Chunk>, &'static str, Option<Error>) {
    // The statement tracker under a session tracker without a limit (mock.Context).
    let session = new_tracker(LabelForSQLText, -1);
    let mut stmt = new_stmt_ctx();
    let stmt_tracker = new_tracker(-1, -1);
    stmt_tracker.attach_to(&session).unwrap();
    stmt.mem_tracker = Some(stmt_tracker);
    let ctx = ExecCtx::new(Arc::new(stmt));

    let mut out = Vec::new();
    if let Err(err) = e.open(&ctx).await {
        return (out, "open", Some(err));
    }
    let mut chk = new_first_chunk(&*e);
    for i in 0.. {
        if i < reqs.len() && reqs[i] > 0 {
            chk.set_required_rows(reqs[i] as isize, max);
        }
        if let Err(err) = e.next(&ctx, &mut chk).await {
            let _ = e.close().await;
            return (out, "next", Some(err));
        }
        if chk.num_rows() == 0 {
            break;
        }
        out.push(chk.copy_construct());
    }
    if let Err(err) = e.close().await {
        return (out, "close", Some(err));
    }
    (out, "", None)
}

/// The cells of every row of chunks, as govec writes them.
fn encode(chunks: &[Chunk]) -> Vec<Value> {
    chunks
        .iter()
        .map(|chk| {
            Value::Array(
                (0..chk.num_rows())
                    .map(|r| {
                        let row = chk.get_row(r);
                        Value::Array(
                            (0..chk.num_cols())
                                .map(|col| {
                                    json!({"null": row.is_null(col), "raw": hex(row.get_raw(col))})
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

/// The domain's `sortexecCellsKey`: "n" for a NULL and "v" otherwise, each followed by the
/// hex of the cell's bytes, joined by ",".
fn cells_key(cells: &Value) -> String {
    cells
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let tag = if c["null"].as_bool().unwrap() {
                "n"
            } else {
                "v"
            };
            format!("{tag}{}", c["raw"].as_str().unwrap())
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The domain's `sortexecTieOrder`: reorders, in place, the encoded rows within each run of
/// consecutive rows whose sort keys compare equal (compare function i with key column i, as
/// Go's `compareRow`), by [`cells_key`]. The chunk sizes do not change.
fn tie_order(chunks: &[Chunk], out: &mut [Value], key_columns: &[usize], cmps: &[CompareFunc]) {
    let refs: Vec<(usize, usize)> = chunks
        .iter()
        .enumerate()
        .flat_map(|(c, chk)| (0..chk.num_rows()).map(move |r| (c, r)))
        .collect();
    let equal = |a: (usize, usize), b: (usize, usize)| {
        let (ra, rb) = (chunks[a.0].get_row(a.1), chunks[b.0].get_row(b.1));
        key_columns
            .iter()
            .enumerate()
            .all(|(i, &col)| cmps[i](ra, col, rb, col) == 0)
    };
    let mut start = 0;
    while start < refs.len() {
        let mut end = start + 1;
        while end < refs.len() && equal(refs[end - 1], refs[end]) {
            end += 1;
        }
        let mut group: Vec<Value> = refs[start..end]
            .iter()
            .map(|&(c, r)| out[c][r].take())
            .collect();
        group.sort_by_cached_key(cells_key);
        for (&(c, r), cells) in refs[start..end].iter().zip(group) {
            out[c][r] = cells;
        }
        start = end;
    }
}

async fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let a = rec["args"].as_array().unwrap();
    let (chunks, step, err) = run(op, a).await;
    let got_err = err.map(|e| e.to_string());
    let want_err = rec["err"]["msg"].as_str().map(str::to_owned);
    if got_err != want_err {
        return Some(format!("error: got {got_err:?}, want {want_err:?}"));
    }
    let got = json!({"chunks": chunks, "step": step});
    if got != rec["out"] {
        return Some(format!(
            "out differs: got {}",
            serde_json::to_string(&got).unwrap()
        ));
    }
    None
}

async fn replay(path: &str) {
    let lines = read_records(path, "sortexec");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let mut fails = vec![];
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec).await {
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
}

// A multi-thread runtime, so the parallel sort's workers run in parallel as Go's goroutines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn govec_sortexec() {
    match std::env::var("GOVEC_SORTEXEC_FILES") {
        Ok(files) => {
            for f in files.split(',').filter(|f| !f.is_empty()) {
                replay(f).await;
            }
        }
        Err(_) => replay(&vectors("sortexec")).await,
    }
}
