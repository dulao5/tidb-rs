//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/model.jsonl.gz` (tracker
//! `govec`, domain "model", block D06.1) against the Rust port. The ops and their arguments
//! are described in the tracker's `govec/domains/model.go`.
//!
//! The central check is "roundtrip": Go `json.Unmarshal` into a new value then `json.Marshal`
//! must give the same bytes as the Rust decode then encode, for meta exported from a v8.5.8
//! cluster, random values of every struct and mutations of both.
//!
//! Errors: the message must be Go's, except JSON *syntax* errors (`*json.SyntaxError` texts
//! are not ported): there only "an error" is required. `err.name == "panic"` records must
//! panic in Rust as well.

use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tidb_base::Error;
use tidb_base::gostd::json::Value as JValue;
use tidb_model::*;
use tidb_parser::model::CIStr;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A Go string as govec writes it (`vec.GoString`).
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({"hex": b.iter().map(|x| format!("{x:02x}")).collect::<String>()}),
    }
}

fn gs_bytes(v: &Value) -> Vec<u8> {
    match v.as_str() {
        Some(s) => s.as_bytes().to_vec(),
        None => unhex(v["hex"].as_str().unwrap()),
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap()
}

fn emsg(e: &Error) -> String {
    String::from_utf8(e.error()).unwrap()
}

/// `marshalOut`.
fn mo(r: Result<String, Error>) -> Value {
    match r {
        Ok(j) => json!({"json": j, "merr": ""}),
        Err(e) => json!({"json": null, "merr": emsg(&e)}),
    }
}

/// `fmt.Sprint(probes[k])` of a string probe.
fn probe<'a>(p: &'a Value, k: &str) -> &'a str {
    s(&p[k])
}

fn id_of<T>(o: Option<&T>, f: impl Fn(&T) -> i64) -> Value {
    o.map_or(Value::Null, |x| json!(f(x)))
}

/// Runs f; a panic gives the string "panic" (the `recover` blocks of model.go).
fn rec<F: FnOnce() -> Value>(f: F) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!("panic"))
}

/// `t.UnixNano()` (wrapping, as Go's).
fn unix_nano(t: &tidb_base::gostd::time::Time) -> i64 {
    t.unix()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(t.nanosecond())
}

fn table(text: &Value) -> TableInfo {
    let mut t = TableInfo::default();
    t.unmarshal_json(&gs_bytes(text)).expect("bad table text");
    t
}

fn col_names(cols: &[Option<&std::sync::Arc<ColumnInfo>>]) -> Value {
    cols.iter()
        .map(|c| c.map_or(Value::Null, |c| json!(c.name.o)))
        .collect()
}

fn sl<T>(s: &Option<Vec<T>>) -> &[T] {
    s.as_deref().unwrap_or(&[])
}

/// `json.Unmarshal` into a new value of the named type then `json.Marshal`.
macro_rules! typed {
    ($name:expr, $base:expr, $text:expr, [$($t:ident),*]) => {
        match $name {
            $(stringify!($t) => {
                let mut v = $t::default();
                if let Some(b) = $base {
                    v.unmarshal_json(b).expect("bad base");
                }
                let err = v.unmarshal_json($text).err();
                (mo(v.marshal_json()), err.map(|e| emsg(&e)))
            })*
            n => panic!("unknown type {n}"),
        }
    };
}

fn roundtrip(name: &str, base: Option<&[u8]>, text: &[u8]) -> (Value, Option<String>) {
    typed!(
        name,
        base,
        text,
        [
            DBInfo,
            TableInfo,
            ColumnInfo,
            IndexInfo,
            PartitionInfo,
            PartitionDefinition,
            ConstraintInfo,
            FKInfo,
            PolicyInfo,
            PlacementSettings,
            ResourceGroupInfo,
            ResourceGroupSettings,
            SchemaDiff,
            StatsOptions,
            TTLInfo,
            ViewInfo,
            SequenceInfo,
            TiFlashReplicaInfo,
            TableLockInfo,
            StatsLoadItem,
            Job,
            SubJob,
            MultiSchemaInfo,
            DDLReorgMeta,
            HistoryInfo,
            TimeZoneLocation,
            TraceInfo,
            InvolvingSchemaInfo
        ]
    )
}

