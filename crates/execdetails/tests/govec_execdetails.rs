//! Replays the govec `execdetails` vectors (tidb-rs-tracker `govec/domains/execdetails.go`):
//! block D14.0 — execution details, runtime stats, percentiles, the optimizer trace — and
//! what they embed (client-go detail types, tdigest, Go's `math.Sin` / `math.Asin`) against
//! Go TiDB v8.5.8.
//!
//! `GOVEC_EXECDETAILS_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::gostd::math;
use tidb_base::gostd::strconv::{format_float, parse_float};
use tidb_base::kv::StoreType;
use tidb_execdetails::clientgo::*;
use tidb_execdetails::execdetails::*;
use tidb_execdetails::tdigest::{Centroid, TDigest};
use tidb_execdetails::tracing::opt_trace::*;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/execdetails.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- inputs

fn i(v: &Value) -> i64 {
    v.as_i64()
        .unwrap_or_else(|| v.as_u64().map(|u| u as i64).unwrap_or(0))
}
fn u(v: &Value) -> u64 {
    v.as_u64()
        .unwrap_or_else(|| v.as_i64().map(|x| x as u64).unwrap_or(0))
}
fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}
fn fl(v: &Value) -> f64 {
    parse_float(v.as_str().expect("float string")).expect("float")
}
fn fo(f: f64) -> Value {
    json!(format_float(f, b'g', -1, 64))
}
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}
fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(s).collect())
        .unwrap_or_default()
}

fn time_detail(v: &Value) -> TimeDetail {
    let a = v.as_array().unwrap();
    TimeDetail {
        process_time: i(&a[0]),
        suspend_time: i(&a[1]),
        wait_time: i(&a[2]),
        kv_read_wall_time: i(&a[3]),
        total_rpc_wall_time: i(&a[4]),
    }
}

fn scan(v: &Value) -> Option<ScanDetail> {
    let a = v.as_array()?;
    Some(ScanDetail {
        total_keys: i(&a[0]),
        processed_keys: i(&a[1]),
        processed_keys_size: i(&a[2]),
        rocksdb_delete_skipped_count: u(&a[3]),
        rocksdb_key_skipped_count: u(&a[4]),
        rocksdb_block_cache_hit_count: u(&a[5]),
        rocksdb_block_read_count: u(&a[6]),
        rocksdb_block_read_byte: u(&a[7]),
        rocksdb_block_read_duration: i(&a[8]),
        get_snapshot_duration: i(&a[9]),
    })
}

fn write(v: &Value) -> Option<WriteDetail> {
    let a: Vec<i64> = v.as_array()?.iter().map(i).collect();
    Some(WriteDetail {
        store_batch_wait_duration: a[0],
        propose_send_wait_duration: a[1],
        persist_log_duration: a[2],
        raft_db_write_leader_wait_duration: a[3],
        raft_db_sync_log_duration: a[4],
        raft_db_write_memtable_duration: a[5],
        commit_log_duration: a[6],
        apply_batch_wait_duration: a[7],
        apply_log_duration: a[8],
        apply_mutex_lock_duration: a[9],
        apply_write_leader_wait_duration: a[10],
        apply_write_wal_duration: a[11],
        apply_write_memtable_duration: a[12],
    })
}

fn tikv(v: &Value) -> TiKVExecDetails {
    TiKVExecDetails {
        time_detail: present(v.get("t")).map(|t| Arc::new(time_detail(t))),
        scan_detail: v.get("s").and_then(scan).map(Arc::new),
        write_detail: v.get("w").and_then(write).map(Arc::new),
    }
}

fn req(v: &Value) -> ReqDetailInfo {
    let a = v.as_array().unwrap();
    ReqDetailInfo {
        req_total_time: i(&a[0]),
        region: u(&a[1]),
        store_addr: s(&a[2]),
        exec_details: tikv(&a[3]),
    }
}

