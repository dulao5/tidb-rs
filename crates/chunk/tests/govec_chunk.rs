//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/chunk.jsonl.gz`
//! (tracker `govec`, domain "chunk", block D01.7) against the Rust port.
//!
//! A `script` record is a sequence of steps on chunks, lists and decoders (the tracker's
//! `govec/domains/chunk.go` lists them); every step's output must be equal. A dump compares
//! the chunk's state, including each column's buffers with Go's capacities and the bytes
//! under NULL cells, and `Codec.Encode`'s output. A Go panic must be a Rust panic (the
//! message is not compared; the script ends there).

use std::collections::HashMap;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;
use serde_json::json;
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_chunk::Column;
use tidb_chunk::List;
use tidb_chunk::MutRow;
use tidb_chunk::Row;
use tidb_chunk::RowPtr;
use tidb_chunk::chunk;
use tidb_chunk::chunk_util;
use tidb_chunk::codec;
use tidb_chunk::codec::Decoder;
use tidb_chunk::compare;
use tidb_chunk::iterator;
use tidb_chunk::list;
use tidb_chunk::mutrow;
use tidb_memory::Tracker;
use tidb_memory::tracker::LabelForChunkList;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::EvalType;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum, GoValue};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::BinaryJSON;
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::set_type::Set;
use tidb_types::time::Duration;
use tidb_types::time::Time;
use tidb_types::time::new_time;
use tidb_types::vector::parse_vector_float32;
use tidb_types::vector::zero_copy_deserialize_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

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
fn go_string(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

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

fn fts_of(v: &Value) -> Vec<FieldType> {
    v.as_array().unwrap().iter().map(ft_of).collect()
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

fn idx(v: &Value) -> usize {
    v.as_i64().unwrap() as usize
}

fn ints(v: &Value) -> Option<Vec<usize>> {
    v.as_array().map(|a| a.iter().map(idx).collect())
}

fn bools(v: &Value) -> Vec<bool> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_bool().unwrap())
        .collect()
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

fn vec_bytes(bits: &[Value]) -> Vec<u8> {
    let mut b = (bits.len() as u32).to_le_bytes().to_vec();
    for x in bits {
        b.extend_from_slice(&(x.as_u64().unwrap() as u32).to_le_bytes());
    }
    b
}

/// govec's chunkAppend.
fn append(c: &mut Chunk, ft: &FieldType, col: usize, v: &Value, col_level: bool) {
    if v.is_null() {
        if col_level {
            c.column_mut(col).append_null();
        } else {
            c.append_null(col);
        }
        return;
    }
    macro_rules! app {
        ($chk:ident, $colm:ident, $val:expr) => {
            if col_level {
                c.column_mut(col).$colm($val)
            } else {
                c.$chk(col, $val)
            }
        };
    }
    match kind(ft) {
        "i64" => app!(append_int64, append_int64, s_i64(v)),
        "u64" => app!(append_uint64, append_uint64, s_u64(v)),
        "f32" => app!(
            append_float32,
            append_float32,
            f32::from_bits(s_u64(v) as u32)
        ),
        "f64" => app!(append_float64, append_float64, f64::from_bits(s_u64(v))),
        "dec" => {
            let d = dec_of(v.as_str().unwrap());
            app!(append_my_decimal, append_my_decimal, &d)
        }
        "time" => {
            let a = v.as_array().unwrap();
            let t = new_time(
                CoreTime(s_u64(&a[0])),
                a[1].as_u64().unwrap() as u8,
                a[2].as_i64().unwrap() as isize,
            );
            app!(append_time, append_time, t)
        }
        "dur" => {
            let a = v.as_array().unwrap();
            let d = Duration {
                duration: s_i64(&a[0]),
                fsp: a[1].as_i64().unwrap() as isize,
            };
            app!(append_duration, append_duration, d)
        }
        "json" => {
            let j = parse_binary_json_from_string(v.as_str().unwrap().as_bytes()).unwrap();
            app!(append_json, append_json, &j)
        }
        "vec" => {
            let b = vec_bytes(v.as_array().unwrap());
            let (vv, _) = zero_copy_deserialize_vector_float32(&b).unwrap();
            app!(append_vector_float32, append_vector_float32, &vv)
        }
        "nv" => {
            let a = v.as_array().unwrap();
            let name = unhex(a[0].as_str().unwrap());
            let val = a[1].as_u64().unwrap();
            if ft.get_type() == mysql::TypeEnum {
                let e = Enum { name, value: val };
                app!(append_enum, append_enum, &e)
            } else {
                let st = Set { name, value: val };
                app!(append_set, append_set, &st)
            }
        }
        _ => {
            let b = unhex(v.as_str().unwrap());
            if ft.get_type() == mysql::TypeVarchar {
                app!(append_string, append_string, &b)
            } else {
                app!(append_bytes, append_bytes, &b)
            }
        }
    }
}

/// govec's chunkGet.
fn get(r: Row<'_>, ft: &FieldType, col: usize) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("null".into(), json!(r.is_null(col)));
    out.insert("raw".into(), json!(hex(r.get_raw(col))));
    if r.is_null(col) {
        return Value::Object(out);
    }
    let v = match kind(ft) {
        "i64" => json!(r.get_int64(col).to_string()),
        "u64" => json!(r.get_uint64(col).to_string()),
        "f32" => json!(r.get_float32(col).to_bits().to_string()),
        "f64" => json!(r.get_float64(col).to_bits().to_string()),
        "dec" => json!(r.get_my_decimal(col).string()),
        "time" => json!(String::from_utf8(r.get_time(col).string()).unwrap()),
        "dur" => {
            let d = r.get_duration(col, ft.get_decimal());
            json!([
                d.duration.to_string(),
                d.fsp,
                String::from_utf8(d.string()).unwrap()
            ])
        }
        "json" => {
            let j = r.get_json(col);
            json!([j.type_code, hex(&j.value), go_string(&j.string())])
        }
        "vec" => json!(r.get_vector_float32(col).string()),
        "nv" => {
            let (name, val) = if ft.get_type() == mysql::TypeEnum {
                let e = r.get_enum(col);
                (e.name, e.value)
            } else {
                let st = r.get_set(col);
                (st.name, st.value)
            };
            json!([hex(&name), val.to_string()])
        }
        "null" => Value::Null,
        _ => json!(hex(if ft.get_type() == mysql::TypeVarchar {
            r.get_string(col)
        } else {
            r.get_bytes(col)
        })),
    };
    if kind(ft) != "null" {
        out.insert("v".into(), v);
    }
    Value::Object(out)
}

/// The strings of a typed column view (`view` step).
fn view_strings(c: &Column, kind: &str) -> Vec<String> {
    match kind {
        "i64" => c.int64s().iter().map(|x| x.get().to_string()).collect(),
        "u64" => c.uint64s().iter().map(|x| x.get().to_string()).collect(),
        "f32" => c
            .float32s()
            .iter()
            .map(|x| x.get().to_bits().to_string())
            .collect(),
        "f64" => c
            .float64s()
            .iter()
            .map(|x| x.get().to_bits().to_string())
            .collect(),
        "dur" => c
            .go_durations()
            .iter()
            .map(|x| x.get().to_string())
            .collect(),
        "time" => c
            .times()
            .iter()
            .map(|x| {
                String::from_utf8(tidb_types::time::Time::from_raw(CoreTime(x.get())).string())
                    .unwrap()
            })
            .collect(),
        "dec" => c
            .decimals()
            .iter()
            .map(|x| MyDecimal::from_wire(x).unwrap().string())
            .collect(),
        k => panic!("view {k}"),
    }
}

fn col_dump(col: &Column) -> Value {
    let s = col.govec_state();
    let (eb, ebc) = match s.elem_buf {
        Some((b, c)) => (json!(hex(b)), c),
        None => (Value::Null, 0),
    };
    json!({
        "l": s.length, "nb": hex(s.null_bitmap.0), "nbc": s.null_bitmap.1, "off": hex(s.offsets.0),
        "oc": s.offsets.1, "d": hex(s.data.0), "dc": s.data.1, "eb": eb, "ebc": ebc,
    })
}

fn dump(c: &Chunk) -> Value {
    let n = c.num_cols();
    let mut alias = Vec::new();
    let mut cols = Vec::new();
    for i in 0..n {
        let mut a = i;
        for j in 0..i {
            if tidb_chunk::ColArc::ptr_eq(c.column_arc(j), c.column_arc(i)) {
                a = j;
                break;
            }
        }
        alias.push(a);
        cols.push(col_dump(c.column(i)));
    }
    json!({
        "n": c.num_rows(), "nc": n, "v": c.get_num_virtual_rows(), "cap": c.capacity(), "req": c.required_rows(),
        "full": c.is_full(), "inc": c.is_in_complete_chunk(), "sel": c.sel(), "mem": c.memory_usage(),
        "enc": hex(&codec::new_codec(Vec::new()).encode(c)), "alias": alias, "cols": cols,
    })
}

fn iter_dump<'a>(it: &mut dyn iterator::Iterator<'a>, pre_begin: bool) -> Value {
    let cur = |r: Row<'_>| -> Value {
        if r.is_empty() {
            Value::Null
        } else {
            json!([r.idx(), r.len()])
        }
    };
    let mut out = vec![json!(it.len())];
    if pre_begin {
        out.push(cur(it.current()));
    }
    let mut r = it.begin();
    while r != it.end() {
        out.push(cur(r));
        out.push(cur(it.current()));
        r = it.next();
    }
    out.push(cur(it.current()));
    let n = it.next();
    out.push(cur(n));
    out.push(cur(it.current()));
    it.reach_end();
    out.push(cur(it.current()));
    out.push(json!(it.error().is_none()));
    Value::Array(out)
}

