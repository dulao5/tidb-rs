//! Replays the govec `aggfuncs` vectors (tidb-rs-tracker `govec/domains/aggfuncs.go`): the
//! aggregate functions of parts A, B1, and B2 of block D12.4 (COUNT, SUM, AVG, FIRST_ROW,
//! MAX / MIN, BIT_*, GROUP_CONCAT, VAR_*, STDDEV_*, JSON_*AGG, APPROX_COUNT_DISTINCT,
//! APPROX_PERCENTILE), against Go TiDB v8.5.8.
//!
//! A record carries the complete descriptor (name, mode, DISTINCT, argument types, return
//! type, `tidb_window_use_high_precision`, `div_precision_increment`); the replay builds the
//! `AggFuncDesc` from it directly (no type inference) and runs `build` and the functions as
//! Go ran them:
//! - `build`: the implementation chosen (Go's `%T`, `<nil>` for none);
//! - `update_final`: alloc, one update per batch, append final;
//! - `reset_reuse`: one partial result over several groups (updates, append final, reset);
//! - `merge`: the partial function of a split pair over several parts, each merged into the
//!   final function's partial result, optionally twice with a reset in between;
//! - `group_concat`, `count_distinct_encode`, `json_agg`, `approx_count_distinct`,
//!   `percentile`: domain-specific sequences for parts B1 and B2.
//!
//! The appended cells (`Row.GetRaw` and the null flag), the error and the step that returned
//! it are compared.
//!
//! `GOVEC_AGGFUNCS_FILES` (comma-separated paths) replays these files instead of the
//! committed one (other seeds, a fresh artifact).

