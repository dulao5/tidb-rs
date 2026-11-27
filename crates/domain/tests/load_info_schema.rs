//! `loadInfoSchema` end to end over an in-memory store (D06.5 spec §5): a full load, a
//! diff load, a cache hit, an older version still readable by version, and the gap
//! threshold's full reload. The meta states are written in TiKV's meta layout (Go's
//! `meta.Mutator` keys: the `DBs` and `DB:<id>` hashes, `SchemaVersionKey`, `Diff:<v>`; the store is in `common`).

mod common;

use std::sync::Arc;

use common::{MvccStore, create_diff, has_table, state, table};
use tidb_domain::new_domain;
use tidb_kv::kv::Storage;
use tidb_model::ActionCreateTable;
use tidb_parser::model::CIStr;

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
