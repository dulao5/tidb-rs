//! Replays the govec `infoschema` vectors (tidb-rs-tracker `govec/domains/infoschema.go`):
//! block D06.3 part 1, infoschema v2 build and cache, against Go TiDB v8.5.8.
//!
//! A record is a scenario on one `Data`: full loads, batches of schema diffs, GC, a tiny
//! cache capacity, and queries of the newest and older InfoSchema versions. The meta
//! states are loaded into [`MemSnap`]s (the same map semantics as the Go generator's) and
//! the store of `autoid.Requirement` serves them by commit ts ([`MvccStore`]). Each
//! step's output is rebuilt here exactly as Go's `isQueries` builds it and compared (Go's
//! nil and empty slices compare equal).
//!
//! `GOVEC_INFOSCHEMA_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use futures::executor::block_on;
use serde_json::{Map, Value, json};
use tidb_base::Error;
use tidb_infoschema::autoid::Requirement;
use tidb_infoschema::builder::{
    convert_charset_collate_to_lower_case_if_need, convert_old_version_utf8_to_utf8mb4_if_need,
};
use tidb_infoschema::context as isctx;
use tidb_infoschema::infoschema::has_auto_increment_column;
use tidb_infoschema::infoschema_v2::{Ctx, InfoSchemaV2, is_special_db, with_refill_option};
use tidb_infoschema::meta::{self, Mutator};
use tidb_infoschema::{Data, new_builder, new_data};
use tidb_model::{DBInfo, PolicyInfo, ResourceGroupInfo, SchemaDiff, TableInfo};
use tidb_parser::model::CIStr;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/infoschema.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- in-memory snapshot

mod common;
use common::{MemSnap, MvccStore, Req};

/// A govec state: hex key-value pairs, sorted.
fn mem_snap(pairs: &Value) -> MemSnap {
    MemSnap::from_pairs(pairs.as_array().into_iter().flatten().map(|p| {
        (
            decode_hex(p[0].as_str().unwrap()),
            decode_hex(p[1].as_str().unwrap()),
        )
    }))
}

// ---------------------------------------------------------------- helpers

fn decode_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn enc_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `vec.GoString`: a JSON string if valid UTF-8, else `{"hex": ...}`.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => json!({ "hex": enc_hex(b) }),
    }
}

/// `metaErr`.
fn err_json(e: &Error) -> Value {
    json!({
        "code": if e.is_normalized() { e.code() as i64 } else { 0 },
        "msg": gs(&e.error()),
    })
}

fn opt_err(e: Option<&Error>) -> Value {
    e.map_or(Value::Null, err_json)
}

/// Go nil and empty slices both: an empty list is `null`.
fn list(v: Vec<Value>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        Value::Array(v)
    }
}

const PROBE_NAMES: &[&str] = &["d1", "D1", "d2", "db_3", "Db_4", "mysql", "x"];
const PROBE_TABLES: &[&str] = &["t", "T", "t1", "t2", "T3", "p", "v", "s", "tt", "x"];
const MAX_PROBE_ID: i64 = 200;

fn tbl_out(t: &TableInfo, with_json: bool) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(t.id));
    m.insert("name".into(), json!(t.name.o));
    m.insert("dbid".into(), json!(t.dbid));
    if with_json {
        m.insert("json".into(), gs(t.marshal_json().unwrap().as_bytes()));
    }
    Value::Object(m)
}

fn db_out(d: Option<&DBInfo>) -> Value {
    let Some(d) = d else { return Value::Null };
    let mut m = Map::new();
    m.insert("id".into(), json!(d.id));
    m.insert("name".into(), json!(d.name.o));
    m.insert("cs".into(), json!(d.charset));
    m.insert("co".into(), json!(d.collate));
    if let Some(p) = &d.placement_policy_ref {
        m.insert("pol".into(), json!(p.id));
    }
    Value::Object(m)
}

