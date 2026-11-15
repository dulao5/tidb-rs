//! `loadInfoSchema` end to end over an in-memory store (D06.5 spec §5): a full load, a
//! diff load, a cache hit, an older version still readable by version, and the gap
//! threshold's full reload. The meta states are written in TiKV's meta layout (Go's
//! `meta.Mutator` keys: the `DBs` and `DB:<id>` hashes, `SchemaVersionKey`, `Diff:<v>`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use tidb_base::Error;
use tidb_domain::new_domain;
use tidb_infoschema::infoschema_v2::Ctx;
use tidb_infoschema::meta;
use tidb_infoschema::structure::new_structure;
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
    OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry, new_value_entry,
};
use tidb_kv::version::{Version, new_version};
use tidb_model::{ActionCreateTable, ColumnInfo, DBInfo, SchemaDiff, StatePublic, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;

type Pairs = BTreeMap<Vec<u8>, Vec<u8>>;

/// A snapshot: sorted pairs with Go's `kv.Retriever` semantics (forward only).
struct MemSnap(Pairs);

impl Getter for MemSnap {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        let r = match self.0.get(&k.0) {
            Some(v) => Ok(new_value_entry(v.clone(), 0)),
            None => Err(ErrNotExist.clone()),
        };
        Box::pin(async move { r })
    }
}

struct It {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    i: usize,
    key: Key,
}

impl KvIterator for It {
    fn valid(&self) -> bool {
        self.i < self.pairs.len()
    }
    fn key(&self) -> &Key {
        &self.key
    }
    fn value(&self) -> &[u8] {
        self.pairs.get(self.i).map_or(&[], |p| p.1.as_slice())
    }
    fn next(&mut self) -> KvFuture<'_, ()> {
        self.i += 1;
        self.key = Key(self
            .pairs
            .get(self.i)
            .map(|p| p.0.clone())
            .unwrap_or_default());
        Box::pin(async { Ok(()) })
    }
    fn close(&mut self) {}
}

impl Retriever for MemSnap {
    fn iter<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        upper: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        let pairs: Vec<_> = self
            .0
            .range(k.0.clone()..)
            .take_while(|(key, _)| upper.0.is_empty() || key.as_slice() < upper.0.as_slice())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let key = Key(pairs.first().map(|p| p.0.clone()).unwrap_or_default());
        Box::pin(async move { Ok(Box::new(It { pairs, i: 0, key }) as Box<dyn KvIterator + 'a>) })
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unimplemented!("schema loading does not iterate backwards")
    }
}

impl BatchGetter for MemSnap {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!("schema loading does not batch-get")
    }
}

impl Snapshot for MemSnap {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

/// The states by commit ts: `GetSnapshot(v)` is the newest state with ts <= v.
#[derive(Default)]
struct MvccStore {
    states: RwLock<Vec<(u64, Arc<MemSnap>)>>,
}

impl Storage for MvccStore {
    fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        unimplemented!()
    }
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        let mut snap = Arc::new(MemSnap(Pairs::new()));
        for (ts, s) in self.states.read().iter() {
            if *ts <= ver.ver {
                snap = Arc::clone(s);
            }
        }
        snap
    }
    fn get_client(&self) -> Arc<dyn Client> {
        unimplemented!()
    }
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn uuid(&self) -> String {
        String::new()
    }
    fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
        let ts = self.states.read().last().map_or(0, |s| s.0);
        Box::pin(async move { Ok(new_version(ts)) })
    }
    fn name(&self) -> String {
        String::new()
    }
    fn describe(&self) -> String {
        String::new()
    }
    fn get_min_safe_ts(&self, _: &str) -> u64 {
        0
    }
}

const DB_ID: i64 = 2;

fn table(id: i64, name: &str) -> TableInfo {
    let col = |cid: i64, n: &str| {
        Arc::new(ColumnInfo {
            id: cid,
            name: CIStr::new(n),
            offset: (cid - 1) as isize,
            field_type: FieldType::new(tidb_base::mysql::TypeLonglong),
            state: StatePublic,
            ..Default::default()
        })
    };
    TableInfo {
        id,
        name: CIStr::new(name),
        columns: Some(vec![col(1, "id"), col(2, "k")]),
        state: StatePublic,
        version: 5,
        max_column_id: 2,
        ..Default::default()
    }
}

