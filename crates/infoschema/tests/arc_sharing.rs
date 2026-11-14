//! Sharing of `TableInfo` columns and indices between the infoschema and its table objects,
//! across schema versions, and their release after GC (spec N13, issue #229).
//!
//! The DDL rounds run on InfoSchema v2 the way the domain applies them: a builder from the
//! newest version, `ApplyDiff` against the meta state of the diff, `Build`, and from time
//! to time `GCOldVersion` at the oldest version still in use.
//!
//! `arc_sharing_ddl_rounds_free_old_versions` runs 2,000 rounds (6,000 schema versions) in
//! a few seconds; `ARC_SHARING_ROUNDS` sets another count.

mod common;

use std::sync::{Arc, Weak};

use futures::executor::block_on;
use tidb_infoschema::infoschema_v2::{Ctx, InfoSchemaV2};
use tidb_infoschema::meta;
use tidb_infoschema::structure::new_structure;
use tidb_infoschema::{Data, new_builder, new_data};
use tidb_model::{
    ActionCreateTable, ActionDropTable, ActionModifyColumn, ColumnInfo, IndexInfo, SchemaDiff,
    TableInfo,
};

use common::{DB_ID, MemSnap, MvccStore, Req, db_info, table_info};

/// A table that no DDL touches.
const ANCHOR_ID: i64 = 2;

/// The meta state with the database and `tables` (Go's `DBs` hash and `DB:<id>` hash).
fn meta_state(tables: &[&TableInfo]) -> MemSnap {
    let s = new_structure(Arc::new(MemSnap::from_pairs([])), b"m");
    let mut pairs = vec![(
        s.encode_hash_data_key(b"DBs", &meta::db_key_of(DB_ID)),
        db_info().marshal_json().unwrap().into_bytes(),
    )];
    for t in tables {
        pairs.push((
            s.encode_hash_data_key(&meta::db_key_of(DB_ID), &meta::table_key_of(t.id)),
            t.marshal_json().unwrap().into_bytes(),
        ));
    }
    pairs.sort();
    MemSnap::from_pairs(pairs)
}

struct Env {
    store: Arc<MvccStore>,
    req: Arc<dyn tidb_infoschema::autoid::Requirement>,
    data: Arc<Data>,
    latest: InfoSchemaV2,
    ver: i64,
    /// Keep only the newest meta state (no older snapshot is read again).
    drop_old_states: bool,
}

impl Env {
    /// A full load of the database with the anchor table at version 1.
    fn new() -> Env {
        let store = Arc::new(MvccStore::default());
        let req: Arc<dyn tidb_infoschema::autoid::Requirement> = Arc::new(Req(store.clone()));
        let data = new_data();
        let anchor = table_info(ANCHOR_ID, 30, 3);
        store
            .states
            .write()
            .push((1, Arc::new(meta_state(&[&anchor]))));
        let mut db = db_info();
        db.deprecated.tables = Some(vec![Arc::new(anchor)]);
        let mut b = new_builder(Some(req.clone()), None, data.clone());
        block_on(b.init_with_db_infos(vec![db], vec![], vec![], 1)).unwrap();
        let latest = block_on(b.build(1));
        Env {
            store,
            req,
            data,
            latest,
            ver: 1,
            drop_old_states: false,
        }
    }

    /// Applies one diff whose meta state has `tables` and builds the next version.
    fn apply(&mut self, tp: tidb_model::ActionType, table_id: i64, tables: &[&TableInfo]) {
        self.ver += 1;
        let ts = self.ver as u64;
        let snap = Arc::new(meta_state(tables));
        self.store.states.write().push((ts, snap.clone()));
        let m = meta::new_reader(snap);
        let mut b = new_builder(Some(self.req.clone()), None, self.data.clone());
        b.init_with_old_info_schema(&self.latest).unwrap();
        b = b.with_store(self.store.clone());
        let diff = SchemaDiff {
            version: self.ver,
            type_: tp,
            schema_id: DB_ID,
            table_id,
            ..Default::default()
        };
        block_on(b.apply_diff(&m, &diff)).unwrap();
        self.latest = block_on(b.build(ts));
        if self.drop_old_states {
            let mut states = self.store.states.write();
            let n = states.len();
            states.drain(..n - 1);
        }
    }