fn table_out(t: &TableInfo, p: &Value, id: i64) -> Value {
    let mut o = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        o.insert(k.to_owned(), v);
    };
    put("sep_auto_inc", json!(t.sep_auto_inc()));
    put("partition", json!(t.get_partition_info().is_some()));
    put("update_time", json!(unix_nano(&t.get_update_time())));
    put("pk_name", json!(t.get_pk_name().o));
    put("pk_col", id_of(t.get_pk_col_info(), |c| c.id));
    put(
        "auto_inc_col",
        id_of(t.get_auto_increment_col_info(), |c| c.id),
    );
    put("auto_inc_unsigned", json!(t.is_auto_inc_col_unsigned()));
    put("auto_random", json!(t.contains_auto_random_bits()));
    put(
        "auto_random_unsigned",
        rec(|| json!(t.is_auto_random_bit_col_unsigned())),
    );
    put("cols", rec(|| col_names(&t.cols())));
    put(
        "find_index_by_name",
        id_of(t.find_index_by_name(probe(p, "index")), |x| x.id),
    );
    put(
        "find_column_by_id",
        id_of(t.find_column_by_id(id), |c| c.offset as i64),
    );
    put("find_index_by_id", id_of(t.find_index_by_id(id), |x| x.id));
    put(
        "find_public_column",
        rec(|| id_of(t.find_public_column_by_name(probe(p, "col")), |c| c.id)),
    );
    put("is_locked", json!(t.is_locked()));
    put("primary_key", rec(|| id_of(t.get_primary_key(), |x| x.id)));
    let cols = sl(&t.columns);
    if let Some(c0) = cols.first() {
        put("column_is_in_index", json!(t.column_is_in_index(c0)));
        put(
            "gen_changing_column",
            json!(gen_unique_changing_column_name(t, c0)),
        );
    }
    if let Some(i0) = sl(&t.indices).first() {
        put(
            "gen_changing_index",
            json!(gen_unique_changing_index_name(t, i0)),
        );
        put(
            "has_column_in_index",
            rec(|| json!(i0.has_column_in_index_columns(t, id))),
        );
    }
    put("clustered", json!(t.has_clustered_index()));
    put("view", json!(t.is_view()));
    put("sequence", json!(t.is_sequence()));
    put("base", json!(t.is_base_table()));
    put(
        "constraint",
        id_of(
            t.find_constraint_info_by_name(probe(p, "constraint")),
            |c| c.id,
        ),
    );
    put("index_name_by_id", json!(t.find_index_name_by_id(id)));
    put("column_name_by_id", json!(t.find_column_name_by_id(id)));
    put("column_by_id", id_of(t.get_column_by_id(id), |c| c.id));
    let mut names: Vec<&str> = t
        .get_non_temp_columns()
        .iter()
        .map(|c| c.name.o.as_str())
        .collect();
    names.sort();
    put("non_temp", json!(names));
    put(
        "find_column_info",
        id_of(find_column_info(cols, probe(p, "col")), |c| c.id),
    );
    put(
        "find_index_info_by_id",
        id_of(find_index_info_by_id(sl(&t.indices), id), |x| x.id),
    );
    put(
        "find_fk",
        id_of(
            find_fk_info_by_name(sl(&t.foreign_keys), probe(p, "fk")),
            |f| f.id,
        ),
    );
    put("cache_status", json!(t.table_cache_status_type.to_string()));
    put("temp", json!(t.temp_table_type.to_string()));
    put("mode", json!(t.mode.to_string()));
    if let Some(l) = &t.lock {
        put("lock_state", json!(l.state.to_string()));
        let ss: Vec<String> = sl(&l.sessions).iter().map(SessionInfo::to_string).collect();
        put(
            "lock_sessions",
            if ss.is_empty() {
                Value::Null
            } else {
                json!(ss)
            },
        );
    }
    if let Some(r) = &t.tiflash_replica {
        put("tiflash_available", json!(r.is_partition_available(id)));
    }
    put("clone", mo(t.clone().marshal_json()));
    let mut c = t.clone();
    c.clear_placement();
    put("clear_placement", mo(c.marshal_json()));
    if !cols.is_empty() {
        let n = cols.len();
        let from = p["from"].as_f64().unwrap() as usize % n;
        let to = p["to"].as_f64().unwrap() as usize % n;
        let mut m = t.clone();
        put(
            "move",
            rec(move || {
                m.move_column_info(from, to);
                mo(m.marshal_json())
            }),
        );
    }
    Value::Object(o)
}

