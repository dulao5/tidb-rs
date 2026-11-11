//! Replays the govec `meta` vectors (tidb-rs-tracker `govec/domains/meta.go`): block D06.2
//! part 1 — the read side of `pkg/structure` and `pkg/meta` (`meta.Reader`), and
//! `pkg/meta/metadef` — against Go TiDB v8.5.8.
//!
//! A `struct` / `meta` record holds a stored state (built by Go's writers, plus injected
//! corruptions) and a list of read calls; the state is loaded into [`MemSnap`], an
//! in-memory snapshot with the `kv.Retriever` semantics of Go's (the same map the Go
//! generator replays on), and each call's result and error are compared.
//!
//! `GOVEC_META_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use futures::executor::block_on;
use serde_json::{Map, Value, json};
use tidb_base::Error;
use tidb_infoschema::meta::{self, Element, MustLoadFilterAttr, Mutator};
use tidb_infoschema::metadef;
use tidb_infoschema::structure::{
    self, HashPair, TxStructure, new_hash_reverse_iter, new_hash_reverse_iter_begin_with_field,
};
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, Iterator as KvIterator, KvFuture, OptionValue,
    Retriever, Snapshot, ValueEntry, new_value_entry,
};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/meta.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- in-memory snapshot

/// The sorted key-value pairs of a state, with Go's `kv.Retriever` semantics (the govec
/// `memSnap`): `Iter(k, upper)` from the first key >= k, below upper (empty: unbounded);
/// `IterReverse(k, lower)` from the last key < k (empty k: the last key), down to lower.
struct MemSnap {
    keys: Vec<Vec<u8>>,
    vals: Vec<Vec<u8>>,
}

impl MemSnap {
    fn new(pairs: &Value) -> MemSnap {
        let mut s = MemSnap {
            keys: vec![],
            vals: vec![],
        };
        for p in pairs.as_array().into_iter().flatten() {
            s.keys.push(hx(&p[0]));
            s.vals.push(hx(&p[1]));
        }
        s
    }
    fn lower_bound(&self, k: &[u8]) -> usize {
        self.keys.partition_point(|x| x.as_slice() < k)
    }
}

impl Getter for MemSnap {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        let i = self.lower_bound(k);
        let r = if i < self.keys.len() && self.keys[i] == k.0 {
            Ok(new_value_entry(self.vals[i].clone(), 0))
        } else {
            Err(ErrNotExist.clone())
        };
        Box::pin(async move { r })
    }
}

struct MemIter<'a> {
    s: &'a MemSnap,
    i: isize,
    step: isize,
    bound: Vec<u8>,
    key: Key,
}

impl MemIter<'_> {
    fn in_range(&self) -> bool {
        if self.i < 0 || self.i as usize >= self.s.keys.len() {
            return false;
        }
        let k = &self.s.keys[self.i as usize];
        if self.bound.is_empty() {
            return true;
        }
        if self.step > 0 {
            k.as_slice() < self.bound.as_slice()
        } else {
            k.as_slice() >= self.bound.as_slice()
        }
    }
    fn load(&mut self) {
        self.key = if self.in_range() {
            Key(self.s.keys[self.i as usize].clone())
        } else {
            Key(vec![])
        };
    }
}

impl KvIterator for MemIter<'_> {
    fn valid(&self) -> bool {
        self.in_range()
    }
    fn key(&self) -> &Key {
        &self.key
    }
    fn value(&self) -> &[u8] {
        if self.in_range() {
            &self.s.vals[self.i as usize]
        } else {
            &[]
        }
    }
    fn next(&mut self) -> KvFuture<'_, ()> {
        self.i += self.step;
        self.load();
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) {}
}

