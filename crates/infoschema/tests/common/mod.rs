//! The in-memory meta store of the infoschema tests: a sorted snapshot with Go's
//! `kv.Retriever` semantics ([`MemSnap`]) and a store serving snapshots by commit ts
//! ([`MvccStore`]).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql;
use tidb_infoschema::autoid::Requirement;
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
    OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry, new_value_entry,
};
use tidb_kv::version::Version;
use tidb_model::{ColumnInfo, DBInfo, IndexColumn, IndexInfo, StatePublic, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;

// ---------------------------------------------------------------- in-memory snapshot

/// The sorted key-value pairs of a state, with Go's `kv.Retriever` semantics (the govec
/// `memSnap`, as in govec_meta.rs).
pub struct MemSnap {
    keys: Vec<Vec<u8>>,
    vals: Vec<Vec<u8>>,
}

impl MemSnap {
    /// The pairs must be sorted by key.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> MemSnap {
        let (keys, vals): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        debug_assert!(keys.windows(2).all(|w| w[0] < w[1]));
        MemSnap { keys, vals }
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
    fn iter<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        upper: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
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
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        lower: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
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

/// The states by commit ts: `GetSnapshot(v)` is the newest state with ts <= v.
#[derive(Default)]
pub struct MvccStore {
    pub states: parking_lot::RwLock<Vec<(u64, Arc<MemSnap>)>>,
}

impl Storage for MvccStore {
    fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        unimplemented!("updateAutoIDForExchangePartition is X")
    }
    fn get_snapshot(&self, ver: Version) -> Arc<dyn Snapshot> {
        let mut snap = Arc::new(MemSnap::from_pairs([]));
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
        unimplemented!()
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

pub struct Req(pub Arc<MvccStore>);

impl Requirement for Req {
    fn store(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.0) as _
    }
}

// ---------------------------------------------------------------- schema fixtures

/// The database of [`db_info`].
pub const DB_ID: i64 = 1;

pub fn column(id: i64, offset: usize, tp: u8) -> Arc<ColumnInfo> {
    let mut ft = FieldType::new(tp);
    if tp == mysql::TypeVarchar {
        ft.set_flen(64);
        ft.set_charset("utf8mb4");
        ft.set_collate("utf8mb4_bin");
    }
    Arc::new(ColumnInfo {
        id,
        name: CIStr::new(&format!("c{offset}")),
        offset: offset as isize,
        field_type: ft,
        state: StatePublic,
        version: 2,
        ..Default::default()
    })
}

/// A public table: `ncols` columns (alternately BIGINT and VARCHAR(64)), `nidx` indices on
/// one or two columns each.
pub fn table_info(id: i64, ncols: usize, nidx: usize) -> TableInfo {
    let columns: Vec<Arc<ColumnInfo>> = (0..ncols)
        .map(|i| {
            let tp = if i % 2 == 0 {
                mysql::TypeLonglong
            } else {
                mysql::TypeVarchar
            };
            column(i as i64 + 1, i, tp)
        })
        .collect();
    let indices = (0..nidx)
        .map(|i| {
            let cols = (0..1 + i % 2)
                .map(|k| {
                    let off = (2 * i + k) % ncols;
                    IndexColumn {
                        name: columns[off].name.clone(),
                        offset: off as isize,
                        length: -1,
                        ..Default::default()
                    }
                })
                .collect();
            Arc::new(IndexInfo {
                id: i as i64 + 1,
                name: CIStr::new(&format!("i{i}")),
                table: CIStr::new(&format!("t{id}")),
                columns: Some(cols),
                state: StatePublic,
                ..Default::default()
            })
        })
        .collect();
    TableInfo {
        id,
        name: CIStr::new(&format!("t{id}")),
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        columns: Some(columns),
        indices: Some(indices),
        state: StatePublic,
        version: 5,
        max_column_id: ncols as i64,
        max_index_id: nidx as i64,
        ..Default::default()
    }
}

pub fn db_info() -> DBInfo {
    DBInfo {
        id: DB_ID,
        name: CIStr::new("test"),
        charset: "utf8mb4".into(),
        collate: "utf8mb4_bin".into(),
        state: StatePublic,
        ..Default::default()
    }
}