/// A Go `any` in a record: a top-level string as {"s": GoString} (`dv`), else the value.
fn dv(d: DefaultValueRef<'_>) -> Value {
    match d {
        DefaultValueRef::Bytes(b) => json!({"s": gs(b)}),
        DefaultValueRef::Value(AnyValue::GoString(b)) => json!({"s": gs(b)}),
        DefaultValueRef::Value(AnyValue::Value(JValue::String(x))) => json!({"s": x}),
        DefaultValueRef::Value(AnyValue::Value(v)) => jv(v),
    }
}

fn jv(v: &JValue) -> Value {
    match v {
        JValue::Null => Value::Null,
        JValue::Bool(b) => json!(b),
        JValue::Number(f) => go_number(*f),
        JValue::Int(n) => json!(*n as i64),
        JValue::String(x) => json!(x),
        JValue::Array(a) => a.iter().map(jv).collect(),
        JValue::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), jv(v))).collect()),
    }
}

/// A float64 as it reads back from Go's record: integral values below 1e21 are written
/// without exponent, so serde_json reads them as integers.
fn go_number(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 1e21 {
        if (0.0..18446744073709551616.0).contains(&f) && !(f == 0.0 && f.is_sign_negative()) {
            return json!(f as u64);
        }
        if (-9223372036854775808.0..0.0).contains(&f) {
            return json!(f as i64);
        }
    }
    json!(f)
}

fn column(text: &Value) -> ColumnInfo {
    let mut c = ColumnInfo::default();
    c.unmarshal_json(&gs_bytes(text)).unwrap();
    c
}

