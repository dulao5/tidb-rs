//! The status port (Go TiDB's `--status`), reduced to what monitoring needs:
//!
//! - `GET /metrics`: Prometheus text exposition of every metric (`tidb_metrics::gather_text`).
//! - `GET /status`: `{"connections":N,"version":"...","git_hash":"..."}` like Go's
//!   `/status` (without Go's `status.init_stats_percentage`: there are no statistics here).
//!
//! Every other path answers `404 page not found` (Go's `http.NotFound` text). The listener
//! speaks HTTP/1.1 and HTTP/2 (prior knowledge), so Go TiDB's gRPC calls to the status port
//! (cluster memory tables, diagnostics) get an immediate HTTP/2 404, which gRPC reports as
//! `Unimplemented`, rather than a protocol error; see `docs/adr/0006-metrics.md`.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::header::HeaderValue;
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// What `/status` reports.
#[derive(Clone, Debug)]
pub struct StatusInfo {
    /// The MySQL server version string (Go: `mysql.ServerVersion`).
    pub version: String,
    pub git_hash: String,
}

/// Serve the status API on `listener` until `shutdown` completes.
pub async fn serve(listener: TcpListener, info: StatusInfo, shutdown: impl Future<Output = ()>) {
    match listener.local_addr() {
        Ok(addr) => info!(%addr, "status server listening"),
        Err(e) => warn!(error = %e, "status server address unknown"),
    }
    let info = Arc::new(info);
    let mut conns = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        while conns.try_join_next().is_some() {}
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "status server accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let info = Arc::clone(&info);
        conns.spawn(async move {
            let service = service_fn(move |req| {
                let info = Arc::clone(&info);
                async move { Ok::<_, Infallible>(handle(&req, &info)) }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                debug!(%peer, error = %e, "status connection ended with an error");
            }
        });
    }
    conns.shutdown().await;
}

fn handle(req: &Request<Incoming>, info: &StatusInfo) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/metrics" => response(
            StatusCode::OK,
            tidb_metrics::encode::CONTENT_TYPE,
            tidb_metrics::gather_text(),
        ),
        "/status" => response(StatusCode::OK, "application/json", status_json(info)),
        _ => response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "404 page not found\n".to_owned(),
        ),
    }
}

fn response(status: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

/// Go's `/status` body, in the field order of Go's `status` struct.
fn status_json(info: &StatusInfo) -> String {
    let string = |s: &str| serde_json::Value::from(s).to_string();
    format!(
        r#"{{"connections":{},"version":{},"git_hash":{}}}"#,
        tidb_metrics::connections().max(0),
        string(&info.version),
        string(&info.git_hash)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn start() -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
        tidb_metrics::init(&tidb_metrics::ServerFacts {
            version: "v8.5.8-tidb-rs-test".to_owned(),
            git_hash: "abc".to_owned(),
            maxprocs: 4,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let info = StatusInfo {
            version: "8.0.11-TiDB-v8.5.8-tidb-rs".to_owned(),
            git_hash: "abc".to_owned(),
        };
        tokio::spawn(serve(listener, info, async move {
            let _ = rx.await;
        }));
        (addr, tx)
    }

    #[tokio::test]
    async fn serves_metrics_status_and_404() {
        let (addr, stop) = start().await;
        // Series of labelled families exist once something was recorded (as in Go).
        let d = std::time::Duration::from_millis(1);
        tidb_metrics::statement(tidb_metrics::StmtType::Select);
        tidb_metrics::distsql_query(d, true);
        tidb_metrics::command_finished(0x03, tidb_metrics::StmtType::Select, d, Some("unknown"));
        let metrics = get(addr, "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200 OK"), "{metrics}");
        assert!(metrics.contains("text/plain; version=0.0.4"), "{metrics}");
        let body = metrics.split("\r\n\r\n").nth(1).unwrap();
        let problems = tidb_metrics::golden::check(body, tidb_metrics::golden::TIDB);
        assert!(problems.is_empty(), "{problems:#?}");

        let status = get(addr, "/status").await;
        assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
        assert!(
            status.ends_with(
                r#"{"connections":0,"version":"8.0.11-TiDB-v8.5.8-tidb-rs","git_hash":"abc"}"#
            ),
            "{status}"
        );

        for path in ["/", "/config", "/info", "/metrics/x"] {
            let resp = get(addr, path).await;
            assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "{path}: {resp}");
            assert!(resp.ends_with("404 page not found\n"), "{path}: {resp}");
        }
        let _ = stop.send(());
    }

    /// A gRPC client (HTTP/2 with prior knowledge, what Go TiDB's coprocessor and diagnostics
    /// clients send to a TiDB status port) gets an HTTP/2 response at once, not a hang.
    #[tokio::test]
    async fn http2_prior_knowledge_gets_a_404() {
        let (addr, stop) = start().await;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Connection preface + an empty SETTINGS frame.
        s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
            .await
            .unwrap();
        let mut buf = [0u8; 9];
        tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut buf))
            .await
            .expect("server answers HTTP/2 promptly")
            .unwrap();
        // The server's first frame is its SETTINGS frame (type 4).
        assert_eq!(buf[3], 4, "{buf:?}");
        let _ = stop.send(());
    }
}