fn g<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn commit(v: &Value) -> Option<CommitDetails> {
    if v.is_null() {
        return None;
    }
    let lag = v
        .get("lag")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_else(|| vec![json!(0); 4]);
    let mut c = CommitDetails {
        get_commit_ts_time: i(g(v, "gct")),
        get_latest_ts_time: i(g(v, "glt")),
        lag_details: CommitTSLagDetails {
            wait_time: i(&lag[0]),
            backoff_cnt: i(&lag[1]),
            first_lag_ts: u(&lag[2]),
            wait_until_ts: u(&lag[3]),
        },
        prewrite_time: i(g(v, "pw")),
        wait_prewrite_binlog_time: i(g(v, "wpb")),
        commit_time: i(g(v, "ct")),
        local_latch_time: i(g(v, "llt")),
        write_keys: i(g(v, "wk")),
        write_size: i(g(v, "ws")),
        prewrite_region_num: i(g(v, "prn")) as i32,
        txn_retry: i(g(v, "tr")),
        resolve_lock: ResolveLockDetail {
            resolve_lock_time: i(g(v, "rl")),
        },
        prewrite_req_num: i(g(v, "prq")),
        ..Default::default()
    };
    c.mu.commit_backoff_time = i(g(v, "cbt"));
    c.mu.prewrite_backoff_types = strs(g(v, "pbt"));
    c.mu.commit_backoff_types = strs(g(v, "cbtypes"));
    if let Some(sp) = v.get("sp") {
        c.mu.slowest_prewrite = req(sp);
    }
    if let Some(cp) = v.get("cp") {
        c.mu.commit_primary = req(cp);
    }
    Some(c)
}

fn lock(v: &Value) -> Option<LockKeysDetails> {
    if v.is_null() {
        return None;
    }
    let mut l = LockKeysDetails {
        total_time: i(g(v, "tt")),
        region_num: i(g(v, "rn")) as i32,
        lock_keys: i(g(v, "lk")) as i32,
        aggressive_lock_new_count: i(g(v, "aln")),
        aggressive_lock_derived_count: i(g(v, "ald")),
        locked_with_conflict_count: i(g(v, "lwc")),
        resolve_lock: ResolveLockDetail {
            resolve_lock_time: i(g(v, "rl")),
        },
        backoff_time: i(g(v, "bt")),
        lock_rpc_time: i(g(v, "rpct")),
        lock_rpc_count: i(g(v, "rpcc")),
        retry_count: i(g(v, "rc")),
        ..Default::default()
    };
    l.mu.backoff_types = strs(g(v, "btypes"));
    l.mu.slowest_req_total_time = i(g(v, "st"));
    l.mu.slowest_region = u(g(v, "sr"));
    l.mu.slowest_store_addr = s(g(v, "sa"));
    if let Some(se) = v.get("se") {
        l.mu.slowest_exec_details = tikv(se);
    }
    Some(l)
}

fn backoff(v: &Value) -> (BTreeMap<String, i64>, BTreeMap<String, i64>) {
    let m = |k: &str| -> BTreeMap<String, i64> {
        g(v, k)
            .as_object()
            .map(|o| o.iter().map(|(k, x)| (k.clone(), i(x))).collect())
            .unwrap_or_default()
    };
    (m("bs"), m("bt"))
}

fn exec(v: &Value) -> Option<ExecDetails> {
    if v.is_null() {
        return None;
    }
    let (bs, bt) = backoff(v);
    Some(ExecDetails {
        details_need_p90: DetailsNeedP90 {
            backoff_sleep: bs,
            backoff_times: bt,
            callee_address: s(g(v, "addr")),
            time_detail: v.get("td").map(time_detail).unwrap_or_default(),
        },
        commit_detail: v.get("commit").and_then(commit).map(Box::new),
        lock_keys_detail: v.get("lock").and_then(lock).map(Box::new),
        shared_lock_keys_detail: v.get("slock").and_then(lock).map(Box::new),
        scan_detail: v.get("scan").and_then(scan).map(Box::new),
        cop_time: i(g(v, "cop")),
        backoff_time: i(g(v, "bo")),
        lock_keys_duration: i(g(v, "lkd")),
        request_count: i(g(v, "rc")),
    })
}

fn cop(v: &Value) -> Option<CopExecDetails> {
    if v.is_null() {
        return None;
    }
    let (bs, bt) = backoff(v);
    Some(CopExecDetails {
        scan_detail: scan(g(v, "scan")).unwrap_or_default(),
        time_detail: time_detail(g(v, "td")),
        callee_address: s(g(v, "addr")),
        backoff_time: i(g(v, "bo")),
        backoff_sleep: bs,
        backoff_times: bt,
    })
}