fn exec(op: &str, a: &[Value]) -> (Value, Option<String>) {
    match op {
        "roundtrip" => roundtrip(s(&a[0]), None, &gs_bytes(&a[1])),
        "into" => roundtrip(s(&a[0]), Some(&gs_bytes(&a[1])), &gs_bytes(&a[2])),
        "marshal_time" => {
            let loc = tidb_base::gostd::time::fixed_zone("", i(&a[2]) as i32);
            let so = StatsOptions {
                stats_window_settings: Some(Box::new(StatsWindowSettings {
                    window_start: tidb_base::gostd::time::unix(i(&a[0]), i(&a[1])).in_loc(&loc),
                    ..StatsWindowSettings::default()
                })),
                ..StatsOptions::default()
            };
            (mo(so.marshal_json()), None)
        }
        "marshal_nan" => {
            let mut t = table(&a[0]);
            let f = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY][i(&a[1]) as usize];
            tidb_model::make_mut(&mut t.columns.as_mut().unwrap()[0]).default_value =
                AnyValue::Value(JValue::Number(f));
            (mo(t.marshal_json()), None)
        }
        "table" => (table_out(&table(&a[0]), &a[1], i(&a[2])), None),
        "column" => {
            let c = column(&a[0]);
            let out = json!({
                "type": c.get_type(), "flag": c.get_flag(), "flen": c.get_flen(), "decimal": c.get_decimal(),
                "charset": c.get_charset(), "collate": c.get_collate(),
                "elems": c.get_elems().map(|es| es.iter().map(|e| gs(e)).collect::<Vec<_>>()),
                "generated": c.is_generated(), "virtual": c.is_virtual_generated(), "changing": c.is_changing(),
                "removing": c.is_removing(), "removing_origin": c.get_removing_origin_name(),
                "changing_origin": c.get_changing_origin_name(),
                "origin_default": dv(c.get_origin_default_value()), "default": dv(c.get_default_value()),
                "type_desc": c.get_type_desc(), "clone": mo(c.clone().marshal_json()),
            });
            (out, None)
        }
        "column_set" => {
            let mut c = column(&a[0]);
            let mut errs = Vec::new();
            for st in a[1].as_array().unwrap() {
                let st = st.as_array().unwrap();
                let n = || i(&st[1]);
                match s(&st[0]) {
                    "type" => c.set_type(n() as u8),
                    "flag" => c.set_flag(n() as usize),
                    "add" => c.add_flag(n() as usize),
                    "and" => c.and_flag(n() as usize),
                    "toggle" => c.toggle_flag(n() as usize),
                    "del" => c.del_flag(n() as usize),
                    "flen" => c.set_flen(n() as isize),
                    "decimal" => c.set_decimal(n() as isize),
                    "charset" => c.set_charset(s(&st[1])),
                    "collate" => c.set_collate(s(&st[1])),
                    "elems" => c.set_elems(Some(vec![b"a".to_vec(), gs_bytes(&st[1])])),
                    k @ ("origin_default" | "default") => {
                        let v = match s(&st[1]) {
                            "nil" => AnyValue::default(),
                            "str" => AnyValue::GoString(gs_bytes(&st[2])),
                            "int" => AnyValue::Value(JValue::Int(i128::from(i(&st[2])))),
                            "float" => AnyValue::Value(JValue::Number(st[2].as_f64().unwrap())),
                            x => panic!("kind {x}"),
                        };
                        let r = if k == "default" {
                            c.set_default_value(v)
                        } else {
                            c.set_origin_default_value(v)
                        };
                        errs.push(r.err().map_or(Value::Null, |e| gs(&e.error())));
                    }
                    x => panic!("step {x}"),
                }
            }
            let errs = if errs.is_empty() {
                Value::Null
            } else {
                json!(errs)
            };
            (json!({"col": mo(c.marshal_json()), "errs": errs}), None)
        }
        "index" => {
            let t = table(&a[0]);
            let name = s(&a[1]);
            let cols = sl(&t.columns);
            let res: Vec<Value> = sl(&t.indices)
                .iter()
                .map(|idx| {
                    let (i2, ic2) = find_index_column_by_name(sl(&idx.columns), name);
                    let fts: Vec<Value> = sl(&idx.columns)
                        .iter()
                        .filter(|c| c.offset >= 0 && (c.offset as usize) < cols.len())
                        .map(|c| json!(get_idx_changing_field_type(c, &cols[c.offset as usize]).marshal_json()))
                        .collect();
                    json!({
                        "changing": idx.is_changing(), "removing": idx.is_removing(),
                        "removing_origin": idx.get_removing_origin_name(), "changing_origin": idx.get_changing_origin_name(),
                        "prefix": idx.has_prefix_index(), "public": idx.is_public(), "tiflash": idx.is_tiflash_local_index(),
                        "cond": idx.has_condition(), "find_col": idx.find_column_by_name(name).is_some(),
                        "find_col_idx": i2, "find_col2": ic2.is_some(), "clone": mo(IndexInfo::clone(idx).marshal_json()),
                        "changing_fts": if fts.is_empty() { Value::Null } else { json!(fts) },
                    })
                })
                .collect();
            (
                if res.is_empty() {
                    Value::Null
                } else {
                    json!(res)
                },
                None,
            )
        }
        "partition" => {
            let mut pi = PartitionInfo::default();
            pi.unmarshal_json(&gs_bytes(&a[0])).unwrap();
            let (id, name, idx, state) = (i(&a[1]), s(&a[2]), i(&a[3]) as isize, i(&a[4]) as u8);
            let mut o = serde_json::Map::new();
            o.insert("name_by_id".into(), json!(pi.get_name_by_id(id)));
            o.insert("state_by_id".into(), json!(pi.get_state_by_id(id).0));
            o.insert(
                "find_def".into(),
                json!(pi.find_partition_definition_by_name(name)),
            );
            o.insert(
                "id_by_name".into(),
                json!(pi.get_partition_id_by_name(name)),
            );
            o.insert(
                "default_list".into(),
                json!(pi.get_default_list_partition()),
            );
            o.insert(
                "overlapping".into(),
                json!(pi.can_have_overlapping_dropping_partition()),
            );
            o.insert("ids_to_ignore".into(), json!(pi.ids_in_ddl_to_ignore()));
            match catch_unwind(AssertUnwindSafe(|| {
                let ov = pi.get_overlapping_dropping_partition_idx(idx);
                let (i1, e1) = pi.replace_with_overlapping_partition_idx(idx, None);
                let (i2, e2) =
                    pi.replace_with_overlapping_partition_idx(idx, Some(Error::plain("e")));
                (ov, json!([i1, e1.is_some(), i2, e2.is_some()]))
            })) {
                Ok((ov, r)) => {
                    o.insert("overlapping_idx".into(), json!(ov));
                    o.insert("replace".into(), r);
                }
                Err(_) => {
                    o.insert("overlapping_idx".into(), json!("panic"));
                }
            }
            o.insert("dropping".into(), rec(|| json!(pi.is_dropping(idx))));
            let mem: Vec<i64> = sl(&pi.definitions)
                .iter()
                .map(PartitionDefinition::memory_usage)
                .collect();
            o.insert(
                "mem".into(),
                if mem.is_empty() {
                    Value::Null
                } else {
                    json!(mem)
                },
            );
            o.insert("clone".into(), mo(pi.clone().marshal_json()));
            let mut p2 = pi.clone();
            p2.set_state_by_id(id, SchemaState(state));
            o.insert("set_state".into(), mo(p2.marshal_json()));
            p2.gc_partition_states();
            o.insert("gc".into(), mo(p2.marshal_json()));
            p2.set_original_partition_ids();
            p2.clear_reorg_intermediate_info();
            o.insert("clear".into(), mo(p2.marshal_json()));
            (Value::Object(o), None)
        }
        "placement" => {
            let mut p = PolicyInfo::default();
            p.unmarshal_json(&gs_bytes(&a[0])).unwrap();
            let mut o = serde_json::Map::new();
            if let Some(ps) = &p.placement_settings {
                o.insert("string".into(), json!(ps.to_string()));
                o.insert(
                    "clone_settings".into(),
                    mo(ps.as_ref().clone().marshal_json()),
                );
            }
            o.insert("clone".into(), rec(|| mo(p.clone().marshal_json())));
            (Value::Object(o), None)
        }
        "resource" => {
            let mut g = ResourceGroupInfo::default();
            g.unmarshal_json(&gs_bytes(&a[0])).unwrap();
            let mut o = serde_json::Map::new();
            o.insert(
                "new".into(),
                mo(new_resource_group_settings().marshal_json()),
            );
            if let Some(rs) = &g.resource_group_settings {
                o.insert("string".into(), json!(rs.to_string()));
                let mut x = rs.as_ref().clone();
                x.adjust();
                o.insert("adjust".into(), mo(x.marshal_json()));
            }
            o.insert("clone".into(), rec(|| mo(g.clone().marshal_json())));
            (Value::Object(o), None)
        }
        "fk" => {
            let mut fk = FKInfo::default();
            fk.unmarshal_json(&gs_bytes(&a[0])).unwrap();
            (
                json!({"string": fk.to_string(s(&a[1]), s(&a[2])), "clone": mo(fk.clone().marshal_json())}),
                None,
            )
        }
        "misc" => misc(a),
        "job" => job_out(&gs_bytes(&a[0])),
        "job_strings" => {
            let n = i(&a[1]);
            let out = match s(&a[0]) {
                "action" => ActionType(n as u8).to_string().to_owned(),
                "schema_state" => SchemaState(n as u8).to_string().to_owned(),
                "job_state" => JobState(n as i32).to_string().to_owned(),
                "job_version" => JobVersion(n).to_string(),
                "reorg_type" => ReorgType(n as i8).to_string().to_owned(),
                k => panic!("job_strings {k}"),
            };
            (json!(out), None)
        }
        "reorg" => {
            let mut rm = DDLReorgMeta::default();
            rm.unmarshal_json(&gs_bytes(&a[0])).unwrap();
            let d = i(&a[1]) as isize;
            let out = json!({
                "concurrency": rm.get_concurrency_or_default(d),
                "batch_size": rm.get_batch_size_or_default(d),
                "max_write_speed": rm.get_max_write_speed_or_default(),
            });
            (out, None)
        }
        "need_restored" => {
            let t = table(&a[0]);
            let old = tidb_types::collate::new_collation_enabled();
            tidb_types::collate::set_new_collation_enabled_for_test(a[3].as_bool().unwrap());
            let out = rec(|| {
                let ic = &sl(&sl(&t.indices)[i(&a[1]) as usize].columns)[i(&a[2]) as usize];
                json!(column_need_restored_data(ic, sl(&t.columns)))
            });
            tidb_types::collate::set_new_collation_enabled_for_test(old);
            (out, None)
        }
        _ => panic!("unknown op {op}"),
    }
}

