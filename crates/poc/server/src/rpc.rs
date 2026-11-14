//! The gRPC services Go TiDB serves on its status port (`pkg/server/rpc_server.go`), reduced to
//! what other TiDB nodes call on a tidb-rs node, so that they get an answer instead of a retry.
//!
//! Go TiDB lists every node of `/tidb/server/info` and calls `ip:status_port` (issue #161):
//! - `tikvpb.Tikv/Coprocessor` with a TiDB-store DAG request (`buildTiDBMemCopTasks`):
//!   the `CLUSTER_*` memory tables, and the `FLUSH STATS_DELTA ... CLUSTER` broadcast that
//!   every `ANALYZE` sends first (`flushStatsDeltaForAnalyze`). client-go retries any
//!   transport error of a TiDB-store request (an `Unimplemented` status included) with
//!   `tikvRPC` backoff until the 40 s budget runs out, so these must be answered.
//! - `diagnosticspb.Diagnostics/server_info` (`cluster_load`, `cluster_hardware`,
//!   `cluster_systeminfo`) and `/search_log` (`cluster_log`): plain gRPC calls without
//!   backoff, which add a warning on an error.
//!
//! The answers:
//! - Coprocessor: Go's `CoprocessorDAGHandler` with executors that produce no rows. A
//!   `FLUSH STATS_DELTA` broadcast succeeds (this node writes nothing, so it has no stats
//!   delta to flush); a memory-table scan returns an empty `SelectResponse` in the requested
//!   encode type, so Go merges zero rows from this node. Anything else gets Go's
//!   `other_error`, which client-go reports at once without backoff
//!   (`copIteratorWorker.handleCopResponse`).
//! - Diagnostics: an empty `ServerInfoResponse` and an empty `search_log` stream, so Go adds
//!   no rows and no warning for this node.
//! - Other methods: `Unimplemented`, as grpc-go answers an unknown method. Go also serves
//!   `CoprocessorStream`, `BatchCommands` (client-go batches only to TiKV),
//!   `ReportMPPTaskStatus` (TiFlash reports to the coordinating TiDB, never this node) and
//!   TopSQL's `TopSQLPubSub` (ng-monitoring finds TiDBs in `/topology/tidb`, which tidb-rs
//!   does not write).
//!
//! There is no gRPC server stack here: the requests arrive in the status server's hyper
//! service (Go uses cmux on one port the same way), and the few kvproto messages are declared
//! below with their kvproto field tags. tipb comes from the `tipb` crate.
//!
//! go-file: pkg/server/rpc_server.go
//! go-file: pkg/executor/coprocessor.go
//! go-file: pkg/planner/core/pb_to_plan.go

use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use http_body_util::BodyExt;
use http_body_util::Limited;
use hyper::HeaderMap;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Version;
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::header::HeaderValue;
use prost::Message as _;
use protobuf::Message as _;
use tipb::DagRequest;
use tipb::ExecType;
use tipb::Executor;
use tipb::ExecutorExecutionSummary;
use tipb::SelectResponse;
use tracing::debug;

use crate::status::Body;

pub(crate) const COPROCESSOR: &str = "/tikvpb.Tikv/Coprocessor";
pub(crate) const SERVER_INFO: &str = "/diagnosticspb.Diagnostics/server_info";
pub(crate) const SEARCH_LOG: &str = "/diagnosticspb.Diagnostics/search_log";

/// `kv.ReqTypeDAG` (`pkg/kv/kv.go`).
const REQ_TYPE_DAG: i64 = 103;

/// grpc-go's default `MaxRecvMsgSize` (4 MiB); Go TiDB's status server keeps it.
const MAX_RECV_MSG_SIZE: usize = 4 << 20;

/// kvproto `coprocessor.Request`: the fields read here (prost skips the others).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CopRequest {
    #[prost(int64, tag = "2")]
    pub tp: i64,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
}