/// The meta state at schema `version` with `tables` in database `test` and `diffs`.
fn state(version: i64, tables: &[&TableInfo], diffs: &[SchemaDiff]) -> MemSnap {
    let s = new_structure(Arc::new(MemSnap(Pairs::new())), b"m");
    let db = DBInfo {
        id: DB_ID,
        name: CIStr::new("test"),
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        state: StatePublic,
        ..Default::default()
    };
    let mut pairs = Pairs::new();
    pairs.insert(
        s.encode_hash_data_key(b"DBs", &meta::db_key_of(DB_ID)),
        db.marshal_json().unwrap().into_bytes(),
    );
    for t in tables {
        pairs.insert(
            s.encode_hash_data_key(&meta::db_key_of(DB_ID), &meta::table_key_of(t.id)),
            t.marshal_json().unwrap().into_bytes(),
        );
    }
    pairs.insert(
        s.encode_string_data_key(b"SchemaVersionKey"),
        version.to_string().into_bytes(),
    );
    for d in diffs {
        pairs.insert(
            s.encode_string_data_key(format!("Diff:{}", d.version).as_bytes()),
            d.marshal_json().unwrap().into_bytes(),
        );
    }
    MemSnap(pairs)
}

fn create_diff(version: i64, table_id: i64) -> SchemaDiff {
    SchemaDiff {
        version,
        type_: ActionCreateTable,
        schema_id: DB_ID,
        table_id,
        ..Default::default()
    }
}

async fn has_table(is: &tidb_infoschema::InfoSchemaV2, name: &str) -> bool {
    is.table_by_name(Ctx::background(), &CIStr::new("test"), &CIStr::new(name))
        .await
        .is_ok()
}

#[tokio::test]
async fn full_load_then_diff_load() {
    let store = Arc::new(MvccStore::default());
    let t1 = table(110, "sbtest1");
    let d1 = create_diff(1, 110);
    store
        .states
        .write()
        .push((10, Arc::new(state(1, &[&t1], std::slice::from_ref(&d1)))));
    let domain = new_domain(Arc::clone(&store) as Arc<dyn Storage>);

    // The first load is a full load.
    let r = domain.reload_at(10).await.unwrap();
    assert_eq!(r.is.schema_meta_version(), 1);
    assert!(!r.hit_cache);
    assert!(r.changes.is_none(), "a full load");
    assert!(has_table(&r.is, "sbtest1").await);
    // TableByName refilled the table cache: the planner's lookup without TiKV reads finds it.
    assert!(
        r.is.table_by_name_resident(&CIStr::new("test"), &CIStr::new("sbtest1"))
            .unwrap()
            .is_some()
    );

    // Same version: from the cache, with the new timestamp.
    let r = domain.reload_at(15).await.unwrap();
    assert!(r.hit_cache);
    assert_eq!(r.is.schema_meta_version(), 1);

    // CREATE TABLE sbtest2 at version 2: a diff load.
    let t2 = table(111, "sbtest2");
    let d2 = create_diff(2, 111);
    store.states.write().push((
        20,
        Arc::new(state(2, &[&t1, &t2], &[d1.clone(), d2.clone()])),
    ));
    let r = domain.reload_at(20).await.unwrap();
    assert_eq!(r.is.schema_meta_version(), 2);
    assert_eq!(r.old_schema_version, 1);
    let changes = r.changes.expect("a diff load");
    assert_eq!(changes.phy_tbl_ids, vec![111]);
    assert_eq!(changes.action_types, vec![ActionCreateTable.0 as u64]);
    assert!(has_table(&r.is, "sbtest2").await);
    assert!(has_table(&r.is, "sbtest1").await);
    assert_eq!(domain.info_schema().unwrap().schema_meta_version(), 2);

    // Version 1 is still in the InfoCache and does not see sbtest2.
    let v1 = domain.info_cache().get_by_version(1).unwrap();
    assert!(!has_table(&v1, "sbtest2").await);
    assert!(has_table(&v1, "sbtest1").await);

    // Version 3 has an empty diff (Diff:3 missing): the newest version with a non-empty diff
    // stays 2, a cache hit.
    store
        .states
        .write()
        .push((30, Arc::new(state(3, &[&t1, &t2], &[d1, d2]))));
    let r = domain.reload_at(30).await.unwrap();
    assert!(r.hit_cache);
    assert_eq!(r.is.schema_meta_version(), 2);
}

/// A gap of at least `LoadSchemaDiffVersionGapThreshold` versions is a full reload.
#[tokio::test]
async fn large_gap_is_a_full_reload() {
    let store = Arc::new(MvccStore::default());
    let t1 = table(110, "sbtest1");
    store
        .states
        .write()
        .push((10, Arc::new(state(1, &[&t1], &[create_diff(1, 110)]))));
    let domain = new_domain(Arc::clone(&store) as Arc<dyn Storage>);
    domain.reload_at(10).await.unwrap();
    let t2 = table(111, "sbtest2");
    let v = 1 + tidb_domain::domain::LOAD_SCHEMA_DIFF_VERSION_GAP_THRESHOLD;
    store
        .states
        .write()
        .push((20, Arc::new(state(v, &[&t1, &t2], &[create_diff(v, 111)]))));
    let r = domain.reload_at(20).await.unwrap();
    assert_eq!(r.is.schema_meta_version(), v);
    assert!(r.changes.is_none(), "a full load");
    assert!(has_table(&r.is, "sbtest2").await);
}