fn err_json(e: Result<(), tidb_base::Error>) -> Value {
    match e {
        Ok(()) => Value::Null,
        Err(e) => json!({ "err": String::from_utf8_lossy(&e.error()) }),
    }
}

#[derive(Default)]
struct State {
    chks: HashMap<usize, Chunk>,
    fts: HashMap<usize, Vec<FieldType>>,
    lists: HashMap<usize, (List, Arc<Tracker>)>,
    decs: HashMap<usize, (Decoder, Vec<FieldType>)>,
    mrs: HashMap<usize, MutRow>,
}

impl State {
    /// Takes chunk `d` out of the map while `f` runs with it and the rest of the state.
    fn with<R>(&mut self, d: usize, f: impl FnOnce(&mut Chunk, &mut State) -> R) -> R {
        let mut c = self.chks.remove(&d).unwrap();
        let r = f(&mut c, self);
        self.chks.insert(d, c);
        r
    }
}

fn step(st: &mut State, a: &[Value]) -> Value {
    let op = a[0].as_str().unwrap();
    let ci = |i: usize| idx(&a[i]);
    match op {
        "new" | "new_empty" | "new_cap" | "new_pool" => {
            let fts = fts_of(&a[2]);
            let c = match op {
                "new" => chunk::new(&fts, ci(3), ci(4)),
                "new_empty" => chunk::new_empty_chunk(&fts),
                "new_cap" => chunk::new_chunk_with_capacity(&fts, ci(3)),
                _ => chunk::new_chunk_from_pool_with_capacity(&fts, ci(3)),
            };
            st.chks.insert(ci(1), c);
            st.fts.insert(ci(1), fts);
        }
        "renew" | "clone_empty" | "copy" | "copy_sel" => {
            let s = &st.chks[&ci(2)];
            let c = match op {
                "renew" => chunk::renew(s, ci(3)),
                "clone_empty" => s.clone_empty(ci(3)),
                "copy" => s.copy_construct(),
                _ => s.copy_construct_sel(),
            };
            let fts = st.fts[&ci(2)].clone();
            st.chks.insert(ci(1), c);
            st.fts.insert(ci(1), fts);
        }
        "prune" => {
            let idxs = ints(&a[3]).unwrap();
            let fts: Vec<FieldType> = idxs.iter().map(|&x| st.fts[&ci(2)][x].clone()).collect();
            let c = st.chks[&ci(2)].prune(&idxs);
            st.chks.insert(ci(1), c);
            st.fts.insert(ci(1), fts);
        }
        "app" | "capp" => {
            let ft = st.fts[&ci(1)][ci(2)].clone();
            append(
                st.chks.get_mut(&ci(1)).unwrap(),
                &ft,
                ci(2),
                &a[3],
                op == "capp",
            );
        }
        "app_nn" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .append_n_nulls(ci(3)),
        "app_nb" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .append_null_bitmap(a[3].as_bool().unwrap()),
        "app_cell" => st.with(ci(1), |d, st| {
            d.column_mut(ci(2))
                .append_cell_n_times(st.chks[&ci(3)].column(ci(4)), ci(5), ci(6))
        }),
        "app_row" => st.with(ci(1), |d, st| d.append_row(st.chks[&ci(2)].get_row(ci(3)))),
        "app_prow" => st.with(ci(1), |d, st| {
            d.append_partial_row(ci(2), st.chks[&ci(3)].get_row(ci(4)))
        }),
        "app_row_idx" => {
            return st.with(ci(1), |d, st| {
                json!(
                    d.append_row_by_col_idxs(
                        st.chks[&ci(2)].get_row(ci(3)),
                        ints(&a[4]).as_deref()
                    )
                )
            });
        }
        "app_prow_idx" => {
            return st.with(ci(1), |d, st| {
                json!(d.append_partial_row_by_col_idxs(
                    ci(2),
                    st.chks[&ci(3)].get_row(ci(4)),
                    ints(&a[5]).as_deref()
                ))
            });
        }
        "app_rows" | "app_prows" => {
            let (s, off, ri) = if op == "app_prows" {
                (ci(3), ci(2), 4)
            } else {
                (ci(2), 0, 3)
            };
            st.with(ci(1), |d, st| {
                let src = &st.chks[&s];
                let rows: Vec<Row<'_>> = ints(&a[ri])
                    .unwrap()
                    .into_iter()
                    .map(|r| src.get_row(r))
                    .collect();
                if op == "app_rows" {
                    d.append_rows(&rows);
                } else {
                    d.append_partial_rows(off, &rows);
                }
            });
        }
        "app_range" => st.with(ci(1), |d, st| d.append(&st.chks[&ci(2)], ci(3), ci(4))),
        "app_raw" => {
            let mut b = unhex(a[3].as_str().unwrap());
            b.extend_from_slice(&[0; 16]);
            let c = st.chks.get_mut(&ci(1)).unwrap();
            return json!(chunk::append_cell_from_raw_data(
                c.column_mut(ci(2)),
                &b,
                ci(4)
            ));
        }
        "truncate" => st.chks.get_mut(&ci(1)).unwrap().truncate_to(ci(2)),
        "reset" => st.chks.get_mut(&ci(1)).unwrap().reset(),
        "grow_reset" => st.chks.get_mut(&ci(1)).unwrap().grow_and_reset(ci(2)),
        "reconstruct" => st.chks.get_mut(&ci(1)).unwrap().reconstruct(),
        "set_sel" => st.chks.get_mut(&ci(1)).unwrap().set_sel(ints(&a[2])),
        "set_req" => {
            st.chks
                .get_mut(&ci(1))
                .unwrap()
                .set_required_rows(a[2].as_i64().unwrap() as isize, ci(3));
        }
        "set_virt" => st.chks.get_mut(&ci(1)).unwrap().set_num_virtual_rows(ci(2)),
        "set_inc" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .set_in_complete_chunk(a[2].as_bool().unwrap()),
        "make_ref" => st.chks.get_mut(&ci(1)).unwrap().make_ref(ci(2), ci(3)),
        "make_ref_to" => {
            return st.with(ci(1), |d, st| {
                err_json(d.make_ref_to(ci(2), &st.chks[&ci(3)], ci(4)))
            });
        }
        "swap_cols" => {
            let mut b = st.chks.remove(&ci(2)).unwrap();
            st.chks.get_mut(&ci(1)).unwrap().swap_columns(&mut b);
            st.chks.insert(ci(2), b);
            let (f1, f2) = (
                st.fts.remove(&ci(1)).unwrap(),
                st.fts.remove(&ci(2)).unwrap(),
            );
            st.fts.insert(ci(1), f2);
            st.fts.insert(ci(2), f1);
        }
        "swap_helper" => {
            let used = ints(&a[3]).unwrap();
            let h = chunk_util::new_column_swap_helper(&used);
            let mut out = st.chks.remove(&ci(2)).unwrap();
            let r = h.swap_columns(st.chks.get_mut(&ci(1)).unwrap(), &mut out);
            st.chks.insert(ci(2), out);
            return err_json(r);
        }
        "set_col" => {
            return st.with(ci(1), |d, st| {
                let col = st.chks[&ci(3)].column_arc(ci(4)).clone();
                json!(d.set_col(ci(2), col).is_some())
            });
        }
        "resize" => {
            let c = st.chks.get_mut(&ci(1)).unwrap().column_mut(ci(2));
            let (n, is_null) = (ci(4), a[5].as_bool().unwrap());
            match a[3].as_str().unwrap() {
                "i64" => c.resize_int64(n, is_null),
                "u64" => c.resize_uint64(n, is_null),
                "f32" => c.resize_float32(n, is_null),
                "f64" => c.resize_float64(n, is_null),
                "dec" => c.resize_decimal(n, is_null),
                "dur" => c.resize_go_duration(n, is_null),
                "time" => c.resize_time(n, is_null),
                k => panic!("resize {k}"),
            }
        }
        "reserve" => {
            let c = st.chks.get_mut(&ci(1)).unwrap().column_mut(ci(2));
            let n = ci(4);
            match a[3].as_str().unwrap() {
                "str" => c.reserve_string(n),
                "bytes" => c.reserve_bytes(n),
                "json" => c.reserve_json(n),
                "vec" => c.reserve_vector_float32(n),
                "set" => c.reserve_set(n),
                "enum" => c.reserve_enum(n),
                k => panic!("reserve {k}"),
            }
        }
        "col_reset" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .reset_for(EvalType(ci(3) as u8)),
        "set_null" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .set_null(ci(3), a[4].as_bool().unwrap()),
        "set_nulls" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .set_nulls(ci(3), ci(4), a[5].as_bool().unwrap()),
        "set_raw" => st
            .chks
            .get_mut(&ci(1))
            .unwrap()
            .column_mut(ci(2))
            .set_raw(ci(3), &unhex(a[4].as_str().unwrap())),
        "merge_nulls" => {
            let others: Vec<Column> = a[3]
                .as_array()
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|p| st.chks[&idx(&p[0])].column(idx(&p[1])).clone())
                .collect();
            let refs: Vec<&Column> = others.iter().collect();
            st.chks
                .get_mut(&ci(1))
                .unwrap()
                .column_mut(ci(2))
                .merge_nulls(&refs);
        }
        "col_copy" | "col_copy_recon" => {
            let recon = op == "col_copy_recon";
            let into = a[if recon { 6 } else { 5 }].as_bool().unwrap();
            let sel = if recon { ints(&a[5]) } else { None };
            st.with(ci(1), |d, st| {
                let src = st.chks[&ci(3)].column(ci(4));
                if into {
                    let dst = d.column(ci(2)).clone();
                    let r = if recon {
                        src.copy_reconstruct(sel.as_deref(), Some(dst))
                    } else {
                        src.copy_construct(Some(dst))
                    };
                    *d.column_mut(ci(2)) = r;
                } else {
                    let r = if recon {
                        src.copy_reconstruct(sel.as_deref(), None)
                    } else {
                        src.copy_construct(None)
                    };
                    d.set_col(ci(2), tidb_chunk::ColArc::new(r));
                }
            });
        }
        "view_set" => {
            let c = st.chks.get_mut(&ci(1)).unwrap().column_mut(ci(2));
            let r = ci(4);
            match a[3].as_str().unwrap() {
                "i64" => c.int64s_mut()[r].set(s_i64(&a[5])),
                "u64" => c.uint64s_mut()[r].set(s_u64(&a[5])),
                "f32" => c.float32s_mut()[r].set(f32::from_bits(s_u64(&a[5]) as u32)),
                "f64" => c.float64s_mut()[r].set(f64::from_bits(s_u64(&a[5]))),
                "dur" => c.go_durations_mut()[r].set(s_i64(&a[5])),
                "time" => c.times_mut()[r].set(
                    new_time(CoreTime(s_u64(&a[5])), mysql::TypeDatetime, 0)
                        .raw()
                        .0,
                ),
                "dec" => c.decimals_mut()[r] = dec_of(a[5].as_str().unwrap()).as_wire(),
                k => panic!("view_set {k}"),
            }
        }
        "get" => {
            let c = &st.chks[&ci(1)];
            return get(c.get_row(ci(2)), &st.fts[&ci(1)][ci(3)], ci(3));
        }
        "view" => {
            let c = st.chks[&ci(1)].column(ci(2));
            let kind = a[3].as_str().unwrap();
            // Go's typed views read past the data's capacity when length * size exceeds it
            // (spec §4 item 4); the generator records that case instead of the bytes read,
            // and the port must panic there.
            let size = match kind {
                "f32" => 4,
                "dec" => 40,
                _ => 8,
            };
            let dc = c.govec_state().data.1;
            if c.rows() * size > dc {
                let r = catch_unwind(AssertUnwindSafe(|| view_strings(c, kind).len()));
                assert!(r.is_err(), "a view past the capacity must panic");
                return json!({ "past_cap": [c.rows(), dc] });
            }
            let out = view_strings(c, kind);
            let out = if out.is_empty() {
                Value::Null
            } else {
                json!(out)
            };
            return json!([out, c.rows()]);
        }
        "cmp" => {
            let Some(f) = compare::get_compare_func(&st.fts[&ci(1)][ci(3)]) else {
                return json!("nil");
            };
            return json!(f(
                st.chks[&ci(1)].get_row(ci(2)),
                ci(3),
                st.chks[&ci(4)].get_row(ci(5)),
                ci(6)
            ));
        }
        "iter" => {
            let c = &st.chks[&ci(1)];
            let mut it = iterator::new_iterator4_chunk(c);
            let r = iter_dump(&mut it, true);
            it.reset_chunk(it.get_chunk());
            return r;
        }
        "iter_slice" => {
            let c = &st.chks[&ci(1)];
            let rows: Vec<Row<'_>> = ints(&a[2])
                .unwrap()
                .into_iter()
                .map(|r| c.get_row(r))
                .collect();
            let half = rows[..rows.len() / 2].to_vec();
            let mut it = iterator::new_iterator4_slice(rows);
            let r = iter_dump(&mut it, true);
            it.reset(half);
            return json!([r, iter_dump(&mut it, true)]);
        }
        "row" => {
            let c = &st.chks[&ci(1)];
            let r = c.get_row(ci(2));
            let cp = r.copy_construct();
            return json!([
                r.idx(),
                r.len(),
                r.is_empty(),
                std::ptr::eq(r.chunk(), c),
                0,
                dump(&cp)
            ]);
        }
        "dump" => return dump(&st.chks[&ci(1)]),
        "encode" => {
            return json!(hex(
                &codec::new_codec(st.fts[&ci(1)].clone()).encode(&st.chks[&ci(1)])
            ));
        }
        "decode" => {
            let fts = fts_of(&a[2]);
            let (c, rest) =
                codec::new_codec(fts.clone()).decode(Bytes::from(unhex(a[3].as_str().unwrap())));
            st.chks.insert(ci(1), c);
            st.fts.insert(ci(1), fts);
            return json!(rest.len());
        }
        "decode_into" => {
            let fts = st.fts[&ci(1)].clone();
            let rest = codec::new_codec(fts).decode_to_chunk(
                Bytes::from(unhex(a[2].as_str().unwrap())),
                st.chks.get_mut(&ci(1)).unwrap(),
            );
            return json!(rest.len());
        }
        "dec_new" => {
            let fts = fts_of(&a[2]);
            let d = codec::new_decoder(chunk::new_chunk_with_capacity(&fts, 0), fts.clone());
            st.decs.insert(ci(1), (d, fts));
        }
        "dec_reset" => {
            let d = &mut st.decs.get_mut(&ci(1)).unwrap().0;
            d.reset(Bytes::from(unhex(a[2].as_str().unwrap())));
            return json!(d.remained_rows());
        }
        "dec_decode" => {
            let d = &mut st.decs.get_mut(&ci(1)).unwrap().0;
            d.decode(st.chks.get_mut(&ci(2)).unwrap());
            return json!([d.remained_rows(), d.is_finished()]);
        }
        "dec_reuse" => {
            let d = &mut st.decs.get_mut(&ci(1)).unwrap().0;
            d.reuse_interm_chk(st.chks.get_mut(&ci(2)).unwrap());
            return json!(d.is_finished());
        }
        "dec_state" => {
            let d = &st.decs[&ci(1)].0;
            return json!([d.remained_rows(), d.is_finished()]);
        }
        "list_new" => {
            let fts = fts_of(&a[2]);
            let tr = new_tracker(LabelForChunkList, -1);
            let l = list::new_list_with_mem_tracker(fts, ci(3), ci(4), Arc::clone(&tr));
            st.lists.insert(ci(1), (l, tr));
        }
        "list_app" => {
            let (l, _) = st.lists.get_mut(&ci(1)).unwrap();
            let p = l.append_row(st.chks[&ci(2)].get_row(ci(3))).unwrap();
            return json!([p.chk_idx, p.row_idx]);
        }
        "list_add" => {
            let c = st.chks.remove(&ci(2)).unwrap();
            st.lists.get_mut(&ci(1)).unwrap().0.add(c).unwrap();
        }
        "list_alloc" => {
            let (l, _) = st.lists.get_mut(&ci(1)).unwrap();
            let c = l.alloc_chunk().unwrap();
            let fts = l.field_types().to_vec();
            let d = dump(&c);
            st.chks.insert(ci(2), c);
            st.fts.insert(ci(2), fts);
            return d;
        }
        "list_reset" => st.lists.get_mut(&ci(1)).unwrap().0.reset().unwrap(),
        "list_clear" => st.lists.get_mut(&ci(1)).unwrap().0.clear().unwrap(),
        "list_get" => {
            let (l, _) = &st.lists[&ci(1)];
            let r = l.get_row(RowPtr {
                chk_idx: ci(2) as u32,
                row_idx: ci(3) as u32,
            });
            let out: Vec<Value> = l
                .field_types()
                .iter()
                .enumerate()
                .map(|(col, ft)| get(r, ft, col))
                .collect();
            let out = if out.is_empty() {
                Value::Null
            } else {
                json!(out)
            };
            return json!([out, dump(l.get_chunk(ci(2)))]);
        }
        "list_walk" => {
            let (l, _) = &st.lists[&ci(1)];
            let mut out = Vec::new();
            let r = l.walk(|row| {
                out.push(json!([row.idx(), row.len()]));
                Ok(())
            });
            let out = if out.is_empty() {
                Value::Null
            } else {
                json!(out)
            };
            return json!([out, r.is_ok()]);
        }
        "list_iter" => {
            let (l, _) = &st.lists[&ci(1)];
            return iter_dump(&mut *iterator::new_iterator4_list(l), true);
        }
        "list_ptr_iter" => {
            let (l, _) = &st.lists[&ci(1)];
            let ptrs: Vec<RowPtr> = a[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| RowPtr {
                    chk_idx: p[0].as_u64().unwrap() as u32,
                    row_idx: p[1].as_u64().unwrap() as u32,
                })
                .collect();
            return iter_dump(&mut *iterator::new_iterator4_row_ptr(l, &ptrs), true);
        }
        "multi_iter" => {
            let mut its: Vec<Box<dyn iterator::Iterator<'_> + '_>> = Vec::new();
            for s in a[1].as_array().unwrap() {
                let s: Vec<usize> = s.as_array().unwrap().iter().map(idx).collect();
                match s[0] {
                    0 => its.push(Box::new(iterator::new_iterator4_chunk(&st.chks[&s[1]]))),
                    1 => its.push(iterator::new_iterator4_list(&st.lists[&s[1]].0)),
                    _ => {
                        let c = &st.chks[&s[1]];
                        its.push(Box::new(iterator::new_iterator4_slice(
                            s[2..].iter().map(|&r| c.get_row(r)).collect(),
                        )));
                    }
                }
            }
            return iter_dump(&mut *iterator::new_multi_iterator(its), ci(2) != 0);
        }
        "copy_join_direct" => {
            return st.with(ci(3), |d, st| {
                let r =
                    chunk_util::copy_selected_join_rows_direct(&st.chks[&ci(1)], &bools(&a[2]), d);
                match r {
                    Ok(ok) => json!([ok, null]),
                    Err(e) => json!([false, { "err": String::from_utf8_lossy(&e.error()) }]),
                }
            });
        }
        "copy_join_outer" => {
            return st.with(ci(7), |d, st| {
                let r = chunk_util::copy_selected_join_rows_with_same_outer_rows(
                    &st.chks[&ci(1)],
                    ci(2),
                    ci(3),
                    ci(4),
                    ci(5),
                    &bools(&a[6]),
                    d,
                );
                match r {
                    Ok(ok) => json!([ok, null]),
                    Err(e) => json!([false, { "err": String::from_utf8_lossy(&e.error()) }]),
                }
            });
        }
        "copy_sel_rows" => st.with(ci(1), |d, st| {
            chunk_util::copy_selected_rows(
                d.column_mut(ci(2)),
                st.chks[&ci(3)].column(ci(4)),
                &bools(&a[5]),
            )
        }),
        "copy_sel_rows_fn" => {
            let sel = bools(&a[5]);
            let n = sel.len();
            st.with(ci(1), |d, st| {
                chunk_util::copy_selected_rows_with_row_id_func(
                    d.column_mut(ci(2)),
                    st.chks[&ci(3)].column(ci(4)),
                    &sel,
                    ci(6),
                    ci(7),
                    |i| n - 1 - i,
                )
            });
        }
        // ---- types.Datum / Enum / Set (after block D01.1) ----
        "app_datum" => {
            let d = datum_in(&a[3]);
            st.chks.get_mut(&ci(1)).unwrap().append_datum(ci(2), &d);
        }
        "get_datum" => {
            let ft = if a[4].is_null() {
                st.fts[&ci(1)][ci(3)].clone()
            } else {
                ft_in(&a[4])
            };
            return datum_out(&st.chks[&ci(1)].get_row(ci(2)).get_datum(ci(3), &ft));
        }
        "datum_row" => {
            let ds = st.chks[&ci(1)]
                .get_row(ci(2))
                .get_datum_row(&st.fts[&ci(1)]);
            return datum_outs(&ds);
        }
        "datum_row_buf" => {
            let mut buf: Vec<Datum> = a[3].as_array().unwrap().iter().map(datum_in).collect();
            let ds = st.chks[&ci(1)]
                .get_row(ci(2))
                .get_datum_row_with_buffer(&st.fts[&ci(1)], &mut buf);
            return datum_outs(ds);
        }
        "to_string" => {
            let c = &st.chks[&ci(1)];
            let rows: Vec<usize> = (0..c.num_rows()).map(|i| c.get_row(i).idx()).collect();
            let all: Vec<usize> = (0..c.num_cols()).collect();
            if str_past_cap(c, &opt_fts(&st.fts[&ci(1)]), &rows, &all, true) {
                return json!({ "past_cap": true });
            }
            return go_string(&c.to_string(&st.fts[&ci(1)]));
        }
        "row_string" => {
            let r = st.chks[&ci(1)].get_row(ci(2));
            let all: Vec<usize> = (0..st.chks[&ci(1)].num_cols()).collect();
            if str_past_cap(
                &st.chks[&ci(1)],
                &opt_fts(&st.fts[&ci(1)]),
                &[r.idx()],
                &all,
                true,
            ) {
                return json!({ "past_cap": true });
            }
            return go_string(&r.to_string(&st.fts[&ci(1)]));
        }
        "cmp_datum" => {
            let d = datum_in(&a[4]);
            return json!(compare::compare(st.chks[&ci(1)].get_row(ci(2)), ci(3), &d));
        }
        "lower_bound" => {
            let d = datum_in(&a[3]);
            let (i, m) = st.chks[&ci(1)].lower_bound(ci(2), &d);
            return json!([i, m]);
        }
        "upper_bound" => {
            let d = datum_in(&a[3]);
            return json!(st.chks[&ci(1)].upper_bound(ci(2), &d));
        }
        // ---- MutRow ----
        "mr_values" => {
            let vals: Vec<GoValue> = a[2].as_array().unwrap().iter().map(go_value_in).collect();
            st.mrs.insert(ci(1), mutrow::mut_row_from_values(&vals));
        }
        "mr_datums" => {
            let ds: Vec<Datum> = a[2].as_array().unwrap().iter().map(datum_in).collect();
            st.mrs.insert(ci(1), mutrow::mut_row_from_datums(&ds));
        }
        "mr_types" => {
            st.mrs
                .insert(ci(1), mutrow::mut_row_from_types(&fts_of(&a[2])));
        }
        "mr_set_row" => {
            let row = st.chks[&ci(2)].get_row(ci(3));
            st.mrs.get_mut(&ci(1)).unwrap().set_row(row);
        }
        "mr_set_values" => {
            let vals: Vec<GoValue> = a[2].as_array().unwrap().iter().map(go_value_in).collect();
            for (i, v) in vals.iter().enumerate() {
                if set_value_past_cap(&st.mrs[&ci(1)], i, v) {
                    return json!({ "past_cap": true });
                }
            }
            st.mrs.get_mut(&ci(1)).unwrap().set_values(&vals);
        }
        "mr_set_value" => {
            let v = go_value_in(&a[3]);
            if set_value_past_cap(&st.mrs[&ci(1)], ci(2), &v) {
                return json!({ "past_cap": true });
            }
            st.mrs.get_mut(&ci(1)).unwrap().set_value(ci(2), &v);
        }
        "mr_set_datums" => {
            let ds: Vec<Datum> = a[2].as_array().unwrap().iter().map(datum_in).collect();
            st.mrs.get_mut(&ci(1)).unwrap().set_datums(&ds);
        }
        "mr_set_datum" => {
            let d = datum_in(&a[3]);
            st.mrs.get_mut(&ci(1)).unwrap().set_datum(ci(2), &d);
        }
        "mr_shallow" => {
            let row = st.chks[&ci(3)].get_row(ci(4));
            st.mrs
                .get_mut(&ci(1))
                .unwrap()
                .shallow_copy_partial_row(ci(2), row);
        }
        "mr_clone" => {
            let m = st.mrs[&ci(2)].clone_row();
            st.mrs.insert(ci(1), m);
        }
        "mr_dump" => {
            let mr = &st.mrs[&ci(1)];
            let len = mr.len();
            return json!([len, dump(mr.to_row().chunk())]);
        }
        "mr_get" => {
            let r = st.mrs[&ci(1)].to_row();
            let fts = fts_of(&a[3]);
            let mut full: Vec<Option<FieldType>> = vec![None; r.len()];
            full[ci(2)] = Some(fts[0].clone());
            if str_past_cap(r.chunk(), &full, &[r.idx()], &[ci(2)], false) {
                return json!({ "past_cap": true });
            }
            return json!([
                datum_out(&r.get_datum(ci(2), &fts[0])),
                hex(r.get_raw(ci(2))),
                r.is_null(ci(2))
            ]);
        }
        "mr_string" => {
            let r = st.mrs[&ci(1)].to_row();
            let fts = fts_of(&a[2]);
            let all: Vec<usize> = (0..r.chunk().num_cols()).collect();
            if str_past_cap(r.chunk(), &opt_fts(&fts), &[r.idx()], &all, true) {
                return json!({ "past_cap": true });
            }
            return go_string(&r.to_string(&fts));
        }
        _ => panic!("unknown step {op}"),
    }
    Value::Null
}