/// kvproto `coprocessor.Response`: the fields Go's TiDB handler sets.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CopResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    #[prost(string, tag = "4")]
    pub other_error: String,
}

/// kvproto `diagnosticspb.ServerInfoRequest`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ServerInfoRequest {
    #[prost(int32, tag = "1")]
    pub tp: i32,
}

/// kvproto `diagnosticspb.ServerInfoResponse`; `items` (tag 1) stays empty.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ServerInfoResponse {}

/// kvproto `diagnosticspb.SearchLogRequest`: the fields read here.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SearchLogRequest {
    #[prost(int64, tag = "1")]
    pub start_time: i64,
    #[prost(int64, tag = "2")]
    pub end_time: i64,
}

/// gRPC status codes used here (`google.golang.org/grpc/codes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    Internal = 13,
    ResourceExhausted = 8,
    Unimplemented = 12,
}

// glue: cmux's HTTP/2 + `content-type: application/grpc` matcher (pkg/server/server.go
// startStatusServerAndRPCServer), applied inside the one hyper service.
pub(crate) fn is_grpc<B>(req: &Request<B>) -> bool {
    req.version() == Version::HTTP_2
        && req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/grpc"))
}

// glue: grpc-go's unary/server-streaming dispatch for the three methods served here.
pub(crate) async fn serve(req: Request<Incoming>) -> Response<Body> {
    let path = req.uri().path().to_owned();
    if !matches!(path.as_str(), COPROCESSOR | SERVER_INFO | SEARCH_LOG) {
        debug!(%path, "status port: unimplemented gRPC method");
        return status_only(Code::Unimplemented, &unknown_method(&path));
    }
    let body = match Limited::new(req.into_body(), MAX_RECV_MSG_SIZE + 5)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            return status_only(
                Code::ResourceExhausted,
                &format!("grpc: received message larger than max ({MAX_RECV_MSG_SIZE})"),
            );
        }
        Err(e) => return status_only(Code::Internal, &format!("grpc: {e}")),
    };
    let msg = match unframe(&body) {
        Ok(m) => m,
        Err(e) => return status_only(Code::Internal, &e),
    };
    let unmarshal_error = |e: prost::DecodeError| {
        status_only(
            Code::Internal,
            &format!("grpc: error unmarshalling request: {e}"),
        )
    };
    match path.as_str() {
        COPROCESSOR => match CopRequest::decode(msg) {
            Ok(r) => {
                let resp = coprocessor(&r);
                debug!(tp = r.tp, other_error = %resp.other_error, "status port: coprocessor");
                reply(&[resp.encode_to_vec()])
            }
            Err(e) => unmarshal_error(e),
        },
        SERVER_INFO => match ServerInfoRequest::decode(msg) {
            Ok(r) => {
                debug!(tp = r.tp, "status port: diagnostics server_info");
                reply(&[server_info(&r).encode_to_vec()])
            }
            Err(e) => unmarshal_error(e),
        },
        _ => match SearchLogRequest::decode(msg) {
            Ok(r) => {
                debug!(
                    r.start_time,
                    r.end_time, "status port: diagnostics search_log"
                );
                reply(&[])
            }
            Err(e) => unmarshal_error(e),
        },
    }
}

/// grpc-go's text for an unknown method or service (`server.go` `handleStream`).
fn unknown_method(path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    match path.rsplit_once('/') {
        Some((service @ ("tikvpb.Tikv" | "diagnosticspb.Diagnostics"), method)) => {
            format!("unknown method {method} for service {service}")
        }
        Some((service, _)) => format!("unknown service {service}"),
        None => format!("malformed method name: {path:?}"),
    }
}

// go: rpcServer.Coprocessor L2 (no session and no panic recovery: nothing here can panic)
// go: rpcServer.handleCopRequest L2 (no session: the executors here read no state)
// go: CoprocessorDAGHandler.HandleRequest L2 (the executors produce no rows, so the chunk loop
// ends at once)
pub(crate) fn coprocessor(req: &CopRequest) -> CopResponse {
    let dag = match build_dag_executor(req) {
        Ok(dag) => dag,
        Err(e) => return build_error_response(e),
    };
    build_unary_response(&dag, Vec::new())
}