/// `jobOut`: Job.Decode, then the job methods; the local start time in Job.String is
/// replaced by "<T>" and given in UTC.
fn job_out(text: &[u8]) -> (Value, Option<String>) {
    let mut job = Job::default();
    if let Err(e) = job.decode(text) {
        return (Value::Null, Some(emsg(&e)));
    }
    let mut out = mo(job.marshal_json());
    let ts = ts_convert2_time(job.start_ts);
    out["string"] = json!(job.to_string().replacen(
        &format!("start time: {},", ts.string()),
        "start time: <T>,",
        1
    ));
    out["start_utc"] = json!(ts.in_loc(&tidb_base::gostd::time::utc()).string());
    out["row_count"] = json!(job.get_row_count());
    out["warnings"] = rec(|| {
        // The two maps, written through a DDLReorgMeta that holds only them.
        let (w, wc) = job.get_warnings();
        let probe = DDLReorgMeta {
            warnings: w.clone(),
            warnings_count: wc.clone(),
            ..DDLReorgMeta::default()
        };
        mo(probe.marshal_json())
    });
    out["may_need_reorg"] = rec(|| json!(job.may_need_reorg()));
    (out, None)
}

fn misc(a: &[Value]) -> (Value, Option<String>) {
    match s(&a[0]) {
        "removing_name" => (json!(gen_removing_obj_name(s(&a[1]))), None),
        "covered" => {
            let t = table(&a[1]);
            let cols: Vec<CIStr> = a[2]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| CIStr::new(s(c)))
                .collect();
            let idxs = sl(&t.indices);
            let cov: Vec<bool> = idxs
                .iter()
                .map(|x| is_index_prefix_covered(&t, x, &cols))
                .collect();
            let cov_fk: Vec<bool> = idxs
                .iter()
                .map(|x| is_index_prefix_covered_for_foreign_key(&t, x, &cols))
                .collect();
            let nul = |v: Vec<bool>| if v.is_empty() { Value::Null } else { json!(v) };
            (
                json!({"cov": nul(cov), "cov_fk": nul(cov_fk),
                    "find": id_of(find_index_by_columns(&t, idxs, &cols), |x| x.id),
                    "find_fk": id_of(find_index_by_columns_for_foreign_key(&t, idxs, &cols), |x| x.id)}),
                None,
            )
        }
        "stats_options" => (mo(new_stats_options().marshal_json()), None),
        "window" => (json!(WindowRepeatType(i(&a[1]) as u8).to_string()), None),
        "job_interval" => {
            let ttl = TTLInfo {
                job_interval: s(&a[1]).to_owned(),
                ..TTLInfo::default()
            };
            match ttl.get_job_interval() {
                Ok(d) => (json!(d), None),
                Err(e) => (json!(0), Some(emsg(&e))),
            }
        }
        "affinity" => match new_table_affinity_info_with_level(s(&a[1])) {
            Err(e) => (Value::Null, Some(emsg(&e))),
            Ok(None) => (Value::Null, None),
            Ok(Some(x)) => (mo(x.clone().marshal_json()), None),
        },
        "item_key" => {
            let it = StatsLoadItem {
                table_item_id: TableItemID {
                    table_id: i(&a[1]),
                    id: i(&a[2]),
                    is_index: a[3].as_bool().unwrap(),
                    ..TableItemID::default()
                },
                full_load: a[4].as_bool().unwrap(),
            };
            (json!([it.table_item_id.key(), it.key()]), None)
        }
        "extra_cols" => (
            json!([
                mo(new_extra_handle_col_info().marshal_json()),
                mo(new_extra_phys_tbl_id_col_info().marshal_json())
            ]),
            None,
        ),
        "ts_time" => (
            json!(unix_nano(&ts_convert2_time(a[1].as_u64().unwrap()))),
            None,
        ),
        "db" => {
            let (mut x, mut y) = (DBInfo::default(), DBInfo::default());
            x.unmarshal_json(&gs_bytes(&a[1])).unwrap();
            y.unmarshal_json(&gs_bytes(&a[2])).unwrap();
            x.deprecated.tables = Some(vec![std::sync::Arc::new(TableInfo {
                id: 7,
                name: CIStr::new("x"),
                ..TableInfo::default()
            })]);
            (
                json!({"clone": mo(x.clone().marshal_json()), "copy": mo(x.copy().marshal_json()),
                    "clone_tables": sl(&x.clone().deprecated.tables).len(), "less": less_db_info(&x, &y)}),
                None,
            )
        }
        m => panic!("unknown misc {m}"),
    }
}

