//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/kv.jsonl.gz` (tracker
//! `govec`, domain `kv`: Go TiDB v8.5.8 `pkg/kv`, block D07.0) against the Rust port.
//!
//! Every record's `out` must be equal as JSON, errors must match by message, and
//! `err.name == "panic"` records must panic in Rust too. The mocks (storage, transaction,
//! snapshot, iterator) are the Go generator's (`govec/domains/kv.go`), written again here.
//! Extra files can be replayed with `GOVEC_KV_FILES=a.jsonl.gz:b.jsonl.gz` (other seeds).

use std::any::Any;
use std::collections::HashMap;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_kv::Context;
use tidb_kv::cachedb::new_cache_db;
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::context::RequestSource;
use tidb_kv::context::with_internal_source_type;
use tidb_kv::error::ErrLockExpire;
use tidb_kv::error::ErrNotExist;
use tidb_kv::error::ErrTxnRetryable;
use tidb_kv::error::ErrWriteConflict;
use tidb_kv::error::ErrWriteConflictInTiDB;
use tidb_kv::error::is_err_not_found;
use tidb_kv::error::is_txn_retryable_error;
use tidb_kv::key::CommonHandle;
use tidb_kv::key::Handle;
use tidb_kv::key::IntHandle;
use tidb_kv::key::Key;
use tidb_kv::key::KeyRange;
use tidb_kv::key::key_range_slice_mem_usage;
use tidb_kv::key::new_handle_map;
use tidb_kv::key::new_mem_aware_handle_map;
use tidb_kv::key::new_partition_handle;
use tidb_kv::kv::BatchGetOption;
use tidb_kv::kv::BatchGetter;
use tidb_kv::kv::EmptyIterator;
use tidb_kv::kv::EmptyRetriever;
use tidb_kv::kv::GetOption;
use tidb_kv::kv::Getter;
use tidb_kv::kv::Iterator as KvIterator;
use tidb_kv::kv::KeyRanges;
use tidb_kv::kv::KvFuture;
use tidb_kv::kv::OptionValue;
use tidb_kv::kv::Retriever;
use tidb_kv::kv::Snapshot;
use tidb_kv::kv::Storage;
use tidb_kv::kv::Transaction;
use tidb_kv::kv::TxnOption;
use tidb_kv::kv::ValueEntry;
use tidb_kv::kv::batch_get_to_get_options;
use tidb_kv::kv::batch_get_value;
use tidb_kv::kv::get_value;
use tidb_kv::kv::new_non_paritioned_key_ranges_with_hint;
use tidb_kv::kv::new_non_partitioned_key_ranges;
use tidb_kv::kv::new_partitioned_key_ranges;
use tidb_kv::kv::new_partitioned_key_ranges_with_hints;
use tidb_kv::kv::new_value_entry;
use tidb_kv::mpp::get_newest_mpp_version;
use tidb_kv::option::ReplicaReadType;
use tidb_kv::option::ResourceGroupName;
use tidb_kv::option::get_internal_source_type;
use tidb_kv::txn::MaxRetryCnt;
use tidb_kv::txn::back_off;
use tidb_kv::txn::get_min_inner_txn_start_ts;
use tidb_kv::txn::run_in_new_txn;
use tidb_kv::txn::set_txn_resource_group;
use tidb_kv::txn_scope_var::new_default_txn_scope_var;
use tidb_kv::txn_scope_var::new_global_txn_scope_var;
use tidb_kv::txn_scope_var::new_local_txn_scope_var;
use tidb_kv::version::new_version;

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

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_owned()),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn key_of(s: &str) -> Key {
    let b = unhex(s);
    let mut v = Vec::with_capacity(b.len());
    v.extend_from_slice(&b);
    Key(v)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

// ---------------------------------------------------------------- handles

/// `vec.MustJSON(specOf(h))`: field order t, v, enc, offs, pid, h; zero values omitted
/// (`omitempty`).
fn spec_json(h: &Handle) -> String {
    match h {
        Handle::Int(x) => {
            if x.0 == 0 {
                r#"{"t":"int"}"#.to_owned()
            } else {
                format!(r#"{{"t":"int","v":{}}}"#, x.0)
            }
        }
        Handle::Common(c) => {
            let mut offs = vec![];
            let mut end = 0u16;
            for i in 0..c.num_cols() {
                end += c.encoded_col(i).len() as u16;
                offs.push(end.to_string());
            }
            let mut s = r#"{"t":"common""#.to_owned();
            if !c.encoded().is_empty() {
                s += &format!(r#","enc":"{}""#, hex(c.encoded()));
            }
            if !offs.is_empty() {
                s += &format!(r#","offs":[{}]"#, offs.join(","));
            }
            s + "}"
        }
        Handle::Partition(p) => {
            let mut s = r#"{"t":"part""#.to_owned();
            if p.partition_id != 0 {
                s += &format!(r#","pid":{}"#, p.partition_id);
            }
            s + &format!(r#","h":{}}}"#, spec_json(&p.handle))
        }
    }
}

fn spec_val(h: &Handle) -> Value {
    serde_json::from_str(&spec_json(h)).unwrap()
}

fn handle_of(v: &Value) -> Handle {
    match v["t"].as_str().unwrap() {
        "int" => Handle::Int(IntHandle(v["v"].as_i64().unwrap_or(0))),
        "common" => {
            let enc = unhex(v["enc"].as_str().unwrap_or(""));
            let offs: Vec<u16> = v["offs"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_u64().unwrap() as u16).collect())
                .unwrap_or_default();
            Handle::from(CommonHandle::new_with_offsets(enc, offs))
        }
        "part" => Handle::Partition(new_partition_handle(
            v["pid"].as_i64().unwrap_or(0),
            handle_of(&v["h"]),
        )),
        t => panic!("bad spec {t}"),
    }
}

fn has_common(h: &Handle) -> bool {
    match h {
        Handle::Common(_) => true,
        Handle::Partition(p) => has_common(&p.handle),
        Handle::Int(_) => false,
    }
}

fn handle_out(h: &Handle) -> Value {
    let mut out = json!({
        "spec": spec_val(h),
        "is_int": h.is_int(),
        "len": h.len(),
        "enc": hex(&h.encoded()),
        "mem": h.mem_usage(),
        "extra": h.extra_mem_size(),
    });
    let m = out.as_object_mut().unwrap();
    if h.is_int() {
        m.insert("int".into(), json!(h.int_value()));
    } else {
        m.insert("ncols".into(), json!(h.num_cols()));
        let cols: Vec<String> = (0..h.num_cols()).map(|i| hex(&h.encoded_col(i))).collect();
        m.insert("cols".into(), json!(cols));
    }
    if !has_common(h) {
        m.insert("str".into(), json!(h.string()));
        let d = h.data().unwrap();
        let ds: Vec<i64> = d.iter().map(|x| x.get_int64()).collect();
        m.insert("data".into(), json!(ds));
    }
    out
}

fn panic_msg(p: Box<dyn Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

// ---------------------------------------------------------------- mocks

fn err_by_name(n: &str) -> Option<Error> {
    match n {
        "" => None,
        "retryable" => Some(ErrTxnRetryable.fast_gen_by_args(vec![])),
        "write_conflict" => Some(
            ErrWriteConflict.fast_gen_by_args(
                ["1", "2", "3", "4", "5", "6", "7"]
                    .iter()
                    .map(|s| (*s).into())
                    .collect(),
            ),
        ),
        "write_conflict_tidb" => Some(ErrWriteConflictInTiDB.fast_gen_by_args(vec!["x".into()])),
        "not_exist" => Some(ErrNotExist.clone()),
        "lock_expire" => Some(ErrLockExpire.fast_gen_by_args(vec![])),
        other => Some(tidb_base::errors::new(other.to_owned())),
    }
}

#[derive(Default)]
struct TxnLog {
    ts: u64,
    rb: bool,
    commit: bool,
    opts: Vec<(isize, String)>,
}

struct MockTxn {
    log: Arc<Mutex<TxnLog>>,
    commit_err: Option<Error>,
    opts: HashMap<isize, OptionValue>,
}

fn any_str(v: &(dyn Any + Send + Sync)) -> String {
    if let Some(b) = v.downcast_ref::<bool>() {
        return b.to_string();
    }
    if let Some(s) = v.downcast_ref::<String>() {
        return s.clone();
    }
    panic!("unexpected option value type")
}

impl Getter for MockTxn {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        unimplemented!()
    }
}
impl Retriever for MockTxn {
    fn iter<'a>(&'a self, _: &'a Key, _: &'a Key) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a>(
        &'a self,
        _: &'a Key,
        _: &'a Key,
    ) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MockTxn {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Transaction for MockTxn {
    fn size(&self) -> isize {
        0
    }
    fn len(&self) -> isize {
        0
    }
    fn commit<'a>(&'a mut self, _: &'a Context) -> KvFuture<'a, ()> {
        self.log.lock().unwrap().commit = true;
        let e = self.commit_err.clone();
        Box::pin(async move { e.map_or(Ok(()), Err) })
    }
    fn rollback(&mut self) -> Result<(), Error> {
        self.log.lock().unwrap().rb = true;
        Ok(())
    }
    fn string(&self) -> String {
        String::new()
    }
    fn set_option(&mut self, opt: isize, val: OptionValue) {
        let mut log = self.log.lock().unwrap();
        log.opts.retain(|(k, _)| *k != opt);
        log.opts.push((opt, any_str(&*val)));
        self.opts.insert(opt, val);
    }
    fn get_option(&self, opt: isize) -> Option<&(dyn Any + Send + Sync)> {
        self.opts.get(&opt).map(|v| &**v)
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn start_ts(&self) -> u64 {
        self.log.lock().unwrap().ts
    }
    fn valid(&self) -> bool {
        true
    }
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        unimplemented!()
    }
    fn is_pessimistic(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
struct Step {
    f: String,
    commit: String,
    begin: String,
}

struct MockStore {
    plan: Vec<Step>,
    state: Mutex<StoreState>,
}

#[derive(Default)]
struct StoreState {
    next_ts: u64,
    attempt: usize,
    begins: usize,
    txns: Vec<Arc<Mutex<TxnLog>>>,
}

impl Storage for MockStore {
    fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        let mut st = self.state.lock().unwrap();
        st.begins += 1;
        let step = self.plan[st.attempt.min(self.plan.len() - 1)].clone();
        if let Some(e) = err_by_name(&step.begin) {
            return Box::pin(async move { Err(e) });
        }
        st.next_ts += 1 << 18;
        let log = Arc::new(Mutex::new(TxnLog {
            ts: st.next_ts,
            ..Default::default()
        }));
        st.txns.push(Arc::clone(&log));
        let txn = MockTxn {
            log,
            commit_err: err_by_name(&step.commit),
            opts: HashMap::new(),
        };
        Box::pin(async move { Ok(Box::new(txn) as Box<dyn Transaction>) })
    }
    fn get_snapshot(&self, _: tidb_kv::version::Version) -> Arc<dyn Snapshot> {
        unimplemented!()
    }
    fn get_client(&self) -> Arc<dyn tidb_kv::kv::Client> {
        unimplemented!()
    }
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn uuid(&self) -> String {
        String::new()
    }
    fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, tidb_kv::version::Version> {
        unimplemented!()
    }
    fn name(&self) -> String {
        String::new()
    }
    fn describe(&self) -> String {
        String::new()
    }
    fn get_mem_cache(&self) -> Arc<dyn tidb_kv::cachedb::MemManager> {
        unimplemented!()
    }
    fn get_min_safe_ts(&self, _: &str) -> u64 {
        0
    }
}

struct MockSnap {
    vals: HashMap<Vec<u8>, Vec<u8>>,
    fail_batch: bool,
    calls: Mutex<usize>,
}

impl Getter for MockSnap {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        *self.calls.lock().unwrap() += 1;
        let r = match self.vals.get(&k.0) {
            Some(v) => Ok(new_value_entry(v.clone(), 7)),
            None => Err(ErrNotExist.clone()),
        };
        Box::pin(async move { r })
    }
}
impl Retriever for MockSnap {
    fn iter<'a>(&'a self, _: &'a Key, _: &'a Key) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a>(
        &'a self,
        _: &'a Key,
        _: &'a Key,
    ) -> KvFuture<'a, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MockSnap {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        keys: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        *self.calls.lock().unwrap() += 1;
        if self.fail_batch {
            return Box::pin(async { Err(tidb_base::errors::new("batch get failed")) });
        }
        let mut out = HashMap::new();
        for k in keys {
            if let Some(v) = self.vals.get(&k.0) {
                out.insert(k.0.clone(), new_value_entry(v.clone(), 7));
            }
        }
        Box::pin(async move { Ok(out) })
    }
}
impl Snapshot for MockSnap {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

struct MockIter {
    keys: Vec<Key>,
    pos: usize,
    fail_at: i64,
}

static NIL: Key = Key(Vec::new());

impl KvIterator for MockIter {
    fn valid(&self) -> bool {
        self.pos < self.keys.len()
    }
    fn key(&self) -> &Key {
        self.keys.get(self.pos).unwrap_or(&NIL)
    }
    fn value(&self) -> &[u8] {
        &[]
    }
    fn next(&mut self) -> KvFuture<'_, ()> {
        if self.pos as i64 == self.fail_at {
            return Box::pin(async { Err(tidb_base::errors::new("next failed")) });
        }
        self.pos += 1;
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) {}
}

// ---------------------------------------------------------------- ops

fn ranges_of(v: &Value) -> Vec<KeyRange> {
    let a = v.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let mut out = Vec::with_capacity(a.len());
    for r in a {
        out.push(KeyRange {
            start_key: key_of(r[0].as_str().unwrap()),
            end_key: key_of(r[1].as_str().unwrap()),
        });
    }
    out
}

fn ranges_out(rs: &[KeyRange]) -> Value {
    json!(
        rs.iter()
            .map(|r| vec![hex(&r.start_key), hex(&r.end_key)])
            .collect::<Vec<_>>()
    )
}

fn cmp_of(name: &str) -> impl FnMut(&KeyRange, &KeyRange) -> isize + use<> {
    let name = name.to_owned();
    move |i: &KeyRange, j: &KeyRange| match name.as_str() {
        "start" => i.start_key.cmp(&j.start_key),
        "end_desc" => j.end_key.cmp(&i.end_key),
        "len" => i.start_key.len() as isize - j.start_key.len() as isize,
        _ => {
            let a = i.start_key.first().map_or(-1, |b| *b as isize);
            let b = j.start_key.first().map_or(-1, |b| *b as isize);
            a - b
        }
    }
}

fn null_if_empty(v: Value) -> Value {
    if v.as_array().is_some_and(|a| a.is_empty()) {
        Value::Null
    } else {
        v
    }
}

fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>) {
    let s = |i: usize| args[i].as_str().unwrap().to_owned();
    match op {
        "key" => {
            let k = key_of(&s(0));
            let (n, p, c) = (k.next(), k.prefix_next(), k.clone_key());
            (
                json!({"next": hex(&n), "next_cap": n.0.capacity(), "pnext": hex(&p), "pnext_cap": p.0.capacity(),
                    "clone": hex(&c), "clone_cap": c.0.capacity(), "str": k.string()}),
                None,
            )
        }
        "key2" => {
            let (a, b) = (key_of(&s(0)), key_of(&s(1)));
            (json!({"cmp": a.cmp(&b), "prefix": a.has_prefix(&b)}), None)
        }
        "key_range_mem" => (json!(key_range_slice_mem_usage(&ranges_of(&args[0]))), None),
        "handle" => {
            let h = handle_of(&args[0]);
            let c = h.copy();
            (
                json!({"h": handle_out(&h), "next": handle_out(&h.next()), "copy": handle_out(&c)}),
                None,
            )
        }
        "handle_call" => {
            let h = handle_of(&args[1]);
            match s(0).as_str() {
                "num_cols" => (json!(h.num_cols()), None),
                "encoded_col" => (
                    json!(hex(&h.encoded_col(args[2].as_i64().unwrap() as isize))),
                    None,
                ),
                _ => (json!(h.int_value()), None),
            }
        }
        "handle2" => {
            let (a, b) = (handle_of(&args[0]), handle_of(&args[1]));
            let mut res = json!({"eq": a.equal(&b)});
            match catch_unwind(AssertUnwindSafe(|| a.compare(&b))) {
                Ok(c) => res["cmp"] = json!(c),
                Err(p) => res["cmp_panic"] = json!(panic_msg(p)),
            }
            (res, None)
        }
        "handle_map" => {
            let mut m = new_handle_map::<i64>();
            let mut outs = vec![];
            for st in args[0].as_array().unwrap() {
                let h = handle_of(&st["h"]);
                match st["op"].as_str().unwrap() {
                    "set" => m.set(&h, st["v"].as_i64().unwrap_or(0)),
                    "del" => m.delete(&h),
                    _ => {
                        let v = m.get(&h);
                        outs.push(json!([v, v.is_some()]));
                    }
                }
                outs.push(json!([m.len(), m.mem_usage()]));
            }
            let mut all = vec![];
            m.range(|h, v| {
                all.push(format!("{}={}", spec_json(h), v));
                true
            });
            all.sort();
            let mut stop = 0;
            m.range(|_, _| {
                stop += 1;
                false
            });
            (
                json!({"steps": outs, "range": null_if_empty(json!(all)), "stop": stop}),
                None,
            )
        }
        "mem_aware_map" => {
            let mut outs = vec![];
            let mut all = vec![];
            if s(0) == "i64" {
                let mut m = new_mem_aware_handle_map::<i64>();
                for st in args[1].as_array().unwrap() {
                    let h = handle_of(&st["h"]);
                    if st["op"] == "set" {
                        outs.push(json!(m.set(&h, st["v"].as_i64().unwrap_or(0))));
                    } else {
                        let v = m.get(&h);
                        outs.push(json!([v.copied().unwrap_or(0), v.is_some()]));
                    }
                }
                m.range(|h, v| {
                    all.push(format!("{}={}", spec_json(h), v));
                    true
                });
            } else {
                let mut m = new_mem_aware_handle_map::<bool>();
                for st in args[1].as_array().unwrap() {
                    let h = handle_of(&st["h"]);
                    if st["op"] == "set" {
                        outs.push(json!(m.set(&h, st["v"].as_i64().unwrap_or(0) % 2 == 1)));
                    } else {
                        let v = m.get(&h);
                        outs.push(json!([v.copied().unwrap_or(false), v.is_some()]));
                    }
                }
                m.range(|h, v| {
                    all.push(format!("{}={}", spec_json(h), v));
                    true
                });
            }
            all.sort();
            (
                json!({"steps": outs, "range": null_if_empty(json!(all))}),
                None,
            )
        }
        "key_ranges" => {
            let mode = s(0);
            let parts: Vec<Vec<KeyRange>> = args[1]
                .as_array()
                .map_or(vec![], |a| a.iter().map(ranges_of).collect());
            let with_hints = args[2].as_bool().unwrap();
            let hints: Vec<Option<Vec<isize>>> = args[3].as_array().map_or(vec![], |a| {
                a.iter()
                    .map(|h| {
                        h.as_array()
                            .map(|x| x.iter().map(|n| n.as_i64().unwrap() as isize).collect())
                    })
                    .collect()
            });
            let first = parts.first().cloned().unwrap_or_default();
            let mut rr: KeyRanges = if mode == "part" && with_hints {
                new_partitioned_key_ranges_with_hints(parts, hints)
            } else if mode == "part" {
                new_partitioned_key_ranges(parts)
            } else if with_hints {
                new_non_paritioned_key_ranges_with_hint(first, hints.into_iter().next().flatten())
            } else {
                new_non_partitioned_key_ranges(first)
            };
            let mut res = json!({"num": rr.partition_num(), "first": ranges_out(rr.first_partition_range()),
                "sorted": rr.is_fully_sorted(),
                "append": ranges_out(&rr.append_self_to(vec![KeyRange { start_key: Key(b"x".to_vec()), end_key: Key::default() }]))});
            let fail_at = args[5].as_i64().unwrap() as usize;
            let mut visits = vec![];
            let ferr = rr.for_each_partition_with_err(|r, h| {
                visits.push(json!([ranges_out(r), h, h.is_none()]));
                if visits.len() == fail_at {
                    return Err(tidb_base::errors::new("stop"));
                }
                Ok(())
            });
            res["visits"] = null_if_empty(json!(visits));
            res["visit_err"] = json!(ferr.is_err());
            rr.sort_by_func(cmp_of(&s(4)));
            let mut after = vec![];
            let _ = rr.for_each_partition_with_err(|r, _| {
                after.push(ranges_out(r));
                Ok(())
            });
            res["sorted_ranges"] = null_if_empty(json!(after));
            res["sorted2"] = json!(rr.is_fully_sorted());
            (res, rr.set_to_non_partitioned().err())
        }
        "req_supported" => {
            (
                json!(RequestTypeSupportedChecker.is_request_type_supported(
                    args[0].as_i64().unwrap(),
                    args[1].as_i64().unwrap()
                )),
                None,
            )
        }
        "err_check" => {
            let n = s(0);
            let e = match n.as_str() {
                "wrapped_retryable" => Some(tidb_base::errors::new(format!(
                    "wrap: {}",
                    String::from_utf8_lossy(&ErrTxnRetryable.error())
                ))),
                "wrapped_not_exist" => Some(ErrNotExist.gen_with_stack_by_args(vec![])),
                _ => err_by_name(&n),
            };
            let msg = e.as_ref().map(|e| e.error()).unwrap_or_default();
            let nf = e.as_ref().is_some_and(is_err_not_found);
            (
                json!({"retry": is_txn_retryable_error(e.as_ref()), "not_found": nf, "msg": gs(&msg)}),
                None,
            )
        }
        "replica_read" => {
            let r = ReplicaReadType(args[0].as_i64().unwrap() as u8);
            (json!([r.is_follower_read(), r.is_closest_read()]), None)
        }
        "source_type" => {
            let mut ctx = Context::background();
            if !s(0).is_empty() {
                ctx = with_internal_source_type(&ctx, &s(0));
            }
            (json!(get_internal_source_type(&ctx)), None)
        }
        "txn_scope" => {
            let v = match s(0).as_str() {
                "global" => new_global_txn_scope_var(),
                "local" => new_local_txn_scope_var(&s(1)),
                _ => new_default_txn_scope_var(),
            };
            (json!([v.get_var_value(), v.get_txn_scope()]), None)
        }
        "misc" => match s(0).as_str() {
            "version" => (json!(new_version(args[1].as_u64().unwrap()).ver), None),
            "mpp" => (json!(get_newest_mpp_version().0), None),
            "value_entry" => {
                let e = new_value_entry(b"v".to_vec(), args[1].as_u64().unwrap());
                (json!([hex(&e.value), e.commit_ts]), None)
            }
            _ => {
                let mut opts = vec![];
                if args[1].as_u64() == Some(1) {
                    opts.push(BatchGetOption::ReturnCommitTS);
                }
                (json!(batch_get_to_get_options(&opts).len()), None)
            }
        },
        "back_off" => {
            let a = args[0].as_u64().unwrap() as usize;
            let upper = (100f64).min(2f64.powf(a as f64)) as isize;
            let d = rt().block_on(back_off(a));
            (
                json!({"upper": upper, "in_range": d >= 0 && d < upper * 1_000_000}),
                None,
            )
        }
        "run_in_new_txn" => {
            let plan: Vec<Step> = args[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| Step {
                    f: p["f"].as_str().unwrap().to_owned(),
                    commit: p["c"].as_str().unwrap().to_owned(),
                    begin: p["b"].as_str().unwrap_or("").to_owned(),
                })
                .collect();
            let retryable = args[1].as_bool().unwrap();
            let (source, internal, explicit) = (s(2), args[3].as_bool().unwrap(), s(4));
            let max_retry = args[5].as_u64().unwrap() as usize;
            let lower = args[6].as_u64().unwrap();
            let old = MaxRetryCnt.swap(max_retry, Ordering::SeqCst);
            let store = MockStore {
                plan: plan.clone(),
                state: Mutex::new(StoreState {
                    next_ts: 1 << 18,
                    ..Default::default()
                }),
            };
            let mut ctx = Context::background();
            if !source.is_empty() {
                ctx = ctx.with_request_source(RequestSource {
                    request_source_internal: internal,
                    request_source_type: source,
                    explicit_request_source_type: explicit,
                });
            }
            let mins = Arc::new(Mutex::new(Vec::<u64>::new()));
            let now = UNIX_EPOCH + Duration::from_millis(10);
            let store_ref = &store;
            let mins2 = Arc::clone(&mins);
            let mut f = txn_fn(move |_, _| {
                mins2
                    .lock()
                    .unwrap()
                    .push(get_min_inner_txn_start_ts(now, lower, u64::MAX));
                let mut st = store_ref.state.lock().unwrap();
                let step = plan[st.attempt.min(plan.len() - 1)].clone();
                st.attempt += 1;
                let e = err_by_name(&step.f);
                Box::pin(async move { e.map_or(Ok(()), Err) })
            });
            let rerr = rt()
                .block_on(run_in_new_txn(&ctx, &store, retryable, &mut f))
                .err();
            MaxRetryCnt.store(old, Ordering::SeqCst);
            let st = store.state.lock().unwrap();
            let txns: Vec<Value> = st
                .txns
                .iter()
                .map(|t| {
                    let t = t.lock().unwrap();
                    let opts: serde_json::Map<String, Value> = t
                        .opts
                        .iter()
                        .map(|(k, v)| (k.to_string(), json!(v)))
                        .collect();
                    json!({"ts": t.ts, "rb": t.rb, "commit": t.commit, "opts": opts})
                })
                .collect();
            let after = get_min_inner_txn_start_ts(now, 0, u64::MAX);
            let mins = mins.lock().unwrap().clone();
            (
                json!({"begins": st.begins, "txns": null_if_empty(json!(txns)), "mins": null_if_empty(json!(mins)), "after": after}),
                rerr,
            )
        }
        "set_rg" => {
            let log = Arc::new(Mutex::new(TxnLog::default()));
            let mut t = MockTxn {
                log,
                commit_err: None,
                opts: HashMap::new(),
            };
            set_txn_resource_group(&mut t, &s(0));
            (
                json!(any_str(t.get_option(ResourceGroupName).unwrap())),
                None,
            )
        }
        "cache_db" => {
            let mm = new_cache_db();
            let mut outs = vec![];
            for st in args[0].as_array().unwrap() {
                let tid = st["tid"].as_i64().unwrap_or(0);
                if st["op"] == "delete" {
                    mm.delete(tid);
                    outs.push(json!("deleted"));
                    continue;
                }
                let k = key_of(st["k"].as_str().unwrap_or(""));
                let mut vals = HashMap::new();
                match st["snap"].as_str().unwrap() {
                    "missing" => {}
                    "big" => {
                        vals.insert(k.0.clone(), vec![0u8; st["n"].as_u64().unwrap() as usize]);
                    }
                    v => {
                        vals.insert(k.0.clone(), unhex(v));
                    }
                }
                let snap = MockSnap {
                    vals,
                    fail_batch: false,
                    calls: Mutex::new(0),
                };
                let r = rt().block_on(mm.union_get(&Context::background(), tid, &snap, &k));
                let (v, e) = match r {
                    Ok(v) => (v, String::new()),
                    Err(e) => (vec![], String::from_utf8_lossy(&e.error()).into_owned()),
                };
                let calls = *snap.calls.lock().unwrap();
                outs.push(json!([
                    v.len(),
                    hex(&v[..v.len().min(8)]),
                    calls,
                    gs(e.as_bytes())
                ]));
            }
            (json!(outs), None)
        }
        "empty" => {
            let mut it = EmptyIterator;
            let nerr = rt().block_on(KvIterator::next(&mut it)).unwrap_err();
            it.close();
            let r = EmptyRetriever;
            let ctx = Context::background();
            let k = Key(b"a".to_vec());
            let gerr = rt().block_on(r.get(&ctx, &k, &[])).unwrap_err();
            let nil = Key::default();
            let i1 = rt().block_on(r.iter(&nil, &nil)).unwrap();
            let i2 = rt().block_on(r.iter_reverse(&nil, &nil)).unwrap();
            (
                json!({"valid": it.valid(), "key": it.key().is_empty(), "value": it.value().is_empty(),
                    "next": String::from_utf8_lossy(&nerr.error()), "get_not_found": is_err_not_found(&gerr),
                    "iter": i1.valid(), "iter_rev": i2.valid()}),
                None,
            )
        }
        "next_until" => {
            let keys: Vec<Key> = args[0].as_array().map_or(vec![], |a| {
                a.iter().map(|k| key_of(k.as_str().unwrap())).collect()
            });
            let mode = args[2].as_i64().unwrap();
            let mut it = MockIter {
                keys,
                pos: 0,
                fail_at: args[3].as_i64().unwrap(),
            };
            let mut calls = 0;
            let mut f = |k: &Key| {
                calls += 1;
                match mode {
                    0 => k.is_empty(),
                    1 => k.first() == Some(&0xff),
                    _ => false,
                }
            };
            let err = rt()
                .block_on(tidb_kv::iter::next_until(&mut it, &mut f))
                .err();
            (json!([it.pos, calls]), err)
        }
        "get_value" => {
            let mut vals = HashMap::new();
            if let Some(m) = args[0].as_object() {
                for (k, v) in m {
                    vals.insert(unhex(k), unhex(v.as_str().unwrap()));
                }
            }
            let snap = MockSnap {
                vals,
                fail_batch: args[2].as_bool().unwrap(),
                calls: Mutex::new(0),
            };
            let ctx = Context::background();
            let mut gets = vec![];
            let mut keys = vec![];
            for k in args[1].as_array().unwrap() {
                let key = key_of(k.as_str().unwrap());
                let r = rt().block_on(get_value(&ctx, &snap, &key));
                let (v, nf, nil) = match &r {
                    Ok(v) => (hex(v), false, false),
                    Err(e) => (String::new(), is_err_not_found(e), true),
                };
                gets.push(json!([v, nil, nf]));
                keys.push(key);
            }
            let r = rt().block_on(batch_get_value(&ctx, &snap, &keys));
            let (bv, err) = match r {
                Ok(vals) => {
                    let mut bv: Vec<String> = vals
                        .iter()
                        .map(|(k, v)| format!("{}={}", hex(k), hex(v)))
                        .collect();
                    bv.sort();
                    (bv, None)
                }
                Err(e) => (vec![], Some(e)),
            };
            (
                json!({"gets": gets, "batch": null_if_empty(json!(bv))}),
                err,
            )
        }
        _ => panic!("unknown op {op}"),
    }
}

/// Gives a closure the higher-ranked signature `RunInNewTxn`'s function has.
fn txn_fn<F>(f: F) -> F
where
    F: for<'a> FnMut(
            &'a Context,
            &'a mut dyn Transaction,
        ) -> futures::future::BoxFuture<'a, Result<(), Error>>
        + Send,
{
    f
}

fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
    if want_err["name"] == "panic" {
        return match got {
            Err(_) => None,
            Ok((out, err)) => Some(format!("Go panics, Rust returned {out} / {err:?}")),
        };
    }
    let (out, err) = match got {
        Ok(v) => v,
        Err(p) => return Some(format!("Rust panicked: {}", panic_msg(p))),
    };
    if out != rec["out"] {
        return Some(format!("out: got {out}, want {}", rec["out"]));
    }
    let got_err = err.as_ref().map(|e| gs(&e.error()));
    let want = if want_err.is_null() {
        None
    } else {
        Some(want_err["msg"].clone())
    };
    if got_err != want {
        return Some(format!("err: got {got_err:?}, want {want:?}"));
    }
    None
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "kv");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            let mut l = line.to_owned();
            l.truncate(500);
            let mut why = why;
            why.truncate(1500);
            fails.push(format!("{l}\n    {why}"));
        }
    }
    std::panic::set_hook(prev);
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_kv() {
    replay(&format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/kv.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    ));
    if let Ok(extra) = std::env::var("GOVEC_KV_FILES") {
        for f in extra.split(':').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