impl Retriever for MemSnap {
    fn iter<'a>(&'a self, k: &'a Key, upper: &'a Key) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        let mut it = MemIter {
            s: self,
            i: self.lower_bound(k) as isize,
            step: 1,
            bound: upper.0.clone(),
            key: Key(vec![]),
        };
        it.load();
        Box::pin(async move { Ok(Box::new(it) as Box<dyn KvIterator + 'a>) })
    }
    fn iter_reverse<'a>(
        &'a self,
        k: &'a Key,
        lower: &'a Key,
    ) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        let i = if k.0.is_empty() {
            self.keys.len() as isize - 1
        } else {
            self.lower_bound(k) as isize - 1
        };
        let mut it = MemIter {
            s: self,
            i,
            step: -1,
            bound: lower.0.clone(),
            key: Key(vec![]),
        };
        it.load();
        Box::pin(async move { Ok(Box::new(it) as Box<dyn KvIterator + 'a>) })
    }
}

impl BatchGetter for MemSnap {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!("meta reads do not batch-get")
    }
}

impl Snapshot for MemSnap {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

// ---------------------------------------------------------------- helpers

fn hx(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![],
        Value::String(s) => decode_hex(s),
        o => panic!("hex {o}"),
    }
}

fn decode_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn enc_hex(b: &[u8]) -> Value {
    Value::String(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn i(v: &Value) -> i64 {
    v.as_i64()
        .unwrap_or_else(|| v.as_u64().map(|u| u as i64).unwrap_or(0))
}

/// `vec.GoString`: a JSON string if valid UTF-8, else `{"hex": ...}`.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => json!({ "hex": enc_hex(b) }),
    }
}

fn err_json(e: &Error) -> Value {
    json!({
        "code": if e.is_normalized() { e.code() as i64 } else { 0 },
        "msg": gs(&e.error()),
    })
}

/// One `metaRes`: `{"r": ...}` (omitted when null) and `{"e": ...}`.
fn res(r: Value, err: Result<(), Error>) -> Value {
    let mut m = Map::new();
    if !r.is_null() {
        m.insert("r".into(), r);
    }
    if let Err(e) = err {
        m.insert("e".into(), err_json(&e));
    }
    Value::Object(m)
}

fn hex_opt(v: Option<Vec<u8>>) -> Value {
    v.map_or(Value::Null, |b| enc_hex(&b))
}

fn pairs(ps: &[HashPair]) -> Value {
    Value::Array(
        ps.iter()
            .map(|p| json!([enc_hex(&p.field), enc_hex(&p.value)]))
            .collect(),
    )
}

/// `errStop` of the generator.
fn err_stop() -> Error {
    tidb_base::errors::new("stop iteration")
}

fn stop_at(n: &Value) -> i64 {
    i(n)
}

fn split<T>(r: Result<T, Error>) -> (Option<T>, Result<(), Error>) {
    match r {
        Ok(v) => (Some(v), Ok(())),
        Err(e) => (None, Err(e)),
    }
}

// ---------------------------------------------------------------- struct

fn struct_call(t: &TxStructure, c: &[Value]) -> Value {
    let name = c[0].as_str().unwrap();
    block_on(async {
        match name {
            "get" => {
                let (v, e) = split(t.get(&hx(&c[1])).await);
                res(hex_opt(v.flatten()), e)
            }
            "getint64" => {
                let (n, e) = t.get_int64(&hx(&c[1])).await;
                res(json!(n), e)
            }
            "iterate" => {
                let stop = stop_at(&c[3]);
                let mut out = vec![];
                let e = t
                    .iterate(&hx(&c[1]), &hx(&c[2]), |k, v| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(json!([enc_hex(k), enc_hex(v)]));
                        Ok(())
                    })
                    .await;
                res(Value::Array(out), e)
            }
            "hget" => {
                let (v, e) = split(t.h_get(&hx(&c[1]), &hx(&c[2])).await);
                res(hex_opt(v.flatten()), e)
            }
            "hgetint64" => {
                let (n, e) = t.h_get_int64(&hx(&c[1]), &hx(&c[2])).await;
                res(json!(n), e)
            }
            "hkeys" => {
                let (ks, e) = t.h_keys(&hx(&c[1])).await;
                res(Value::Array(ks.iter().map(|k| enc_hex(k)).collect()), e)
            }
            "hgetall" => {
                let (ps, e) = t.h_get_all(&hx(&c[1])).await;
                res(pairs(&ps), e)
            }
            "hgetiter" => {
                let stop = stop_at(&c[2]);
                let mut out = vec![];
                let e = t
                    .h_get_iter(&hx(&c[1]), |p| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(p);
                        Ok(())
                    })
                    .await;
                res(pairs(&out), e)
            }
            "hgetlen" => {
                let (n, e) = t.h_get_len(&hx(&c[1])).await;
                res(json!(n), e)
            }
            "hgetlastn" => {
                let (ps, e) = t.h_get_last_n(&hx(&c[1]), i(&c[2]) as usize).await;
                res(pairs(&ps), e)
            }
            "iteratehash" => {
                let stop = stop_at(&c[2]);
                let mut out = vec![];
                let e = t
                    .iterate_hash(&hx(&c[1]), |f, v| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(json!([enc_hex(f), enc_hex(v)]));
                        Ok(())
                    })
                    .await;
                res(Value::Array(out), e)
            }
            "iterbounded" => {
                let stop = stop_at(&c[3]);
                let mut out = vec![];
                let e = t
                    .iterate_hash_with_bounded_key(&hx(&c[1]), &hx(&c[2]), |k, f, v| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(json!([enc_hex(k), enc_hex(f), enc_hex(v)]));
                        Ok(())
                    })
                    .await;
                res(Value::Array(out), e)
            }
            "hrev" => {
                let key = hx(&c[1]);
                let it = if c[2].is_null() {
                    new_hash_reverse_iter(t, &key).await
                } else {
                    new_hash_reverse_iter_begin_with_field(t, &key, &hx(&c[2])).await
                };
                let mut it = match it {
                    Ok(it) => it,
                    Err(e) => return res(Value::Null, Err(e)),
                };
                let mut out = vec![];
                for _ in 0..i(&c[3]) {
                    if !it.valid() {
                        out.push(Value::Null);
                        break;
                    }
                    out.push(json!([enc_hex(it.key()), enc_hex(it.value())]));
                    if let Err(e) = it.next().await {
                        return res(Value::Array(out), Err(e));
                    }
                }
                it.close();
                res(Value::Array(out), Ok(()))
            }
            "lgetall" => {
                let (v, e) = split(t.l_get_all(&hx(&c[1])).await);
                let list = v.flatten().unwrap_or_default();
                res(Value::Array(list.iter().map(|b| enc_hex(b)).collect()), e)
            }
            "llen" => {
                let (n, e) = t.l_len(&hx(&c[1])).await;
                res(json!(n), e)
            }
            "lindex" => {
                let (v, e) = split(t.l_index(&hx(&c[1]), i(&c[2])).await);
                res(hex_opt(v.flatten()), e)
            }
            "encstr" => res(enc_hex(&t.encode_string_data_key(&hx(&c[1]))), Ok(())),
            "enchashmeta" => res(enc_hex(&t.encode_hash_meta_key(&hx(&c[1]))), Ok(())),
            "enchashdata" => res(
                enc_hex(&t.encode_hash_data_key(&hx(&c[1]), &hx(&c[2]))),
                Ok(()),
            ),
            o => panic!("struct call {o}"),
        }
    })
}