/// govec's chunkReadSize.
fn read_size(ft: &FieldType, str: bool) -> usize {
    match ft.get_type() {
        mysql::TypeFloat => 4,
        mysql::TypeNewDecimal => 40,
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear
        | mysql::TypeDouble
        | mysql::TypeDate
        | mysql::TypeDatetime
        | mysql::TypeTimestamp
        | mysql::TypeDuration => 8,
        mysql::TypeBit if str => 8,
        _ => 0,
    }
}

/// govec's chunkStrPastCap: a typed read past the data's capacity (Go: undefined).
fn str_past_cap(
    c: &Chunk,
    fts: &[Option<FieldType>],
    rows: &[usize],
    cols: &[usize],
    str: bool,
) -> bool {
    for &idx in rows {
        for &col in cols {
            let cc = c.column(col);
            let size = fts[col].as_ref().map_or(0, |ft| read_size(ft, str));
            if cc.is_null(idx) || size == 0 {
                continue;
            }
            let s = cc.govec_state();
            let off = idx * size;
            if off >= s.data.0.len() {
                return false;
            }
            if off + size > s.data.1 {
                return true;
            }
        }
    }
    false
}

/// govec's chunkSetValuePastCap.
fn set_value_past_cap(mr: &MutRow, col: usize, v: &GoValue) -> bool {
    let size = match v {
        GoValue::Duration(_) | GoValue::Time(_) => 8,
        GoValue::Decimal(_) => tidb_types::MY_DECIMAL_STRUCT_SIZE,
        _ => return false,
    };
    let r = mr.to_row();
    let s = r.chunk().column(col).govec_state();
    !s.data.0.is_empty() && s.data.1 < size
}

