//! Replays the govec `copr` vectors (tidb-rs-tracker `govec/domains/copr.go`): block D07.2
//! part 1 — key ranges, region / bucket splitting, task building, small-task concurrency,
//! the paging range arithmetic, request-field conversions — and `pkg/util/paging`, against
//! Go TiDB v8.5.8.
//!
//! Records that split by region carry the region layout Go's region cache served (a
//! mocktikv cluster); the replay serves the same layout through [`LayoutCache`], a
//! [`TiKVRegionCache`] with client-go's `BatchLocateKeyRanges` result for a static layout.
//!
//! `GOVEC_COPR_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tidb_base::errors::Error;
use tidb_base::gostd::strconv::format_float;
use tidb_base::kv::StoreType;
use tidb_kv::context::RequestSource;
use tidb_kv::kv::{IsoLevel, Request};
use tidb_kv::option::ReplicaReadType;
use tidb_kv::{Key, KeyRange};
use tidb_store::copr::coprocessor::*;
use tidb_store::copr::key_ranges::{KeyRanges, new_key_ranges};
use tidb_store::copr::region_cache::{RegionCache, new_region_cache};
use tidb_store::copr::tikv::{KeyLocation, RPCContext, RegionVerID, TiKVRegionCache};
use tidb_store::util::paging;
use tikv_client::proto::{coprocessor, metapb};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/copr.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- values