// ---------------------------------------------------------------- meta

fn js(r: Result<String, Error>) -> Value {
    Value::String(r.expect("marshal"))
}

fn tbl(t: &tidb_model::TableInfo) -> Value {
    json!({ "j": js(t.marshal_json()), "db": t.dbid })
}

fn float_bits(f: f64) -> Value {
    json!(format!("{:016x}", f.to_bits()))
}

fn meta_call(m: &Mutator, c: &[Value]) -> Value {
    let name = c[0].as_str().unwrap();
    block_on(async {
        match name {
            "globalid" => {
                let (n, e) = m.get_global_id().await;
                res(json!(n), e)
            }
            "policyid" => {
                let (n, e) = m.get_policy_id().await;
                res(json!(n), e)
            }
            "globalidkey" => res(enc_hex(&m.global_id_key()), Ok(())),
            "histkey" => res(enc_hex(&meta::ddl_job_history_key(m, i(&c[1]))), Ok(())),
            "schemaversion" => {
                let (n, e) = m.get_schema_version().await;
                res(json!(n), e)
            }
            "schemaversionnonempty" => match m.get_schema_version_with_non_empty_diff().await {
                Ok(n) => res(json!(n), Ok(())),
                Err(e) => res(json!(0), Err(e)),
            },
            "diffkey" => res(enc_hex(&m.encode_schema_diff_key(i(&c[1]))), Ok(())),
            "schemadiff" => {
                let (d, e) = m.get_schema_diff(i(&c[1])).await;
                res(d.map_or(Value::Null, |d| js(d.marshal_json())), e)
            }
            "bdr" => match m.get_bdr_role().await {
                Ok(r) => res(gs(&r), Ok(())),
                Err(e) => res(json!(""), Err(e)),
            },
            "ddltablever" => {
                let (v, e) = m.check_ddl_table_version().await;
                res(json!(v), e)
            }
            "systemdbid" => match m.get_system_db_id().await {
                Ok(n) => res(json!(n), Ok(())),
                Err(e) => res(json!(0), Err(e)),
            },
            "mdl" => {
                let (en, is_null, e) = m.get_metadata_lock().await;
                res(json!([en, is_null]), e)
            }
            "schemacachesize" => {
                let (s, is_null, e) = m.get_schema_cache_size().await;
                res(json!([s, is_null]), e)
            }
            "iterdbs" => {
                let stop = stop_at(&c[1]);
                let mut out = vec![];
                let e = m
                    .iter_databases(|db| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(js(db.marshal_json()));
                        Ok(())
                    })
                    .await;
                res(Value::Array(out), e)
            }
            "itertables" => {
                let stop = stop_at(&c[2]);
                let mut out = vec![];
                let e = m
                    .iter_tables(i(&c[1]), |t| {
                        if stop >= 0 && out.len() as i64 == stop {
                            return Err(err_stop());
                        }
                        out.push(tbl(&t));
                        Ok(())
                    })
                    .await;
                res(Value::Array(out), e)
            }
            "metasbydb" => {
                let (ps, e) = split(m.get_metas_by_db_id(i(&c[1])).await);
                res(pairs(&ps.unwrap_or_default()), e)
            }
            "nametoid" => {
                let (names, tbls, e) = m
                    .get_all_name_to_id_and_the_must_loaded_table_info(i(&c[1]))
                    .await;
                let ns: Vec<Value> = names
                    .iter()
                    .map(|(n, id)| json!({ "n": gs(n), "i": id }))
                    .collect();
                let ts: Vec<Value> = tbls.iter().map(tbl).collect();
                res(json!([ns, ts]), e)
            }
            "tableswithattrs" => {
                let attrs: Vec<MustLoadFilterAttr> = c[2]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| MustLoadFilterAttr {
                        attr: a["Attr"].as_str().unwrap().to_string(),
                        load_if_missing: a["LoadIfMissing"].as_bool().unwrap(),
                    })
                    .collect();
                let (ts, e) = meta::get_table_info_with_attributes(m, i(&c[1]), &attrs).await;
                res(Value::Array(ts.iter().map(tbl).collect()), e)
            }
            "listtables" => {
                let (ts, e) = split(m.list_tables(i(&c[1])).await);
                res(
                    Value::Array(ts.unwrap_or_default().iter().map(tbl).collect()),
                    e,
                )
            }
            "listdbs" => {
                let (dbs, e) = split(m.list_databases().await);
                res(
                    Value::Array(
                        dbs.unwrap_or_default()
                            .iter()
                            .map(|d| js(d.marshal_json()))
                            .collect(),
                    ),
                    e,
                )
            }
            "getdb" => {
                let (d, e) = m.get_database(i(&c[1])).await;
                res(d.map_or(Value::Null, |d| js(d.marshal_json())), e)
            }
            "listpolicies" => {
                let (ps, e) = split(m.list_policies().await);
                res(
                    Value::Array(
                        ps.unwrap_or_default()
                            .iter()
                            .map(|p| js(p.marshal_json()))
                            .collect(),
                    ),
                    e,
                )
            }
            "getpolicy" => {
                let (p, e) = m.get_policy(i(&c[1])).await;
                res(p.map_or(Value::Null, |p| js(p.marshal_json())), e)
            }
            "listrgs" => {
                let (gs_, e) = split(m.list_resource_groups().await);
                res(
                    Value::Array(
                        gs_.unwrap_or_default()
                            .iter()
                            .map(|g| js(g.marshal_json()))
                            .collect(),
                    ),
                    e,
                )
            }
            "getrg" => {
                let (g, e) = m.get_resource_group(i(&c[1])).await;
                res(g.map_or(Value::Null, |g| js(g.marshal_json())), e)
            }
            "gettable" => {
                let (t, e) = m.get_table(i(&c[1]), i(&c[2])).await;
                if e.is_err() {
                    return res(Value::Null, e);
                }
                res(t.as_ref().map_or(Value::Null, tbl), Ok(()))
            }
            "tableexists" => match m.check_table_exists(i(&c[1]), i(&c[2])).await {
                Ok(b) => res(json!(b), Ok(())),
                Err(e) => res(json!(false), Err(e)),
            },
            "alljobs" => {
                let key = c[1].as_str().unwrap();
                let r = if key.is_empty() {
                    m.get_all_ddl_jobs_in_queue(None).await
                } else {
                    m.get_all_ddl_jobs_in_queue(Some(key.as_bytes())).await
                };
                let (jobs, e) = split(r);
                res(
                    Value::Array(
                        jobs.unwrap_or_default()
                            .iter()
                            .map(|j| js(j.marshal_json()))
                            .collect(),
                    ),
                    e,
                )
            }
            "histjob" => {
                let (j, e) = m.get_history_ddl_job(i(&c[1])).await;
                res(j.map_or(Value::Null, |j| js(j.marshal_json())), e)
            }
            "histcount" => {
                let (n, e) = m.get_history_ddl_count().await;
                res(json!(n), e)
            }
            "ingest" => {
                let (a, an, e) = m.get_ingest_max_batch_split_ranges().await;
                if e.is_err() {
                    return res(Value::Null, e);
                }
                let (b, bn, e) = m.get_ingest_max_split_ranges_per_sec().await;
                if e.is_err() {
                    return res(Value::Null, e);
                }
                let (cc, cn, e) = m.get_ingest_max_inflight().await;
                if e.is_err() {
                    return res(Value::Null, e);
                }
                let (d, dn, e) = m.get_ingest_max_per_sec().await;
                res(
                    json!([a, an, float_bits(b), bn, cc, cn, float_bits(d), dn]),
                    e,
                )
            }
            "lastjobs" | "histiter" => {
                let (it, rounds_arg) = if name == "lastjobs" {
                    (m.get_last_history_ddl_jobs_iterator().await, &c[1])
                } else {
                    (m.get_history_ddl_jobs_iterator(i(&c[1])).await, &c[2])
                };
                let mut it = match it {
                    Ok(it) => it,
                    Err(e) => return res(Value::Null, Err(e)),
                };
                let mut rounds = vec![];
                for n in rounds_arg.as_array().unwrap() {
                    match it.get_last_jobs(i(n) as usize).await {
                        Ok(jobs) => rounds.push(Value::Array(
                            jobs.iter().map(|j| js(j.marshal_json())).collect(),
                        )),
                        Err(e) => return res(Value::Array(rounds), Err(e)),
                    }
                }
                res(Value::Array(rounds), Ok(()))
            }
            "bootstrap" => {
                let (n, e) = m.get_bootstrap_version().await;
                res(json!(n), e)
            }
            "autoids" => {
                let r#as = m.get_auto_id_accessors(i(&c[1]), i(&c[2]));
                let (g, e) = r#as.get().await;
                if e.is_err() {
                    return res(Value::Null, e);
                }
                let mut out = vec![g.row_id, g.increment_id, g.random_id];
                for a in [
                    r#as.row_id(),
                    r#as.increment_id(i(&c[3]) as u16),
                    r#as.random_id(),
                    r#as.sequence_value(),
                    r#as.sequence_cycle(),
                ] {
                    let (v, e) = a.get().await;
                    if e.is_err() {
                        return res(json!(out), e);
                    }
                    out.push(v);
                }
                res(json!(out), Ok(()))
            }
            o => panic!("meta call {o}"),
        }
    })
}