fn opt_fts(fts: &[FieldType]) -> Vec<Option<FieldType>> {
    fts.iter().cloned().map(Some).collect()
}

// ---- datum2 / goValueIn forms of the tracker (as in tidb-types' govec_datum.rs) ----

fn bits_f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
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
        dm::KindFloat32 => d.set_float32(bits_f(&p[1]) as f32),
        dm::KindFloat64 => d.set_float64(bits_f(&p[1])),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => d.set_mysql_decimal(dec_of(p[1].as_str().unwrap())),
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
    m.insert("coll".into(), go_string(d.collation().as_bytes()));
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
                json!({ "raw": t.raw().0, "s": go_string(&t.string()) }),
            );
        }
        _ => {}
    }
    Value::Object(m)
}

fn datum_outs(ds: &[Datum]) -> Value {
    Value::Array(ds.iter().map(datum_out).collect())
}

fn ft_in(v: &Value) -> FieldType {
    let p = v.as_array().unwrap();
    let mut ft = new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p();
    if !p[6].is_null() {
        ft.set_elems(Some(p[6].as_array().unwrap().iter().map(ungs).collect()));
    }
    ft
}

fn go_value_in(v: &Value) -> GoValue {
    let p = v.as_array().unwrap();
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    match p[0].as_str().unwrap() {
        "nil" => GoValue::Nil,
        "bool" => GoValue::Bool(p[1].as_bool().unwrap()),
        "int" => GoValue::Int(i(1)),
        "int64" => GoValue::Int64(i(1)),
        "uint64" => GoValue::Uint64(u(1)),
        "float32" => GoValue::Float32(bits_f(&p[1]) as f32),
        "float64" => GoValue::Float64(bits_f(&p[1])),
        "string" => GoValue::String(ungs(&p[1])),
        "bytes" => GoValue::Bytes(hex_opt(&p[1])),
        "decimal" => GoValue::Decimal(Box::new(dec_of(p[1].as_str().unwrap()))),
        "duration" => GoValue::Duration(Duration {
            duration: i(1),
            fsp: i(2) as isize,
        }),
        "enum" => GoValue::Enum(Enum {
            value: u(1),
            name: ungs(&p[2]),
        }),
        "set" => GoValue::Set(Set {
            value: u(1),
            name: ungs(&p[2]),
        }),
        "binlit" => GoValue::BinaryLiteral(BinaryLiteral(unhex(p[1].as_str().unwrap()))),
        "json" => GoValue::BinaryJSON(BinaryJSON::owned(u(1) as u8, unhex(p[2].as_str().unwrap()))),
        "time" => GoValue::Time(Time::from_raw(CoreTime(u(1)))),
        "vector" => {
            GoValue::VectorFloat32(parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap())
        }
        t => panic!("go_value_in {t}"),
    }
}