    fn table(&self, id: i64) -> Arc<dyn tidb_infoschema::table::Table> {
        block_on(self.latest.table_by_id(Ctx::background(), id)).expect("table exists")
    }
}

/// The table object shares the `ColumnInfo` / `IndexInfo` values of its `TableInfo`.
fn assert_table_object_shares(t: &dyn tidb_infoschema::table::Table) {
    let info = t.meta();
    let cols = info.columns.as_deref().unwrap();
    assert_eq!(t.cols().len(), cols.len());
    for (c, ci) in t.cols().iter().zip(cols) {
        assert!(Arc::ptr_eq(&c.info, ci), "column {} copied", ci.name.o);
    }
    let idxs = info.indices.as_deref().unwrap();
    assert_eq!(t.indices().len(), idxs.len());
    for (i, ii) in t.indices().iter().zip(idxs) {
        assert!(Arc::ptr_eq(i.meta(), ii), "index {} copied", ii.name.o);
    }
}

#[test]
fn table_objects_share_column_and_index_infos() {
    let env = Env::new();
    let anchor = env.table(ANCHOR_ID);
    assert_table_object_shares(anchor.as_ref());
    // The infoschema's TableInfo is the table object's.
    let info = block_on(env.latest.table_info_by_id(ANCHOR_ID)).unwrap();
    assert!(Arc::ptr_eq(&info, anchor.meta()));
}

/// Reviewer request on #229: a diff that changes one column. Go re-reads the changed table
/// from meta (`applyTableUpdate` → `applyCreateTable` → `m.GetTable`, builder.go:801), so
/// every column of the changed table is a new value in the new version; tables the diff
/// does not touch keep the same `TableInfo` and columns (Go: the same pointers). The
/// old version is unchanged.
#[test]
fn modify_column_diff_shares_unchanged_tables_only() {
    let mut env = Env::new();
    let t = table_info(10, 4, 1);
    env.apply(ActionCreateTable, 10, &[&table_info(ANCHOR_ID, 30, 3), &t]);
    let old = env.latest.clone();
    let old_t = block_on(old.table_info_by_id(10)).unwrap();
    let old_anchor = block_on(old.table_info_by_id(ANCHOR_ID)).unwrap();

    let mut t2 = t.clone();
    tidb_model::make_mut(&mut t2.columns.as_mut().unwrap()[1])
        .field_type
        .set_flen(128);
    env.apply(
        ActionModifyColumn,
        10,
        &[&table_info(ANCHOR_ID, 30, 3), &t2],
    );

    let new_t = block_on(env.latest.table_info_by_id(10)).unwrap();
    let new_anchor = block_on(env.latest.table_info_by_id(ANCHOR_ID)).unwrap();
    assert!(Arc::ptr_eq(&old_anchor, &new_anchor));
    let (oc, nc) = (
        old_t.columns.as_deref().unwrap(),
        new_t.columns.as_deref().unwrap(),
    );
    assert_eq!(nc[1].field_type.get_flen(), 128);
    assert_eq!(
        oc[1].field_type.get_flen(),
        64,
        "the old version is unchanged"
    );
    for (o, n) in oc.iter().zip(nc) {
        assert!(!Arc::ptr_eq(o, n), "re-read from meta, as Go");
    }
    assert_table_object_shares(env.table(10).as_ref());
    assert_table_object_shares(
        block_on(old.table_by_id(Ctx::background(), 10))
            .unwrap()
            .as_ref(),
    );
}