// go: CoprocessorDAGHandler.buildDAGExecutor L2 (user, time zone and flags are not applied: no
// expression is evaluated; Go rejects an unknown time zone name, this accepts it)
fn build_dag_executor(req: &CopRequest) -> Result<DagRequest, String> {
    if req.tp != REQ_TYPE_DAG {
        return Err(format!("unsupported request type {}", req.tp));
    }
    let dag: DagRequest = protobuf::parse_from_bytes(&req.data).map_err(|e| e.to_string())?;
    build(dag.get_executors())?;
    Ok(dag)
}

// go: PBPlanBuilder.Build L2 (checks each executor; there is no plan to build)
// go: PBPlanBuilder.pbToPhysicalPlan L2 (same dispatch; Kill is not supported: Go sends a
// remote KILL only to the node of the connection's server ID, and tidb-rs registers none)
fn build(executors: &[Executor]) -> Result<(), String> {
    if executors.is_empty() {
        // glue: Go dereferences the nil plan and answers with its recovered panic.
        return Err("empty executors in DAG request".to_owned());
    }
    for e in executors {
        match e.get_tp() {
            // pbToTableScan: a TiDB memory table, read from this node. tidb-rs has no TiDB
            // memory-table rows yet, so every table is empty (Go also checks that the table
            // ID is a cluster table; tidb-rs has no table for the ID). Go's partial
            // aggregation without GROUP BY sends one row (e.g. count 0) for an empty table;
            // here none, which Go's final aggregation merges to the same result.
            ExecType::TypeTableScan
            | ExecType::TypeSelection
            | ExecType::TypeTopN
            | ExecType::TypeLimit
            | ExecType::TypeAggregation
            | ExecType::TypeStreamAgg => {}
            ExecType::TypeBroadcastQuery => pb_to_broadcast_query(e)?,
            tp => return Err(format!("this exec type {tp:?} doesn't support yet")),
        }
    }
    Ok(())
}

// go: PBPlanBuilder.pbToBroadcastQuery L2 (keyword check instead of parser.ParseOneStmt: the
// parser here has no FLUSH; Go executes the FLUSH, which has nothing to flush on this node)
fn pb_to_broadcast_query(e: &Executor) -> Result<(), String> {
    let query = e.get_broadcast_query().get_query();
    let mut words = query.split_ascii_whitespace();
    if !words
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("FLUSH"))
    {
        return Err(format!("unexpected statement {query} in broadcast query"));
    }
    // FlushStmt: "FLUSH" NoWriteToBinLogAliasOpt FlushOption (parser.y).
    let mut word = words.next();
    if word.is_some_and(|w| {
        w.eq_ignore_ascii_case("NO_WRITE_TO_BINLOG") || w.eq_ignore_ascii_case("LOCAL")
    }) {
        word = words.next();
    }
    if !word.is_some_and(|w| w.eq_ignore_ascii_case("STATS_DELTA")) {
        return Err(format!(
            "unexpected flush statement {query} in broadcast query"
        ));
    }
    Ok(())
}

// go: CoprocessorDAGHandler.buildUnaryResponse L1
fn build_unary_response(dag: &DagRequest, chunks: Vec<tipb::Chunk>) -> CopResponse {
    let mut sel_resp = SelectResponse::default();
    sel_resp.set_chunks(chunks.into());
    sel_resp.set_encode_type(dag.get_encode_type());
    if dag.has_collect_execution_summaries() && dag.get_collect_execution_summaries() {
        // TODO in Go: "Add real executor execution summary information."
        let exec_summary = vec![ExecutorExecutionSummary::default(); dag.get_executors().len()];
        sel_resp.set_execution_summaries(exec_summary.into());
    }
    match sel_resp.write_to_bytes() {
        Ok(data) => CopResponse {
            data,
            other_error: String::new(),
        },
        Err(e) => build_error_response(e.to_string()),
    }
}