fn summary(v: &Value) -> Option<tipb::ExecutorExecutionSummary> {
    if v.is_null() {
        return None;
    }
    let mut sm = tipb::ExecutorExecutionSummary::default();
    if let Some(x) = present(v.get("tp")) {
        sm.set_time_processed_ns(u(x));
    }
    if let Some(x) = present(v.get("rows")) {
        sm.set_num_produced_rows(u(x));
    }
    if let Some(x) = present(v.get("iters")) {
        sm.set_num_iterations(u(x));
    }
    if let Some(x) = present(v.get("conc")) {
        sm.set_concurrency(u(x));
    }
    if let Some(x) = present(v.get("id")) {
        sm.set_executor_id(s(x));
    }
    if let Some(sc) = present(v.get("scan")) {
        let mut c = tipb::TiFlashScanContext::default();
        let f = |k: &str| sc.get(k).map(u);
        macro_rules! set {
            ($($k:literal => $m:ident),* $(,)?) => { $( if let Some(x) = f($k) { c.$m(x); } )* };
        }
        set!(
            "dds" => set_dmfile_data_scanned_rows, "dsk" => set_dmfile_data_skipped_rows,
            "dms" => set_dmfile_mvcc_scanned_rows, "dmk" => set_dmfile_mvcc_skipped_rows,
            "dls" => set_dmfile_lm_filter_scanned_rows, "dlk" => set_dmfile_lm_filter_skipped_rows,
            "rsc" => set_total_dmfile_rs_check_ms, "rd" => set_total_dmfile_read_ms, "bs" => set_total_build_snapshot_ms,
            "lr" => set_local_regions, "rr" => set_remote_regions, "lrm" => set_total_learner_read_ms,
            "ch" => set_disagg_read_cache_hit_bytes, "cm" => set_disagg_read_cache_miss_bytes,
            "seg" => set_segments, "rt" => set_read_tasks, "dr" => set_delta_rows, "db" => set_delta_bytes,
            "mir" => set_mvcc_input_rows, "mib" => set_mvcc_input_bytes, "mor" => set_mvcc_output_rows,
            "lsr" => set_lm_skip_rows, "bb" => set_total_build_bitmap_ms, "bi" => set_total_build_inputstream_ms,
            "srr" => set_stale_read_regions, "mnl" => set_min_local_stream_ms, "mxl" => set_max_local_stream_ms,
            "mnr" => set_min_remote_stream_ms, "mxr" => set_max_remote_stream_ms,
            "v3" => set_vector_idx_load_from_s3, "vd" => set_vector_idx_load_from_disk,
            "vc" => set_vector_idx_load_from_cache, "vlt" => set_vector_idx_load_time_ms,
            "vst" => set_vector_idx_search_time_ms, "vvn" => set_vector_idx_search_visited_nodes,
            "vdn" => set_vector_idx_search_discarded_nodes, "vrv" => set_vector_idx_read_vec_time_ms,
            "vro" => set_vector_idx_read_others_time_ms,
        );
        if let Some(rs) = sc.get("regions").and_then(|r| r.as_array()) {
            for r in rs {
                let mut inst = tipb::TiFlashRegionNumOfInstance::default();
                inst.set_instance_id(s(&r[0]));
                inst.set_region_num(u(&r[1]));
                c.mut_regions_of_instance().push(inst);
            }
        }
        sm.set_tiflash_scan_context(c);
    }
    if let Some(w) = present(v.get("wait")) {
        let mut ws = tipb::TiFlashWaitSummary::default();
        ws.set_min_tso_wait_ns(u(&w[0]));
        ws.set_pipeline_breaker_wait_ns(u(&w[1]));
        ws.set_pipeline_queue_wait_ns(u(&w[2]));
        sm.set_tiflash_wait_summary(ws);
    }
    Some(sm)
}

fn stats(v: &Value) -> Box<dyn RuntimeStats> {
    let a = v.as_array().unwrap();
    match a[0].as_str().unwrap() {
        "basic" => {
            let b = BasicRuntimeStats::default();
            for r in a[1].as_array().unwrap() {
                b.record(i(&r[0]), i(&r[1]));
            }
            if let Some(n) = present(a.get(2)) {
                b.set_row_num(i(n));
            }
            Box::new(b)
        }
        "commit" => {
            let mut c = RuntimeStatsWithCommit {
                commit: commit(&a[1]).map(Box::new),
                lock_keys: lock(&a[2]).map(Box::new),
                shared_lock_keys: lock(&a[3]).map(Box::new),
                txn_cnt: i(&a[4]),
            };
            for x in a[5].as_array().unwrap() {
                c.merge_commit_details(commit(x).as_ref());
            }
            Box::new(c)
        }
        "conc" => {
            let c = RuntimeStatsWithConcurrencyInfo::default();
            let infos: Vec<ConcurrencyInfo> = a[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| ConcurrencyInfo::new(x[0].as_str().unwrap(), i(&x[1])))
                .collect();
            c.set_concurrency_info(&infos);
            Box::new(c)
        }
        "ru" => match present(a.get(1)) {
            None => Box::new(RURuntimeStats::default()),
            Some(r) => Box::new(RURuntimeStats {
                ru_details: Some(RUDetails::new_with(fl(&r[0]), fl(&r[1]), i(&r[2]))),
            }),
        },
        other => panic!("unknown stats {other}"),
    }
}