/// Owner request on #229 (dangling / never-freed shared objects): thousands of
/// create / alter / drop rounds, then the `TableInfo`, `ColumnInfo` and `IndexInfo` values
/// of every version older than the GC point are freed.
///
/// What keeps an old version's values alive is the table cache (`Data.tableCache`, a SIEVE
/// keyed by table ID and schema version): `GCOldVersion` removes the name / ID items only,
/// and `Data.remove` removes the cache entry of the drop version, not of the versions that
/// created the table. Go has the same: old entries leave the cache by eviction, bounded by
/// `tidb_schema_cache_size`. So the rounds run with a small cache, the number of values
/// alive before the final eviction is bounded by it (not by the number of rounds), and
/// after evicting the cache none is alive.
#[test]
fn arc_sharing_ddl_rounds_free_old_versions() {
    let rounds: usize = std::env::var("ARC_SHARING_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    // tidb_schema_cache_size (NewBuilder sets the cache capacity from it).
    tidb_sessionctx::variable::tidb_vars::SchemaCacheSize
        .store(256 << 10, std::sync::atomic::Ordering::SeqCst);
    let mut env = Env::new();
    env.drop_old_states = true;
    let anchor_info = table_info(ANCHOR_ID, 30, 3);
    let mut weak_tables: Vec<Weak<TableInfo>> = vec![];
    let mut weak_cols: Vec<Weak<ColumnInfo>> = vec![];
    let mut weak_idxs: Vec<Weak<IndexInfo>> = vec![];
    let mut track = |env: &Env, id: i64| {
        let info = block_on(env.latest.table_info_by_id(id)).unwrap();
        // The table object is in the version's table cache.
        assert_table_object_shares(env.table(id).as_ref());
        weak_tables.push(Arc::downgrade(&info));
        weak_cols.extend(info.columns.iter().flatten().map(Arc::downgrade));
        weak_idxs.extend(info.indices.iter().flatten().map(Arc::downgrade));
    };
    for r in 0..rounds {
        let id = 100 + r as i64;
        let t = table_info(id, 8, 2);
        env.apply(ActionCreateTable, id, &[&anchor_info, &t]);
        track(&env, id);
        let mut t2 = t.clone();
        tidb_model::make_mut(&mut t2.columns.as_mut().unwrap()[3])
            .field_type
            .set_flen(128);
        env.apply(ActionModifyColumn, id, &[&anchor_info, &t2]);
        track(&env, id);
        env.apply(ActionDropTable, id, &[&anchor_info]);
        // The domain GCs at the oldest version in use (GetOldestSchemaVersion); here no
        // older snapshot is in use.
        if r % 100 == 99 {
            env.data.gc_old_version(env.ver);
        }
    }
    env.data.gc_old_version(env.ver);
    let alive = |w_t: &[Weak<TableInfo>], w_c: &[Weak<ColumnInfo>], w_i: &[Weak<IndexInfo>]| {
        (
            w_t.iter().filter(|w| w.upgrade().is_some()).count(),
            w_c.iter().filter(|w| w.upgrade().is_some()).count(),
            w_i.iter().filter(|w| w.upgrade().is_some()).count(),
        )
    };
    let in_cache = alive(&weak_tables, &weak_cols, &weak_idxs);
    // Evict the whole cache (the cache-pressure case).
    tidb_sessionctx::variable::tidb_vars::SchemaCacheSize
        .store(0, std::sync::atomic::Ordering::SeqCst);
    env.data.set_cache_capacity(1);
    let after = alive(&weak_tables, &weak_cols, &weak_idxs);
    eprintln!(
        "arc_sharing: {rounds} rounds, {} versions, tracked {} TableInfo / {} ColumnInfo / {} IndexInfo; \
         alive after GC with a 256 KiB cache: {in_cache:?}; after evicting the cache: {after:?}",
        env.ver,
        weak_tables.len(),
        weak_cols.len(),
        weak_idxs.len()
    );
    assert!(
        in_cache.0 * 10 < weak_tables.len().max(100),
        "old versions are kept beyond the cache bound: {in_cache:?}"
    );
    assert_eq!(after, (0, 0, 0));
    // The anchor, which no DDL touched, is read again from meta and shared as before.
    env.data.set_cache_capacity(0);
    assert_table_object_shares(env.table(ANCHOR_ID).as_ref());
}