// go: CoprocessorDAGHandler.buildErrorResponse L2 (the message without errors.ErrorStack's
// stack trace)
fn build_error_response(err: String) -> CopResponse {
    CopResponse {
        data: Vec::new(),
        other_error: err,
    }
}

// go: github.com/pingcap/sysutil@v1.0.1-0.20240311050922-ae78a3e01b8b.DiagnosticsServer.ServerInfo L2
// (no items: load, hardware and system information of this node are not collected yet)
fn server_info(_req: &ServerInfoRequest) -> ServerInfoResponse {
    ServerInfoResponse {}
}

/// The one length-prefixed message of a unary or server-streaming request.
// glue: gRPC over HTTP/2 framing (grpc-go rpc_util.go recvMsg).
fn unframe(body: &Bytes) -> Result<Bytes, String> {
    if body.len() < 5 {
        return Err("grpc: missing request message".to_owned());
    }
    if body[0] != 0 {
        return Err("grpc: compressed flag set with identity or empty encoding".to_owned());
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if len > MAX_RECV_MSG_SIZE {
        return Err(format!(
            "grpc: received message larger than max ({len} vs. {MAX_RECV_MSG_SIZE})"
        ));
    }
    if body.len() != 5 + len {
        return Err("grpc: malformed request message".to_owned());
    }
    Ok(body.slice(5..))
}

/// An OK response carrying `messages` (none for an empty server stream).
// glue: gRPC over HTTP/2 framing.
fn reply(messages: &[Vec<u8>]) -> Response<Body> {
    let mut data = BytesMut::new();
    for m in messages {
        data.put_u8(0);
        data.put_u32(u32::try_from(m.len()).expect("response message below 4 GiB"));
        data.put_slice(m);
    }
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));
    grpc_response(Body::with_trailers(data.freeze(), trailers))
}

/// A "trailers-only" error response: the status in the headers, no body.
// glue: gRPC over HTTP/2 framing.
fn status_only(code: Code, message: &str) -> Response<Body> {
    let mut resp = grpc_response(Body::empty());
    let headers = resp.headers_mut();
    headers.insert("grpc-status", HeaderValue::from(code as u16));
    if let Ok(v) = HeaderValue::from_str(&percent_encode(message)) {
        headers.insert("grpc-message", v);
    }
    resp
}

fn grpc_response(body: Body) -> Response<Body> {
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
    resp
}

/// `grpc-message` percent-encoding (gRPC HTTP/2 protocol spec).
// glue: grpc-go internal/grpcutil encodeGrpcMessage.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Encoded tipb messages for the tests.
#[cfg(test)]
pub(crate) mod testdata {
    use super::*;
    pub(crate) use tipb::EncodeType;

    pub(crate) fn dag(executors: Vec<Executor>, encode_type: EncodeType, collect: bool) -> Vec<u8> {
        let mut dag = DagRequest::default();
        dag.set_start_ts_fallback(u64::MAX);
        dag.set_time_zone_name("UTC".to_owned());
        dag.set_flags(0);
        dag.set_output_offsets(vec![0, 1]);
        dag.set_executors(executors.into());
        dag.set_encode_type(encode_type);
        if collect {
            dag.set_collect_execution_summaries(true);
        }
        dag.write_to_bytes().unwrap()
    }

    pub(crate) fn broadcast(query: &str) -> Executor {
        let mut e = Executor::default();
        e.set_tp(ExecType::TypeBroadcastQuery);
        e.mut_broadcast_query().set_query(query.to_owned());
        e
    }

    pub(crate) fn table_scan(table_id: i64) -> Executor {
        let mut e = Executor::default();
        e.set_tp(ExecType::TypeTableScan);
        e.mut_tbl_scan().set_table_id(table_id);
        e
    }

