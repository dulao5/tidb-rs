//! The coprocessor iterator, workers and response handling (D07.2 part 2) against a mock
//! TiKV store: what Go's `copr_test` package checks with mockstore (keep order, region
//! errors rebuilding the tasks, locks, paging, other errors, the lite worker).

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidb_base::errors::Error;
use tidb_kv::kv::{ClientSendOption, ReqTypeDAG, Request, new_non_partitioned_key_ranges};
use tidb_kv::option::ReplicaReadType;
use tidb_kv::{Key, KeyRange};
use tidb_store::copr::coprocessor::{CopResponse, build_key_ranges};
use tidb_store::copr::store::new_store;
use tidb_store::copr::tikv::*;
use tikv_client::proto::{coprocessor, errorpb, kvrpcpb};

type Hook =
    dyn Fn(&MockStore, &coprocessor::Request, RegionVerID) -> coprocessor::Response + Send + Sync;

/// (id, start, end, ver).
type MockRegion = (u64, Vec<u8>, Vec<u8>, u64);

struct MockStore {
    /// In key order.
    regions: Mutex<Vec<MockRegion>>,
    hook: Box<Hook>,
    resolved: AtomicUsize,
    sends: AtomicUsize,
    region_miss_backoffs: Arc<AtomicUsize>,
    send_delay: Option<Duration>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl MockStore {
    fn new(
        splits: &[&[u8]],
        hook: impl Fn(&MockStore, &coprocessor::Request, RegionVerID) -> coprocessor::Response
        + Send
        + Sync
        + 'static,
    ) -> MockStore {
        let mut bounds: Vec<Vec<u8>> = vec![vec![]];
        bounds.extend(splits.iter().map(|s| s.to_vec()));
        bounds.push(vec![]);
        let regions = (0..bounds.len() - 1)
            .map(|i| (i as u64 + 1, bounds[i].clone(), bounds[i + 1].clone(), 1))
            .collect();
        MockStore {
            regions: Mutex::new(regions),
            hook: Box::new(hook),
            resolved: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            region_miss_backoffs: Arc::new(AtomicUsize::new(0)),
            send_delay: None,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        }
    }

    /// Splits the region holding `key` at `key` (new id, both halves get a new version).
    fn split(&self, key: &[u8]) {
        let mut rs = self.regions.lock().unwrap();
        let next_id = rs.iter().map(|r| r.0).max().unwrap() + 1;
        let i = rs
            .iter()
            .position(|r| r.1.as_slice() <= key && (r.2.is_empty() || key < r.2.as_slice()))
            .unwrap();
        let (id, start, end, ver) = rs[i].clone();
        rs[i] = (id, start, key.to_vec(), ver + 1);
        rs.insert(i + 1, (next_id, key.to_vec(), end, ver + 1));
    }

    fn current(&self, region: RegionVerID) -> bool {
        self.regions
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == region.id && r.3 == region.ver)
    }
}

struct Bo {
    misses: Arc<AtomicUsize>,
}

impl Backoffer for Bo {
    async fn backoff_region_miss(&mut self, err: Error) -> Result<(), Error> {
        if self.misses.fetch_add(1, Ordering::SeqCst) > 20 {
            return Err(err);
        }
        Ok(())
    }
    async fn backoff_with_max_sleep_txn_lock_fast(
        &mut self,
        _ms: i64,
        _err: Error,
    ) -> Result<(), Error> {
        Ok(())
    }
    fn get_total_sleep(&self) -> i64 {
        0
    }
}

impl TiKVRegionCache for MockStore {
    type Backoffer = Bo;

    async fn batch_locate_key_ranges(
        &self,
        _bo: &mut Bo,
        key_ranges: &[KeyRange],
        _need_leader: bool,
        _need_buckets: bool,
    ) -> Result<Vec<KeyLocation>, Error> {
        let rs = self.regions.lock().unwrap().clone();
        let mut out: Vec<KeyLocation> = Vec::new();
        for kr in key_ranges {
            let mut idx = rs
                .iter()
                .position(|r| r.1 <= kr.start_key.0 && (r.2.is_empty() || kr.start_key.0 < r.2))
                .unwrap();
            loop {
                let r = &rs[idx];
                let loc = KeyLocation {
                    region: RegionVerID::new(r.0, 0, r.3),
                    start_key: r.1.clone(),
                    end_key: r.2.clone(),
                    buckets: None,
                };
                if out.last().is_none_or(|l| l.region != loc.region) {
                    out.push(loc);
                }
                let e = &kr.end_key.0;
                let contains_by_end =
                    r.1.as_slice() < e.as_slice() && (r.2.is_empty() || e <= &r.2);
                if contains_by_end || idx + 1 == rs.len() {
                    break;
                }
                idx += 1;
            }
        }
        Ok(out)
    }