fn filters() -> Vec<(&'static str, isctx::SpecialAttributeFilter)> {
    vec![
        ("all", isctx::AllSpecialAttribute),
        ("ttl", isctx::ttl_attribute),
        ("tiflash", isctx::tiflash_attribute),
        ("placement", isctx::placement_policy_attribute),
        ("allplacement", isctx::all_placement_policy_attribute),
        ("lock", isctx::table_lock_attribute),
        ("partition", isctx::partition_attribute),
        ("affinity", isctx::affinity_attribute),
    ]
}

/// Go `isQueries`, in the same order.
async fn queries(is: &InfoSchemaV2) -> Value {
    let ctx = Ctx::background();
    let mut q = Map::new();
    q.insert("ver".into(), json!(is.base().schema_meta_version()));
    let names: Vec<Value> = is
        .all_schema_names()
        .iter()
        .filter(|n| !is_special_db(&n.l))
        .map(|n| json!(n.o))
        .collect();
    q.insert("names".into(), list(names));
    let all: Vec<Value> = is
        .all_schemas()
        .iter()
        .filter(|d| !is_special_db(&d.name.l))
        .map(|d| db_out(Some(d)))
        .collect();
    q.insert("all".into(), list(all));
    let cached = |is: &InfoSchemaV2| -> Value {
        Value::Array(
            (1..=MAX_PROBE_ID)
                .filter(|&id| is.table_is_cached(id))
                .map(|id| json!(id))
                .collect(),
        )
    };
    q.insert("cached0".into(), cached(is));
    let (mut schemas, mut simple, mut infos) = (vec![], vec![], vec![]);
    for n in PROBE_NAMES {
        let cn = CIStr::new(n);
        let d = is.schema_by_name(&cn);
        schemas.push(json!([
            n,
            db_out(d.as_deref()),
            d.is_some(),
            is.schema_exists(&cn)
        ]));
        let s: Vec<Value> = is
            .schema_simple_table_infos(&cn)
            .into_iter()
            .flatten()
            .map(|t| json!([t.id, t.name.o]))
            .collect();
        simple.push(list(s));
        let (ts, err) = match is.schema_table_infos(ctx, &cn).await {
            Ok(t) => (t.unwrap_or_default(), None),
            Err(e) => (vec![], Some(e)),
        };
        let ts: Vec<Value> = ts.iter().map(|t| tbl_out(t, false)).collect();
        infos.push(json!([list(ts), opt_err(err.as_ref())]));
    }
    q.insert("schemas".into(), list(schemas));
    q.insert("simple".into(), list(simple));
    q.insert("infos".into(), list(infos));
    let (mut by_id, mut items, mut parts, mut bundles) = (vec![], vec![], vec![], vec![]);
    for id in 1..=MAX_PROBE_ID {
        if let Some(d) = is.schema_by_id(id) {
            by_id.push(json!([id, db_out(Some(&d))]));
        }
        if let Some(it) = is.table_item_by_id(id) {
            items.push(json!([id, it.db_name.o, it.table_name.o]));
        }
        if let Some(tid) = is.table_id_by_partition_id(id) {
            let it = is.table_item_by_partition_id(id);
            let ok2 = it.is_some();
            let it = it.unwrap_or_default();
            let (tbl, db, def) = is.find_table_by_partition_id(id).await;
            parts.push(json!([
                id,
                tid,
                it.db_name.o,
                it.table_name.o,
                ok2,
                tbl.map(|t| t.meta().id),
                db.map(|d| d.id),
                def.map(|d| d.id)
            ]));
        }
        if is
            .base()
            .placement_bundle_by_physical_table_id(id)
            .is_some()
        {
            bundles.push(json!(id));
        }
    }
    q.insert("byid".into(), list(by_id));
    q.insert("items".into(), list(items));
    q.insert("parts".into(), list(parts));
    q.insert("bundles".into(), list(bundles));
    let mut tbls = vec![];
    for id in 1..=MAX_PROBE_ID {
        if let Some(t) = is.table_by_id(ctx, id).await {
            let ti = is.table_info_by_id(id).await;
            tbls.push(json!([
                id,
                tbl_out(t.meta(), false),
                ti.map_or(0, |t| t.id)
            ]));
        }
    }
    q.insert("tbyid".into(), list(tbls));
    let mut refill = vec![];
    let rctx = with_refill_option(ctx, true);
    for id in (1..=MAX_PROBE_ID).step_by(3) {
        if let Some(t) = is.table_by_id(rctx, id).await {
            let (ti, db, def) = is.find_table_info_by_partition_id(id).await;
            refill.push(json!([
                id,
                t.meta().id,
                ti.map(|t| t.id),
                db.map(|d| d.id),
                def.map(|d| d.id)
            ]));
        }
    }
    q.insert("refill".into(), list(refill));
    let (mut by_name, mut fks) = (vec![], vec![]);
    for n in PROBE_NAMES {
        for tn in PROBE_TABLES {
            let (cn, ct) = (CIStr::new(n), CIStr::new(tn));
            let r = is.table_by_name(ctx, &cn, &ct).await;
            let (to, auto_inc) = match &r {
                Ok(t) => {
                    let (has, col) = has_auto_increment_column(t.meta());
                    (tbl_out(t.meta(), true), json!([has, col]))
                }
                Err(_) => (Value::Null, Value::Null),
            };
            if r.is_ok() || is.schema_exists(&cn) {
                let (ti, err2) = is.table_info_by_name(&cn, &ct).await;
                by_name.push(json!([
                    n,
                    tn,
                    to,
                    opt_err(r.as_ref().err()),
                    is.table_exists(&cn, &ct).await,
                    ti.map(|t| t.id),
                    opt_err(err2.as_ref().err()),
                    auto_inc
                ]));
            }
            let fk: Vec<Value> = is
                .get_table_referred_foreign_keys(&cn.l, &ct.l)
                .iter()
                .map(|f| {
                    let cols: Vec<Value> = f.cols.iter().flatten().map(|c| json!(c.o)).collect();
                    json!([
                        list(cols),
                        f.child_schema.o,
                        f.child_table.o,
                        f.child_fk_name.o
                    ])
                })
                .collect();
            if !fk.is_empty() {
                fks.push(json!([n, tn, fk]));
            }
        }
    }
    q.insert("byname".into(), list(by_name));
    q.insert("fks".into(), list(fks));
    let mut special = vec![];
    for (name, f) in filters() {
        let res: Vec<Value> = is
            .list_tables_with_special_attribute(f)
            .iter()
            .map(|r| {
                let ids: Vec<Value> = r.table_infos.iter().map(|t| json!(t.id)).collect();
                json!([r.db_name.o, list(ids)])
            })
            .collect();
        special.push(json!([name, list(res)]));
    }
    q.insert("special".into(), list(special));
    let mut iter = vec![];
    is.iterate_all_table_items(|t| {
        iter.push(json!([t.db_name.o, t.table_name.o]));
        true
    });
    q.insert("iter".into(), list(iter));
    let mut pols: Vec<(String, i64)> = is
        .base()
        .all_placement_policies()
        .iter()
        .map(|p| (p.name.o.clone(), p.id))
        .collect();
    pols.sort();
    q.insert(
        "policies".into(),
        list(pols.into_iter().map(|(n, i)| json!([n, i])).collect()),
    );
    let mut rgs: Vec<(String, i64)> = is
        .base()
        .all_resource_groups()
        .iter()
        .map(|g| (g.name.o.clone(), g.id))
        .collect();
    rgs.sort();
    q.insert(
        "groups".into(),
        list(rgs.into_iter().map(|(n, i)| json!([n, i])).collect()),
    );
    let mut lookups = vec![];
    for n in ["p1", "P1", "p2", "default", "rg1", "RG2"] {
        let p = is.base().policy_by_name(&CIStr::new(n));
        let g = is.base().resource_group_by_name(&CIStr::new(n));
        lookups.push(json!([n, p.map(|p| p.id), g.map(|g| g.id)]));
    }
    q.insert("lookups".into(), list(lookups));
    q.insert(
        "nbundles".into(),
        json!(is.base().all_placement_bundles().len()),
    );
    q.insert("temp".into(), json!(is.base().has_temporary_table()));
    q.insert("cached1".into(), cached(is));
    for n in &PROBE_NAMES[..3] {
        for tn in &PROBE_TABLES[..4] {
            is.evict_table(&CIStr::new(n), &CIStr::new(tn));
        }
    }
    q.insert("cached2".into(), cached(is));
    let c = is.clone_and_update_ts(1);
    q.insert("clone".into(), json!(c.base().schema_meta_version()));
    Value::Object(q)
}