// ---------------------------------------------------------------- keys / metadef

/// A `keys` record: the out and the error as vec.Record has them.
fn keys_call(c: &[Value]) -> (Value, Result<(), Error>) {
    let name = c[0].as_str().unwrap();
    let id = || i(&c[1]);
    let b = || hx(&c[1]);
    let parsed = |(n, e): (i64, Result<(), Error>)| (json!(n), e);
    match name {
        "DBkey" => (gs(&meta::db_key_of(id())), Ok(())),
        "AutoTableIDKey" => (gs(&meta::auto_table_id_key(id())), Ok(())),
        "AutoIncrementIDKey" => (gs(&meta::auto_increment_id_key(id())), Ok(())),
        "AutoRandomTableIDKey" => (gs(&meta::auto_random_table_id_key(id())), Ok(())),
        "TableKey" => (gs(&meta::table_key_of(id())), Ok(())),
        "SequenceKey" => (gs(&meta::sequence_key_of(id())), Ok(())),
        "IsDBkey" => (json!(meta::is_db_key(&b())), Ok(())),
        "IsAutoTableIDKey" => (json!(meta::is_auto_table_id_key(&b())), Ok(())),
        "IsAutoIncrementIDKey" => (json!(meta::is_auto_increment_id_key(&b())), Ok(())),
        "IsAutoRandomTableIDKey" => (json!(meta::is_auto_random_table_id_key(&b())), Ok(())),
        "IsTableKey" => (json!(meta::is_table_key(&b())), Ok(())),
        "IsSequenceKey" => (json!(meta::is_sequence_key(&b())), Ok(())),
        "ParseDBKey" => parsed(meta::parse_db_key(&b())),
        "ParseAutoTableIDKey" => parsed(meta::parse_auto_table_id_key(&b())),
        "ParseAutoIncrementIDKey" => parsed(meta::parse_auto_increment_id_key(&b())),
        "ParseAutoRandomTableIDKey" => parsed(meta::parse_auto_random_table_id_key(&b())),
        "ParseTableKey" => parsed(meta::parse_table_key(&b())),
        "ParseSequenceKey" => parsed(meta::parse_sequence_key(&b())),
        "IsTableInfoMustLoad" => (json!(meta::is_table_info_must_load(&b())), Ok(())),
        "Unescape" => (gs(&meta::unescape(&b())), Ok(())),
        "Element" => {
            let e = Element {
                id: i(&c[1]),
                type_key: hx(&c[2]),
            };
            (
                json!([gs(&e.string()), enc_hex(&e.encode_element())]),
                Ok(()),
            )
        }
        "DecodeElement" => match meta::decode_element(&b()) {
            Ok(e) => (json!([e.id, enc_hex(&e.type_key), gs(&e.string())]), Ok(())),
            Err(e) => (Value::Null, Err(e)),
        },
        o => panic!("keys {o}"),
    }
}