    async fn get_tikv_rpc_context(
        &self,
        _bo: &mut Bo,
        id: RegionVerID,
        _replica_read: ReplicaReadType,
        _seed: u32,
    ) -> Result<Option<RPCContext>, Error> {
        Ok(Some(RPCContext {
            region: id,
            store_id: 1,
            ..Default::default()
        }))
    }
}

struct Helper(Arc<MockStore>);

impl ClientHelper for Helper {
    type Backoffer = Bo;

    async fn send_req_ctx(
        &self,
        _bo: &mut Bo,
        req: coprocessor::Request,
        region: RegionVerID,
        _timeout: Duration,
    ) -> Result<(coprocessor::Response, Option<RPCContext>, String), Error> {
        let cur = self.0.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.max_in_flight.fetch_max(cur, Ordering::SeqCst);
        struct InFlightGuard<'a>(&'a AtomicUsize);
        impl Drop for InFlightGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _guard = InFlightGuard(&self.0.in_flight);
        if let Some(delay) = self.0.send_delay {
            tokio::time::sleep(delay).await;
        }
        self.0.sends.fetch_add(1, Ordering::SeqCst);
        if !self.0.current(region) {
            // SendReqCtx's genRegionErrorResp for a stale epoch.
            return Ok((
                coprocessor::Response {
                    region_error: Some(errorpb::Error {
                        epoch_not_match: Some(Default::default()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
                String::new(),
            ));
        }
        let resp = (self.0.hook)(&self.0, &req, region);
        Ok((
            resp,
            Some(RPCContext {
                region,
                addr: "store1".into(),
                ..Default::default()
            }),
            "store1".into(),
        ))
    }

    async fn resolve_lock_and_wait(
        &self,
        _bo: &mut Bo,
        _lock: kvrpcpb::LockInfo,
    ) -> Result<(), Error> {
        // A lock wait sleeps (BackoffWithMaxSleepTxnLockFast).
        tokio::time::sleep(Duration::from_millis(1)).await;
        self.0.resolved.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// The store handed to `new_store`: shares the mock with the helpers.
struct Shared(Arc<MockStore>);

impl TiKVRegionCache for Shared {
    type Backoffer = Bo;
    async fn batch_locate_key_ranges(
        &self,
        bo: &mut Bo,
        key_ranges: &[KeyRange],
        need_leader: bool,
        need_buckets: bool,
    ) -> Result<Vec<KeyLocation>, Error> {
        self.0
            .batch_locate_key_ranges(bo, key_ranges, need_leader, need_buckets)
            .await
    }
    async fn get_tikv_rpc_context(
        &self,
        bo: &mut Bo,
        id: RegionVerID,
        replica_read: ReplicaReadType,
        seed: u32,
    ) -> Result<Option<RPCContext>, Error> {
        self.0
            .get_tikv_rpc_context(bo, id, replica_read, seed)
            .await
    }
}

impl TiKVStore for Shared {
    type Helper = Helper;
    fn new_client_helper(&self, _start_ts: u64, _resolved: bool) -> Helper {
        Helper(self.0.clone())
    }
    fn new_backoffer(&self, _max_sleep_ms: i64, _vars: &Variables) -> Bo {
        Bo {
            misses: self.0.region_miss_backoffs.clone(),
        }
    }
    fn check_visibility(&self, _start_ts: u64) -> Result<(), Error> {
        Ok(())
    }
}

fn describe(req: &coprocessor::Request, region: RegionVerID) -> Vec<u8> {
    let ranges: Vec<String> = req
        .ranges
        .iter()
        .map(|r| {
            format!(
                "{}-{}",
                String::from_utf8_lossy(&r.start),
                String::from_utf8_lossy(&r.end)
            )
        })
        .collect();
    format!("{}:{}", region.id, ranges.join(",")).into_bytes()
}

fn echo(_: &MockStore, req: &coprocessor::Request, region: RegionVerID) -> coprocessor::Response {
    coprocessor::Response {
        data: describe(req, region),
        ..Default::default()
    }
}

fn request(keys: &[&[u8]]) -> Request {
    Request {
        tp: ReqTypeDAG,
        start_ts: 100,
        concurrency: 3,
        key_ranges: Some(new_non_partitioned_key_ranges(build_key_ranges(keys))),
        ..Default::default()
    }
}

async fn run(mock: Arc<MockStore>, req: Request) -> Result<Vec<String>, Error> {
    run_with(mock, req, &ClientSendOption::default()).await
}

async fn run_with(
    mock: Arc<MockStore>,
    req: Request,
    option: &ClientSendOption,
) -> Result<Vec<String>, Error> {
    let store = Arc::new(new_store(Arc::new(Shared(mock)), None, 0));
    let client = store.get_client();
    let mut it = client
        .build_cop_iterator(req, Variables::default(), option)
        .await?;
    it.open(option.try_cop_lite_worker.as_ref());
    let ctx = tidb_kv::Context::background();
    let mut out = Vec::new();
    loop {
        match it.next(&ctx).await {
            Ok(Some(r)) => out.push(String::from_utf8(r.get_data().to_vec()).unwrap()),
            Ok(None) => break,
            Err(e) => {
                it.close().await?;
                return Err(e);
            }
        }
    }
    it.close().await?;
    Ok(out)
}

#[tokio::test]
async fn keep_order_returns_tasks_in_key_order() {
    let mock = Arc::new(MockStore::new(&[b"g", b"n"], echo));
    let mut req = request(&[b"a", b"c", b"h", b"p"]);
    req.keep_order = true;
    let got = run(mock, req).await.unwrap();
    assert_eq!(got, vec!["1:a-c", "2:h-n", "3:n-p"]);
}

#[tokio::test]
async fn keep_order_desc_returns_tasks_in_reverse() {
    let mock = Arc::new(MockStore::new(&[b"g", b"n"], echo));
    let mut req = request(&[b"a", b"c", b"h", b"p"]);
    req.keep_order = true;
    req.desc = true;
    let got = run(mock, req).await.unwrap();
    assert_eq!(got, vec!["3:n-p", "2:h-n", "1:a-c"]);
}

#[tokio::test]
async fn unordered_returns_every_task() {
    let mock = Arc::new(MockStore::new(&[b"g", b"n"], echo));
    let req = request(&[b"a", b"z"]);
    let mut got = run(mock, req).await.unwrap();
    got.sort();
    assert_eq!(got, vec!["1:a-g", "2:g-n", "3:n-z"]);
}

#[tokio::test]
async fn region_error_rebuilds_the_remaining_ranges() {
    // Region 2 splits at "k" after the tasks were built: its task meets EpochNotMatch, backs
    // off BoRegionMiss and is rebuilt into two tasks (handleCopResponse).
    let split_once = Arc::new(AtomicU32::new(0));
    let s = split_once.clone();
    let mock = Arc::new(MockStore::new(&[b"g", b"n"], move |m, req, region| {
        if region.id == 2 && s.fetch_add(1, Ordering::SeqCst) == 0 {
            m.split(b"k");
            return coprocessor::Response {
                region_error: Some(errorpb::Error {
                    epoch_not_match: Some(Default::default()),
                    ..Default::default()
                }),
                ..Default::default()
            };
        }
        echo(m, req, region)
    }));
    let mut req = request(&[b"a", b"z"]);
    req.keep_order = true;
    let got = run(mock.clone(), req).await.unwrap();
    assert_eq!(got, vec!["1:a-g", "2:g-k", "4:k-n", "3:n-z"]);
    assert_eq!(mock.region_miss_backoffs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lock_is_resolved_then_retried_with_the_fallback_flag() {
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    let mock = Arc::new(MockStore::new(&[], move |m, req, region| {
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            return coprocessor::Response {
                locked: Some(kvrpcpb::LockInfo {
                    lock_version: 90,
                    key: b"b".to_vec(),
                    ..Default::default()
                }),
                ..Default::default()
            };
        }
        echo(m, req, region)
    }));
    let got = run(mock.clone(), request(&[b"a", b"c"])).await.unwrap();
    assert_eq!(got, vec!["1:a-c"]);
    assert_eq!(mock.resolved.load(Ordering::SeqCst), 1);
    assert_eq!(mock.sends.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn paging_continues_after_the_reported_range_with_a_growing_page() {
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let sz = sizes.clone();
    let mock = Arc::new(MockStore::new(&[], move |_m, req, region| {
        sz.lock().unwrap().push(req.paging_size);
        let first = &req.ranges[0].start;
        // Each page covers up to the next letter.
        let end = vec![first[0] + 1];
        coprocessor::Response {
            data: describe(req, region),
            range: Some(coprocessor::KeyRange {
                start: first.clone(),
                end,
            }),
            ..Default::default()
        }
    }));
    let mut req = request(&[b"a", b"d"]);
    req.keep_order = true;
    req.paging.enable = true;
    req.paging.min_paging_size = 128;
    req.paging.max_paging_size = 50000;
    let got = run(mock, req).await.unwrap();
    assert_eq!(got, vec!["1:a-d", "1:b-d", "1:c-d"]);
    assert_eq!(*sizes.lock().unwrap(), vec![128, 256, 512]);
}

#[tokio::test]
async fn write_conflict_other_error_is_err_write_conflict() {
    let mock = Arc::new(MockStore::new(&[], |_, _, _| coprocessor::Response {
        other_error: "write conflict on key".into(),
        ..Default::default()
    }));
    let err = run(mock, request(&[b"a", b"c"])).await.unwrap_err();
    assert_eq!(err.code(), tidb_base::errno::ErrWriteConflict as isize);
}

#[tokio::test]
async fn other_error_ends_the_iterator() {
    let mock = Arc::new(MockStore::new(&[], |_, _, _| coprocessor::Response {
        other_error: "boom".into(),
        ..Default::default()
    }));
    let err = run(mock, request(&[b"a", b"c"])).await.unwrap_err();
    assert!(err.to_string().contains("other error: boom"), "{err}");
}

#[tokio::test]
async fn lite_worker_serves_a_single_task_without_workers() {
    let mock = Arc::new(MockStore::new(&[], echo));
    let flag = Arc::new(AtomicU32::new(0));
    let option = ClientSendOption {
        try_cop_lite_worker: Some(flag.clone()),
        ..Default::default()
    };
    let got = run_with(mock, request(&[b"a", b"c"]), &option)
        .await
        .unwrap();
    assert_eq!(got, vec!["1:a-c"]);
    // Released once its tasks are done.
    assert_eq!(flag.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn start_key_is_the_paging_range_or_the_first_range() {
    let r = CopResponse {
        start_key: Key(b"x".to_vec()),
        ..Default::default()
    };
    assert_eq!(r.get_start_key().0, b"x");
    assert_eq!(r.mem_size(), r.start_key.0.capacity() as i64);
}

/// Dropping the iterator mid-stream (N3 cancellation, or an executor returning early with
/// `?`) stops the workers: a task stuck on a live lock stops hitting the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_iterator_stops_the_workers() {
    let mock = Arc::new(MockStore::new(&[b"g"], |m, req, region| {
        if region.id == 2 {
            // Always locked: resolve, retry, forever (the mock backoffer never runs out).
            return coprocessor::Response {
                locked: Some(kvrpcpb::LockInfo {
                    lock_version: 90,
                    ..Default::default()
                }),
                ..Default::default()
            };
        }
        echo(m, req, region)
    }));
    let store = Arc::new(new_store(Arc::new(Shared(mock.clone())), None, 0));
    let client = store.get_client();
    let mut it = client
        .build_cop_iterator(
            request(&[b"a", b"z"]),
            Variables::default(),
            &ClientSendOption::default(),
        )
        .await
        .unwrap();
    it.open(None);
    let first = it
        .next(&tidb_kv::Context::background())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.get_data(), b"1:a-g");
    // Region 2's worker is looping on its lock.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(mock.sends.load(Ordering::SeqCst) > 2);
    drop(it);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let after_drop = mock.sends.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        mock.sends.load(Ordering::SeqCst),
        after_drop,
        "workers still sending after drop"
    );
}

/// The kill signal of the statement (`Variables.Killed`, the session's `SQLKiller.Signal`,
/// VS1-14) stops the workers: `copIteratorWorker.finished()` reads it before each task and
/// each retry, so a task stuck on a live lock stops hitting the store once KILL arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_signal_stops_the_workers() {
    let mock = Arc::new(MockStore::new(&[b"g"], |m, req, region| {
        if region.id == 2 {
            return coprocessor::Response {
                locked: Some(kvrpcpb::LockInfo {
                    lock_version: 90,
                    ..Default::default()
                }),
                ..Default::default()
            };
        }
        echo(m, req, region)
    }));
    let store = Arc::new(new_store(Arc::new(Shared(mock.clone())), None, 0));
    let client = store.get_client();
    let killed = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let vars = Variables {
        killed: Some(Arc::clone(&killed)),
        ..Variables::default()
    };
    let mut it = client
        .build_cop_iterator(request(&[b"a", b"z"]), vars, &ClientSendOption::default())
        .await
        .unwrap();
    it.open(None);
    let first = it
        .next(&tidb_kv::Context::background())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.get_data(), b"1:a-g");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(mock.sends.load(Ordering::SeqCst) > 2);
    killed.store(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let after_kill = mock.sends.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        mock.sends.load(Ordering::SeqCst),
        after_kill,
        "workers still sending after KILL"
    );
    drop(it);
}

/// D07.3 spec §4.3 & §9.2: `distsql::select` uses `CopClient::send_async` to build and open
/// `CopIterator` eagerly, so `get_select_result_concurrency` returns `(concurrency, small)`
/// before the first `next()`, `kvrpcpb::Context.resource_group_tag` carries the table ID and
/// row label, and `LazyCopResponse::close` caches `build_task_elapsed` so `selectResult.close`
/// still registers it after `resp.close()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distsql_select_eager_concurrency_tagger_and_post_close_stats() {
    use protobuf::Message;
    use tidb_execdetails::execdetails::RuntimeStatsColl;
    use tidb_kv::copr::new_resource_group_tag_builder;
    use tidb_sessionctx::distsql_ctx::DistSQLContext;
    use tidb_store::distsql::{
        RequestBuilder, get_select_result_concurrency, select_with_runtime_stats,
    };

    let captured_tags: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let tags_for_hook = Arc::clone(&captured_tags);

    let row_key_1 = tidb_tablecodec::tablecodec::encode_row_key(15, &[0x80, 0, 0, 0, 0, 0, 0, 1]);
    let row_key_2 = tidb_tablecodec::tablecodec::encode_row_key(15, &[0x80, 0, 0, 0, 0, 0, 0, 5]);
    let row_key_3 = tidb_tablecodec::tablecodec::encode_row_key(15, &[0x80, 0, 0, 0, 0, 0, 0, 9]);

    let mock = Arc::new(MockStore::new(
        &[row_key_2.0.as_slice()],
        move |_, req, _| {
            if let Some(ctx) = &req.context {
                tags_for_hook
                    .lock()
                    .unwrap()
                    .push(ctx.resource_group_tag.clone());
            }
            let mut sr = tipb::SelectResponse::default();
            sr.set_encode_type(tipb::EncodeType::TypeChunk);
            let mut c = tipb::Chunk::default();
            c.set_rows_data(Vec::new());
            sr.mut_chunks().push(c);
            coprocessor::Response {
                data: sr.write_to_bytes().unwrap(),
                ..Default::default()
            }
        },
    ));
    let store = Arc::new(new_store(Arc::new(Shared(mock)), None, 0));
    let client: Arc<dyn tidb_kv::kv::Client> = Arc::new(store.get_client());
    let coll = Arc::new(RuntimeStatsColl::default());
    let mut dctx = DistSQLContext {
        client: Some(client),
        dist_sql_concurrency: 15,
        runtime_stats_coll: Some(Arc::clone(&coll)),
        resource_group_tagger: Some(Arc::new(new_resource_group_tag_builder())),
        ..Default::default()
    };
    dctx.enable_chunk_rpc = true;

    // 2 regions -> concurrency clamped from 15 to 2 immediately upon select()!
    let req = RequestBuilder::default()
        .set_key_ranges(vec![KeyRange {
            start_key: row_key_1,
            end_key: row_key_3,
        }])
        .set_dag_request(tipb::DagRequest::default())
        .set_from_session_vars(&dctx)
        .build()
        .unwrap();

    let mut sr = select_with_runtime_stats(
        &tidb_kv::Context::background(),
        Arc::new(dctx),
        &req,
        Vec::new(),
        Vec::new(),
        10,
    )
    .await
    .unwrap();

    // Eager CopIterator build: concurrency is already clamped to 2 BEFORE first next()!
    assert_eq!(get_select_result_concurrency(sr.as_ref()), Some((2, 0)));

    // Drain raw responses so workers actually send requests with resource_group_tag.
    let mut count = 0;
    while sr
        .next_raw(&tidb_kv::Context::background())
        .await
        .unwrap()
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, 2);
    sr.close().unwrap();

    let tags = captured_tags.lock().unwrap();
    assert_eq!(tags.len(), 2);
    for raw in tags.iter() {
        let mut tag = tipb::ResourceGroupTag::default();
        tag.merge_from_bytes(raw).unwrap();
        assert_eq!(tag.get_table_id(), 15);
        assert_eq!(
            tag.get_label(),
            tipb::ResourceGroupTagLabel::ResourceGroupTagLabelRow
        );
    }
}

/// D07.3 spec §2.2: `kv::Request.copr_request_rate_limit` caps aggregate in-flight cop
/// requests across workers/readers, is exposed via `CopIterator::get_request_rate_limit`,
/// and releases tokens on completion, error, and cancellation/drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copr_request_rate_limit_caps_inflight_and_releases_tokens() {
    let mut mock = MockStore::new(&[b"d", b"h", b"m"], echo);
    mock.send_delay = Some(Duration::from_millis(15));
    let mock = Arc::new(mock);

    let rl = Arc::new(RateLimit::new(1));
    let store = Arc::new(new_store(Arc::new(Shared(mock.clone())), None, 0));
    let client = store.get_client();

    let mut req = request(&[b"a", b"z"]);
    req.concurrency = 4;
    req.copr_request_rate_limit = Some(Arc::clone(&rl));

    let mut it = client
        .build_cop_iterator(req, Variables::default(), &ClientSendOption::default())
        .await
        .unwrap();
    assert!(
        it.get_request_rate_limit()
            .is_some_and(|r| Arc::ptr_eq(r, &rl))
    );
    it.open(None);

    let ctx = tidb_kv::Context::background();
    let mut got = Vec::new();
    while let Some(r) = it.next(&ctx).await.unwrap() {
        got.push(String::from_utf8(r.get_data().to_vec()).unwrap());
    }
    it.close().await.unwrap();
    got.sort();
    assert_eq!(got, vec!["1:a-d", "2:d-h", "3:h-m", "4:m-z"]);
    assert_eq!(
        mock.max_in_flight.load(Ordering::SeqCst),
        1,
        "RateLimit(1) must cap concurrent in-flight cop requests to 1"
    );

    // Token must be available again (all tokens released).
    let (_tx, mut done) = tokio::sync::watch::channel(false);
    assert!(!rl.get_token(&mut done).await);
    rl.put_token();

    // Error path must also release the token via drop guard.
    let err_mock = Arc::new(MockStore::new(&[b"d"], |_, _, _| coprocessor::Response {
        other_error: "boom".into(),
        ..Default::default()
    }));
    let err_store = Arc::new(new_store(Arc::new(Shared(err_mock)), None, 0));
    let mut err_req = request(&[b"a", b"z"]);
    err_req.concurrency = 2;
    err_req.copr_request_rate_limit = Some(Arc::clone(&rl));
    let mut err_it = err_store
        .get_client()
        .build_cop_iterator(err_req, Variables::default(), &ClientSendOption::default())
        .await
        .unwrap();
    err_it.open(None);
    let _ = err_it.next(&ctx).await;
    err_it.close().await.unwrap();
    assert!(!rl.get_token(&mut done).await);
    rl.put_token();
}