/// domain.fetchAllSchemasWithTables (v2), fetchPolicies, fetchResourceGroups.
async fn fetch_all(
    m: &Mutator,
) -> Result<(Vec<DBInfo>, Vec<PolicyInfo>, Vec<ResourceGroupInfo>), Error> {
    let mut dbs = m.list_databases().await?;
    for di in &mut dbs {
        let (name2_id, mut tables, err) = m
            .get_all_name_to_id_and_the_must_loaded_table_info(di.id)
            .await;
        err?;
        di.table_name2_id = Some(
            name2_id
                .into_iter()
                .map(|(k, v)| (String::from_utf8(k).expect("utf-8 name"), v))
                .collect::<BTreeMap<_, _>>(),
        );
        for tb in &mut tables {
            convert_old_version_utf8_to_utf8mb4_if_need(tb);
        }
        for tb in &mut tables {
            convert_charset_collate_to_lower_case_if_need(tb);
        }
        di.deprecated.tables = Some(tables.into_iter().map(Arc::new).collect());
    }
    let policies = m.list_policies().await?;
    let groups = m.list_resource_groups().await?;
    Ok((dbs, policies, groups))
}

async fn full_load(
    req: &Arc<dyn Requirement>,
    data: &Arc<Data>,
    m: &Mutator,
    ver: i64,
    ts: u64,
) -> Result<InfoSchemaV2, Error> {
    let (dbs, policies, groups) = fetch_all(m).await?;
    let mut b = new_builder(Some(Arc::clone(req)), None, Arc::clone(data));
    b.init_with_db_infos(dbs, policies, groups, ver).await?;
    Ok(b.build(ts).await)
}

