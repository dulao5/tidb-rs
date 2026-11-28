//! Cluster integration tests for `/tidb/server_id/<id>` leases, `ServerIdLease`,
//! `SchemaSync` `ServerInfo.server_id`, and `find_server_rpc_url` against real PD etcd.
//!
//! Skipped unless `TIDB_RS_PD` (PD address) is set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use etcd_client::Client as EtcdClient;
use etcd_client::ConnectOptions;
use poc_domain::{
    SchemaSync, ServerIdLease, ServerIdTiming, ServerInfo, SyncConfig, find_server_rpc_url, keys,
};
use poc_kv::KvClient;
use tidb_distsql::DagContext;

fn pd_endpoint() -> Option<String> {
    match std::env::var("TIDB_RS_PD") {
        Ok(pd) if !pd.is_empty() => Some(pd),
        _ => {
            eprintln!("TIDB_RS_PD not set; skipping");
            None
        }
    }
}

#[tokio::test]
async fn server_id_lease_lifecycle_and_keeper_against_real_etcd() {
    let Some(pd) = pd_endpoint() else {
        return;
    };
    let etcd = EtcdClient::connect(
        &[pd],
        Some(ConnectOptions::new().with_connect_timeout(Duration::from_secs(5))),
    )
    .await
    .expect("connect to PD etcd");

    let timing = ServerIdTiming {
        ttl: Duration::from_secs(60),
        keepalive_interval: Duration::from_millis(200),
        check_restored_interval: Duration::from_millis(200),
        lost_connection_to_pd_timeout: Duration::from_secs(30),
    };
    let changed_id = Arc::new(AtomicU64::new(0));
    let changed_valid = Arc::new(AtomicBool::new(false));
    let cid = Arc::clone(&changed_id);
    let cvalid = Arc::clone(&changed_valid);
    let lease = Arc::new(ServerIdLease::new(
        etcd.clone(),
        true,
        timing,
        Some(Arc::new(move |id, valid| {
            cid.store(id, Ordering::SeqCst);
            cvalid.store(valid, Ordering::SeqCst);
        })),
        None,
    ));

    // 1. acquire_server_id writes /tidb/server_id/<id> = "0" bound to an etcd lease.
    let sid = lease.acquire_server_id().await.expect("acquire_server_id");
    assert!(
        sid > 0 && sid <= 2047,
        "expected 32-bit server_id, got {sid}"
    );
    assert_eq!(lease.server_id(), sid);
    assert_eq!(changed_id.load(Ordering::SeqCst), sid);
    assert!(changed_valid.load(Ordering::SeqCst));
    let etcd_lease_id = lease.lease_id().await.expect("lease_id must be stored");

    let key = keys::server_id_key(sid);
    let resp = etcd.clone().get(key.as_str(), None).await.unwrap();
    assert_eq!(resp.kvs().len(), 1, "missing etcd key {key}");
    assert_eq!(resp.kvs()[0].value(), b"0");
    assert_eq!(resp.kvs()[0].lease(), etcd_lease_id);

    // 2. refresh_server_id_ttl renews the lease and preserves the key.
    lease
        .refresh_server_id_ttl()
        .await
        .expect("refresh_server_id_ttl");
    let resp_after = etcd.clone().get(key.as_str(), None).await.unwrap();
    assert_eq!(resp_after.kvs().len(), 1);
    assert_eq!(resp_after.kvs()[0].value(), b"0");

    // 3. run_keeper ticks cleanly without dropping lease, then exits on stop signal.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let keeper = tokio::spawn(Arc::clone(&lease).run_keeper(stop_rx, Arc::new(|| {})));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!lease.is_lost_connection_to_pd());
    assert_eq!(lease.lease_id().await, Some(etcd_lease_id));
    let _ = stop_tx.send(true);
    tokio::time::timeout(Duration::from_secs(5), keeper)
        .await
        .expect("keeper must exit on stop")
        .unwrap();

    // 4. release_server_id deletes /tidb/server_id/<id> and clears local validity.
    lease.release_server_id().await;
    assert_eq!(lease.server_id(), 0);
    assert!(!changed_valid.load(Ordering::SeqCst));
    assert!(lease.lease_id().await.is_none());
    let resp_deleted = etcd.clone().get(key.as_str(), None).await.unwrap();
    assert!(
        resp_deleted.kvs().is_empty(),
        "key {key} must be deleted on release"
    );
}

#[tokio::test]
async fn schema_sync_publishes_server_id_resolves_rpc_url_and_cleans_up_on_shutdown() {
    let Some(pd) = pd_endpoint() else {
        return;
    };
    let etcd = EtcdClient::connect(
        &[pd.as_str()],
        Some(ConnectOptions::new().with_connect_timeout(Duration::from_secs(5))),
    )
    .await
    .expect("connect to PD etcd");
    let kv = KvClient::connect(vec![pd.clone()])
        .await
        .expect("connect KvClient");

    // Allocate an ephemeral port so our ServerInfo entry is distinct and realistic.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let status_port = u32::from(listener.local_addr().unwrap().port());
    drop(listener);
    let mysql_port = status_port + 1;

    let info =
        ServerInfo::new("127.0.0.1".to_owned(), mysql_port, status_port).with_disable_rpc(false);
    let info_key = info.key();
    let mut cfg = SyncConfig::new(vec![pd], info, DagContext::select_utc());
    cfg.acquire_server_id = true;
    cfg.enable_32bits_conn_id = true;

    let sync = SchemaSync::start(kv, cfg).await.expect("SchemaSync::start");
    let sid = sync.server_id();
    assert!(sid > 0);
    assert!(!sync.is_lost_connection_to_pd());
    assert!(
        sync.wait_admissible(Duration::from_secs(15)).await,
        "SchemaSync must register in etcd"
    );

    // Verify /tidb/server/info/<ddl_id> carries server_id and disable_rpc=false.
    let info_resp = etcd.clone().get(info_key.as_str(), None).await.unwrap();
    assert_eq!(info_resp.kvs().len(), 1);
    let parsed: ServerInfo = serde_json::from_slice(info_resp.kvs()[0].value()).unwrap();
    assert_eq!(parsed.server_id, sid);
    assert!(!parsed.disable_rpc);

    // Verify find_server_rpc_url resolves the node's status endpoint from etcd.
    let resolved = find_server_rpc_url(&etcd, sid)
        .await
        .expect("find_server_rpc_url");
    assert_eq!(resolved, Some(format!("http://127.0.0.1:{status_port}")));

    // Verify /tidb/server_id/<sid> is present.
    let sid_key = keys::server_id_key(sid);
    let sid_resp = etcd.clone().get(sid_key.as_str(), None).await.unwrap();
    assert_eq!(sid_resp.kvs().len(), 1);
    assert_eq!(sid_resp.kvs()[0].value(), b"0");

    // Graceful shutdown removes both /tidb/server_id/<sid> and /tidb/server/info/<ddl_id>.
    sync.shutdown().await;
    let sid_after = etcd.clone().get(sid_key.as_str(), None).await.unwrap();
    assert!(sid_after.kvs().is_empty(), "{sid_key} must be deleted");
    let info_after = etcd.clone().get(info_key.as_str(), None).await.unwrap();
    assert!(info_after.kvs().is_empty(), "{info_key} must be deleted");
}