/// The first difference of two record values, for the failure message.
fn diff(path: &str, want: &Value, got: &Value) -> String {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            for k in w.keys().chain(g.keys()) {
                let (a, b) = (
                    w.get(k).unwrap_or(&Value::Null),
                    g.get(k).unwrap_or(&Value::Null),
                );
                if a != b || w.contains_key(k) != g.contains_key(k) {
                    return diff(&format!("{path}.{k}"), a, b);
                }
            }
            String::new()
        }
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
            for (k, (a, b)) in w.iter().zip(g).enumerate() {
                if a != b {
                    return diff(&format!("{path}[{k}]"), a, b);
                }
            }
            String::new()
        }
        (Value::String(w), Value::String(g)) => {
            let n = w.bytes().zip(g.bytes()).take_while(|(a, b)| a == b).count();
            let lo = n.saturating_sub(60);
            let cut = |x: &str| {
                x.get(lo..)
                    .unwrap_or(x)
                    .chars()
                    .take(160)
                    .collect::<String>()
            };
            format!("{path}: go …{} | rust …{}", cut(w), cut(g))
        }
        _ => format!("{path}: go {want} | rust {got}"),
    }
}

/// Documented deviations (spec D06.1 §4), by the id govec writes in `out.known_deviation`.
/// Such a record must differ from Go in `out`, and is counted apart from real mismatches.
const KNOWN_DEVIATIONS: &[(&str, &str)] = &[(
    "nil-ptr-elem",
    "a null element of a []*T decodes as the zero value: the port holds the elements by value (spec D06.1 §4.1)",
)];