fn panic_msg(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// One record: Go `infoschemaExec`.
async fn exec(steps: &[Value]) -> Value {
    let store = Arc::new(MvccStore::default());
    let req: Arc<dyn Requirement> = Arc::new(Req(Arc::clone(&store)));
    let data = new_data();
    let mut built: Vec<InfoSchemaV2> = vec![];
    let mut latest: Option<InfoSchemaV2> = None;
    let mut out = vec![];
    for st in steps {
        let mut o = Map::new();
        let r = AssertUnwindSafe(step(
            st,
            &store,
            &req,
            &data,
            &mut built,
            &mut latest,
            &mut o,
        ))
        .catch_unwind()
        .await;
        if let Err(e) = r {
            o.insert("panic".into(), json!(panic_msg(e)));
        }
        out.push(Value::Object(o));
    }
    Value::Array(out)
}

async fn step(
    st: &Value,
    store: &Arc<MvccStore>,
    req: &Arc<dyn Requirement>,
    data: &Arc<Data>,
    built: &mut Vec<InfoSchemaV2>,
    latest: &mut Option<InfoSchemaV2>,
    o: &mut Map<String, Value>,
) {
    let ts = st["ts"].as_u64().unwrap_or(0);
    match st["k"].as_str().unwrap() {
        "full" => {
            let snap = Arc::new(mem_snap(&st["pairs"]));
            store.states.write().push((ts, Arc::clone(&snap)));
            let m = meta::new_reader(snap);
            match full_load(req, data, &m, st["ver"].as_i64().unwrap_or(0), ts).await {
                Ok(is) => {
                    o.insert("q".into(), queries(&is).await);
                    *latest = Some(is.clone());
                    built.push(is);
                }
                Err(e) => {
                    o.insert("err".into(), err_json(&e));
                }
            }
        }
        "diffs" => {
            let snap = Arc::new(mem_snap(&st["pairs"]));
            store.states.write().push((ts, Arc::clone(&snap)));
            let m = meta::new_reader(snap);
            let mut b = new_builder(Some(Arc::clone(req)), None, Arc::clone(data));
            b.init_with_old_info_schema(latest.as_ref().expect("a full load first"))
                .unwrap();
            b = b.with_store(Arc::clone(store) as _);
            b.set_delta_update_bundles();
            let diffs: Vec<SchemaDiff> = st["diffs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    let mut diff = SchemaDiff::default();
                    diff.unmarshal_json(d.to_string().as_bytes()).unwrap();
                    diff
                })
                .collect();
            let max_ver = diffs.iter().map(|d| d.version).max().unwrap_or(0);
            let mut ids = vec![];
            for diff in &diffs {
                match b.apply_diff(&m, diff).await {
                    Ok(v) => ids.push(json!(v)),
                    Err(e) => {
                        if !ids.is_empty() {
                            o.insert("ids".into(), Value::Array(ids));
                        }
                        o.insert("err".into(), err_json(&e));
                        // The domain falls back to a full load.
                        let is = full_load(req, data, &m, max_ver, ts).await.unwrap();
                        o.insert("q".into(), queries(&is).await);
                        *latest = Some(is.clone());
                        built.push(is);
                        return;
                    }
                }
                if !ids.is_empty() {
                    o.insert("ids".into(), Value::Array(ids.clone()));
                }
            }
            let is = b.build(ts).await;
            o.insert("q".into(), queries(&is).await);
            *latest = Some(is.clone());
            built.push(is);
        }
        "gc" => {
            let (d, total) = data.gc_old_version(st["ver"].as_i64().unwrap_or(0));
            o.insert("gc".into(), json!([d, total]));
        }
        "cap" => {
            data.set_cache_capacity(st["n"].as_u64().unwrap_or(0));
            let c = data.cache_capacity();
            if c != 0 {
                o.insert("cap".into(), json!(c));
            }
        }
        "query" => {
            let at = st["at"].as_u64().unwrap_or(0) as usize;
            let is = built[at % built.len()].clone();
            o.insert("q".into(), queries(&is).await);
        }
        k => panic!("unknown step {k}"),
    }
}