fn run_script(steps: &[Value]) -> Vec<Value> {
    let mut st = State::default();
    let mut out = Vec::new();
    for s in steps {
        match catch_unwind(AssertUnwindSafe(|| step(&mut st, s.as_array().unwrap()))) {
            Ok(v) => out.push(v),
            Err(e) => {
                if std::env::var("CHUNK_DEBUG").is_ok() {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                    eprintln!("panic at {s}: {msg:?}");
                    if let Some(id) = s.get(1).and_then(|v| v.as_u64())
                        && let Some(c) = st.chks.get(&(id as usize))
                    {
                        eprintln!("  chunk {id}: {}", dump(c));
                    }
                    let prev: Vec<&Value> =
                        steps.iter().take_while(|x| !std::ptr::eq(*x, s)).collect();
                    eprintln!("  steps: {}", serde_json::to_string(&prev).unwrap());
                }
                out.push(json!({ "panic": "" }));
                return out;
            }
        }
    }
    out
}

/// Go's panic message is not compared: `{"panic": <anything>}` matches `{"panic": ""}`.
fn normalize(mut v: Value) -> Value {
    if let Value::Array(a) = &mut v {
        for x in a {
            if let Value::Object(m) = x
                && m.len() == 1
                && m.contains_key("panic")
            {
                m.insert("panic".into(), json!(""));
            }
        }
    }
    v
}

