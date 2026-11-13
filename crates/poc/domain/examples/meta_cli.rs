//! Mixed-cluster check of block D06.2 (scripts/meta-mixed.sh): reads the meta data a real
//! Go TiDB v8.5.8 wrote to TiKV with tidb-infoschema's `meta.Reader` and prints it in a
//! form the script compares with Go's SQL view.
//!
//! The meta range (`m`..`n`) of a playground is small: it is scanned at the latest
//! snapshot into memory and read through an in-memory `kv.Snapshot` with TiKV's key order.
//!
//!   meta_cli <pd> dbs         # "<db id>\t<db name>" per database (tab-separated output)
//!   meta_cli <pd> tables      # "<db name> <table id> <table name>" per table (ListTables)
//!   meta_cli <pd> nametoid    # "<db name> <table name> <table id>" (GetAllNameToID...)
//!   meta_cli <pd> jobs <n>    # "<job id> <schema> <table> <type>" of the last n history jobs
//!   meta_cli <pd> misc        # schema version (raw / non-empty diff), bootstrap version, MDL, ...

use std::collections::HashMap;
use std::sync::Arc;

use poc_kv::{KeyRange, KvClient};
use tidb_infoschema::meta;
use tidb_kv::Context;
use tidb_kv::error::ErrNotExist;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, GetOption, Getter, Iterator as KvIterator, KvFuture, OptionValue,
    Retriever, Snapshot, ValueEntry, new_value_entry,
};

/// Sorted pairs with Go's `kv.Retriever` semantics.
struct Pairs(Vec<(Vec<u8>, Vec<u8>)>);

impl Pairs {
    fn lower_bound(&self, k: &[u8]) -> usize {
        self.0.partition_point(|(x, _)| x.as_slice() < k)
    }
}

impl Getter for Pairs {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        k: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        let i = self.lower_bound(k);
        let r = match self.0.get(i) {
            Some((x, v)) if x == &k.0 => Ok(new_value_entry(v.clone(), 0)),
            _ => Err(ErrNotExist.clone()),
        };
        Box::pin(async move { r })
    }
}

struct It<'a> {
    p: &'a Pairs,
    i: isize,
    step: isize,
    bound: Vec<u8>,
    key: Key,
}

impl It<'_> {
    fn ok(&self) -> bool {
        if self.i < 0 || self.i as usize >= self.p.0.len() {
            return false;
        }
        let k = &self.p.0[self.i as usize].0;
        self.bound.is_empty()
            || if self.step > 0 {
                k.as_slice() < self.bound.as_slice()
            } else {
                k.as_slice() >= self.bound.as_slice()
            }
    }
    fn load(&mut self) {
        self.key = Key(if self.ok() {
            self.p.0[self.i as usize].0.clone()
        } else {
            vec![]
        });
    }
}

impl KvIterator for It<'_> {
    fn valid(&self) -> bool {
        self.ok()
    }
    fn key(&self) -> &Key {
        &self.key
    }
    fn value(&self) -> &[u8] {
        if self.ok() {
            &self.p.0[self.i as usize].1
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

impl Retriever for Pairs {
    fn iter<'a: 'f, 'f>(
        &'a self,
        k: &'f Key,
        upper: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        let mut it = It {
            p: self,
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
            self.0.len() as isize - 1
        } else {
            self.lower_bound(k) as isize - 1
        };
        let mut it = It {
            p: self,
            i,
            step: -1,
            bound: lower.0.clone(),
            key: Key(vec![]),
        };
        it.load();
        Box::pin(async move { Ok(Box::new(it) as Box<dyn KvIterator + 'a>) })
    }
}

impl BatchGetter for Pairs {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unimplemented!()
    }
}

impl Snapshot for Pairs {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pd = args[1].clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let kv = rt.block_on(KvClient::connect(vec![pd])).unwrap();
    let mut snap = kv.latest_snapshot();
    let pairs = rt
        .block_on(snap.scan_all(KeyRange::new(b"m".to_vec(), b"n".to_vec())))
        .unwrap();
    let m = meta::new_reader(Arc::new(Pairs(pairs)));
    rt.block_on(async {
        let dbs = m.list_databases().await.unwrap();
        match args[2].as_str() {
            "dbs" => {
                for db in &dbs {
                    println!("{}\t{}", db.id, db.name.o);
                }
            }
            "tables" => {
                for db in &dbs {
                    for t in m.list_tables(db.id).await.unwrap() {
                        assert_eq!(t.dbid, db.id);
                        println!("{}\t{}\t{}", db.name.l, t.id, t.name.o);
                    }
                }
            }
            "nametoid" => {
                for db in &dbs {
                    let (names, _, err) = m
                        .get_all_name_to_id_and_the_must_loaded_table_info(db.id)
                        .await;
                    err.unwrap();
                    for (n, id) in names {
                        println!("{}\t{}\t{}", db.name.l, String::from_utf8_lossy(&n), id);
                    }
                }
            }
            "jobs" => {
                let n: usize = args[3].parse().unwrap();
                let mut it = m.get_last_history_ddl_jobs_iterator().await.unwrap();
                for j in it.get_last_jobs(n).await.unwrap() {
                    println!(
                        "{}\t{}\t{}\t{}",
                        j.id, j.schema_name, j.table_name, j.type_.0
                    );
                }
            }
            "misc" => {
                let (v, e) = m.get_schema_version().await;
                e.unwrap();
                println!("schema_version {v}");
                println!(
                    "schema_version_non_empty {}",
                    m.get_schema_version_with_non_empty_diff().await.unwrap()
                );
                let (b, e) = m.get_bootstrap_version().await;
                e.unwrap();
                println!("bootstrap_version {b}");
                let (en, null, e) = m.get_metadata_lock().await;
                e.unwrap();
                println!("mdl {en} {null}");
                println!("system_db_id {}", m.get_system_db_id().await.unwrap());
                let (ver, e) = m.check_ddl_table_version().await;
                e.unwrap();
                println!("ddl_table_version {ver}");
                let (n, e) = m.get_history_ddl_count().await;
                e.unwrap();
                println!("history_ddl_count {n}");
                println!(
                    "resource_groups {}",
                    m.list_resource_groups().await.unwrap().len()
                );
                println!("policies {}", m.list_policies().await.unwrap().len());
            }
            o => panic!("unknown command {o}"),
        }
    });
}