/// Go's nil and empty slices are the same here (`null` == `[]`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Array(v)) | (Value::Array(v), Value::Null) => v.is_empty(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            keys.into_iter().all(|k| {
                same(
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                )
            })
        }
        _ => a == b,
    }
}

/// The first differing path, for the report.
fn first_diff(a: &Value, b: &Value, path: &str) -> Option<String> {
    if same(a, b) {
        return None;
    }
    match (a, b) {
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (a, b)) in x.iter().zip(y).enumerate() {
                if let Some(d) = first_diff(a, b, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            None
        }
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                if let Some(d) = first_diff(
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    &format!("{path}.{k}"),
                ) {
                    return Some(d);
                }
            }
            None
        }
        _ => Some(format!(
            "{path}:\n      go   {}\n      rust {}",
            a.to_string().chars().take(1500).collect::<String>(),
            b.to_string().chars().take(1500).collect::<String>()
        )),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "infoschema");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        assert_eq!(rec["op"], "build");
        let steps = rec["args"][0].as_array().cloned().unwrap_or_default();
        let got = block_on(exec(&steps));
        if let Some(d) = first_diff(&rec["out"], &got, "out") {
            fails.push(format!("{} {d}", rec["src"]));
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails.iter().take(10).cloned().collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

/// The tests of this file run one at a time: `loadTableSF` is process-wide and keyed by
/// (schema ID, table ID, schema version), like Go's, which assumes one cluster per process;
/// two scenarios replayed at once are two clusters with the same IDs and versions.
static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn govec_infoschema() {
    let _serial = SERIAL.lock();
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_INFOSCHEMA_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

/// The query results that do not depend on the table cache (Go's `isQueries` minus the
/// `TableIsCached` probes and the clone).
fn cache_independent(q: Value) -> Value {
    let mut q = q;
    if let Value::Object(m) = &mut q {
        for k in ["cached0", "cached1", "cached2", "clone"] {
            m.remove(k);
        }
    }
    q
}

/// Exception A3 §5.1: the multi-version maps are updated in place under locks, not
/// copied. A reader pinned at version v must see the same results while another thread
/// applies schema diffs, full reloads (at later versions) and GC (cut <= v, the oldest
/// version present, as Go's GetOldestSchemaVersion guarantees) to the same `Data`.
#[test]
fn readers_at_a_fixed_version_while_the_builder_writes() {
    let _serial = SERIAL.lock();
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    // A scenario with many tables at its first version and at least 3 batches of diffs.
    let rec: Value = text
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|r| {
            r["args"][0]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["k"] == "diffs")
                .count()
                >= 3
        })
        .max_by_key(|r| r["out"][0]["q"]["iter"].as_array().map_or(0, Vec::len))
        .unwrap();
    let steps = rec["args"][0].as_array().unwrap().clone();
    assert_eq!(steps[0]["k"], "full");
    let v = steps[0]["ver"].as_i64().unwrap();

    let store = Arc::new(MvccStore::default());
    let req: Arc<dyn Requirement> = Arc::new(Req(Arc::clone(&store)));
    let data = new_data();
    let mut built = vec![];
    let mut latest = None;
    let mut o = Map::new();
    block_on(step(
        &steps[0],
        &store,
        &req,
        &data,
        &mut built,
        &mut latest,
        &mut o,
    ));
    let is_v = latest.clone().expect("the full load builds");
    let pinned = cache_independent(block_on(queries(&is_v)));

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (store, req, data, done) = (
            Arc::clone(&store),
            Arc::clone(&req),
            Arc::clone(&data),
            Arc::clone(&done),
        );
        let steps = steps.clone();
        std::thread::spawn(move || {
            let mut built = built;
            let mut latest = latest;
            for _ in 0..20 {
                for st in &steps[1..] {
                    let mut st = st.clone();
                    match st["k"].as_str().unwrap() {
                        // A full load at v itself would rewrite v's items (as in Go).
                        "full" if st["ver"].as_i64().unwrap_or(0) <= v => continue,
                        "gc" => st["ver"] = json!(st["ver"].as_i64().unwrap_or(0).min(v)),
                        _ => {}
                    }
                    let mut o = Map::new();
                    block_on(step(
                        &st,
                        &store,
                        &req,
                        &data,
                        &mut built,
                        &mut latest,
                        &mut o,
                    ));
                    // GC at the oldest version in use (Go: GetOldestSchemaVersion).
                    data.gc_old_version(v);
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (is_v, pinned, done) = (is_v.clone(), pinned.clone(), Arc::clone(&done));
            std::thread::spawn(move || {
                let mut n = 0;
                while !done.load(std::sync::atomic::Ordering::SeqCst) || n == 0 {
                    let got = cache_independent(block_on(queries(&is_v)));
                    if let Some(d) = first_diff(&pinned, &got, "q") {
                        panic!("version {v} changed under the writer: {d}");
                    }
                    n += 1;
                }
                n
            })
        })
        .collect();
    writer.join().expect("writer");
    let rounds: usize = readers.into_iter().map(|r| r.join().expect("reader")).sum();
    assert!(rounds > 0);
}
