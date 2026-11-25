//! go-file: pkg/util/channel/channel.go
//!
//! Channel draining helper (`pkg/util/channel`, block D00.3, placed in `tidb-runtime` as the
//! lowest crate carrying `tokio::sync::mpsc`).

use tokio::sync::mpsc;

/// Clear is to clear the channel.
// go: Clear L2 (async tokio mpsc::Receiver instead of Go chan range)
pub async fn clear<T>(ch: &mut mpsc::Receiver<T>) {
    while ch.recv().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::clear;

    #[tokio::test]
    async fn clear_drains_until_closed() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        drop(tx);
        clear(&mut rx).await;
        assert!(rx.recv().await.is_none());
    }
}