/// The committed vectors, or the files named in `GOVEC_CHUNK_FILES` (comma separated): extra
/// seeds generated with `govec chunk -seed N -o f.jsonl.gz` (tracker CI does this for a few
/// seeds; such files are never committed).
#[test]
fn govec_chunk() {
    match std::env::var("GOVEC_CHUNK_FILES") {
        Ok(files) => {
            for f in files.split(',').filter(|f| !f.is_empty()) {
                replay_file(f);
            }
        }
        Err(_) => replay_file(&format!(
            "{}/tests/vectors/v8.5.8/{GO_TARGET}/chunk.jsonl.gz",
            env!("CARGO_MANIFEST_DIR")
        )),
    }
}

fn replay_file(path: &str) {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "chunk");
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let (mut n, mut bad) = (0, 0);
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let got = match rec["op"].as_str().unwrap() {
            "script" => json!(run_script(rec["args"][0].as_array().unwrap())),
            "type_width" => json!(
                fts_of(&rec["args"][0])
                    .iter()
                    .map(|ft| json!([codec::get_fixed_len_pub(ft), codec::estimate_type_width(ft)]))
                    .collect::<Vec<_>>()
            ),
            op => panic!("unknown op {op}"),
        };
        n += 1;
        if normalize(got.clone()) != normalize(rec["out"].clone()) {
            bad += 1;
            if bad <= 3 {
                // Locate the first differing step.
                let (g, w) = (got.as_array(), rec["out"].as_array());
                let mut at = String::new();
                if let (Some(g), Some(w)) = (g, w) {
                    for i in 0..g.len().max(w.len()) {
                        let (gi, wi) = (
                            g.get(i).cloned().map(normalize_one),
                            w.get(i).cloned().map(normalize_one),
                        );
                        if gi != wi {
                            at = format!(
                                "step {i} {}\n got  {}\n want {}",
                                rec["args"][0][i],
                                gi.unwrap_or(Value::Null),
                                wi.unwrap_or(Value::Null)
                            );
                            break;
                        }
                    }
                }
                eprintln!("MISMATCH {} {at}", rec["src"]);
            }
        }
    }
    std::panic::set_hook(hook);
    assert_eq!(n, header["records"].as_i64().unwrap());
    assert_eq!(bad, 0, "{path}: {bad} of {n} records differ");
}

fn normalize_one(v: Value) -> Value {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("panic") => json!({ "panic": "" }),
        v => v,
    }
}