fn metadef_call(f: &str, s: &str) -> bool {
    match f {
        "IsMemOrSysDB" => metadef::is_mem_or_sys_db(s),
        "IsMemDB" => metadef::is_mem_db(s),
        "IsSystemRelatedDB" => metadef::is_system_related_db(s),
        "IsSystemDB" => metadef::is_system_db(s),
        "IsBRRelatedDB" => metadef::is_br_related_db(s),
        o => panic!("metadef {o}"),
    }
}

// ---------------------------------------------------------------- replay

/// The record's (out, err) as Rust computes them; err is vec.Err without the name.
fn exec(op: &str, args: &[Value]) -> (Value, Value) {
    match op {
        "struct" => {
            let snap: Arc<dyn Retriever> = Arc::new(MemSnap::new(&args[1]));
            let t = structure::new_structure(snap, &hx(&args[0]));
            let out: Vec<Value> = args[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| struct_call(&t, c.as_array().unwrap()))
                .collect();
            (Value::Array(out), Value::Null)
        }
        "meta" => {
            let snap: Arc<dyn Snapshot> = Arc::new(MemSnap::new(&args[0]));
            let m = meta::new_reader(snap);
            let out: Vec<Value> = args[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| meta_call(&m, c.as_array().unwrap()))
                .collect();
            (Value::Array(out), Value::Null)
        }
        "keys" => {
            let (out, err) = keys_call(args[0].as_array().unwrap());
            let err = match err {
                Ok(()) => Value::Null,
                Err(e) => {
                    let mut m = Map::new();
                    m.insert("name".into(), json!("other"));
                    if e.is_normalized() && e.code() != 0 {
                        m.insert("code".into(), json!(e.code() as i64));
                    }
                    m.insert("msg".into(), gs(&e.error()));
                    Value::Object(m)
                }
            };
            (out, err)
        }
        "metadef" => (
            json!(metadef_call(
                args[0].as_str().unwrap(),
                args[1].as_str().unwrap()
            )),
            Value::Null,
        ),
        o => panic!("unknown op {o}"),
    }
}