fn unhex(v: &Value) -> Vec<u8> {
    let s = v.as_str().expect("hex string");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn range(v: &Value) -> KeyRange {
    KeyRange {
        start_key: Key(unhex(&v[0])),
        end_key: Key(unhex(&v[1])),
    }
}

fn ranges(v: &Value) -> Vec<KeyRange> {
    v.as_array().unwrap().iter().map(range).collect()
}

fn ranges_out(rs: &[KeyRange]) -> Value {
    Value::Array(
        rs.iter()
            .map(|r| json!([hex(&r.start_key), hex(&r.end_key)]))
            .collect(),
    )
}

fn pb_ranges_out(rs: &[coprocessor::KeyRange]) -> Value {
    Value::Array(
        rs.iter()
            .map(|r| json!([hex(&r.start), hex(&r.end)]))
            .collect(),
    )
}

fn shape(r: &KeyRanges) -> Value {
    let (f, m, l) = r.shape();
    json!({"f": f, "m": m, "l": l, "r": ranges_out(&r.to_ranges())})
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn request(v: &Value) -> Request {
    let mut req = Request {
        tp: i(&v["tp"]),
        store_type: StoreType(u(&v["st"]) as u8),
        keep_order: v["keep"].as_bool().unwrap(),
        desc: v["desc"].as_bool().unwrap(),
        limit_size: u(&v["limit"]),
        store_batch_size: i(&v["batch"]) as isize,
        store_busy_threshold: Duration::from_millis(u(&v["busy"])),
        tikv_client_read_timeout: u(&v["rt"]),
        replica_read: ReplicaReadType(u(&v["rr"]) as u8),
        priority: i(&v["pri"]) as isize,
        request_source: RequestSource {
            request_source_internal: v["internal"].as_bool().unwrap(),
            request_source_type: v["source"].as_str().unwrap().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    req.paging.enable = v["paging"].as_bool().unwrap();
    req.paging.min_paging_size = u(&v["minp"]);
    req.paging.max_paging_size = u(&v["maxp"]);
    req
}

// ---------------------------------------------------------------- the region layout

struct Region {
    loc: KeyLocation,
    buckets: Option<metapb::Buckets>,
    store: u64,
    peer: metapb::Peer,
}

/// The region cache over Go's layout: client-go's `BatchLocateKeyRanges` gives, for a
/// static layout, each region the ranges cover once, in key order; `GetTiKVRPCContext`
/// gives the leader (mocktikv's stores are never busy).
struct LayoutCache {
    regions: Vec<Region>,
}

impl LayoutCache {
    fn new(layout: &Value) -> LayoutCache {
        let regions = layout
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let buckets = r["bkeys"].as_array().map(|keys| metapb::Buckets {
                    region_id: u(&r["id"]),
                    version: u(&r["bver"]),
                    keys: keys.iter().map(unhex).collect(),
                    ..Default::default()
                });
                Region {
                    loc: KeyLocation {
                        region: RegionVerID::new(u(&r["id"]), u(&r["conf"]), u(&r["ver"])),
                        start_key: unhex(&r["start"]),
                        end_key: unhex(&r["end"]),
                        buckets: None,
                    },
                    buckets,
                    store: u(&r["store"]),
                    peer: metapb::Peer {
                        id: u(&r["peer"][0]),
                        store_id: u(&r["peer"][1]),
                        role: i(&r["peer"][2]) as i32,
                        ..Default::default()
                    },
                }
            })
            .collect();
        LayoutCache { regions }
    }
}

impl TiKVRegionCache for LayoutCache {
    type Backoffer = ();

    async fn batch_locate_key_ranges(
        &self,
        _bo: &mut (),
        key_ranges: &[KeyRange],
        _need_leader: bool,
        need_buckets: bool,
    ) -> Result<Vec<KeyLocation>, Error> {
        // client-go: the region holding the start key, then the following regions until
        // one contains the end key by end (`ContainsByEnd`), each region once.
        let contains_by_end = |r: &Region, e: &[u8]| {
            r.loc.start_key.as_slice() < e
                && (r.loc.end_key.is_empty() || e <= r.loc.end_key.as_slice())
        };
        let mut out: Vec<KeyLocation> = Vec::new();
        for kr in key_ranges {
            let Some(mut idx) = self
                .regions
                .iter()
                .position(|r| r.loc.contains(&kr.start_key))
            else {
                continue;
            };
            loop {
                let r = &self.regions[idx];
                if out.last().is_none_or(|l| l.region != r.loc.region) {
                    let mut loc = r.loc.clone();
                    if need_buckets {
                        loc.buckets = r.buckets.clone();
                    }
                    out.push(loc);
                }
                if contains_by_end(r, &kr.end_key) || idx + 1 == self.regions.len() {
                    break;
                }
                idx += 1;
            }
        }
        Ok(out)
    }

    async fn get_tikv_rpc_context(
        &self,
        _bo: &mut (),
        id: RegionVerID,
        _replica_read: ReplicaReadType,
        _seed: u32,
    ) -> Result<Option<RPCContext>, Error> {
        Ok(self
            .regions
            .iter()
            .find(|r| r.loc.region == id)
            .map(|r| RPCContext {
                region: id,
                peer: r.peer.clone(),
                addr: String::new(),
                store_id: r.store,
                store_estimated_wait_time: Duration::ZERO,
            }))
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

// ---------------------------------------------------------------- outputs

fn task_out(t: &CopTask, req: &Request) -> Value {
    assert_eq!(
        t.request_source, req.request_source,
        "requestSource is the request's"
    );
    let pb: Vec<Value> = t
        .to_pb_batch_tasks()
        .iter()
        .map(|b| {
            let e = b.region_epoch.clone().unwrap_or_default();
            let p = b.peer.clone().unwrap_or_default();
            json!([
                b.region_id,
                e.conf_ver,
                e.version,
                [p.id, p.store_id, p.role],
                pb_ranges_out(&b.ranges),
                b.task_id
            ])
        })
        .collect();
    let batched = t.batch_task_list.as_ref().map(|m| {
        Value::Array(
            m.iter()
                .map(|(id, b)| {
                    let e = b.region.region_epoch.clone().unwrap_or_default();
                    json!({
                        "id": id,
                        "store": b.store_id,
                        "lb": b.load_based_replica_retry,
                        "region": [b.region.region_id, e.conf_ver, e.version, pb_ranges_out(&b.region.ranges)],
                        "peer": [b.peer.id, b.peer.store_id, b.peer.role],
                        "task": task_out(&b.task, req),
                    })
                })
                .collect(),
        )
    });
    json!({
        "id": t.task_id,
        "region": [t.region.id, t.region.conf_ver, t.region.ver],
        "bver": t.buckets_ver,
        "ranges": shape(&t.ranges),
        "chan": t.resp_chan.as_ref().map_or(-1, |c| c.tx.max_capacity() as i64),
        "st": t.store_type.0,
        "cmd": t.cmd_type.0,
        "paging": t.paging,
        "psize": t.paging_size,
        "hint": t.row_count_hint,
        "busy": t.busy_threshold.as_nanos() as i64,
        "rt": t.tikv_client_read_timeout,
        "redirect": t.redirect2_replica,
        "str": t.string(),
        "pb": if pb.is_empty() { Value::Null } else { Value::Array(pb) },
        "batched": batched.unwrap_or(Value::Null),
    })
}

/// Go's out without what the replay does not compute (`source`: the formatted
/// `GetRequestSource`; checked as "the task's source is the request's" instead).
fn strip_source(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("source");
            for x in m.values_mut() {
                strip_source(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_source),
        _ => {}
    }
}

// ---------------------------------------------------------------- ops

fn exec_kr(args: &[Value]) -> Value {
    let mut r = KeyRanges::from_parts(
        (!args[0].is_null()).then(|| range(&args[0])),
        ranges(&args[1]),
        (!args[2].is_null()).then(|| range(&args[2])),
    );
    let mut out = Vec::new();
    for st in args[3].as_array().unwrap() {
        match st[0].as_str().unwrap() {
            "split" => {
                let (left, right) = r.split(&unhex(&st[1]));
                r = if st[2] == "L" { left } else { right };
                out.push(shape(&r));
            }
            "slice" => {
                r = r.slice(u(&st[1]) as usize, u(&st[2]) as usize);
                out.push(shape(&r));
            }
            "at" => out.push(ranges_out(&[r.at(u(&st[1]) as usize)])[0].clone()),
            "string" => out.push(json!(r.string())),
            "pb" => out.push(pb_ranges_out(&r.to_pb_ranges())),
            "len" => out.push(json!(r.len())),
            other => panic!("kr step {other}"),
        }
    }
    Value::Array(out)
}

fn exec_split(args: &[Value], out: &Value) -> Value {
    let cache = new_region_cache(Arc::new(LayoutCache::new(&out["layout"])));
    let rs = ranges(&args[1]);
    let limit = i(&args[3]) as isize;
    let res = match args[2].as_str().unwrap() {
        "regions" => {
            let v = block_on(cache.split_region_ranges(&mut (), rs, limit)).unwrap();
            ranges_out(&v)
        }
        f => {
            let locs = if f == "locs" {
                block_on(cache.split_key_ranges_by_locations(
                    &mut (),
                    new_key_ranges(rs),
                    limit,
                    args[4].as_bool().unwrap(),
                    args[5].as_bool().unwrap(),
                ))
            } else {
                block_on(cache.split_key_ranges_by_buckets(&mut (), new_key_ranges(rs)))
            }
            .unwrap();
            if locs.is_empty() {
                Value::Null
            } else {
                Value::Array(
                    locs.iter()
                        .map(|l| json!([l.location.region.id, shape(&l.ranges)]))
                        .collect(),
                )
            }
        }
    };
    json!({"layout": out["layout"], "res": res})
}

fn exec_build(args: &[Value], out: &Value) -> Value {
    let cache: RegionCache<LayoutCache> =
        new_region_cache(Arc::new(LayoutCache::new(&out["layout"])));
    let req = request(&args[2]);
    let hints: Option<Vec<isize>> = args[3]
        .as_array()
        .map(|h| h.iter().map(|x| i(x) as isize).collect());
    let mut opt = BuildCopTaskOpt::new(&req, &cache);
    opt.row_hints = hints.as_deref();
    opt.resp_chan = args[4].as_bool().unwrap();
    opt.ignore_tikv_client_read_timeout = args[5].as_bool().unwrap();
    let tasks = block_on(build_cop_tasks(
        &mut (),
        &new_key_ranges(ranges(&args[1])),
        &mut opt,
    ))
    .unwrap();
    json!({
        "layout": out["layout"],
        "tasks": tasks.iter().map(|t| task_out(t, &req)).collect::<Vec<_>>(),
    })
}

fn exec_small(args: &[Value]) -> Value {
    let tasks: Vec<CopTask> = args[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut task = CopTask {
                row_count_hint: i(&t[0]) as isize,
                ..Default::default()
            };
            let n = u(&t[1]);
            if n > 0 {
                let mut m = BTreeMap::new();
                for id in 1..=n {
                    m.insert(
                        id,
                        BatchedCopTask {
                            task: CopTask::default(),
                            region: Default::default(),
                            store_id: 0,
                            peer: Default::default(),
                            load_based_replica_retry: false,
                        },
                    );
                }
                task.batch_task_list = Some(m);
            }
            task
        })
        .collect();
    let smalls: Vec<bool> = tasks.iter().map(is_small_task).collect();
    let (n, conc) = small_task_concurrency(&tasks, i(&args[1]) as isize);
    json!([
        if smalls.is_empty() {
            Value::Null
        } else {
            json!(smalls)
        },
        n,
        conc
    ])
}

fn exec(op: &str, args: &[Value], out: &Value) -> Value {
    match op {
        "kr" => exec_kr(args),
        "bkr" => {
            let keys: Vec<Vec<u8>> = args[0].as_array().unwrap().iter().map(unhex).collect();
            let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
            ranges_out(&build_key_ranges(&refs))
        }
        "calc" => {
            let rs = new_key_ranges(ranges(&args[0]));
            let split = (!args[1].is_null()).then(|| coprocessor::KeyRange {
                start: unhex(&args[1][0]),
                end: unhex(&args[1][1]),
            });
            let desc = args[2].as_bool().unwrap();
            let res = if args[3] == "remain" {
                calculate_remain(&rs, split.as_ref(), desc)
            } else {
                calculate_retry(&rs, split.as_ref(), desc)
            };
            shape(&res)
        }
        "split" => exec_split(args, out),
        "build" => exec_build(args, out),
        "small" => exec_small(args),
        "reqflags" => {
            let req = request(&args[0]);
            json!([opt_row_hint(&req), check_store_batch_copr(&req)])
        }
        "pb" => json!([
            priority_to_pb(i(&args[0]) as isize) as i32,
            isolation_level_to_pb(IsoLevel(i(&args[1]) as isize)) as i32
        ]),
        "paging" => {
            if args[0] == "grow" {
                json!(paging::grow_paging_size(u(&args[1]), u(&args[2])))
            } else {
                json!(format_float(
                    paging::calculate_seek_cnt(u(&args[1])),
                    b'g',
                    -1,
                    64
                ))
            }
        }
        other => panic!("unknown op {other}"),
    }
}

fn replay(path: &str) -> usize {
    let mut s = String::new();
    flate2::read::GzDecoder::new(
        std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}")),
    )
    .read_to_string(&mut s)
    .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "copr");
    let mut n = 0;
    let mut fails = Vec::new();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let err = &rec["err"];
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, args, &rec["out"])));
        n += 1;
        if !err.is_null() {
            if err["name"] == "panic" {
                if let Ok(v) = got {
                    fails.push(format!("{line}\n  Go panicked, Rust returned {v:?}"));
                }
                continue;
            }
            panic!("unexpected Go error record: {line}");
        }
        let mut want = rec["out"].clone();
        strip_source(&mut want);
        match got {
            Ok(got) if got == want => {}
            Ok(got) => fails.push(format!("{line}\n  got  {got}")),
            Err(e) => fails.push(format!(
                "{line}\n  Rust panicked: {:?}",
                e.downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
            )),
        }
    }
    if !fails.is_empty() {
        panic!(
            "{} of {n} records differ, first ones:\n{}",
            fails.len(),
            fails[..fails.len().min(5)].join("\n")
        );
    }
    n
}

#[test]
fn govec_copr() {
    let n = replay(&vectors());
    assert!(n > 2000, "{n} records");
    if let Ok(files) = std::env::var("GOVEC_COPR_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

/// `kv.TiDB` requests need `infosync.GetAllServerInfo` (D06.5): the boundary stub fails
/// instead of building no task.
#[test]
fn tidb_store_type_is_a_boundary_stub() {
    let cache = new_region_cache(Arc::new(LayoutCache {
        regions: Vec::new(),
    }));
    let req = Request {
        store_type: tidb_base::kv::TiDB,
        ..Default::default()
    };
    let mut opt = BuildCopTaskOpt::new(&req, &cache);
    let ranges = new_key_ranges(build_key_ranges(&[b"a", b"b"]));
    let err = block_on(build_cop_tasks(&mut (), &ranges, &mut opt)).unwrap_err();
    assert!(err.to_string().contains("D06.5"), "{err}");
}