use std::io::Read;
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::WarnHandler;
use tidb_base::farm;
use tidb_base::mysql;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::row::Row;
use tidb_executor::aggfuncs::func_count_distinct::{
    new_partial_result_4_approx_count_distinct, write_time,
};
use tidb_executor::aggfuncs::{AggFunc, PartialResult, build, build_window_functions, new_deque};
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, BaseFuncDesc};
use tidb_expression::byitem::ByItems;
use tidb_expression::constant::new_null;
use tidb_expression::context::BuildContext;
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Constant, Expr, ExprRef};
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::context::FlagTruncateAsWarning;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{new_int_datum, new_string_datum, new_uint_datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type::new_field_type;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, new_time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

/// No records remain blocked after part B2 removes `build_not_ported` (spec §4).
const EXPECTED_BLOCKED: usize = 0;

/// Records intentionally diverging from Go v8.5.8 under ruling R23 (#390, constitution art. 9.3):
/// `countOriginalWithDistinct` keys on canonical values in Rust (folding `-0.0` to `+0.0`,
/// ignoring `Duration.Fsp`, and ignoring `Time.Type`/`Time.Fsp`) instead of Go's raw bytes.
/// Exactly 3 records contain distinct raw representations of equal values (`reset_reuse`
/// TIMESTAMP, `count_distinct_encode` multi-arg `{+0.0, -0.0}`, and `count_distinct_encode`
/// DATETIME differing in `Type`). `APPROX_COUNT_DISTINCT` uses Go's raw encoding (#393 / #394)
/// and has 0 diffs.
const EXPECTED_CANONICAL_DISTINCT_DIFFS: usize = 3;

/// Records intentionally diverging from Go v8.5.8 under ruling R26 (#405, constitution art. 9.3):
/// `lead.AppendFinalResult2Chunk` in Go adds `curIdx + offset` as wrapping `uint64`, so offsets
/// `2^64-1` and `2^64-2` wrap around to earlier rows on rows `1..` / `2..`. Rust uses
/// `checked_add` and returns the default value (NULL or `-1`) on every row.
const EXPECTED_LEAD_WRAPAROUND_DIFFS: usize = 4;

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

// ---------------------------------------------------------------- values (as govec_sortexec.rs)

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

// ---------------------------------------------------------------- the functions

struct Built {
    ctx: Arc<es::ExprContext>,
    eval_ctx: Arc<es::EvalContext>,
    desc: AggFuncDesc,
    fts: Vec<FieldType>,
    func: Option<Box<dyn AggFunc>>,
}

enum Outcome {
    Built(Built),
    Blocked,
    Failed(Error),
}

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

fn warnings_of(ec: &es::EvalContext) -> Value {
    let ws: Vec<Value> = ec
        .copy_warnings(vec![])
        .into_iter()
        .map(|w| Value::String(w.err.to_string()))
        .collect();
    Value::Array(ws)
}

/// The domain's afDesc.make, then `build`.
fn build_desc(d: &Value) -> Outcome {
    let dpi = d["dpi"].as_i64().unwrap();
    let mut eval_opts = vec![es::with_div_precision_increment(dpi)];
    if d.get("trunc_warn")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        eval_opts.push(es::with_type_flags(FlagTruncateAsWarning));
    }
    let eval_ctx = es::new_eval_context(eval_opts);
    let mut expr_opts = vec![
        es::with_eval_ctx(Arc::clone(&eval_ctx)),
        es::with_windowing_use_high_precision(d["hp"].as_bool().unwrap()),
    ];
    if let Some(max_len) = d.get("max_len").and_then(Value::as_u64)
        && max_len > 0
    {
        expr_opts.push(es::with_group_concat_max_len(max_len));
    }
    let ctx = es::new_expr_context(expr_opts);
    let arg_fts: Vec<FieldType> = d["args"]
        .as_array()
        .map(|a| a.iter().map(ft_of).collect())
        .unwrap_or_default();
    let extra_fts: Vec<FieldType> = d
        .get("extra_cols")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(ft_of).collect())
        .unwrap_or_default();
    let mut fts = arg_fts.clone();
    fts.extend(extra_fts);

    let mut args: Vec<ExprRef> = arg_fts
        .iter()
        .enumerate()
        .map(|(i, ft)| {
            Expr::Column(Column {
                ret_type: Arc::new(ft.clone()),
                index: i as isize,
                ..Column::default()
            })
            .into_ref()
        })
        .collect();
    if d.get("has_sep").and_then(Value::as_bool).unwrap_or(false) {
        let sep_bytes = unhex(d.get("sep").and_then(Value::as_str).unwrap_or(""));
        args.push(
            Expr::Constant(Constant::new(
                new_string_datum(sep_bytes),
                new_field_type(mysql::TypeString),
            ))
            .into_ref(),
        );
    }
    if d.get("pct_bad").and_then(Value::as_bool).unwrap_or(false) {
        args.push(
            Expr::Constant(Constant::new(
                new_string_datum(b"not-an-int".to_vec()),
                new_field_type(mysql::TypeString),
            ))
            .into_ref(),
        );
    } else if d.get("has_pct").and_then(Value::as_bool).unwrap_or(false) {
        let pct = d.get("pct").and_then(Value::as_i64).unwrap_or(0);
        args.push(
            Expr::Constant(Constant::new(
                new_int_datum(pct),
                new_field_type(mysql::TypeLong),
            ))
            .into_ref(),
        );
    }
    if d.get("u64_arg_null")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push(Expr::Constant(new_null()).into_ref());
    } else if d
        .get("has_u64_arg")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let v: u64 = d["u64_arg"].as_str().unwrap().parse().unwrap();
        let mut u64_ft = new_field_type(mysql::TypeLonglong);
        u64_ft.add_flag(mysql::UnsignedFlag);
        args.push(Expr::Constant(Constant::new(new_uint_datum(v), u64_ft)).into_ref());
    }
    match d.get("def_kind").and_then(Value::as_str).unwrap_or("") {
        "null" => args.push(Expr::Constant(new_null()).into_ref()),
        "const_int" => {
            let v = d.get("def_int").and_then(Value::as_i64).unwrap_or(0);
            args.push(
                Expr::Constant(Constant::new(
                    new_int_datum(v),
                    new_field_type(mysql::TypeLonglong),
                ))
                .into_ref(),
            );
        }
        "const_str" => {
            let b = unhex(d.get("def_str").and_then(Value::as_str).unwrap_or(""));
            args.push(
                Expr::Constant(Constant::new(
                    new_string_datum(b),
                    new_field_type(mysql::TypeVarchar),
                ))
                .into_ref(),
            );
        }
        "col" => {
            let idx = d.get("def_col").and_then(Value::as_u64).unwrap_or(0) as usize;
            args.push(
                Expr::Column(Column {
                    ret_type: Arc::new(fts[idx].clone()),
                    index: idx as isize,
                    ..Column::default()
                })
                .into_ref(),
            );
        }
        _ => {}
    }
    let base = BaseFuncDesc::from_parts(
        d["name"].as_str().unwrap().to_owned(),
        args,
        Some(Arc::new(ft_of(&d["ret"]))),
    );
    let mut desc = AggFuncDesc::from_base(base, d["distinct"].as_bool().unwrap());
    desc.mode = mode_of(d["mode"].as_i64().unwrap());
    if let Some(obs) = d.get("order_by").and_then(Value::as_array) {
        desc.order_by_items = obs
            .iter()
            .map(|o| {
                let col = o["col"].as_u64().unwrap() as usize;
                ByItems {
                    expr: Expr::Column(Column {
                        ret_type: Arc::new(fts[col].clone()),
                        index: col as isize,
                        ..Column::default()
                    })
                    .into_ref(),
                    desc: o["desc"].as_bool().unwrap(),
                }
            })
            .collect();
    }
    let is_window = d.get("is_window").and_then(Value::as_bool).unwrap_or(false);
    let build_res = if is_window {
        let order_cols: Vec<Column> = d
            .get("win_order_cols")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|v| {
                        let idx = v.as_u64().unwrap() as usize;
                        Column {
                            ret_type: Arc::new(fts[idx].clone()),
                            index: idx as isize,
                            ..Column::default()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        build_window_functions(&*ctx, &desc, 0, &order_cols)
    } else {
        build(&*ctx, &desc, 0)
    };
    match build_res {
        Ok(mut func) => {
            if let Some(f) = func.as_deref_mut() {
                f.set_rng_seed_for_test(20261125);
            }
            Outcome::Built(Built {
                ctx,
                eval_ctx,
                desc,
                fts,
                func,
            })
        }
        Err(e) if blocked_by(&e) == Some("D12.4") => Outcome::Blocked,
        Err(e) => Outcome::Failed(e),
    }
}

/// Go's `%T` of the function.
fn type_name(f: &Option<Box<dyn AggFunc>>) -> String {
    match f {
        None => "<nil>".to_owned(),
        Some(f) => {
            let name = f.type_name().rsplit("::").next().unwrap();
            let mut cs = name.chars();
            let first = cs.next().unwrap().to_ascii_lowercase();
            format!("*aggfuncs.{first}{}", cs.as_str())
        }
    }
}

fn update(
    b: &Built,
    f: &dyn AggFunc,
    batches: &Value,
    pr: &mut PartialResult,
) -> Result<(), (String, Error)> {
    let ec = b.ctx.get_eval_ctx();
    let st = EvalState::new();
    for (i, batch) in batches.as_array().unwrap().iter().enumerate() {
        let batch = batch.as_array().unwrap();
        let mut c = chunk::new_chunk_with_capacity(&b.fts, batch.len());
        for row in batch {
            for (col, v) in row.as_array().unwrap().iter().enumerate() {
                append(&mut c, &b.fts[col], col, v);
            }
        }
        let rows: Vec<Row<'_>> = (0..c.num_rows()).map(|r| c.get_row(r)).collect();
        f.update_partial_result(&*ec, &st, &rows, pr)
            .map_err(|e| (format!("update {i}"), e))?;
    }
    Ok(())
}

fn final_cells(b: &Built, f: &dyn AggFunc, pr: &mut PartialResult) -> (Value, Option<Error>) {
    let ec = b.ctx.get_eval_ctx();
    let ret = &*b.desc.ret_tp;
    let mut c = chunk::new_chunk_with_capacity(std::slice::from_ref(ret), 1);
    let err = f.append_final_result2_chunk(&*ec, pr, &mut c).err();
    let cells: Vec<Value> = (0..c.num_rows())
        .map(|r| {
            let row = c.get_row(r);
            json!({"null": row.is_null(0), "raw": hex(row.get_raw(0))})
        })
        .collect();
    (Value::Array(cells), err)
}

/// Runs one record: (out, error), or None when a function is not ported yet.
fn run(op: &str, a: &[Value]) -> Option<(Value, Option<Error>)> {
    match op {
        "json_agg" | "percentile" => {
            let action = a[0].as_str().unwrap();
            return run(action, &a[1..]);
        }
        "window" => {
            let action = a[0].as_str().unwrap();
            return match action {
                "run" => {
                    let mut b = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let tname = type_name(&b.func);
                    let f_ref = b.func.as_deref_mut().expect("window func builds");
                    let is_sliding = f_ref.as_sliding_window().is_some();
                    let is_mm = f_ref.as_max_min_sliding_mut().is_some();
                    let f = b.func.as_deref().unwrap();
                    let mut pr = f.alloc_partial_result();
                    let ec = b.ctx.get_eval_ctx();
                    let ret = &*b.desc.ret_tp;
                    let mut out_parts = vec![];
                    for (pi, pt) in a[2].as_array().unwrap().iter().enumerate() {
                        if let Err((step, e)) = update(&b, f, &pt["batches"], &mut pr) {
                            return Some((
                                json!({
                                    "type": tname,
                                    "sliding": is_sliding,
                                    "max_min_sliding": is_mm,
                                    "partitions": out_parts,
                                    "step": format!("part {pi} {step}"),
                                }),
                                Some(e),
                            ));
                        }
                        let finals = pt["finals"].as_u64().unwrap() as usize;
                        let mut c =
                            chunk::new_chunk_with_capacity(std::slice::from_ref(ret), finals);
                        for fi in 0..finals {
                            if let Err(e) = f.append_final_result2_chunk(&*ec, &mut pr, &mut c) {
                                return Some((
                                    json!({
                                        "type": tname,
                                        "sliding": is_sliding,
                                        "max_min_sliding": is_mm,
                                        "partitions": out_parts,
                                        "step": format!("part {pi} final {fi}"),
                                    }),
                                    Some(e),
                                ));
                            }
                        }
                        let cells: Vec<Value> = (0..c.num_rows())
                            .map(|r| {
                                let row = c.get_row(r);
                                json!({"null": row.is_null(0), "raw": hex(row.get_raw(0))})
                            })
                            .collect();
                        out_parts.push(Value::Array(cells));
                        f.reset_partial_result(&mut pr);
                    }
                    Some((
                        json!({
                            "type": tname,
                            "sliding": is_sliding,
                            "max_min_sliding": is_mm,
                            "partitions": out_parts,
                            "step": "",
                        }),
                        None,
                    ))
                }
                "slide" => {
                    let mut b = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let tname = type_name(&b.func);
                    let raw_rows = a[2].as_array().unwrap();
                    let mut src = chunk::new_chunk_with_capacity(&b.fts, raw_rows.len());
                    for row in raw_rows {
                        for (col, v) in row.as_array().unwrap().iter().enumerate() {
                            append(&mut src, &b.fts[col], col, v);
                        }
                    }
                    let all_rows: Vec<Row<'_>> =
                        (0..src.num_rows()).map(|r| src.get_row(r)).collect();
                    let get_row = |idx: u64| all_rows[idx as usize];
                    let f = b.func.as_deref_mut().expect("sliding func builds");
                    let is_sliding = f.as_sliding_window().is_some();
                    let is_mm = f.as_max_min_sliding_mut().is_some();
                    let mut pr = f.alloc_partial_result();
                    let ec = b.ctx.get_eval_ctx();
                    let st = EvalState::new();
                    let ret = &*b.desc.ret_tp;
                    let mut out_seqs = vec![];
                    for (si, frames) in a[3].as_array().unwrap().iter().enumerate() {
                        let frames = frames.as_array().unwrap();
                        let mut c =
                            chunk::new_chunk_with_capacity(std::slice::from_ref(ret), frames.len());
                        let mut prev_start = 0u64;
                        let mut prev_end = 0u64;
                        for (fi, fr) in frames.iter().enumerate() {
                            let fr = fr.as_array().unwrap();
                            let s = fr[0].as_u64().unwrap();
                            let e = fr[1].as_u64().unwrap();
                            if fi == 0 || s > prev_end {
                                if fi > 0 {
                                    f.reset_partial_result(&mut pr);
                                }
                                if let Some(mm) = f.as_max_min_sliding_mut() {
                                    mm.set_window_start(s);
                                }
                                if let Err(err) = f.update_partial_result(
                                    &*ec,
                                    &st,
                                    &all_rows[s as usize..e as usize],
                                    &mut pr,
                                ) {
                                    return Some((
                                        json!({
                                            "type": tname,
                                            "sliding": is_sliding,
                                            "max_min_sliding": is_mm,
                                            "seqs": out_seqs,
                                            "step": format!("seq {si} frame {fi} update"),
                                        }),
                                        Some(err),
                                    ));
                                }
                            } else {
                                let sw = f.as_sliding_window().expect("sliding window func");
                                if let Err(err) = sw.slide(
                                    &*ec,
                                    &st,
                                    &get_row,
                                    prev_start,
                                    prev_end,
                                    s - prev_start,
                                    e - prev_end,
                                    &mut pr,
                                ) {
                                    return Some((
                                        json!({
                                            "type": tname,
                                            "sliding": is_sliding,
                                            "max_min_sliding": is_mm,
                                            "seqs": out_seqs,
                                            "step": format!("seq {si} frame {fi} slide"),
                                        }),
                                        Some(err),
                                    ));
                                }
                            }
                            if let Err(err) = f.append_final_result2_chunk(&*ec, &mut pr, &mut c) {
                                return Some((
                                    json!({
                                        "type": tname,
                                        "sliding": is_sliding,
                                        "max_min_sliding": is_mm,
                                        "seqs": out_seqs,
                                        "step": format!("seq {si} frame {fi} final"),
                                    }),
                                    Some(err),
                                ));
                            }
                            prev_start = s;
                            prev_end = e;
                        }
                        let cells: Vec<Value> = (0..c.num_rows())
                            .map(|r| {
                                let row = c.get_row(r);
                                json!({"null": row.is_null(0), "raw": hex(row.get_raw(0))})
                            })
                            .collect();
                        out_seqs.push(Value::Array(cells));
                        f.reset_partial_result(&mut pr);
                    }
                    Some((
                        json!({
                            "type": tname,
                            "sliding": is_sliding,
                            "max_min_sliding": is_mm,
                            "seqs": out_seqs,
                            "step": "",
                        }),
                        None,
                    ))
                }
                "deque" => {
                    let is_max = a[1].as_bool().unwrap();
                    let ops = a[2].as_array().unwrap();
                    let mut dq = new_deque(is_max, |i: &i64, j: &i64| i.cmp(j) as isize);
                    let mut res = Vec::with_capacity(ops.len());
                    for op in ops {
                        let arr = op.as_array().unwrap();
                        match arr[0].as_str().unwrap() {
                            "push_back" => {
                                dq.push_back(arr[1].as_u64().unwrap(), arr[2].as_i64().unwrap());
                                res.push(Value::Null);
                            }
                            "pop_front" => {
                                let s = dq
                                    .pop_front()
                                    .err()
                                    .map(|e| e.to_string())
                                    .unwrap_or_default();
                                res.push(json!(s));
                            }
                            "pop_back" => {
                                let s = dq
                                    .pop_back()
                                    .err()
                                    .map(|e| e.to_string())
                                    .unwrap_or_default();
                                res.push(json!(s));
                            }
                            "front" => match dq.front() {
                                None => res.push(Value::Null),
                                Some(p) => res.push(json!([p.idx, p.item])),
                            },
                            "back" => match dq.back() {
                                None => res.push(Value::Null),
                                Some(p) => res.push(json!([p.idx, p.item])),
                            },
                            "is_empty" => res.push(json!(dq.is_empty())),
                            "reset" => {
                                dq.reset();
                                res.push(json!(dq.is_empty()));
                            }
                            "enqueue" => {
                                let s = dq
                                    .enqueue(arr[1].as_u64().unwrap(), arr[2].as_i64().unwrap())
                                    .err()
                                    .map(|e| e.to_string())
                                    .unwrap_or_default();
                                res.push(json!(s));
                            }
                            "dequeue" => {
                                let s = dq
                                    .dequeue(arr[1].as_u64().unwrap())
                                    .err()
                                    .map(|e| e.to_string())
                                    .unwrap_or_default();
                                res.push(json!(s));
                            }
                            k => panic!("unknown deque op {k}"),
                        }
                    }
                    Some((json!({"res": res}), None))
                }
                _ => panic!("unknown window action {action}"),
            };
        }
        "approx_count_distinct" => {
            let action = a[0].as_str().unwrap();
            return match action {
                "farm_hash64" => {
                    let b = unhex(a[1].as_str().unwrap());
                    Some((json!({"hash": farm::hash64(&b).to_string()}), None))
                }
                "update_final" | "read_err" => run("update_final", &a[1..]),
                "reset_reuse" => run("reset_reuse", &a[1..]),
                "merge" => run("merge", &a[1..]),
                "wire" => {
                    let b1 = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let b2 = match build_desc(&a[2]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let bf = match build_desc(&a[3]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let f1 = b1.func.as_deref().expect("p1 builds");
                    let f2 = b2.func.as_deref().expect("p2 builds");
                    let ff = bf.func.as_deref().expect("final builds");
                    let types = json!([
                        type_name(&b1.func),
                        type_name(&b2.func),
                        type_name(&bf.func)
                    ]);
                    let mut out = json!({
                        "type": types,
                        "p1": [],
                        "p2": null,
                        "final": null,
                        "step": "",
                    });
                    let parts = a[4].as_array().unwrap();
                    let mut p1_out = vec![];
                    let mut p2_rows = vec![];
                    for (i, part) in parts.iter().enumerate() {
                        let mut pr1 = f1.alloc_partial_result();
                        if let Err((step, e)) = update(&b1, f1, part, &mut pr1) {
                            out["p1"] = Value::Array(p1_out);
                            out["step"] = json!(format!("p1 {i} {step}"));
                            return Some((out, Some(e)));
                        }
                        let (cells1, err) = final_cells(&b1, f1, &mut pr1);
                        let raw_hex = cells1[0]["raw"].as_str().unwrap().to_owned();
                        p1_out.push(cells1);
                        if let Some(e) = err {
                            out["p1"] = Value::Array(p1_out);
                            out["step"] = json!(format!("p1 {i} final"));
                            return Some((out, Some(e)));
                        }
                        p2_rows.push(json!([raw_hex]));
                        if i == 0 && parts.len() > 1 {
                            p2_rows.push(json!([null]));
                        }
                    }
                    let mut pr2 = f2.alloc_partial_result();
                    if let Err((step, e)) = update(&b2, f2, &json!([p2_rows]), &mut pr2) {
                        out["p1"] = Value::Array(p1_out);
                        out["step"] = json!(format!("p2 {step}"));
                        return Some((out, Some(e)));
                    }
                    let (cells2, err) = final_cells(&b2, f2, &mut pr2);
                    out["p1"] = Value::Array(p1_out);
                    out["p2"] = cells2.clone();
                    if let Some(e) = err {
                        out["step"] = json!("p2 final");
                        return Some((out, Some(e)));
                    }
                    let p2_hex = cells2[0]["raw"].as_str().unwrap().to_owned();
                    let mut prf = ff.alloc_partial_result();
                    if let Err((step, e)) = update(&bf, ff, &json!([[[p2_hex]]]), &mut prf) {
                        out["step"] = json!(format!("final {step}"));
                        return Some((out, Some(e)));
                    }
                    let (cellsf, err) = final_cells(&bf, ff, &mut prf);
                    out["final"] = cellsf;
                    if err.is_some() {
                        out["step"] = json!("final final");
                    }
                    Some((out, err))
                }
                "sketch" => {
                    let n = a[1].as_u64().unwrap();
                    let base = a[2].as_u64().unwrap();
                    let include_zero = a[3].as_bool().unwrap();
                    let mut p = new_partial_result_4_approx_count_distinct();
                    if include_zero {
                        p.insert_hash64(0);
                    }
                    for i in 0..n {
                        p.insert_hash64(farm::hash64(&(base + i).to_le_bytes()));
                    }
                    let ser = p.serialize();
                    let str_tp = json!([
                        mysql::TypeString,
                        mysql::BinaryFlag,
                        255,
                        0,
                        "binary",
                        "binary",
                        []
                    ]);
                    let int_tp = json!([mysql::TypeLonglong, 0, 20, 0, "binary", "binary", []]);
                    let find = json!({
                        "name": "approx_count_distinct",
                        "mode": 1,
                        "distinct": false,
                        "args": [str_tp],
                        "ret": int_tp,
                        "hp": false,
                        "dpi": 4,
                    });
                    let bf = match build_desc(&find) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let ff = bf.func.as_deref().expect("final builds");
                    let mut prf = ff.alloc_partial_result();
                    if let Err((_, e)) = update(&bf, ff, &json!([[[hex(&ser)]]]), &mut prf) {
                        return Some((Value::Null, Some(e)));
                    }
                    let (cellsf, err) = final_cells(&bf, ff, &mut prf);
                    if let Some(e) = err {
                        return Some((Value::Null, Some(e)));
                    }

                    let mut p_small = new_partial_result_4_approx_count_distinct();
                    for i in 0..8u64 {
                        p_small.insert_hash64(farm::hash64(&(base + i + 1_000_000).to_le_bytes()));
                    }
                    let mut pr_dst = ff.alloc_partial_result();
                    if let Err((_, e)) = update(
                        &bf,
                        ff,
                        &json!([[[hex(&p_small.serialize())]]]),
                        &mut pr_dst,
                    ) {
                        return Some((Value::Null, Some(e)));
                    }
                    let fec = bf.ctx.get_eval_ctx();
                    if let Err(e) = ff.merge_partial_result(&*fec, &mut prf, &mut pr_dst) {
                        return Some((Value::Null, Some(e)));
                    }
                    let (merged_cells, err) = final_cells(&bf, ff, &mut pr_dst);
                    if let Some(e) = err {
                        return Some((Value::Null, Some(e)));
                    }
                    Some((
                        json!({
                            "ser_len": ser.len(),
                            "skip_degree": ser[0],
                            "ser_hash": farm::hash64(&ser).to_string(),
                            "final": cellsf,
                            "merged": merged_cells,
                        }),
                        None,
                    ))
                }
                _ => panic!("unknown approx_count_distinct action {action}"),
            };
        }
        "count_distinct_encode" => {
            let action = a[0].as_str().unwrap();
            return match action {
                "run" => run("update_final", &a[1..]),
                "write_time" => {
                    let tv = a[1].as_array().unwrap();
                    let t = new_time(
                        CoreTime(s_u64(&tv[0])),
                        tv[1].as_u64().unwrap() as u8,
                        tv[2].as_i64().unwrap() as isize,
                    );
                    let mut buf = [0xffu8; 16];
                    write_time(&mut buf, t);
                    Some((json!({"raw": hex(&buf)}), None))
                }
                _ => panic!("unknown count_distinct_encode action {action}"),
            };
        }
        "group_concat" => {
            let action = a[0].as_str().unwrap();
            return match action {
                "run" => {
                    let mut b = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let tname = type_name(&b.func);
                    let has_trunc = {
                        let f = b.func.as_deref_mut().expect("group_concat builds");
                        if let Some(t) = f.get_truncated() {
                            f.set_truncated(t);
                            true
                        } else {
                            false
                        }
                    };
                    let f = b.func.as_deref().expect("group_concat builds");
                    let mut pr = f.alloc_partial_result();
                    let mut groups = vec![];
                    for (gi, g) in a[2].as_array().unwrap().iter().enumerate() {
                        if let Err((step, e)) = update(&b, f, g, &mut pr) {
                            return Some((
                                json!({
                                    "type": tname,
                                    "has_trunc": has_trunc,
                                    "groups": groups,
                                    "warnings": warnings_of(&b.eval_ctx),
                                    "step": format!("group {gi} {step}"),
                                }),
                                Some(e),
                            ));
                        }
                        let (cells, err) = final_cells(&b, f, &mut pr);
                        groups.push(cells);
                        if let Some(e) = err {
                            return Some((
                                json!({
                                    "type": tname,
                                    "has_trunc": has_trunc,
                                    "groups": groups,
                                    "warnings": warnings_of(&b.eval_ctx),
                                    "step": format!("group {gi} final"),
                                }),
                                Some(e),
                            ));
                        }
                        f.reset_partial_result(&mut pr);
                    }
                    Some((
                        json!({
                            "type": tname,
                            "has_trunc": has_trunc,
                            "groups": groups,
                            "warnings": warnings_of(&b.eval_ctx),
                            "step": "",
                        }),
                        None,
                    ))
                }
                "merge" => {
                    let pb = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let mut fb = match build_desc(&a[2]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    // Wire shared truncated sentinel and share the warning sink (as Go does).
                    let pf = pb.func.as_deref().expect("partial group_concat builds");
                    let ff = fb.func.as_deref_mut().expect("final group_concat builds");
                    if let Some(t) = pf.get_truncated() {
                        ff.set_truncated(t);
                    }
                    let types = json!([type_name(&pb.func), type_name(&fb.func)]);
                    let pf = pb.func.as_deref().unwrap();
                    let ff = fb.func.as_deref().unwrap();
                    let mut out = json!({
                        "type": types,
                        "partials": [],
                        "final": null,
                        "warnings": [],
                        "step": "",
                    });
                    let mut fpr = ff.alloc_partial_result();
                    let mut partials = vec![];
                    let ec = pb.ctx.get_eval_ctx();
                    for (i, part) in a[3].as_array().unwrap().iter().enumerate() {
                        let mut pr = pf.alloc_partial_result();
                        if let Err((step, e)) = update(&pb, pf, part, &mut pr) {
                            out["partials"] = Value::Array(partials);
                            out["warnings"] = warnings_of(&pb.eval_ctx);
                            out["step"] = json!(format!("part {i} {step}"));
                            return Some((out, Some(e)));
                        }
                        let (cells, err) = final_cells(&pb, pf, &mut pr);
                        partials.push(cells);
                        if let Some(e) = err {
                            out["partials"] = Value::Array(partials);
                            out["warnings"] = warnings_of(&pb.eval_ctx);
                            out["step"] = json!(format!("part {i} final"));
                            return Some((out, Some(e)));
                        }
                        if let Err(e) = ff.merge_partial_result(&*ec, &mut pr, &mut fpr) {
                            out["partials"] = Value::Array(partials);
                            out["warnings"] = warnings_of(&pb.eval_ctx);
                            out["step"] = json!(format!("merge {i}"));
                            return Some((out, Some(e)));
                        }
                    }
                    let (cells, err) = final_cells(&pb, ff, &mut fpr);
                    out["partials"] = Value::Array(partials);
                    out["final"] = cells;
                    out["warnings"] = warnings_of(&pb.eval_ctx);
                    if err.is_some() {
                        out["step"] = json!("final");
                    }
                    Some((out, err))
                }
                "merge_err" => {
                    let b = match build_desc(&a[1]) {
                        Outcome::Built(b) => b,
                        Outcome::Blocked => return None,
                        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
                    };
                    let tname = type_name(&b.func);
                    let f = b.func.as_deref().expect("group_concat builds");
                    let mut pr1 = f.alloc_partial_result();
                    let mut pr2 = f.alloc_partial_result();
                    let ec = b.ctx.get_eval_ctx();
                    let err = f.merge_partial_result(&*ec, &mut pr1, &mut pr2).err();
                    Some((json!({"type": tname, "step": "merge"}), err))
                }
                _ => panic!("unknown group_concat action {action}"),
            };
        }
        _ => {}
    }

    let b = match build_desc(&a[0]) {
        Outcome::Built(b) => b,
        Outcome::Blocked => return None,
        Outcome::Failed(e) => return Some((Value::Null, Some(e))),
    };
    let tname = type_name(&b.func);
    if op == "build" {
        return Some((json!({"type": tname}), None));
    }
    let f = b.func.as_deref().expect("the records build a function");
    match op {
        "update_final" => {
            let mut pr = f.alloc_partial_result();
            if let Err((step, e)) = update(&b, f, &a[1], &mut pr) {
                return Some((json!({"type": tname, "cells": null, "step": step}), Some(e)));
            }
            let (cells, err) = final_cells(&b, f, &mut pr);
            let step = if err.is_some() { "final" } else { "" };
            Some((json!({"type": tname, "cells": cells, "step": step}), err))
        }
        "reset_reuse" => {
            let mut pr = f.alloc_partial_result();
            let mut groups = vec![];
            for (gi, g) in a[1].as_array().unwrap().iter().enumerate() {
                if let Err((step, e)) = update(&b, f, g, &mut pr) {
                    let step = format!("group {gi} {step}");
                    return Some((
                        json!({"type": tname, "groups": groups, "step": step}),
                        Some(e),
                    ));
                }
                let (cells, err) = final_cells(&b, f, &mut pr);
                groups.push(cells);
                if let Some(e) = err {
                    let step = format!("group {gi} final");
                    return Some((
                        json!({"type": tname, "groups": groups, "step": step}),
                        Some(e),
                    ));
                }
                f.reset_partial_result(&mut pr);
            }
            Some((json!({"type": tname, "groups": groups, "step": ""}), None))
        }
        "merge" => {
            let fb = match build_desc(&a[1]) {
                Outcome::Built(fb) => fb,
                Outcome::Blocked => return None,
                Outcome::Failed(e) => return Some((Value::Null, Some(e))),
            };
            let ff = fb
                .func
                .as_deref()
                .expect("the records build a final function");
            let types = json!([tname, type_name(&fb.func)]);
            let reuse = a[3].as_bool().unwrap();
            let rounds = if reuse { 2 } else { 1 };
            let mut out =
                json!({"type": types, "partials": [], "first": null, "final": null, "step": ""});
            let mut partials = vec![];
            let mut fpr = ff.alloc_partial_result();
            let fec = fb.ctx.get_eval_ctx();
            for round in 0..rounds {
                for (i, part) in a[2].as_array().unwrap().iter().enumerate() {
                    let mut pr = f.alloc_partial_result();
                    if let Err((step, e)) = update(&b, f, part, &mut pr) {
                        out["partials"] = Value::Array(partials);
                        out["step"] = json!(format!("round {round} part {i} {step}"));
                        return Some((out, Some(e)));
                    }
                    let (cells, err) = final_cells(&b, f, &mut pr);
                    partials.push(cells);
                    if let Some(e) = err {
                        out["partials"] = Value::Array(partials);
                        out["step"] = json!(format!("round {round} part {i} final"));
                        return Some((out, Some(e)));
                    }
                    if let Err(e) = ff.merge_partial_result(&*fec, &mut pr, &mut fpr) {
                        out["partials"] = Value::Array(partials);
                        out["step"] = json!(format!("round {round} merge {i}"));
                        return Some((out, Some(e)));
                    }
                }
                let (cells, err) = final_cells(&fb, ff, &mut fpr);
                out["partials"] = Value::Array(partials.clone());
                if round + 1 < rounds {
                    out["first"] = cells;
                    if let Some(e) = err {
                        out["step"] = json!(format!("round {round} final"));
                        return Some((out, Some(e)));
                    }
                    ff.reset_partial_result(&mut fpr);
                    continue;
                }
                out["final"] = cells;
                if err.is_some() {
                    out["step"] = json!(format!("round {round} final"));
                }
                return Some((out, err));
            }
            unreachable!("at least one round")
        }
        _ => panic!("unknown op {op}"),
    }
}

enum CheckResult {
    Matched,
    Blocked,
    CanonicalDistinctDiff,
    LeadWraparoundDiff,
}

fn decode_count_cell(cell: &Value) -> Option<i64> {
    if cell["null"] != false {
        return None;
    }
    let hex = cell["raw"].as_str()?;
    let bytes: [u8; 8] = unhex(hex).try_into().ok()?;
    Some(i64::from_le_bytes(bytes))
}

/// Computes `(raw_distinct, canonical_distinct)` for a `countOriginalWithDistinct` group
/// whose arguments are scalar types covered by ruling R23 (#390). Returns `None` if an
/// argument type is outside that set.
fn count_distinct_raw_and_canonical(arg_types: &[u8], chunks: &[Value]) -> Option<(i64, i64)> {
    let mut raw_set: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    let mut canon_set: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    for ch in chunks {
        for row in ch.as_array()? {
            let cells = row.as_array()?;
            if cells.len() != arg_types.len() || cells.iter().any(Value::is_null) {
                continue;
            }
            let mut raw_row = Vec::with_capacity(arg_types.len());
            let mut canon_row = Vec::with_capacity(arg_types.len());
            for (tp, cell) in arg_types.iter().zip(cells.iter()) {
                match *tp {
                    mysql::TypeLonglong => {
                        let s = cell.to_string();
                        raw_row.push(s.clone());
                        canon_row.push(s);
                    }
                    mysql::TypeDouble | mysql::TypeFloat => {
                        let bits = cell.as_str()?;
                        raw_row.push(bits.to_owned());
                        // Fold -0.0 (sign bit only) to +0.0.
                        let canon = if bits == "9223372036854775808" {
                            "0"
                        } else {
                            bits
                        };
                        canon_row.push(canon.to_owned());
                    }
                    mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => {
                        let arr = cell.as_array()?;
                        let core = arr.first()?.as_str()?;
                        raw_row.push(cell.to_string());
                        canon_row.push(core.to_owned());
                    }
                    mysql::TypeDuration => {
                        let arr = cell.as_array()?;
                        let dur = arr.first()?.as_str()?;
                        raw_row.push(cell.to_string());
                        canon_row.push(dur.to_owned());
                    }
                    _ => return None,
                }
            }
            raw_set.insert(raw_row);
            canon_set.insert(canon_row);
        }
    }
    Some((raw_set.len() as i64, canon_set.len() as i64))
}

/// Returns true when `rec` differs from `got_out` solely because Go v8.5.8's
/// `countOriginalWithDistinct` hashes raw bytes (under #390 / ruling R23) while Rust keys on
/// canonical values, verifying that Go's output matches the raw count and Rust's output matches
/// the canonical count for every group.
fn is_canonical_distinct_diff(rec: &Value, got_out: &Value) -> bool {
    let want_out = &rec["out"];
    if want_out["type"] != "*aggfuncs.countOriginalWithDistinct"
        || got_out["type"] != want_out["type"]
        || want_out["step"] != ""
        || got_out["step"] != ""
    {
        return false;
    }
    let op = rec["op"].as_str().unwrap_or("");
    let (desc, groups, want_cells, got_cells): (&Value, Vec<&[Value]>, Vec<&Value>, Vec<&Value>) =
        match op {
            "count_distinct_encode" if rec["args"][0] == "run" => {
                let Some(chunks) = rec["args"][2].as_array() else {
                    return false;
                };
                let (Some(wc), Some(gc)) = (
                    want_out["cells"].as_array().and_then(|a| a.first()),
                    got_out["cells"].as_array().and_then(|a| a.first()),
                ) else {
                    return false;
                };
                (&rec["args"][1], vec![chunks.as_slice()], vec![wc], vec![gc])
            }
            "reset_reuse" => {
                let Some(raw_groups) = rec["args"][1].as_array() else {
                    return false;
                };
                let (Some(wg), Some(gg)) =
                    (want_out["groups"].as_array(), got_out["groups"].as_array())
                else {
                    return false;
                };
                if raw_groups.len() != wg.len() || wg.len() != gg.len() {
                    return false;
                }
                let mut groups = Vec::with_capacity(raw_groups.len());
                let mut wc = Vec::with_capacity(wg.len());
                let mut gc = Vec::with_capacity(gg.len());
                for ((rg, w), g) in raw_groups.iter().zip(wg.iter()).zip(gg.iter()) {
                    let Some(chunks) = rg.as_array() else {
                        return false;
                    };
                    let (Some(w0), Some(g0)) = (
                        w.as_array().and_then(|a| a.first()),
                        g.as_array().and_then(|a| a.first()),
                    ) else {
                        return false;
                    };
                    groups.push(chunks.as_slice());
                    wc.push(w0);
                    gc.push(g0);
                }
                (&rec["args"][0], groups, wc, gc)
            }
            _ => return false,
        };
    if desc["name"] != "count" || desc["distinct"] != true {
        return false;
    }
    let Some(arg_fts) = desc["args"].as_array() else {
        return false;
    };
    let Some(arg_types): Option<Vec<u8>> = arg_fts
        .iter()
        .map(|ft| ft[0].as_u64().and_then(|v| u8::try_from(v).ok()))
        .collect()
    else {
        return false;
    };
    let mut saw_diff = false;
    for ((chunks, wc), gc) in groups.iter().zip(want_cells.iter()).zip(got_cells.iter()) {
        let Some((raw_cnt, canon_cnt)) = count_distinct_raw_and_canonical(&arg_types, chunks)
        else {
            return false;
        };
        let (Some(want_cnt), Some(got_cnt)) = (decode_count_cell(wc), decode_count_cell(gc)) else {
            return false;
        };
        if want_cnt != raw_cnt || got_cnt != canon_cnt {
            return false;
        }
        if raw_cnt != canon_cnt {
            saw_diff = true;
        }
    }
    saw_diff
}

/// Returns true when `rec` differs from `got_out` solely because Go v8.5.8's `lead` wraps
/// `curIdx + offset` as `uint64` (#405 / ruling R26) while Rust uses `checked_add` and returns
/// the default value on every row.
fn is_lead_wraparound_diff(rec: &Value, got_out: &Value) -> bool {
    if rec["op"] != "window" || rec["args"][0] != "run" {
        return false;
    }
    let desc = &rec["args"][1];
    if desc["name"] != "lead" || desc["has_u64_arg"] != true {
        return false;
    }
    let Ok(offset) = desc["u64_arg"].as_str().unwrap_or("").parse::<u64>() else {
        return false;
    };
    let want_out = &rec["out"];
    if want_out["type"] != "*aggfuncs.lead"
        || got_out["type"] != "*aggfuncs.lead"
        || want_out["step"] != ""
        || got_out["step"] != ""
    {
        return false;
    }
    let (Some(w_parts), Some(g_parts), Some(in_parts)) = (
        want_out["partitions"].as_array(),
        got_out["partitions"].as_array(),
        rec["args"][2].as_array(),
    ) else {
        return false;
    };
    if w_parts.len() != g_parts.len() || w_parts.len() != in_parts.len() {
        return false;
    }
    let def_cell = match desc.get("def_kind").and_then(Value::as_str).unwrap_or("") {
        "" | "null" => json!({"null": true, "raw": "0000000000000000"}),
        "const_int" => {
            let v = desc["def_int"].as_i64().unwrap();
            json!({"null": false, "raw": hex(&v.to_le_bytes())})
        }
        _ => return false,
    };
    let mut saw_wrap = false;
    for ((wp, gp), ip) in w_parts.iter().zip(g_parts.iter()).zip(in_parts.iter()) {
        let (Some(w_cells), Some(g_cells)) = (wp.as_array(), gp.as_array()) else {
            return false;
        };
        if w_cells.len() != g_cells.len() {
            return false;
        }
        let mut flat_vals: Vec<Value> = vec![];
        for batch in ip["batches"].as_array().unwrap() {
            for row in batch.as_array().unwrap() {
                let cell = &row.as_array().unwrap()[0];
                if cell.is_null() {
                    flat_vals.push(json!({"null": true, "raw": ""}));
                } else {
                    let v: i64 = cell.as_str().unwrap().parse().unwrap();
                    flat_vals.push(json!({"null": false, "raw": hex(&v.to_le_bytes())}));
                }
            }
        }
        let n = flat_vals.len() as u64;
        for (i, (wc, gc)) in w_cells.iter().zip(g_cells.iter()).enumerate() {
            let idx = i as u64;
            let expected_rust = match idx.checked_add(offset) {
                Some(t) if t < n => &flat_vals[t as usize],
                _ => &def_cell,
            };
            let wrapped = idx.wrapping_add(offset);
            let expected_go = if wrapped < n {
                &flat_vals[wrapped as usize]
            } else {
                &def_cell
            };
            if gc != expected_rust || wc != expected_go {
                return false;
            }
            if expected_rust != expected_go {
                saw_wrap = true;
            }
        }
    }
    saw_wrap
}

/// Compares one record; Err(why) on a difference.
fn check(rec: &Value) -> Result<CheckResult, String> {
    let op = rec["op"].as_str().unwrap();
    let Some((out, err)) = run(op, rec["args"].as_array().unwrap()) else {
        return Ok(CheckResult::Blocked);
    };
    let got_err = err.map(|e| e.to_string());
    let want_err = rec["err"]["msg"].as_str().map(str::to_owned);
    if got_err != want_err {
        return Err(format!("error: got {got_err:?}, want {want_err:?}"));
    }
    if out != rec["out"] {
        if is_canonical_distinct_diff(rec, &out) {
            return Ok(CheckResult::CanonicalDistinctDiff);
        }
        if is_lead_wraparound_diff(rec, &out) {
            return Ok(CheckResult::LeadWraparoundDiff);
        }
        return Err(format!(
            "out differs: got {}",
            serde_json::to_string(&out).unwrap()
        ));
    }
    Ok(CheckResult::Matched)
}

fn replay(path: &str, pin_blocked: bool) {
    let lines = read_records(path, "aggfuncs");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let mut fails = vec![];
    let mut blocked = 0;
    let mut canonical_distinct_diffs = 0;
    let mut lead_wraparound_diffs = 0;
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        match check(&rec) {
            Ok(CheckResult::Matched) => {}
            Ok(CheckResult::Blocked) => blocked += 1,
            Ok(CheckResult::CanonicalDistinctDiff) => canonical_distinct_diffs += 1,
            Ok(CheckResult::LeadWraparoundDiff) => lead_wraparound_diffs += 1,
            Err(why) => {
                let cut = line
                    .char_indices()
                    .nth(600)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len());
                fails.push(format!("{}\n    {why}", &line[..cut]));
            }
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
    eprintln!(
        "{path}: {} records, {blocked} blocked, {canonical_distinct_diffs} ruled (R23), {lead_wraparound_diffs} ruled (R26)",
        lines.len()
    );
    if pin_blocked {
        assert_eq!(blocked, EXPECTED_BLOCKED, "{path}: blocked records");
        assert_eq!(
            canonical_distinct_diffs, EXPECTED_CANONICAL_DISTINCT_DIFFS,
            "{path}: ruled R23 records"
        );
        assert_eq!(
            lead_wraparound_diffs, EXPECTED_LEAD_WRAPAROUND_DIFFS,
            "{path}: ruled R26 records"
        );
    }
}

#[test]
fn govec_aggfuncs() {
    match std::env::var("GOVEC_AGGFUNCS_FILES") {
        Ok(files) => {
            for f in files.split(',').filter(|f| !f.is_empty()) {
                replay(f, false);
            }
        }
        Err(_) => replay(&vectors("aggfuncs"), true),
    }
}