/// The first differing call of a record (the whole record is too long to print).
fn describe(op: &str, args: &[Value], rec: &Value, got: &Value, err: &Value) -> String {
    let calls = match op {
        "struct" => args[2].as_array(),
        "meta" => args[1].as_array(),
        _ => None,
    };
    if let (Some(calls), Some(g), Some(w)) = (calls, got.as_array(), rec["out"].as_array()) {
        for (k, c) in calls.iter().enumerate() {
            if g.get(k) != w.get(k) {
                return format!(
                    "{op} src={} call {c}\n    go   {}\n    rust {}",
                    rec["src"],
                    w.get(k).unwrap_or(&Value::Null),
                    g.get(k).unwrap_or(&Value::Null)
                );
            }
        }
    }
    format!(
        "{op} {args:?}\n    go   {} / {}\n    rust {got} / {err}",
        rec["out"], rec["err"]
    )
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "meta");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap().to_string();
        let got = catch_unwind(AssertUnwindSafe(|| exec(&op, &args)));
        let go_panicked = rec["err"]["name"] == "panic";
        match got {
            Err(e) if go_panicked => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                if Value::String(msg.clone()) != rec["err"]["msg"] {
                    fails.push(format!("{line}\n    rust panicked with {msg:?}"));
                }
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                fails.push(format!("{line}\n    rust panicked: {msg:?}"));
            }
            Ok((got, _)) if go_panicked => fails.push(format!("{line}\n    rust returned {got}")),
            Ok((got, err)) => {
                if got != rec["out"] || err != rec["err"] {
                    fails.push(describe(&op, &args, &rec, &got, &err));
                }
            }
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails
        .iter()
        .take(20)
        .map(|s| s.chars().take(4000).collect::<String>())
        .collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_meta() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_META_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
