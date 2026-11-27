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
use http_body_util::BodyExt as _;
use hyper::HeaderMap;
use hyper::Method;
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
use tracing::warn;

/// What `/status` reports.
#[derive(Clone, Debug)]
pub struct StatusInfo {
    /// The MySQL server version string (Go: `mysql.ServerVersion`).
    pub version: String,
    pub git_hash: String,
}

/// Formats a bound listen address the way Go's `net.Listen("tcp", ...)` reports it:
/// unspecified addresses (`0.0.0.0` / `::`) render as `[::]:<port>` because Go opens a dual-stack
/// listener; specific IP addresses keep their standard `<ip>:<port>` formatting.
fn format_go_listen_addr(addr: std::net::SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("[::]:{}", addr.port())
    } else {
        addr.to_string()
    }
}

/// Serve the status API on `listener` until `shutdown` completes.
pub async fn serve(listener: TcpListener, info: StatusInfo, shutdown: impl Future<Output = ()>) {
    let local_addr = match listener.local_addr() {
        Ok(addr) => {
            let addr_str = format_go_listen_addr(addr);
            tidb_base::log_info!(
                "for status and metrics report",
                "listening on addr" => &addr_str,
            );
            Some(addr_str)
        }
        Err(e) => {
            warn!(error = %e, "status server address unknown");
            None
        }
    };
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
                        handle(req, &info).await
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
    // Emit the three Go status/RPC shutdown WARNs (pkg/server/http_status.go:535,540,545,
    // spec §4.2) when the status/gRPC server stops. Match Go's `net.OpError` format:
    // `accept tcp <addr>: use of closed network connection`.
    let accept_err = match local_addr {
        Some(addr) => format!("accept tcp {addr}: use of closed network connection"),
        None => "use of closed network connection".to_owned(),
    };
    tidb_base::log_warn!("grpc server error", "error" => "mux: server closed");
    tidb_base::log_warn!("http server error", "error" => "http: Server closed");
    tidb_base::log_warn!("start status/rpc server error", "error" => &accept_err);
}

async fn handle(req: Request<Incoming>, info: &StatusInfo) -> Response<Body> {
    match (req.method(), req.uri().path()) {
        (_, "/metrics") => response(
            StatusCode::OK,
            tidb_metrics::encode::CONTENT_TYPE,
            tidb_metrics::gather_text(),
        ),
        (_, "/status") => response(StatusCode::OK, "application/json", status_json(info)),
        (_, "/debug/route") => route_trace(req.uri().query()),
        (&Method::POST, "/settings") => handle_settings(req).await,
        _ => response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "404 page not found\n".to_owned(),
        ),
    }
}

/// `POST /settings` with form fields `log_level=<lv>` and/or `tidb_general_log=<0|1|off|on>`
/// (Go `pkg/server/handler/tikvhandler/tikv_handler.go:418-437`).
async fn handle_settings(req: Request<Incoming>) -> Response<Body> {
    const MAX_SETTINGS_BODY: usize = 64 * 1024;
    let mut raw = Vec::new();
    let mut body = req.into_body();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Some(chunk) = f.data_ref() {
                    if raw.len().saturating_add(chunk.len()) > MAX_SETTINGS_BODY {
                        return response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "text/plain; charset=utf-8",
                            "settings body too large\n".to_owned(),
                        );
                    }
                    raw.extend_from_slice(chunk);
                }
            }
            Err(e) => {
                return response(
                    StatusCode::BAD_REQUEST,
                    "text/plain; charset=utf-8",
                    format!("read body failed: {e}\n"),
                );
            }
        }
    }
    let form = String::from_utf8_lossy(&raw);
    let mut handled = false;
    for pair in form.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let key = percent_decode_form(k);
        let val = percent_decode_form(v);
        match key.as_str() {
            "log_level" => {
                if val.is_empty() {
                    return response(
                        StatusCode::BAD_REQUEST,
                        "text/plain; charset=utf-8",
                        "empty log_level value\n".to_owned(),
                    );
                }
                if let Err(e) = tidb_log::set_level(&val) {
                    return response(
                        StatusCode::BAD_REQUEST,
                        "text/plain; charset=utf-8",
                        format!("{e}\n"),
                    );
                }
                // Keep global config in sync so later `SET GLOBAL tidb_log_file_max_days`
                // (`replace_logger`) does not revert the dynamic log level.
                tidb_base::config::update_global_config(|c| c.log.level = val.clone());
                handled = true;
            }
            "tidb_general_log" => {
                // Match Go tikv_handler.go:428-436: strictly "0" or "1", else "illegal argument".
                let on = match val.as_str() {
                    "1" => true,
                    "0" => false,
                    _ => {
                        return response(
                            StatusCode::BAD_REQUEST,
                            "text/plain; charset=utf-8",
                            "illegal argument\n".to_owned(),
                        );
                    }
                };
                tidb_sessionctx::variable::tidb_vars::ProcessGeneralLog
                    .store(on, std::sync::atomic::Ordering::SeqCst);
                handled = true;
            }
            _ => {}
        }
    }
    if handled {
        response(
            StatusCode::OK,
            "text/plain; charset=utf-8",
            "success\n".to_owned(),
        )
    } else {
        response(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            "missing log_level or tidb_general_log parameter\n".to_owned(),
        )
    }
}

