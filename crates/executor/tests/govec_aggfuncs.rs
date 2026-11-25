//! Replays the govec `aggfuncs` vectors (tidb-rs-tracker `govec/domains/aggfuncs.go`): the
//! core aggregate functions of part A of the block D12.4 (COUNT, SUM, AVG, FIRST_ROW,
//! MAX / MIN, BIT_*), against Go TiDB v8.5.8.
//!
//! A record carries the complete descriptor (name, mode, DISTINCT, argument types, return
//! type, `tidb_window_use_high_precision`, `div_precision_increment`); the replay builds the
//! `AggFuncDesc` from it directly (no type inference) and runs `build` and the functions as
//! Go ran them:
//! - `build`: the implementation chosen (Go's `%T`, `<nil>` for none);
//! - `update_final`: alloc, one update per batch, append final;
//! - `reset_reuse`: one partial result over several groups (updates, append final, reset);
//! - `merge`: the partial function of a split pair over several parts, each merged into the
//!   final function's partial result, optionally twice with a reset in between.
//!
//! The appended cells (`Row.GetRaw` and the null flag), the error and the step that returned
//! it are compared. The functions of the later parts of the block (COUNT(DISTINCT),
//! GROUP_CONCAT, VAR / STDDEV, JSON_*AGG, APPROX_*) return a temporary `blocked` from `build`
//! (spec §4); their records are counted, and the count is pinned by [`EXPECTED_BLOCKED`].
//!
//! `GOVEC_AGGFUNCS_FILES` (comma-separated paths) replays these files instead of the
//! committed one (other seeds, a fresh artifact).

use std::io::Read;
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::WarnHandler;
use tidb_base::mysql;
use tidb_chunk::chunk::{self, Chunk};
use tidb_chunk::row::Row;
use tidb_executor::aggfuncs::func_count_distinct::write_time;
use tidb_executor::aggfuncs::{AggFunc, PartialResult, build};
use tidb_expression::aggregation::{AggFuncDesc, AggFunctionMode, BaseFuncDesc};
use tidb_expression::byitem::ByItems;
use tidb_expression::context::BuildContext;
use tidb_expression::expression::EvalState;
use tidb_expression::{Column, Constant, Expr, ExprRef};
use tidb_parser::types::FieldType;
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::context::FlagTruncateAsWarning;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::new_string_datum;
use tidb_types::enum_type::Enum;
use tidb_types::field_type::new_field_type;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, new_time};
use tidb_types::vector::zero_copy_deserialize_vector_float32;

/// The records whose function `build` reports as not ported yet (part B2 of the block:
/// `APPROX_COUNT_DISTINCT` and `APPROX_PERCENTILE`), in every op that builds them.
const EXPECTED_BLOCKED: usize = 560;

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
    let arg_fts: Vec<FieldType> = d["args"].as_array().unwrap().iter().map(ft_of).collect();
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
    match build(&*ctx, &desc, 0) {
        Ok(func) => Outcome::Built(Built {
            ctx,
            eval_ctx,
            desc,
            fts,
            func,
        }),
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
        "json_agg" => {
            let action = a[0].as_str().unwrap();
            return run(action, &a[1..]);
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

/// Compares one record; Err(why) on a difference, Ok(false) when its function is blocked.
fn check(rec: &Value) -> Result<bool, String> {
    let op = rec["op"].as_str().unwrap();
    let Some((out, err)) = run(op, rec["args"].as_array().unwrap()) else {
        return Ok(false);
    };
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
    Ok(true)
}

fn replay(path: &str, pin_blocked: bool) {
    let lines = read_records(path, "aggfuncs");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let mut fails = vec![];
    let mut blocked = 0;
    for line in &lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        match check(&rec) {
            Ok(true) => {}
            Ok(false) => blocked += 1,
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
    eprintln!("{path}: {} records, {blocked} blocked", lines.len());
    if pin_blocked {
        assert_eq!(blocked, EXPECTED_BLOCKED, "{path}: blocked records");
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