// ---------------------------------------------------------------- outputs

fn zap(fields: &[LogField]) -> Value {
    Value::Array(
        fields
            .iter()
            .map(|f| match &f.value {
                LogValue::String(v) => json!([f.key, "s", v]),
                LogValue::Int(v) => json!([f.key, "i", v]),
            })
            .collect(),
    )
}

fn commit_out(c: Option<&CommitDetails>) -> Value {
    let Some(c) = c else { return Value::Null };
    let rs = RuntimeStatsWithCommit {
        commit: Some(Box::new(c.clone())),
        txn_cnt: 1,
        ..Default::default()
    };
    json!([
        rs.string(),
        c.get_latest_ts_time,
        c.lag_details.wait_time,
        c.lag_details.backoff_cnt,
        c.lag_details.first_lag_ts,
        c.lag_details.wait_until_ts,
        c.prewrite_req_num
    ])
}

fn lock_out(l: Option<&LockKeysDetails>) -> Value {
    let Some(l) = l else { return Value::Null };
    let rs = RuntimeStatsWithCommit {
        lock_keys: Some(Box::new(l.clone())),
        ..Default::default()
    };
    json!([
        rs.string(),
        l.aggressive_lock_new_count,
        l.aggressive_lock_derived_count,
        l.locked_with_conflict_count
    ])
}

fn exec_out(d: &ExecDetails) -> Value {
    let scan = d
        .scan_detail
        .as_ref()
        .map(|s| json!([s.string(), s.processed_keys_size]))
        .unwrap_or(Value::Null);
    json!({
        "s": d.string(), "z": zap(&d.to_zap_fields()), "scan": scan,
        "commit": commit_out(d.commit_detail.as_deref()), "lock": lock_out(d.lock_keys_detail.as_deref()),
        "slock": lock_out(d.shared_lock_keys_detail.as_deref()), "td": d.details_need_p90.time_detail.string(),
        "rc": d.request_count, "cop": d.cop_time, "bo": d.backoff_time,
    })
}

fn sorted<V: serde::Serialize>(m: &BTreeMap<String, V>) -> Value {
    Value::Array(m.iter().map(|(k, v)| json!([k, v])).collect())
}

fn tasks_out(d: &CopTasksDetails) -> Value {
    let dm = |m: &Option<BTreeMap<String, i64>>| m.as_ref().map(sorted).unwrap_or(Value::Null);
    json!({
        "n": d.num_cop_tasks, "pavg": d.avg_process_time, "p90": d.p90_process_time, "paddr": d.max_process_address,
        "pmax": d.max_process_time, "wavg": d.avg_wait_time, "w90": d.p90_wait_time, "waddr": d.max_wait_address,
        "wmax": d.max_wait_time, "bmax": dm(&d.max_backoff_time),
        "baddr": d.max_backoff_address.as_ref().map(sorted).unwrap_or(Value::Null),
        "bavg": dm(&d.avg_backoff_time), "b90": dm(&d.p90_backoff_time), "btot": dm(&d.tot_backoff_time),
        "btimes": dm(&d.tot_backoff_times), "z": zap(&CopTasksDetails::to_zap_fields(Some(d))),
    })
}

// ---------------------------------------------------------------- bulk (splitmix64, as Go's edSplitMix)

struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

fn gen_value(r: &mut SplitMix, kind: &str, i: usize) -> f64 {
    match kind {
        "int" => (r.next() % 1000000) as f64,
        "dup" => (r.next() % 7) as f64,
        "wide" => {
            let v = r.next();
            ((v >> 8) % (1u64 << (v % 44))) as f64
        }
        "float" => (r.next() >> 11) as f64 / (1u64 << 53) as f64 * 1000.0,
        "mono" => i as f64 * 3.5 + (r.next() % 3) as f64,
        _ => panic!("gen kind {kind}"),
    }
}

fn bulk(kind: &str, n: usize, seed: u64, mut f: impl FnMut(usize, f64, &mut SplitMix)) {
    let mut r = SplitMix(seed);
    for i in 0..n {
        let x = gen_value(&mut r, kind, i);
        f(i, x, &mut r);
    }
}

// ---------------------------------------------------------------- ops