fn percent_decode_form(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `GET /debug/route?conn=<id>`: the route trace of a connection
/// (`poc_session::router::trace`), as `<seq>\t<engine>\t<reason>\n`; 404 while the trace is
/// off (`--route-trace`).
fn route_trace(query: Option<&str>) -> Response<Body> {
    use poc_session::router::trace;
    const TEXT: &str = "text/plain; charset=utf-8";
    if !trace::enabled() {
        return response(
            StatusCode::NOT_FOUND,
            TEXT,
            "route trace off (start with --route-trace)\n".to_owned(),
        );
    }
    let conn = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("conn="))
        .and_then(|v| v.parse::<u64>().ok());
    match conn {
        Some(c) => {
            let l = trace::get(c);
            response(
                StatusCode::OK,
                TEXT,
                format!("{}\t{}\t{}\n", l.seq, l.engine, l.reason),
            )
        }
        None => response(
            StatusCode::BAD_REQUEST,
            TEXT,
            "want ?conn=<connection id>\n".to_owned(),
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
        grpc_call_encoded(addr, path, msg, None).await
    }

    /// `grpc_call` with the message compressed in `encoding` (`grpc-encoding`).
    async fn grpc_call_encoded(
        addr: std::net::SocketAddr,
        path: &str,
        msg: &[u8],
        encoding: Option<&str>,
    ) -> (hyper::HeaderMap, Vec<Vec<u8>>, Option<hyper::HeaderMap>) {
        use http_body_util::BodyExt;
        let gzipped;
        let (flag, msg) = match encoding {
            Some("gzip") => {
                gzipped = crate::rpc::testdata::gzip(msg);
                (1u8, &gzipped[..])
            }
            Some(_) => (1u8, msg),
            None => (0u8, msg),
        };
        let mut framed = vec![flag];
        framed.extend_from_slice(&u32::try_from(msg.len()).unwrap().to_be_bytes());
        framed.extend_from_slice(msg);
        let mut req = Request::post(format!("http://{addr}{path}"))
            .version(hyper::Version::HTTP_2)
            .header(CONTENT_TYPE, "application/grpc")
            .header("te", "trailers");
        if let Some(encoding) = encoding {
            req = req.header("grpc-encoding", encoding);
        }
        let req = req
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

    /// Go TiDB with `tikv-client.grpc-compression-type = "gzip"` compresses its requests to
    /// status ports too: they are answered, not refused (which client-go would retry for 40 s).
    #[tokio::test]
    async fn gzip_grpc_requests_are_answered() {
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
        let (_, messages, trailers) = grpc_call_encoded(
            addr,
            crate::rpc::COPROCESSOR,
            &req.encode_to_vec(),
            Some("gzip"),
        )
        .await;
        assert_eq!(trailers.unwrap()["grpc-status"], "0");
        let resp = crate::rpc::CopResponse::decode(&messages[0][..]).unwrap();
        assert_eq!(resp.other_error, "");

        let (headers, messages, _) =
            grpc_call_encoded(addr, crate::rpc::SERVER_INFO, &[8, 1], Some("snappy")).await;
        assert_eq!(headers["grpc-status"], "12");
        assert_eq!(
            headers["grpc-message"],
            "grpc: Decompressor is not installed for grpc-encoding \"snappy\""
        );
        assert!(messages.is_empty());
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

    fn normalize_status_caller_line(line: &str) -> String {
        if let Some(start) = line.find("[status.rs:")
            && let Some(rel_end) = line[start..].find(']')
        {
            let digits = &line[start + "[status.rs:".len()..start + rel_end];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return format!(
                    "{}[status.rs:<line>]{}",
                    &line[..start],
                    &line[start + rel_end + 1..]
                );
            }
        }
        line.to_owned()
    }

    #[tokio::test]
    async fn status_shutdown_warns_include_bound_addr() {
        let sink = tidb_log::MemorySink::new();
        let cfg = tidb_base::config::LogConfig {
            level: "info".to_owned(),
            format: "text".to_owned(),
            disable_timestamp: true,
            disable_error_verbose: true,
            ..tidb_base::config::LogConfig::default()
        };
        tidb_log::init_logger_with_sinks(&cfg, "", sink.clone(), None).unwrap();

        for (bind_addr, expect_unspecified) in [("127.0.0.1:0", false), ("0.0.0.0:0", true)] {
            let listener = TcpListener::bind(bind_addr).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let port = addr.port();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let info = StatusInfo {
                version: "8.5.8-TiDB-v8.5.8".into(),
                git_hash: "test".into(),
            };
            let handle = tokio::spawn(async move {
                serve(listener, info, async move {
                    let _ = stop_rx.await;
                })
                .await;
            });
            let _ = stop_tx.send(());
            handle.await.unwrap();

            let captured = String::from_utf8(sink.take()).unwrap();
            let lines: Vec<String> = captured.lines().map(normalize_status_caller_line).collect();
            let (want_listen_field, want_accept_addr) = if expect_unspecified {
                (
                    format!(r#"["listening on addr"="[::]:{port}"]"#),
                    format!("[::]:{port}"),
                )
            } else {
                (
                    format!(r#"["listening on addr"=127.0.0.1:{port}]"#),
                    format!("127.0.0.1:{port}"),
                )
            };
            assert_eq!(
                lines,
                vec![
                    format!(
                        r#"[INFO] [status.rs:<line>] ["for status and metrics report"] {want_listen_field}"#
                    ),
                    r#"[WARN] [status.rs:<line>] ["grpc server error"] [error="mux: server closed"]"#
                        .to_owned(),
                    r#"[WARN] [status.rs:<line>] ["http server error"] [error="http: Server closed"]"#
                        .to_owned(),
                    format!(
                        r#"[WARN] [status.rs:<line>] ["start status/rpc server error"] [error="accept tcp {want_accept_addr}: use of closed network connection"]"#
                    ),
                ],
                "bind_addr={bind_addr}"
            );
        }
    }
}
