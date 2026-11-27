//! The commit ts lookup of a schema version is a boundary stub (it needs `store/helper`'s
//! TiKV MVCC API, part of the store driver D07.1): every schema load reaches it, and the
//! domain warns about it once, not on every reload of the sync loop (#484).
//!
//! A test binary of its own: installs the global `tidb-log` `MemorySink` at `WARN` level.

mod common;

use std::sync::Arc;

use common::{MvccStore, create_diff, has_table, state, table};
use tidb_base::config::LogConfig;
use tidb_domain::new_domain;
use tidb_kv::kv::Storage;
use tidb_log::sink::MemorySink;

/// Drives the first (full) load, cache hits, a diff load and a snapshot load, and compares
/// the captured WARN lines whole: one line, at the first load.
#[tokio::test]
async fn blocked_schema_ts_lookup_warns_once() {
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "warn".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    let log_sink = Arc::clone(&sink);
    tidb_log::init_logger_with_sinks(&cfg, "", log_sink, None).unwrap();

    let store = Arc::new(MvccStore::default());
    let t1 = table(110, "sbtest1");
    let d1 = create_diff(1, 110);
    store
        .states
        .write()
        .push((10, Arc::new(state(1, &[&t1], std::slice::from_ref(&d1)))));
    let domain = new_domain(Arc::clone(&store) as Arc<dyn Storage>);

    // The first load (startup): a full load.
    let r = domain.reload_at(10).await.unwrap();
    assert!(!r.hit_cache);
    // The sync loop's ticks with an unchanged version: cache hits.
    for ts in 11..20 {
        assert!(domain.reload_at(ts).await.unwrap().hit_cache);
    }
    // A new version: a diff load, then more ticks.
    let t2 = table(111, "sbtest2");
    let d2 = create_diff(2, 111);
    store
        .states
        .write()
        .push((20, Arc::new(state(2, &[&t1, &t2], &[d1, d2]))));
    let r = domain.reload_at(20).await.unwrap();
    assert!(r.changes.is_some(), "a diff load");
    assert!(has_table(&r.is, "sbtest2").await);
    for ts in 21..30 {
        assert!(domain.reload_at(ts).await.unwrap().hit_cache);
    }
    // A snapshot read at an old ts (its schema ts is 0, so it is loaded again).
    let is = domain.get_snapshot_info_schema(10).await.unwrap();
    assert_eq!(is.schema_meta_version(), 1);

    let out = String::from_utf8(sink.take()).unwrap();
    let lines: Vec<String> = out
        .lines()
        .map(|l| {
            // Normalize `[domain.rs:<digits>]` to `[domain.rs:<LINE>]` for whole-line comparison.
            if let Some((head, tail)) = l.split_once("[domain.rs:")
                && let Some((digits, rest)) = tail.split_once(']')
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                format!("{head}[domain.rs:<LINE>]{rest}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    assert_eq!(
        lines,
        vec![concat!(
            "[WARN] [domain.rs:<LINE>] [\"schema version commit ts is not available, logged once\"]",
            " [error=\"[blocked-by:D07.1]This version of TiDB doesn't yet support",
            " 'the commit ts of a schema version (TiKV MVCC API of store/helper)",
            " (blocked-by:D07.1)'\"] [version=1] [reason=\"store/helper",
            " GetMvccByEncodedKeyWithTS (TiKV MVCC API) is not ported; schema versions",
            " are cached with ts 0, so a snapshot read loads its schema again\"]",
        )],
        "the blocked lookup warns once over 21 loads"
    );
}