/// Go's JSON syntax error texts (`*json.SyntaxError`), not ported.
fn is_syntax_error(msg: &str) -> bool {
    msg.starts_with("invalid character") || msg.starts_with("unexpected end of JSON input")
}

#[test]
fn govec_model() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/model.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "model");
    std::panic::set_hook(Box::new(|_| {}));
    let (mut n, mut nbad, mut bad) = (0, 0, Vec::new());
    let (mut nknown, mut stale) = (0, 0);
    for line in lines {
        n += 1;
        let r: Value = serde_json::from_str(line).unwrap();
        let op = s(&r["op"]);
        let args = r["args"].as_array().unwrap();
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
        let err = &r["err"];
        let ok = match got {
            Err(_) => {
                err["name"] == "panic" || {
                    if bad.len() < 10 {
                        bad.push(format!("{line}\n  rust: panic"));
                    }
                    false
                }
            }
            Ok(_) if err["name"] == "panic" => {
                if bad.len() < 10 {
                    bad.push(format!("{line}\n  rust: no panic"));
                }
                false
            }
            Ok((out, e)) => {
                let want =
                    (!err.is_null()).then(|| String::from_utf8(gs_bytes(&err["msg"])).unwrap());
                let err_ok = match (&want, &e) {
                    (None, None) => true,
                    (Some(w), Some(g)) => w == g || is_syntax_error(w),
                    _ => false,
                };
                // A record marked as a known deviation must differ from Go's `out` and be
                // exactly what the port is specified to write ("ported", the output after the
                // nil []*T elements become zero values); a marked record that matches Go's
                // `out` means the entry is stale.
                let mut want_out = r["out"].clone();
                let (known, ported) = match want_out.as_object_mut() {
                    Some(m) => (
                        m.remove("known_deviation").map(|k| s(&k).to_owned()),
                        m.remove("ported"),
                    ),
                    None => (None, None),
                };
                let same = (want_out.is_null() && e.is_some()) || want_out == out;
                let out_ok = match &known {
                    None => same,
                    Some(k) => {
                        assert!(
                            KNOWN_DEVIATIONS.iter().any(|(id, _)| id == k),
                            "unknown known_deviation {k}"
                        );
                        let ported = ported.expect("known_deviation without ported");
                        if same {
                            stale += 1;
                        } else {
                            nknown += 1;
                        }
                        !same && ported == out
                    }
                };
                if !(err_ok && out_ok)
                    && (bad.len() < 10 || std::env::var_os("GOVEC_ALL").is_some())
                {
                    let src: String = line.chars().take(300).collect();
                    bad.push(format!(
                        "{src}\n  op {op} src {}: err go {want:?} rust {e:?}; out {}",
                        r["src"],
                        diff("", &r["out"], &out)
                    ));
                }
                err_ok && out_ok
            }
        };
        if !ok {
            nbad += 1;
        }
    }
    let _ = std::panic::take_hook();
    eprintln!(
        "govec_model: {n} records, {nknown} known deviations (KNOWN_DEVIATIONS), {nbad} mismatches"
    );
    assert!(
        stale == 0,
        "{stale} records marked as a known deviation no longer differ"
    );
    assert!(
        nbad == 0,
        "{nbad} of {n} records differ, e.g.:\n{}",
        bad.join("\n")
    );
    assert_eq!(n, header["records"].as_u64().unwrap() as usize);
}
