//! The in-memory store of the domain tests: a snapshot per commit ts, the meta states
//! written in TiKV's meta layout (Go's `meta.Mutator` keys: the `DBs` and `DB:<id>` hashes,
//! `SchemaVersionKey`, `Diff:<v>`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use tidb_base::Error;
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
pub struct MemSnap(Pairs);

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
pub struct MvccStore {
    pub states: RwLock<Vec<(u64, Arc<MemSnap>)>>,
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

pub fn table(id: i64, name: &str) -> TableInfo {
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
pub fn state(version: i64, tables: &[&TableInfo], diffs: &[SchemaDiff]) -> MemSnap {
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

pub fn create_diff(version: i64, table_id: i64) -> SchemaDiff {
    SchemaDiff {
        version,
        type_: ActionCreateTable,
        schema_id: DB_ID,
        table_id,
        ..Default::default()
    }
}

pub async fn has_table(is: &tidb_infoschema::InfoSchemaV2, name: &str) -> bool {
    is.table_by_name(Ctx::background(), &CIStr::new("test"), &CIStr::new(name))
        .await
        .is_ok()
}