fn tdigest(steps: &[Value]) -> Value {
    let mut t = TDigest::new();
    let mut out = vec![];
    for st in steps {
        let a = st.as_array().unwrap();
        match a[0].as_str().unwrap() {
            "add" => {
                t.add(fl(&a[1]), fl(&a[2]));
                out.push(Value::Null);
            }
            "list" => {
                let cl: Vec<Centroid> = a[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| Centroid {
                        mean: fl(&c[0]),
                        weight: fl(&c[1]),
                    })
                    .collect();
                t.add_centroid_list(&cl);
                out.push(Value::Null);
            }
            "gen" => {
                let weights = a[4].as_str().unwrap().to_string();
                bulk(
                    a[1].as_str().unwrap(),
                    i(&a[2]) as usize,
                    u(&a[3]),
                    |_, x, r| {
                        let w = match weights.as_str() {
                            "int" => (r.next() % 5 + 1) as f64,
                            "frac" => (r.next() >> 11) as f64 / (1u64 << 53) as f64 * 4.0 + 0.01,
                            _ => 1.0,
                        };
                        t.add(x, w);
                    },
                );
                out.push(Value::Null);
            }
            "q" => out.push(fo(t.quantile(fl(&a[1])))),
            "cent" => out.push(Value::Array(
                t.centroids()
                    .iter()
                    .map(|c| {
                        json!([
                            format_float(c.mean, b'g', -1, 64),
                            format_float(c.weight, b'g', -1, 64)
                        ])
                    })
                    .collect(),
            )),
            o => panic!("tdigest step {o}"),
        }
    }
    Value::Array(out)
}

fn percentile(pc: &mut Percentile<DurationWithAddr>, steps: &[Value]) -> Value {
    let mut out = vec![];
    for st in steps {
        let a = st.as_array().unwrap();
        match a[0].as_str().unwrap() {
            "add" => {
                pc.add(DurationWithAddr {
                    d: i(&a[1]),
                    addr: s(&a[2]),
                });
                out.push(Value::Null);
            }
            "gen" => {
                bulk(
                    a[1].as_str().unwrap(),
                    i(&a[2]) as usize,
                    u(&a[3]),
                    |_, x, r| {
                        pc.add(DurationWithAddr {
                            d: tidb_base::gostd::conv::f64_to_i64(x),
                            addr: format!("s{}", r.next() % 5),
                        });
                    },
                );
                out.push(Value::Null);
            }
            "p" => out.push(fo(pc.get_percentile(fl(&a[1])))),
            "max" => {
                let m = pc.get_max();
                out.push(json!([
                    m.d,
                    m.addr,
                    format_float(m.get_float64(), b'g', -1, 64)
                ]));
            }
            "min" => {
                let m = pc.get_min();
                out.push(json!([m.d, m.addr]));
            }
            "size" => out.push(json!(pc.size())),
            "sum" => out.push(fo(pc.sum())),
            "merge" => {
                let mut other = Percentile::default();
                let sub = percentile(&mut other, a[1].as_array().unwrap());
                pc.merge_percentile(&mut other);
                out.push(sub);
            }
            "int64" => {
                let n = i(&a[1]);
                out.push(json!([
                    format_float(Int64(n).get_float64(), b'g', -1, 64),
                    format_float(DurationWrapper(n).get_float64(), b'g', -1, 64)
                ]));
            }
            o => panic!("percentile step {o}"),
        }
    }
    Value::Array(out)
}

fn cgdetails(p: &[Value]) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(mut ca) = commit(&p[0]) {
        let cl = ca.clone_details();
        if let Some(cb) = commit(&p[1]) {
            ca.merge(&cb);
        }
        out.insert(
            "commit".into(),
            json!([commit_out(Some(&ca)), commit_out(Some(&cl))]),
        );
    }
    if let Some(mut la) = lock(&p[2]) {
        let cl = la.clone_details();
        if let Some(lb) = lock(&p[3]) {
            la.merge(&lb);
        }
        out.insert(
            "lock".into(),
            json!([lock_out(Some(&la)), lock_out(Some(&cl))]),
        );
    }
    let t = tikv(&p[4]);
    out.insert("tikv".into(), json!(t.string()));
    if let Some(td) = &t.time_detail {
        let mut td2 = **td;
        td2.merge(Some(td));
        out.insert("td2".into(), json!(td2.string()));
    }
    if let Some(sd) = &t.scan_detail {
        let mut sd2 = **sd;
        sd2.merge(sd);
        out.insert("sd2".into(), json!(sd2.string()));
    }
    let r = p[5].as_array().unwrap();
    let mut a = RUDetails::new_with(fl(&r[0]), fl(&r[1]), i(&r[2]));
    let b = RUDetails::new_with(fl(&r[3]), fl(&r[4]), i(&r[5]));
    let c = a.clone_details();
    a.merge(&b);
    out.insert(
        "ru".into(),
        json!([
            a.string(),
            c.string(),
            format_float(a.rru(), b'g', -1, 64),
            format_float(a.wru(), b'g', -1, 64),
            a.ru_wait_duration()
        ]),
    );
    Value::Object(out)
}