    pub(crate) fn of_type(tp: ExecType) -> Executor {
        let mut e = Executor::default();
        e.set_tp(tp);
        e
    }

    pub(crate) fn select_response(data: &[u8]) -> SelectResponse {
        protobuf::parse_from_bytes(data).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testdata::*;
    use super::*;

    fn cop(tp: i64, data: Vec<u8>) -> CopResponse {
        coprocessor(&CopRequest { tp, data })
    }

    #[test]
    fn broadcast_flush_stats_delta_succeeds() {
        for (query, collect) in [
            ("FLUSH STATS_DELTA `a`.`t` CLUSTER", false),
            ("flush no_write_to_binlog stats_delta CLUSTER", true),
            ("FLUSH LOCAL STATS_DELTA", false),
        ] {
            let resp = cop(
                REQ_TYPE_DAG,
                dag(vec![broadcast(query)], EncodeType::TypeDefault, collect),
            );
            assert_eq!(resp.other_error, "", "{query}");
            let sel = select_response(&resp.data);
            assert!(sel.get_chunks().is_empty());
            assert_eq!(sel.get_encode_type(), EncodeType::TypeDefault);
            assert!(sel.has_encode_type());
            assert_eq!(sel.get_execution_summaries().len(), usize::from(collect));
        }
    }

    #[test]
    fn broadcast_other_statements_fail_like_go() {
        let resp = cop(
            REQ_TYPE_DAG,
            dag(
                vec![broadcast("FLUSH TABLES")],
                EncodeType::TypeDefault,
                false,
            ),
        );
        assert_eq!(
            resp.other_error,
            "unexpected flush statement FLUSH TABLES in broadcast query"
        );
        let resp = cop(
            REQ_TYPE_DAG,
            dag(vec![broadcast("SELECT 1")], EncodeType::TypeDefault, false),
        );
        assert_eq!(
            resp.other_error,
            "unexpected statement SELECT 1 in broadcast query"
        );
    }

    #[test]
    fn memtable_scan_is_empty_in_the_requested_encoding() {
        for encode in [EncodeType::TypeDefault, EncodeType::TypeChunk] {
            let executors = vec![
                table_scan(4611686018427387967),
                of_type(ExecType::TypeSelection),
                of_type(ExecType::TypeAggregation),
            ];
            let resp = cop(REQ_TYPE_DAG, dag(executors, encode, true));
            assert_eq!(resp.other_error, "");
            let sel = select_response(&resp.data);
            assert!(sel.get_chunks().is_empty());
            assert_eq!(sel.get_encode_type(), encode);
            assert_eq!(sel.get_execution_summaries().len(), 3);
        }
    }

    #[test]
    fn everything_else_is_an_other_error() {
        assert_eq!(
            cop(104, Vec::new()).other_error,
            "unsupported request type 104"
        );
        assert!(!cop(REQ_TYPE_DAG, vec![0xff, 0xff]).other_error.is_empty());
        assert_eq!(
            cop(
                REQ_TYPE_DAG,
                dag(
                    vec![of_type(ExecType::TypeKill)],
                    EncodeType::TypeDefault,
                    false
                )
            )
            .other_error,
            "this exec type TypeKill doesn't support yet"
        );
        assert_eq!(
            cop(REQ_TYPE_DAG, dag(vec![], EncodeType::TypeDefault, false)).other_error,
            "empty executors in DAG request"
        );
    }

    #[test]
    fn grpc_message_is_percent_encoded() {
        assert_eq!(percent_encode("a b%c\n日"), "a b%25c%0A%E6%97%A5");
        assert_eq!(
            unknown_method("/tikvpb.Tikv/KvGet"),
            "unknown method KvGet for service tikvpb.Tikv"
        );
        assert_eq!(unknown_method("/x.Y/Z"), "unknown service x.Y");
    }
}
