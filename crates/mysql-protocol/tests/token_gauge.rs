//! `tidb_server_tokens` follows the token: a command task aborted while it holds (or waits
//! for) one does not leak the gauge (PR #9 review). Its own test binary: the gauge is global.

use std::time::Duration;

use tidb_mysql_protocol::admission::HeldToken;

#[tokio::test]
async fn aborted_command_releases_token_and_gauge() {
    let adm = tidb_runtime::Admission::new(&tidb_runtime::AdmissionConfig {
        token_limit: 1,
        ..Default::default()
    })
    .unwrap();
    let base = tidb_metrics::tokens();

    // A command holding the only token is aborted mid-way.
    let a2 = adm.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let h = tokio::spawn(async move {
        let _t = HeldToken::acquire(Some(&a2)).await;
        let _ = tx.send(());
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    rx.await.unwrap();
    assert_eq!(tidb_metrics::tokens(), base + 1);
    assert_eq!(adm.limiter().available(), 0);
    h.abort();
    let _ = h.await;
    assert_eq!(tidb_metrics::tokens(), base);
    assert_eq!(adm.limiter().available(), 1);

    // Without admission (tests) the gauge still counts the command.
    let t = HeldToken::acquire(None).await;
    assert_eq!(tidb_metrics::tokens(), base + 1);
    drop(t);
    assert_eq!(tidb_metrics::tokens(), base);
}
