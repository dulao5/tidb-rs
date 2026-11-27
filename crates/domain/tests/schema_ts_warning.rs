//! The commit ts lookup of a schema version is a boundary stub (it needs `store/helper`'s
//! TiKV MVCC API, part of the store driver D07.1): every schema load reaches it, and the
//! domain warns about it once, not on every reload of the sync loop (#484).
//!
//! A test binary of its own: the warning is captured with a thread-local `tracing`
//! subscriber, and `tracing` caches per call site whether anyone listens, which other tests
//! running on other threads of the same binary would race with.

mod common;

use std::sync::Arc;

use common::{MvccStore, create_diff, has_table, state, table};
use tidb_domain::new_domain;
use tidb_kv::kv::Storage;

/// A `tracing` writer into a shared buffer.
#[derive(Clone, Default)]
struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Drives the first (full) load, cache hits, a diff load and a snapshot load, and compares
/// the captured WARN lines whole: one line, at the first load.
#[tokio::test]
async fn blocked_schema_ts_lookup_warns_once() {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .without_time()
        .with_target(false)
        // `poc-server` turns on tracing-subscriber's `ansi` feature; in a workspace build the
        // features are unified and the writer would colour the captured line.
        .with_ansi(false)
        .finish();
    // The current-thread runtime of `#[tokio::test]` polls the loads on this thread.
    let _guard = tracing::subscriber::set_default(subscriber);

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

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        vec![concat!(
            " WARN schema version commit ts is not available, logged once",
            " error=[blocked-by:D07.1]This version of TiDB doesn't yet support",
            " 'the commit ts of a schema version (TiKV MVCC API of store/helper)",
            " (blocked-by:D07.1)' version=1 reason=\"store/helper",
            " GetMvccByEncodedKeyWithTS (TiKV MVCC API) is not ported; schema versions",
            " are cached with ts 0, so a snapshot read loads its schema again\"",
        )],
        "the blocked lookup warns once over 21 loads"
    );
}