fn sync(steps: &[Value]) -> Value {
    let s = SyncExecDetails::default();
    let mut out = vec![];
    for st in steps {
        let a = st.as_array().unwrap();
        match a[0].as_str().unwrap() {
            "merge" => {
                s.merge_exec_details(exec(&a[1]).as_ref(), commit(&a[2]).as_ref());
                out.push(Value::Null);
            }
            "cop" => {
                s.merge_cop_exec_details(cop(&a[1]).as_ref(), i(&a[2]));
                out.push(Value::Null);
            }
            "lock" => {
                s.merge_lock_keys_exec_details(lock(&a[1]).as_ref());
                out.push(Value::Null);
            }
            "slock" => {
                s.merge_shared_lock_keys_exec_details(lock(&a[1]).as_ref());
                out.push(Value::Null);
            }
            "reset" => {
                s.reset();
                out.push(Value::Null);
            }
            "copgen" => {
                bulk(
                    a[1].as_str().unwrap(),
                    i(&a[2]) as usize,
                    u(&a[3]),
                    |_, x, r| {
                        let addr = format!("tikv-{}", r.next() % 4);
                        let wait = (r.next() % 50000) as i64;
                        let mut d = CopExecDetails {
                            callee_address: addr,
                            time_detail: TimeDetail {
                                process_time: tidb_base::gostd::conv::f64_to_i64(x),
                                wait_time: wait,
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        if r.next() % 3 == 0 {
                            let k = format!("bo{}", r.next() % 3);
                            d.backoff_sleep
                                .insert(k.clone(), (r.next() % 100000) as i64);
                            d.backoff_times.insert(k, (r.next() % 4) as i64 + 1);
                        }
                        let cop_time = (r.next() % 1000) as i64;
                        s.merge_cop_exec_details(Some(&d), cop_time);
                    },
                );
                out.push(Value::Null);
            }
            "get" => out.push(exec_out(&s.get_exec_details())),
            "tasks" => match s.cop_tasks_details() {
                None => out.push(json!([null, zap(&CopTasksDetails::to_zap_fields(None))])),
                Some(d) => out.push(json!([tasks_out(&d)])),
            },
            "sum" => match s.cop_tasks_summary() {
                None => out.push(Value::Null),
                Some(c) => out.push(json!([
                    c.num_cop_tasks,
                    c.max_process_address,
                    c.max_process_time,
                    c.tot_process_time,
                    c.max_wait_address,
                    c.max_wait_time,
                    c.tot_wait_time
                ])),
            },
            o => panic!("sync step {o}"),
        }
    }
    Value::Array(out)
}

fn coll(steps: &[Value]) -> Value {
    let mut c = RuntimeStatsColl::new(None);
    let mut out = vec![];
    for st in steps {
        let a = st.as_array().unwrap();
        let id = a.get(1).map(i).unwrap_or(0);
        let store = |k: usize| StoreType(u(&a[k]) as u8);
        match a[0].as_str().unwrap() {
            "basic" => {
                let b = c.get_basic_runtime_stats(id);
                b.record(i(&a[2]), i(&a[3]));
                out.push(json!([b.get_act_rows(), b.get_time(), b.string()]));
            }
            "setrows" => {
                c.get_basic_runtime_stats(id).set_row_num(i(&a[2]));
                out.push(Value::Null);
            }
            "reg" => {
                c.register_stats(id, stats(&a[2]));
                out.push(Value::Null);
            }
            "cop" => {
                let sm = summary(&a[5]);
                let r = c.record_cop_stats(
                    id,
                    store(2),
                    scan(&a[3]).as_ref(),
                    time_detail(&a[4]),
                    sm.as_ref(),
                );
                out.push(json!(r));
            }
            "one" => {
                let sm = summary(&a[3]).unwrap();
                out.push(json!(c.record_one_cop_task(id, store(2), &sm)));
            }
            "cops" => {
                let st = store(2);
                bulk(
                    a[3].as_str().unwrap(),
                    i(&a[4]) as usize,
                    u(&a[5]),
                    |_, x, r| {
                        let mut sm = tipb::ExecutorExecutionSummary::default();
                        sm.set_time_processed_ns(tidb_base::gostd::conv::f64_to_u64(x));
                        sm.set_num_produced_rows(r.next() % 100);
                        sm.set_num_iterations(r.next() % 10);
                        sm.set_concurrency(r.next() % 4);
                        c.record_one_cop_task(id, st, &sm);
                    },
                );
                out.push(Value::Null);
            }
            "root" => {
                let r = c.get_root_stats(id);
                let (bs, n) = r.merge_stats(|basic, groups| {
                    (
                        basic
                            .map(|b| BasicRuntimeStats::string_of(Some(b)))
                            .unwrap_or_default(),
                        groups.len(),
                    )
                });
                out.push(json!([r.string(), r.get_act_rows(), bs, n]));
            }
            "copstr" => match c.get_cop_stats(id) {
                None => out.push(Value::Null),
                Some(cs) => {
                    let mut cs = cs.lock().unwrap();
                    out.push(json!([cs.string(), cs.get_act_rows(), cs.get_tasks()]));
                }
            },
            "coprows" => {
                let (n, rows) = c.get_cop_count_and_rows(id);
                out.push(json!([n, rows]));
            }
            "actrows" => out.push(json!(c.get_plan_act_rows(id))),
            "exists" => out.push(json!([c.exists_root_stats(id), c.exists_cop_stats(id)])),
            "new" => {
                c = RuntimeStatsColl::new(Some(c));
                out.push(Value::Null);
            }
            "bcop" => match c.get_cop_stats(id) {
                None => out.push(Value::Null),
                Some(cs) => {
                    let mut a_ = cs.lock().unwrap().basic_cop_runtime_stats().clone_stats();
                    let mut r = vec![json!(a_.string()), json!(a_.tp())];
                    let b = a_.clone_stats();
                    match present(a.get(2)) {
                        Some(o) => {
                            if let Some(os) = c.get_cop_stats(i(o)) {
                                let other =
                                    os.lock().unwrap().basic_cop_runtime_stats().clone_stats();
                                a_.merge(other.as_ref());
                            }
                        }
                        None => a_.merge(&BasicRuntimeStats::default()),
                    }
                    r.push(json!(a_.string()));
                    r.push(json!(b.string()));
                    out.push(Value::Array(r));
                }
            },
            o => panic!("coll step {o}"),
        }
    }
    Value::Array(out)
}

fn plan(v: &Value) -> Option<PlanTrace> {
    let a = v.as_array()?;
    let mut p = PlanTrace::default();
    p.id = i(&a[0]);
    p.tp = s(&a[1]);
    p.explain_info = s(&a[2]);
    p.cost = fl(&a[3]);
    p.children = a[4]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| plan(c).unwrap())
        .collect();
    Some(p)
}

fn json_res(r: Result<String, UnsupportedValueError>) -> Value {
    match r {
        Ok(s) => json!(s),
        Err(e) => json!({ "err": e.error() }),
    }
}

fn opttrace(steps: &[Value]) -> Value {
    let mut t = OptimizeTracer::default();
    let mut out = vec![];
    for st in steps {
        let a = st.as_array().unwrap();
        let mut r = Value::Null;
        match a[0].as_str().unwrap() {
            "init" => {
                t.logical = Some(LogicalOptimizeTracer::default());
                let mut p = PhysicalOptimizeTracer::default();
                if !a[1].as_bool().unwrap() {
                    p.candidates = Some(BTreeMap::new());
                    p.physical_plan_cost_details = Some(BTreeMap::new());
                }
                t.physical = Some(p);
            }
            "before" => t
                .logical
                .as_mut()
                .unwrap()
                .append_rule_tracer_before_rule_optimize(
                    i(&a[1]),
                    a[2].as_str().unwrap(),
                    &plan(&a[3]).unwrap(),
                ),
            "step" => t
                .logical
                .as_mut()
                .unwrap()
                .append_rule_tracer_step_to_current(
                    i(&a[1]),
                    a[2].as_str().unwrap(),
                    a[3].as_str().unwrap(),
                    a[4].as_str().unwrap(),
                ),
            "final" => t
                .logical
                .as_mut()
                .unwrap()
                .record_final_logical_plan(&plan(&a[1]).unwrap()),
            "cand" => t
                .physical
                .as_mut()
                .unwrap()
                .append_candidate(CandidatePlanTrace {
                    plan_trace: plan(&a[1]),
                    mapping_logical_plan: s(&a[2]),
                }),
            "pfinal" => t
                .physical
                .as_mut()
                .unwrap()
                .record_final_plan_trace(&plan(&a[1]).unwrap()),
            "cost" => {
                let mut d = new_physical_plan_cost_detail(i(&a[2]), a[3].as_str().unwrap());
                d.set_desc(a[4].as_str().unwrap());
                for kv in a[5].as_array().unwrap() {
                    let k = kv[0].as_str().unwrap();
                    let v = kv[1].as_array().unwrap();
                    let p = match v[0].as_str().unwrap() {
                        "f" => CostParam::Float(fl(&v[1])),
                        "i" => CostParam::Int(i(&v[1])),
                        "s" => CostParam::String(s(&v[1])),
                        "b" => CostParam::Bool(v[1].as_bool().unwrap()),
                        o => panic!("param {o}"),
                    };
                    d.add_param(k, p);
                }
                d.cost = fl(&a[6]);
                r = json!([
                    d.get_plan_id(),
                    d.get_plan_type(),
                    d.exists("a"),
                    d.exists("")
                ]);
                t.physical
                    .as_mut()
                    .unwrap()
                    .physical_plan_cost_details
                    .as_mut()
                    .expect("assignment to entry in nil map")
                    .insert(s(&a[1]), d);
            }
            "fast" => t.set_fast_plan(&plan(&a[1]).unwrap()),
            "rfinal" => t.record_final_plan(&plan(&a[1]).unwrap()),
            "ids" => {
                let mut p = plan(&a[1]).unwrap();
                let ids: Vec<i64> = a[2].as_array().unwrap().iter().map(i).collect();
                p.append_children_id(&ids);
                p.append_children_id(&ids);
                r = json_res(p.marshal_json());
            }
            o => panic!("opttrace step {o}"),
        }
        if r.is_null() {
            r = json_res(t.marshal_json());
        }
        out.push(r);
    }
    Value::Array(out)
}

fn exec_op(op: &str, p: &[Value]) -> Value {
    match op {
        "fmt" => {
            let n = i(&p[1]);
            json!(match p[0].as_str().unwrap() {
                "fd" => tidb_execdetails::execdetails::format_duration(n),
                "cgfd" => tidb_execdetails::clientgo::format_duration(n),
                "fb" => format_bytes(n),
                "bts" => bytes_to_string(n),
                o => panic!("fmt {o}"),
            })
        }
        "gomath" => {
            let x = fl(&p[1]);
            let r = match p[0].as_str().unwrap() {
                "sin" => math::sin(x),
                "asin" => math::asin(x),
                o => panic!("gomath {o}"),
            };
            json!(format!("{:016x}", r.to_bits()))
        }
        "tdigest" => tdigest(p[0].as_array().unwrap()),
        "percentile" => percentile(&mut Percentile::default(), p[0].as_array().unwrap()),
        "details" => exec_out(&exec(&p[0]).unwrap()),
        "cgdetails" => cgdetails(p),
        "sync" => sync(p[0].as_array().unwrap()),
        "coll" => coll(p[0].as_array().unwrap()),
        "stats" => {
            let mut a = stats(&p[0]);
            let b = stats(&p[1]);
            let mut out = serde_json::Map::new();
            out.insert("a".into(), json!(a.string()));
            out.insert("tp".into(), json!(a.tp()));
            out.insert("b".into(), json!(b.string()));
            let c = a.clone_stats();
            out.insert("clone".into(), json!(c.string()));
            a.merge(b.as_ref());
            out.insert("merged".into(), json!(a.string()));
            out.insert("clone2".into(), json!(c.string()));
            Value::Object(out)
        }
        "opttrace" => opttrace(p[0].as_array().unwrap()),
        "ce" => {
            let ids = p[1].as_array().unwrap();
            let recs: Vec<CETraceRecord> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(k, r)| CETraceRecord {
                    table_name: s(g(r, "table_name")),
                    tp: s(g(r, "type")),
                    expr: s(g(r, "expr")),
                    row_count: u(g(r, "row_count")),
                    table_id: i(&ids[k][0]),
                })
                .collect();
            Value::Array(
                dedup_ce_trace(&recs)
                    .iter()
                    .map(|r| json!([r.marshal_json(), r.table_id]))
                    .collect(),
            )
        }
        "codec" => json!(codec_plan_name(p[0].as_str().unwrap(), i(&p[1]))),
        o => panic!("unknown op {o}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "execdetails");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap().to_string();
        let got = catch_unwind(AssertUnwindSafe(|| exec_op(&op, &args)));
        let go_panicked = rec["err"]["name"] == "panic";
        match got {
            Err(_) if go_panicked => {}
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                fails.push(format!("{line}\n    rust panicked: {msg:?}"));
            }
            Ok(got) if go_panicked => fails.push(format!("{line}\n    rust returned {got}")),
            Ok(got) => {
                if got != rec["out"] {
                    fails.push(format!("{line}\n    got {got}"));
                }
            }
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails
        .iter()
        .take(20)
        .map(|s| s.chars().take(3000).collect::<String>())
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
fn govec_execdetails() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_EXECDETAILS_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
