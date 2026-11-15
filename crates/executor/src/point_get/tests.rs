//! Tests of the point get executor over an in-memory snapshot: the builder (snapshot and
//! its options, the checks before it), `Next` (handle path, a missing row, defaults of added
//! columns, the stubs), `Init` on reuse, the shared field types (N13).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_chunk::chunk::Chunk;
use tidb_expression::Column;
use tidb_expression::schema::new_schema;
use tidb_kv::context::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::{Handle, IntHandle, Key};
use tidb_kv::kv::Iterator as KvIterator;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, KvFuture, OptionValue, Retriever, Snapshot,
    ValueEntry,
};
use tidb_kv::option as kvopt;
use tidb_model::{IndexInfo, TableInfo};
use tidb_tablecodec::rowcodec::Encoder;
use tidb_tablecodec::tablecodec;
use tidb_types::datum::Datum;

use super::*;
use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::new_executor_builder;
use crate::exec::ExecCtx;
use crate::internal::exec::executor as exec;

/// A snapshot over a map; records the keys read and the options set.
#[derive(Default)]
pub(crate) struct MemSnapshot {
    pub data: HashMap<Vec<u8>, Vec<u8>>,
    pub gets: Mutex<Vec<Vec<u8>>>,
    pub options: Mutex<Vec<isize>>,
}

impl Getter for MemSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        self.gets.lock().push(k.0.clone());
        let v = self.data.get(&k.0).cloned();
        Box::pin(async move {
            match v {
                Some(value) => Ok(ValueEntry {
                    value,
                    commit_ts: 0,
                }),
                None => Err(ErrNotExist.clone()),
            }
        })
    }
}
impl Retriever for MemSnapshot {
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!()
    }
}
impl BatchGetter for MemSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}
impl Snapshot for MemSnapshot {
    fn set_option(&self, opt: isize, _val: OptionValue) {
        self.options.lock().push(opt);
    }
}

