//! The status port (Go TiDB's `--status`), reduced to what monitoring needs:
//!
//! - `GET /metrics`: Prometheus text exposition of every metric (`tidb_metrics::gather_text`).
//! - `GET /status`: `{"connections":N,"version":"...","git_hash":"..."}` like Go's
//!   `/status` (without Go's `status.init_stats_percentage`: there are no statistics here).
//!
//! Every other path answers `404 page not found` (Go's `http.NotFound` text). The listener
//! speaks HTTP/1.1 and HTTP/2 (prior knowledge). HTTP/2 requests with a gRPC content type go
//! to `rpc` (Go's cmux splits the port the same way): the TiDB coprocessor and diagnostics
//! services that Go TiDB calls on every node's status port (issue #161,
//! `docs/adr/0006-metrics.md`).

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use hyper::HeaderMap;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::body::SizeHint;
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
                async move {
                    let resp = if crate::rpc::is_grpc(&req) {
                        crate::rpc::serve(req).await
                    } else {
                        handle(&req, &info)
                    };
                    Ok::<_, Infallible>(resp)
                }
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

fn handle(req: &Request<Incoming>, info: &StatusInfo) -> Response<Body> {
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

fn response(status: StatusCode, content_type: &'static str, body: String) -> Response<Body> {
    let mut resp = Response::new(Body::full(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

/// A response body: one data frame, then optional trailers (gRPC's status).
// glue: Go's net/http and grpc-go write bodies and trailers directly.
#[derive(Debug, Default)]
pub(crate) struct Body {
    data: Option<Bytes>,
    trailers: Option<HeaderMap>,
}

impl Body {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn full(data: Bytes) -> Self {
        Self {
            data: Some(data).filter(|d| !d.is_empty()),
            trailers: None,
        }
    }

    pub(crate) fn with_trailers(data: Bytes, trailers: HeaderMap) -> Self {
        Self {
            trailers: Some(trailers),
            ..Self::full(data)
        }
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        let frame = match this.data.take() {
            Some(d) => Some(Frame::data(d)),
            None => this.trailers.take().map(Frame::trailers),
        };
        Poll::Ready(frame.map(Ok))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none() && self.trailers.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let len = self.data.as_ref().map_or(0, |d| d.len() as u64);
        if self.trailers.is_some() {
            // No content-length on a gRPC message stream.
            let mut hint = SizeHint::new();
            hint.set_lower(len);
            hint
        } else {
            SizeHint::with_exact(len)
        }
    }
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

    /// An HTTP/2 (prior knowledge) client, as Go TiDB's gRPC clients connect to a status port.
    async fn h2(
        addr: std::net::SocketAddr,
    ) -> hyper::client::conn::http2::SendRequest<http_body_util::Full<Bytes>> {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (send, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);
        send
    }

    /// One gRPC call: the response headers, the response messages, the trailers.
    async fn grpc_call(
        addr: std::net::SocketAddr,
        path: &str,
        msg: &[u8],
    ) -> (hyper::HeaderMap, Vec<Vec<u8>>, Option<hyper::HeaderMap>) {
        use http_body_util::BodyExt;
        let mut framed = vec![0u8];
        framed.extend_from_slice(&u32::try_from(msg.len()).unwrap().to_be_bytes());
        framed.extend_from_slice(msg);
        let req = Request::post(format!("http://{addr}{path}"))
            .version(hyper::Version::HTTP_2)
            .header(CONTENT_TYPE, "application/grpc")
            .header("te", "trailers")
            .body(http_body_util::Full::new(Bytes::from(framed)))
            .unwrap();
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            h2(addr).await.send_request(req),
        )
        .await
        .expect("server answers gRPC promptly")
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        let collected = resp.into_body().collect().await.unwrap();
        let trailers = collected.trailers().cloned();
        let mut body = collected.to_bytes();
        let mut messages = Vec::new();
        while !body.is_empty() {
            assert_eq!(body[0], 0, "uncompressed");
            let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
            messages.push(body[5..5 + len].to_vec());
            body = body.slice(5 + len..);
        }
        (headers, messages, trailers)
    }

    /// gRPC requests on the status port reach `rpc`: Go TiDB's ANALYZE broadcast and memory-table
    /// scans get an answer, unknown methods get `Unimplemented` (issue #161).
    #[tokio::test]
    async fn grpc_requests_are_routed_to_rpc() {
        use crate::rpc::testdata::*;
        use prost::Message as _;
        let (addr, stop) = start().await;

        let req = crate::rpc::CopRequest {
            tp: 103,
            data: dag(
                vec![broadcast("FLUSH STATS_DELTA `a`.`t` CLUSTER")],
                EncodeType::TypeDefault,
                false,
            ),
        };
        let (headers, messages, trailers) =
            grpc_call(addr, crate::rpc::COPROCESSOR, &req.encode_to_vec()).await;
        assert_eq!(headers[CONTENT_TYPE], "application/grpc");
        assert_eq!(trailers.unwrap()["grpc-status"], "0");
        assert_eq!(messages.len(), 1);
        let resp = crate::rpc::CopResponse::decode(&messages[0][..]).unwrap();
        assert_eq!(resp.other_error, "");
        assert!(select_response(&resp.data).get_chunks().is_empty());

        let req = crate::rpc::CopRequest {
            tp: 103,
            data: dag(vec![table_scan(1)], EncodeType::TypeChunk, false),
        };
        let (_, messages, trailers) =
            grpc_call(addr, crate::rpc::COPROCESSOR, &req.encode_to_vec()).await;
        assert_eq!(trailers.unwrap()["grpc-status"], "0");
        let resp = crate::rpc::CopResponse::decode(&messages[0][..]).unwrap();
        assert_eq!(resp.other_error, "");
        assert_eq!(
            select_response(&resp.data).get_encode_type(),
            EncodeType::TypeChunk
        );

        // server_info: one empty response; search_log: an empty stream.
        let (_, messages, trailers) = grpc_call(addr, crate::rpc::SERVER_INFO, &[8, 1]).await;
        assert_eq!(trailers.unwrap()["grpc-status"], "0");
        assert_eq!(messages, vec![Vec::<u8>::new()]);
        let (_, messages, trailers) = grpc_call(addr, crate::rpc::SEARCH_LOG, &[8, 1, 16, 2]).await;
        assert_eq!(trailers.unwrap()["grpc-status"], "0");
        assert!(messages.is_empty());

        // Trailers-only answers: the status is in the headers.
        for (path, code, message) in [
            (
                "/tikvpb.Tikv/KvGet",
                "12",
                "unknown method KvGet for service tikvpb.Tikv",
            ),
            (
                "/tikvpb.Tikv/CoprocessorStream",
                "12",
                "unknown method CoprocessorStream for service tikvpb.Tikv",
            ),
            (
                "/grpc.health.v1.Health/Check",
                "12",
                "unknown service grpc.health.v1.Health",
            ),
        ] {
            let (headers, messages, trailers) = grpc_call(addr, path, &[]).await;
            assert_eq!(headers["grpc-status"], code, "{path}");
            assert_eq!(headers["grpc-message"], message, "{path}");
            assert!(messages.is_empty() && trailers.is_none(), "{path}");
        }
        let (headers, _, _) = grpc_call(addr, crate::rpc::COPROCESSOR, &[0xff]).await;
        assert_eq!(headers["grpc-status"], "13");
        let _ = stop.send(());
    }

    /// HTTP/2 without a gRPC content type is still the status API.
    #[tokio::test]
    async fn http2_without_grpc_is_the_status_api() {
        use http_body_util::BodyExt;
        let (addr, stop) = start().await;
        for (path, status) in [
            ("/status", StatusCode::OK),
            ("/tikvpb.Tikv/Coprocessor", StatusCode::NOT_FOUND),
        ] {
            let req = Request::get(format!("http://{addr}{path}"))
                .version(hyper::Version::HTTP_2)
                .body(http_body_util::Full::new(Bytes::new()))
                .unwrap();
            let resp = h2(addr).await.send_request(req).await.unwrap();
            assert_eq!(resp.status(), status, "{path}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            if status == StatusCode::OK {
                assert!(body.starts_with(b"{\"connections\""), "{body:?}");
            } else {
                assert_eq!(&body[..], b"404 page not found\n");
            }
        }
        let _ = stop.send(());
    }
}