/// t(id BIGINT PRIMARY KEY (the int handle), c VARCHAR(10), d INT DEFAULT 7 added later,
/// v INT AS (d + 1) VIRTUAL, u INT UNIQUE KEY).
const TABLE: &str = r#"{"id":100,"name":{"O":"t","L":"t"},"charset":"utf8mb4","collate":"utf8mb4_bin","pk_is_handle":true,"state":5,"version":5,
"cols":[
 {"id":1,"name":{"O":"id","L":"id"},"offset":0,"type":{"Tp":8,"Flag":3,"Flen":20,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2},
 {"id":2,"name":{"O":"c","L":"c"},"offset":1,"type":{"Tp":15,"Flag":0,"Flen":10,"Decimal":0,"Charset":"utf8mb4","Collate":"utf8mb4_bin"},"state":5,"version":2},
 {"id":3,"name":{"O":"d","L":"d"},"offset":2,"origin_default":"7","default":"7","type":{"Tp":3,"Flag":0,"Flen":11,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2},
 {"id":4,"name":{"O":"v","L":"v"},"offset":3,"generated_expr_string":"`d` + 1","generated_stored":false,"type":{"Tp":3,"Flag":0,"Flen":11,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2},
 {"id":5,"name":{"O":"u","L":"u"},"offset":4,"type":{"Tp":3,"Flag":4,"Flen":11,"Decimal":0,"Charset":"binary","Collate":"binary"},"state":5,"version":2}],
"index_info":[
 {"id":1,"idx_name":{"O":"PRIMARY","L":"primary"},"idx_cols":[{"name":{"O":"id","L":"id"},"offset":0,"length":-1}],"is_unique":true,"is_primary":true,"state":5},
 {"id":2,"idx_name":{"O":"u","L":"u"},"idx_cols":[{"name":{"O":"u","L":"u"},"offset":4,"length":-1}],"is_unique":true,"state":5}]}"#;

fn table() -> Arc<TableInfo> {
    let mut t = TableInfo::default();
    t.unmarshal_json(TABLE.as_bytes()).unwrap();
    Arc::new(t)
}

fn tweak(f: impl FnOnce(&mut TableInfo)) -> Arc<TableInfo> {
    let mut t = TableInfo::clone(&table());
    f(&mut t);
    Arc::new(t)
}

/// The plan schema of the public columns at `offs` (Go's `colInfoToColumn`). The virtual
/// column `v` stands for `d` (its expression reads `d`'s schema column).
fn plan(tbl: &Arc<TableInfo>, offs: &[usize], handle: i64) -> PointGetPlan {
    let cols = tbl.columns.as_deref().unwrap();
    let mk = |i: usize, o: usize| Column {
        ret_type: Arc::new(cols[o].field_type.clone()),
        id: cols[o].id,
        unique_id: 10 + i as i64,
        index: i as isize,
        ..Column::default()
    };
    let schema = new_schema(
        offs.iter()
            .enumerate()
            .map(|(i, &o)| {
                let mut c = mk(i, o);
                if o == 3 {
                    let d = offs.iter().position(|&x| x == 2).expect("v needs d");
                    c.virtual_expr = Some(Arc::new(tidb_expression::expression::Expr::Column(mk(
                        d, 2,
                    ))));
                }
                Arc::new(c)
            })
            .collect(),
    );
    let mut p = PointGetPlan {
        schema,
        tbl_info: Arc::clone(tbl),
        handle: Some(Handle::Int(IntHandle(handle))),
        columns: cols.to_vec(),
        ..PointGetPlan::default()
    };
    p.set_id(7);
    p
}

/// The stored row of handle `h` (row format v2): only `c` (d was added later).
fn snapshot_with_row(tbl_id: i64, h: i64, c: &str) -> MemSnapshot {
    let mut enc = Encoder::default();
    enc.enable = true;
    let mut d = Datum::default();
    d.set_string(c.as_bytes().to_vec(), "utf8mb4_bin".to_owned());
    let val = tablecodec::encode_row(None, &[d], &[2], None, Some(&mut enc)).unwrap();
    let key = tablecodec::encode_row_key_with_handle(tbl_id, &Handle::Int(IntHandle(h)));
    let mut s = MemSnapshot::default();
    s.data.insert(key.0, val);
    s
}

async fn build(s: &mut MockSession, p: PointGetPlan) -> Result<Box<dyn Executor>, Error> {
    let mut b = new_executor_builder(s, None, Arc::new(TelemetryInfo::default()));
    let plan: crate::glue::Plan = Arc::new(p);
    let e = b.build(s, Some(&plan)).await;
    match b.err {
        Some(err) => Err(err),
        None => Ok(e.expect("built")),
    }
}

async fn rows(e: &mut dyn Executor, s: &MockSession) -> Vec<Vec<Datum>> {
    let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
    exec::open(&ctx, e).await.unwrap();
    let mut out = Vec::new();
    let mut chk: Chunk = exec::new_first_chunk(e);
    loop {
        exec::next(&ctx, e, &mut chk).await.unwrap();
        if chk.num_rows() == 0 {
            break;
        }
        let fts: Vec<_> = e.ret_field_types().iter().map(|f| (**f).clone()).collect();
        for r in 0..chk.num_rows() {
            out.push(chk.get_row(r).get_datum_row(&fts));
        }
    }
    exec::close(e).await.unwrap();
    out
}

fn ints(r: &[Datum]) -> Vec<String> {
    r.iter()
        .map(|d| {
            if d.is_null() {
                "NULL".to_owned()
            } else {
                String::from_utf8_lossy(&d.to_string().unwrap()).into_owned()
            }
        })
        .collect()
}

#[tokio::test]
async fn reads_the_row_of_an_int_handle() {
    let tbl = table();
    let snap = Arc::new(snapshot_with_row(100, 5, "x"));
    let mut s = MockSession::new(u64::MAX);
    s.snapshot = Some(Arc::clone(&snap) as Arc<dyn Snapshot>);
    let mut e = build(&mut s, plan(&tbl, &[0, 1, 2], 5)).await.unwrap();
    let got = rows(e.as_mut(), &s).await;
    // id from the handle, c from the row, d (added column) from its origin default.
    assert_eq!(got.len(), 1);
    assert_eq!(ints(&got[0]), ["5", "x", "7"]);
    // One read, of the record key of (100, 5).
    let key = tablecodec::encode_row_key_with_handle(100, &Handle::Int(IntHandle(5)));
    assert_eq!(*snap.gets.lock(), vec![key.0]);
    // getSnapshot's options, then Close's CollectRuntimeStats reset is not set (no stats).
    let opts = snap.options.lock().clone();
    for o in [
        kvopt::ReadReplicaScope,
        kvopt::TaskID,
        kvopt::TiKVClientReadTimeout,
        kvopt::ResourceGroupName,
        kvopt::ExplicitRequestSourceType,
    ] {
        assert!(opts.contains(&o), "option {o} not set: {opts:?}");
    }
    assert!(!opts.contains(&kvopt::MatchStoreLabels));
    assert!(
        s.vars
            .stmt_ctx()
            .is_tikv
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test]
async fn a_missing_row_gives_no_rows() {
    let tbl = table();
    let mut s = MockSession::new(u64::MAX);
    s.snapshot = Some(Arc::new(snapshot_with_row(100, 5, "x")));
    let mut e = build(&mut s, plan(&tbl, &[0, 1], 6)).await.unwrap();
    assert!(rows(e.as_mut(), &s).await.is_empty());
}

#[tokio::test]
async fn reinit_reads_the_new_handle_and_fills_virtual_columns() {
    let tbl = table();
    let mut snap = snapshot_with_row(100, 5, "x");
    let other = snapshot_with_row(100, 9, "y");
    snap.data.extend(other.data);
    let mut s = MockSession::new(u64::MAX);
    s.snapshot = Some(Arc::new(snap));
    let mut e = build(&mut s, plan(&tbl, &[3, 0, 1, 2], 5)).await.unwrap();
    // v (virtual, = d) is filled after the decode.
    assert_eq!(ints(&rows(e.as_mut(), &s).await[0]), ["7", "5", "x", "7"]);

    // ExecStmt.PointGet's reuse at MaxTS: Init with the next execution's plan.
    let p2: crate::glue::Plan = Arc::new(plan(&tbl, &[3, 0, 1, 2], 9));
    e.as_point_get().unwrap().init(&mut s, &p2).unwrap();
    assert_eq!(ints(&rows(e.as_mut(), &s).await[0]), ["7", "9", "y", "7"]);
    assert!(!e.as_point_get().unwrap().is_partitioned());
}

#[test]
fn init_shares_the_schema_field_types() {
    let tbl = table();
    let mut s = MockSession::new(u64::MAX);
    let p = plan(&tbl, &[0, 3, 2], 5);
    let mut e = PointGetExecutor::new(&mut s, &p, None);
    e.init(&mut s, &p);
    // The virtual column's type is the schema column's own (N13: shared, not copied).
    assert_eq!(e.virtual_column_index, vec![1]);
    assert!(Arc::ptr_eq(
        &e.virtual_column_ret_field_types[0],
        &p.schema.columns[1].ret_type
    ));
    // So are the executor's result field types.
    assert!(Arc::ptr_eq(
        &e.ret_field_types()[2],
        &p.schema.columns[2].ret_type
    ));
}

#[tokio::test]
async fn locks_and_unported_paths_stop_at_their_blocks() {
    let tbl = table();
    let mut s = MockSession::new(u64::MAX);
    s.snapshot = Some(Arc::new(MemSnapshot::default()));

    // SELECT ... FOR UPDATE: the write path.
    let mut p = plan(&tbl, &[0], 5);
    p.lock = true;
    let err = build(&mut s, p).await.err().unwrap();
    assert_eq!(blocked_by(&err), Some("X"));

    // Partitioned table: pruning by value needs the partitioned table (D06.4 part 2).
    let pt = tweak(|t| {
        t.partition = Some(Box::new(tidb_model::PartitionInfo {
            enable: true,
            ..Default::default()
        }))
    });
    let err = build(&mut s, plan(&pt, &[0], 5)).await.err().unwrap();
    assert_eq!(blocked_by(&err), Some("D06.4"));

    // Cached table: D06.4 part 2.
    let ct = tweak(|t| t.table_cache_status_type = tidb_model::TableCacheStatusEnable);
    let err = build(&mut s, plan(&ct, &[0], 5)).await.err().unwrap();
    assert_eq!(blocked_by(&err), Some("D06.4"));

    // Local temporary table under tidb_snapshot: Go's error.
    let lt = tweak(|t| t.temp_table_type = tidb_model::TempTableLocal);
    s.vars.snapshot_ts = 1;
    let err = build(&mut s, plan(&lt, &[0], 5)).await.err().unwrap();
    assert_eq!(
        err.to_string(),
        "can not read local temporary table when 'tidb_snapshot' is set"
    );
    s.vars.snapshot_ts = 0;
}

/// A unique index point get (`u = 3`): the index key, the handle in the index value, the row;
/// no index entry; an index entry without its row (the inconsistency error).
#[tokio::test]
async fn unique_index_point_get() {
    let tbl = table();
    let idx = Arc::clone(&tbl.indices.as_ref().unwrap()[1]);
    let mut d = Datum::default();
    d.set_int64(3);
    let mut enc_d = Vec::new();
    tidb_codec::encode_key(&tidb_base::gostd::time::utc(), &mut enc_d, &[d.clone()]).unwrap();
    let idx_key = tablecodec::encode_index_seek_key(100, 2, &enc_d).0;
    let idx_val =
        tablecodec::encode_handle_in_unique_index_value(&Handle::Int(IntHandle(5)), false);
    let row_key = tablecodec::encode_row_key_with_handle(100, &Handle::Int(IntHandle(5))).0;
    let mk_plan = || {
        let mut p = plan(&tbl, &[0, 1], 0);
        p.handle = None;
        p.index_info = Some(Arc::clone(&idx));
        p.index_values = vec![d.clone()];
        p
    };
    async fn run(
        snap: MemSnapshot,
        p: PointGetPlan,
    ) -> (MockSession, Result<Vec<Vec<Datum>>, Error>, Vec<Vec<u8>>) {
        let snap = Arc::new(snap);
        let mut s = MockSession::new(u64::MAX);
        s.snapshot = Some(Arc::clone(&snap) as Arc<dyn Snapshot>);
        let s = &mut s;
        let mut e = build(s, p).await.unwrap();
        let ctx = ExecCtx::new(s.vars.stmt_ctx_arc());
        exec::open(&ctx, e.as_mut()).await.unwrap();
        let mut chk = exec::new_first_chunk(e.as_ref());
        let res = exec::next(&ctx, e.as_mut(), &mut chk).await.map(|()| {
            let fts = [
                tidb_types::field_type::new_field_type(tidb_base::mysql::TypeLonglong),
                tidb_types::field_type::new_field_type(tidb_base::mysql::TypeVarchar),
            ];
            (0..chk.num_rows())
                .map(|i| chk.get_row(i).get_datum_row(&fts))
                .collect()
        });
        drop(e);
        let gets = snap.gets.lock().clone();
        (std::mem::replace(s, MockSession::new(u64::MAX)), res, gets)
    }

    // The index entry and the row.
    let mut snap = snapshot_with_row(100, 5, "x");
    snap.data.insert(idx_key.clone(), idx_val.clone());
    let (s, rows, gets) = run(snap, mk_plan()).await;
    let rows = rows.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].get_int64(), 5);
    assert_eq!(rows[0][1].get_string(), b"x");
    assert_eq!(gets, [idx_key.clone(), row_key.clone()]);
    assert_eq!(s.vars.stmt_ctx().index_names, ["t:u"]);

    // No index entry: no row, the row key is not read.
    let (_, rows, gets) = run(MemSnapshot::default(), mk_plan()).await;
    assert!(rows.unwrap().is_empty());
    assert_eq!(gets, std::slice::from_ref(&idx_key));

    // The index entry without its row: Go's ReportLookupInconsistent error.
    let mut snap = MemSnapshot::default();
    snap.data.insert(idx_key.clone(), idx_val);
    let (_, rows, gets) = run(snap, mk_plan()).await;
    let err = rows.unwrap_err();
    assert_eq!(
        err.to_string(),
        "[executor:8133]data inconsistency in table: t, index: u, index-count:1 != record-count:0"
    );
    assert_eq!(gets, [idx_key, row_key]);
}

#[test]
fn phys_id_and_partition_names() {
    let tbl = table();
    assert_eq!(get_phys_id(&tbl, None), 100);
    let pi: tidb_model::PartitionInfo = tidb_model::PartitionInfo {
        enable: true,
        definitions: Some(vec![
            tidb_model::PartitionDefinition {
                id: 101,
                name: CIStr::new("p0"),
                ..Default::default()
            },
            tidb_model::PartitionDefinition {
                id: 102,
                name: CIStr::new("p1"),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let pt = tweak(|t| t.partition = Some(Box::new(pi.clone())));
    assert_eq!(get_phys_id(&pt, Some(1)), 102);
    assert!(match_partition_names(102, &[], &pi));
    assert!(match_partition_names(102, &[CIStr::new("P1")], &pi));
    assert!(!match_partition_names(102, &[CIStr::new("p0")], &pi));
    assert!(!match_partition_names(103, &[CIStr::new("p0")], &pi));
}

#[test]
fn runtime_stats_with_snapshot() {
    let mut a = RuntimeStatsWithSnapshot::default();
    assert_eq!(a.string(), "");
    assert_eq!(a.tp(), TpRuntimeStatsWithSnapshot);
    let b = RuntimeStatsWithSnapshot {
        snapshot_runtime_stats: Some(SnapshotRuntimeStats::default()),
    };
    a.merge(&b);
    assert!(a.snapshot_runtime_stats.is_some());
    let c = a.clone_stats();
    assert!(
        c.as_any()
            .downcast_ref::<RuntimeStatsWithSnapshot>()
            .unwrap()
            .snapshot_runtime_stats
            .is_some()
    );
}

#[test]
fn index_info_is_shared() {
    let tbl = table();
    let idx: &Arc<IndexInfo> = &tbl.indices.as_ref().unwrap()[1];
    let mut p = plan(&tbl, &[0], 1);
    p.index_info = Some(Arc::clone(idx));
    assert!(Arc::ptr_eq(p.index_info.as_ref().unwrap(), idx));
}
