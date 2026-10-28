//! Prometheus metrics of tidb-rs, named, labelled and bucketed exactly like Go TiDB v8.5.8 so
//! the stock TiDB Grafana dashboard shows the node (see `docs/adr/0006-metrics.md` for the
//! table of Go sources and the semantic deviations).
//!
//! Everything is registered in the `prometheus` crate's default registry, which tikv-client
//! (`tidb_tikvclient_*`, `pd_client_*`) uses too; [`gather_text`] renders all of it in Go's
//! text format ([`encode`]).
//!
//! Hot-path cost: every series that statements touch is a pre-resolved child (an `Arc` to the
//! counter or histogram) kept in a static, created once on first use or at [`init`]. Recording
//! is then a few relaxed atomic adds, with no label hashing, locking or allocation.

#![forbid(unsafe_code)]

pub mod encode;
pub mod golden;
pub mod process;

use std::sync::LazyLock;
use std::sync::OnceLock;
use std::time::Duration;

use prometheus::Histogram;
use prometheus::HistogramOpts;
use prometheus::HistogramVec;
use prometheus::IntCounter;
use prometheus::IntCounterVec;
use prometheus::IntGauge;
use prometheus::IntGaugeVec;
use prometheus::Opts;
use prometheus::exponential_buckets;

/// Go `resourcegroup.DefaultResourceGroupName`: every session of this node is in it.
pub const DEFAULT_RESOURCE_GROUP: &str = "default";
/// Go `metrics.LblGeneral`: user (not internal) statements.
pub const GENERAL: &str = "general";
/// The `db` label: Go only fills it with `status.record-db-label = true` (default false).
const NO_DB: &str = "";

/// Go's slow-query threshold (`instance.tidb_slow_log_threshold`, default 300 ms): statements
/// at least this long are observed in `tidb_server_slow_query_process_duration_seconds`.
pub const SLOW_THRESHOLD: Duration = Duration::from_millis(300);

fn exp_buckets(start: f64, factor: f64, count: usize) -> Vec<f64> {
    exponential_buckets(start, factor, count).expect("valid buckets")
}

fn register<C: prometheus::core::Collector + Clone + 'static>(c: C) -> C {
    // A second registration of the same collector (tests running `init` twice) is harmless.
    let _ = prometheus::register(Box::new(c.clone()));
    c
}

fn counter_vec(ns_sub_name: (&str, &str, &str), help: &str, labels: &[&str]) -> IntCounterVec {
    let (ns, sub, name) = ns_sub_name;
    register(
        IntCounterVec::new(Opts::new(name, help).namespace(ns).subsystem(sub), labels)
            .expect("valid metric"),
    )
}

fn histogram_vec(
    ns_sub_name: (&str, &str, &str),
    help: &str,
    buckets: Vec<f64>,
    labels: &[&str],
) -> HistogramVec {
    let (ns, sub, name) = ns_sub_name;
    register(
        HistogramVec::new(
            HistogramOpts::new(name, help)
                .namespace(ns)
                .subsystem(sub)
                .buckets(buckets),
            labels,
        )
        .expect("valid metric"),
    )
}

fn histogram(ns_sub_name: (&str, &str, &str), help: &str, buckets: Vec<f64>) -> Histogram {
    let (ns, sub, name) = ns_sub_name;
    register(
        Histogram::with_opts(
            HistogramOpts::new(name, help)
                .namespace(ns)
                .subsystem(sub)
                .buckets(buckets),
        )
        .expect("valid metric"),
    )
}

// ---------------------------------------------------------------------------------------------
// Label values.

/// Statement type label (Go `ast.GetStmtLabel`, stored in `StmtCtx.StmtType`), plus
/// `general` for commands that ran no statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StmtType {
    General,
    Select,
    Use,
    Set,
    Show,
    Begin,
    Commit,
    Rollback,
}

impl StmtType {
    const COUNT: usize = 8;

    /// The label value.
    pub fn as_str(self) -> &'static str {
        match self {
            StmtType::General => GENERAL,
            StmtType::Select => "Select",
            StmtType::Use => "Use",
            StmtType::Set => "Set",
            StmtType::Show => "Show",
            StmtType::Begin => "Begin",
            StmtType::Commit => "Commit",
            StmtType::Rollback => "Rollback",
        }
    }
}

/// Go `server/metrics.CmdToString`: named commands, others as their decimal number.
pub fn command_label(cmd: u8) -> &'static str {
    static NUMBERS: LazyLock<Vec<String>> =
        LazyLock::new(|| (0..=255u16).map(|n| n.to_string()).collect());
    match cmd {
        0x00 => "Sleep",
        0x01 => "Quit",
        0x02 => "InitDB",
        0x03 => "Query",
        0x0e => "Ping",
        0x04 => "FieldList",
        0x16 => "StmtPrepare",
        0x17 => "StmtExecute",
        0x1c => "StmtFetch",
        0x19 => "StmtClose",
        0x18 => "StmtSendLongData",
        0x1a => "StmtReset",
        0x1b => "SetOption",
        n => &NUMBERS[usize::from(n)],
    }
}

/// The `type` of `tidb_server_execute_error_total` for a MySQL error code (Go
/// `metrics.ExecuteErrorToLabel`: the `class:code` RFC code of a `terror.Error`, `unknown`
/// for other errors). Codes whose Go error class is unambiguous for the statements this node
/// runs get Go's label; the rest are `unknown`.
pub fn error_label(code: u16) -> &'static str {
    match code {
        // parser.ErrParse
        1064 => "parser:1064",
        // infoschema.ErrTableNotExists / ErrDatabaseNotExists
        1146 => "schema:1146",
        1049 => "schema:1049",
        // plannererrors.ErrUnknownColumn / ErrNoDB / ErrNonUniq
        1054 => "planner:1054",
        1046 => "planner:1046",
        1052 => "planner:1052",
        _ => "unknown",
    }
}

/// The commands whose `query_total` children Go creates at start-up
/// (`server/metrics.InitMetricsVars`).
const NAMED_COMMANDS: [u8; 13] = [
    0x00, 0x01, 0x02, 0x03, 0x0e, 0x04, 0x16, 0x17, 0x1c, 0x19, 0x18, 0x1a, 0x1b,
];

/// `tidb_executor_expensive_total` types (Go `executor/metrics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExpensiveExec {
    MergeJoin,
    HashJoin,
    HashAgg,
    StreamAgg,
    Sort,
    TopN,
    NestedLoopApply,
    IndexLookUpJoin,
    IndexLookUpExecutor,
    IndexMergeReaderExecutor,
}

const EXPENSIVE_NAMES: [&str; 10] = [
    "MergeJoinExec",
    "HashJoinExec",
    "HashAggExec",
    "StreamAggExec",
    "SortExec",
    "TopNExec",
    "NestedLoopApplyExec",
    "IndexLookUpJoin",
    "IndexLookUpExecutor",
    "IndexMergeReaderExecutor",
];

// ---------------------------------------------------------------------------------------------
// tidb_server_* (Go pkg/metrics/server.go).

static PACKET_IO_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "packet_io_bytes"),
        "Counters of packet IO bytes.",
        &["type"],
    )
});
static PACKET_IN: LazyLock<IntCounter> =
    LazyLock::new(|| PACKET_IO_BYTES.with_label_values(&["In"]));
static PACKET_OUT: LazyLock<IntCounter> =
    LazyLock::new(|| PACKET_IO_BYTES.with_label_values(&["Out"]));

static QUERY_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "server", "handle_query_duration_seconds"),
        "Bucketed histogram of processing time (s) of handled queries.",
        exp_buckets(0.0005, 2.0, 29),
        &["sql_type", "db", "resource_group"],
    )
});
static QUERY_DURATION_CHILDREN: [OnceLock<Histogram>; StmtType::COUNT] =
    [const { OnceLock::new() }; StmtType::COUNT];

static QUERY_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "query_total"),
        "Counter of queries.",
        &["type", "result", "resource_group"],
    )
});
/// `[ok, error][command byte]`.
static QUERY_TOTAL_CHILDREN: [[OnceLock<IntCounter>; 256]; 2] =
    [const { [const { OnceLock::new() }; 256] }; 2];

static CONNECTIONS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register(
        IntGaugeVec::new(
            Opts::new("connections", "Number of connections.")
                .namespace("tidb")
                .subsystem("server"),
            &["resource_group"],
        )
        .expect("valid metric"),
    )
});
static CONNECTIONS_DEFAULT: LazyLock<IntGauge> =
    LazyLock::new(|| CONNECTIONS.with_label_values(&[DEFAULT_RESOURCE_GROUP]));

static DISCONNECTION_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "disconnection_total"),
        "Counter of connections disconnected.",
        &["result"],
    )
});

static EXECUTE_ERROR_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "execute_error_total"),
        "Counter of execute errors.",
        &["type", "db", "resource_group"],
    )
});

static PLAN_CACHE_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "plan_cache_total"),
        "Counter of query using plan cache.",
        &["type"],
    )
});
static PLAN_CACHE_MISS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "server", "plan_cache_miss_total"),
        "Counter of plan cache miss.",
        &["type"],
    )
});
static PLAN_CACHE_MISS_PREPARED: LazyLock<IntCounter> =
    LazyLock::new(|| PLAN_CACHE_MISS_TOTAL.with_label_values(&["prepared"]));

static GET_TOKEN_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    histogram(
        ("tidb", "server", "get_token_duration_seconds"),
        "Duration (us) for getting token, it should be small until concurrency limit is reached.",
        exp_buckets(1.0, 2.0, 30),
    )
});

static MULTI_QUERY_NUM: LazyLock<Histogram> = LazyLock::new(|| {
    histogram(
        ("tidb", "server", "multi_query_num"),
        "The number of queries contained in a multi-query statement.",
        exp_buckets(1.0, 2.0, 20),
    )
});

static SLOW_QUERY_PROCESS: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "server", "slow_query_process_duration_seconds"),
        "Bucketed histogram of processing time (s) of of slow queries.",
        exp_buckets(0.001, 2.0, 28),
        &["sql_type"],
    )
});
static SLOW_QUERY_PROCESS_GENERAL: LazyLock<Histogram> =
    LazyLock::new(|| SLOW_QUERY_PROCESS.with_label_values(&[GENERAL]));

static MAXPROCS: LazyLock<IntGauge> = LazyLock::new(|| {
    register(
        IntGauge::with_opts(
            Opts::new("maxprocs", "The value of GOMAXPROCS.")
                .namespace("tidb")
                .subsystem("server"),
        )
        .expect("valid metric"),
    )
});

static CONN_IDLE: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "server", "conn_idle_duration_seconds"),
        "Bucketed histogram of connection idle time (s).",
        exp_buckets(0.0005, 2.0, 29),
        &["in_txn"],
    )
});
static CONN_IDLE_NOT_IN_TXN: LazyLock<Histogram> =
    LazyLock::new(|| CONN_IDLE.with_label_values(&["0"]));

static SERVER_INFO: LazyLock<prometheus::GaugeVec> = LazyLock::new(|| {
    register(
        prometheus::GaugeVec::new(
            Opts::new(
                "info",
                "Indicate the tidb server info, and the value is the start timestamp (s).",
            )
            .namespace("tidb")
            .subsystem("server"),
            &["version", "hash"],
        )
        .expect("valid metric"),
    )
});

static TOKENS: LazyLock<IntGauge> = LazyLock::new(|| {
    register(
        IntGauge::with_opts(
            Opts::new("tokens", "The number of concurrent executing session")
                .namespace("tidb")
                .subsystem("server"),
        )
        .expect("valid metric"),
    )
});

// ---------------------------------------------------------------------------------------------
// tidb_session_* (Go pkg/metrics/session.go).

static PARSE_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "session", "parse_duration_seconds"),
        "Bucketed histogram of processing time (s) in parse SQL.",
        exp_buckets(0.00004, 2.0, 28),
        &["sql_type"],
    )
});
static PARSE_GENERAL: LazyLock<Histogram> =
    LazyLock::new(|| PARSE_DURATION.with_label_values(&[GENERAL]));

static COMPILE_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "session", "compile_duration_seconds"),
        "Bucketed histogram of processing time (s) in query optimize.",
        exp_buckets(0.00004, 2.0, 28),
        &["sql_type"],
    )
});
static COMPILE_GENERAL: LazyLock<Histogram> =
    LazyLock::new(|| COMPILE_DURATION.with_label_values(&[GENERAL]));

static EXECUTE_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "session", "execute_duration_seconds"),
        "Bucketed histogram of processing time (s) in running executor.",
        exp_buckets(0.0001, 2.0, 30),
        &["sql_type"],
    )
});
static EXECUTE_GENERAL: LazyLock<Histogram> =
    LazyLock::new(|| EXECUTE_DURATION.with_label_values(&[GENERAL]));

static TRANSACTION_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "session", "transaction_duration_seconds"),
        "Bucketed histogram of a transaction execution duration, including retry.",
        exp_buckets(0.001, 2.0, 28),
        &["txn_mode", "type", "scope"],
    )
});
static TXN_OPTIMISTIC_COMMIT: LazyLock<Histogram> =
    LazyLock::new(|| TRANSACTION_DURATION.with_label_values(&["optimistic", "commit", GENERAL]));
static TXN_OPTIMISTIC_ABORT: LazyLock<Histogram> =
    LazyLock::new(|| TRANSACTION_DURATION.with_label_values(&["optimistic", "abort", GENERAL]));

// ---------------------------------------------------------------------------------------------
// tidb_executor_* (Go pkg/metrics/executor.go).

static EXPENSIVE_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "executor", "expensive_total"),
        "Counter of Expensive Executors.",
        &["type"],
    )
});
static EXPENSIVE_CHILDREN: LazyLock<Vec<IntCounter>> = LazyLock::new(|| {
    EXPENSIVE_NAMES
        .iter()
        .map(|n| EXPENSIVE_TOTAL.with_label_values(&[n]))
        .collect()
});

static STATEMENT_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    counter_vec(
        ("tidb", "executor", "statement_total"),
        "Counter of StmtNode.",
        &["type", "db", "resource_group"],
    )
});
static STATEMENT_TOTAL_CHILDREN: [OnceLock<IntCounter>; StmtType::COUNT] =
    [const { OnceLock::new() }; StmtType::COUNT];

// ---------------------------------------------------------------------------------------------
// tidb_distsql_* (Go pkg/metrics/distsql.go).

static DISTSQL_QUERY: LazyLock<HistogramVec> = LazyLock::new(|| {
    histogram_vec(
        ("tidb", "distsql", "handle_query_duration_seconds"),
        "Bucketed histogram of processing time (s) of handled queries.",
        exp_buckets(0.0005, 2.0, 29),
        &["type", "sql_type", "copr_type"],
    )
});
static DISTSQL_DAG_PAGING: LazyLock<Histogram> =
    LazyLock::new(|| DISTSQL_QUERY.with_label_values(&["dag", GENERAL, "paging"]));
static DISTSQL_DAG_COMMON: LazyLock<Histogram> =
    LazyLock::new(|| DISTSQL_QUERY.with_label_values(&["dag", GENERAL, "common"]));

// ---------------------------------------------------------------------------------------------
// Start-up.

/// Static facts about this server, set once by [`init`].
#[derive(Clone, Debug)]
pub struct ServerFacts {
    /// `tidb_server_info{version}` (Go: `mysql.TiDBReleaseVersion`).
    pub version: String,
    /// `tidb_server_info{hash}` (Go: `versioninfo.TiDBGitHash`).
    pub git_hash: String,
    /// `tidb_server_maxprocs` (Go: `GOMAXPROCS`): the runtime's worker threads.
    pub maxprocs: usize,
}

static PROCESS_START: OnceLock<f64> = OnceLock::new();

/// Register every metric (so the series Go creates at start-up exist from the first scrape,
/// in particular `tidb_server_connections`, which the dashboard's `$instance` variable lists),
/// the process collector, and the server facts. Idempotent apart from the facts.
pub fn init(facts: &ServerFacts) {
    let start = *PROCESS_START.get_or_init(|| {
        let collector = process::ProcessCollector::new();
        let start = collector.start_time();
        let _ = prometheus::register(Box::new(collector));
        start
    });
    SERVER_INFO
        .with_label_values(&[facts.version.as_str(), facts.git_hash.as_str()])
        .set(start.floor());
    MAXPROCS.set(i64::try_from(facts.maxprocs).unwrap_or(i64::MAX));
    register_all();
}

/// Force every metric and the children Go creates eagerly.
fn register_all() {
    LazyLock::force(&PACKET_IN);
    LazyLock::force(&PACKET_OUT);
    LazyLock::force(&QUERY_DURATION);
    for cmd in NAMED_COMMANDS {
        query_total_child(cmd, true);
        query_total_child(cmd, false);
    }
    LazyLock::force(&CONNECTIONS_DEFAULT);
    for r in ["ok", "error", "undetermined"] {
        DISCONNECTION_TOTAL.with_label_values(&[r]);
    }
    LazyLock::force(&EXECUTE_ERROR_TOTAL);
    for t in ["prepared", "non-prepared"] {
        PLAN_CACHE_TOTAL.with_label_values(&[t]);
    }
    for t in ["prepared", "non-prepared", "non-prepared-unsupported"] {
        PLAN_CACHE_MISS_TOTAL.with_label_values(&[t]);
    }
    LazyLock::force(&GET_TOKEN_DURATION);
    LazyLock::force(&MULTI_QUERY_NUM);
    LazyLock::force(&SLOW_QUERY_PROCESS_GENERAL);
    LazyLock::force(&MAXPROCS);
    LazyLock::force(&CONN_IDLE_NOT_IN_TXN);
    CONN_IDLE.with_label_values(&["1"]);
    LazyLock::force(&SERVER_INFO);
    LazyLock::force(&TOKENS);
    LazyLock::force(&PARSE_GENERAL);
    LazyLock::force(&COMPILE_GENERAL);
    LazyLock::force(&EXECUTE_GENERAL);
    LazyLock::force(&TXN_OPTIMISTIC_COMMIT);
    LazyLock::force(&TXN_OPTIMISTIC_ABORT);
    LazyLock::force(&EXPENSIVE_CHILDREN);
    LazyLock::force(&STATEMENT_TOTAL);
    LazyLock::force(&DISTSQL_QUERY);
}

/// Every registered metric in Go's text format (the body of `GET /metrics`).
pub fn gather_text() -> String {
    encode::encode_text(&prometheus::gather())
}

// ---------------------------------------------------------------------------------------------
// Recording (protocol layer).

/// Packet payload bytes read (Go counts the payload, without the 4-byte header).
#[inline]
pub fn packet_in(bytes: usize) {
    PACKET_IN.inc_by(bytes as u64);
}

/// Packet bytes written (Go counts header + payload).
#[inline]
pub fn packet_out(bytes: usize) {
    PACKET_OUT.inc_by(bytes as u64);
}

/// A client connection was accepted (Go counts it before the handshake).
pub fn connection_opened() {
    CONNECTIONS_DEFAULT.inc();
}

pub fn connection_closed() {
    CONNECTIONS_DEFAULT.dec();
}

/// Current `tidb_server_connections` (Go's `/status` reports the live connection count).
pub fn connections() -> i64 {
    CONNECTIONS_DEFAULT.get()
}

/// Why a connection ended (Go `server/metrics`: `ok` after COM_QUIT, `error` when reading the
/// next command failed, including EOF without COM_QUIT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disconnect {
    Ok,
    Error,
}

pub fn disconnected(how: Disconnect) {
    let label = match how {
        Disconnect::Ok => "ok",
        Disconnect::Error => "error",
    };
    DISCONNECTION_TOTAL.with_label_values(&[label]).inc();
}

fn query_total_child(cmd: u8, ok: bool) -> &'static IntCounter {
    QUERY_TOTAL_CHILDREN[usize::from(!ok)][usize::from(cmd)].get_or_init(|| {
        QUERY_TOTAL.with_label_values(&[
            command_label(cmd),
            if ok { "OK" } else { "Error" },
            DEFAULT_RESOURCE_GROUP,
        ])
    })
}

fn query_duration_child(t: StmtType) -> &'static Histogram {
    QUERY_DURATION_CHILDREN[t as usize].get_or_init(|| {
        QUERY_DURATION.with_label_values(&[t.as_str(), NO_DB, DEFAULT_RESOURCE_GROUP])
    })
}

/// Start of a command (Go `clientConn.dispatch`): the idle time since the previous command
/// ended, and the token: there is no token limiter, so the wait is 0 µs (Go observes
/// microseconds, truncated, in this "seconds" histogram) and `tokens` counts commands in
/// progress.
pub fn command_started(idle: Duration) {
    CONN_IDLE_NOT_IN_TXN.observe(idle.as_secs_f64());
    TOKENS.inc();
    GET_TOKEN_DURATION.observe(0.0);
}

/// End of a command (Go `clientConn.addMetrics` + `releaseToken`): `query_total` by command
/// and result, the handling time by the type of the last statement, and an execute error.
pub fn command_finished(cmd: u8, stmt_type: StmtType, elapsed: Duration, error: Option<&str>) {
    TOKENS.dec();
    query_total_child(cmd, error.is_none()).inc();
    query_duration_child(stmt_type).observe(elapsed.as_secs_f64());
    if let Some(label) = error {
        EXECUTE_ERROR_TOTAL
            .with_label_values(&[label, NO_DB, DEFAULT_RESOURCE_GROUP])
            .inc();
    }
}

/// A multi-statement COM_QUERY with `n` statements (never: multi-statements are rejected).
pub fn multi_query(n: usize) {
    MULTI_QUERY_NUM.observe(n as f64);
}

// ---------------------------------------------------------------------------------------------
// Recording (session / executor).

/// A statement started executing (Go `executor.CountStmtNode`).
pub fn statement(t: StmtType) {
    STATEMENT_TOTAL_CHILDREN[t as usize]
        .get_or_init(|| {
            STATEMENT_TOTAL.with_label_values(&[t.as_str(), NO_DB, DEFAULT_RESOURCE_GROUP])
        })
        .inc();
}

pub fn parse_duration(d: Duration) {
    PARSE_GENERAL.observe(d.as_secs_f64());
}

pub fn compile_duration(d: Duration) {
    COMPILE_GENERAL.observe(d.as_secs_f64());
}

pub fn execute_duration(d: Duration) {
    EXECUTE_GENERAL.observe(d.as_secs_f64());
}

/// An autocommit transaction (one that took a start timestamp) ended.
pub fn transaction(d: Duration, committed: bool) {
    if committed {
        TXN_OPTIMISTIC_COMMIT.observe(d.as_secs_f64());
    } else {
        TXN_OPTIMISTIC_ABORT.observe(d.as_secs_f64());
    }
}

/// A statement took at least [`SLOW_THRESHOLD`] (total cost, Go `LogSlowQuery`).
pub fn slow_query(total: Duration) {
    SLOW_QUERY_PROCESS_GENERAL.observe(total.as_secs_f64());
}

/// A prepared statement was executed without a cached plan: this node has no plan cache, so
/// every `COM_STMT_EXECUTE` is a miss (Go `plan_cache.go`: `GetPlanCacheMissCounter`).
pub fn plan_cache_miss_prepared() {
    PLAN_CACHE_MISS_PREPARED.inc();
}

/// An executor Go counts as expensive was built.
pub fn expensive(e: ExpensiveExec) {
    EXPENSIVE_CHILDREN[e as usize].inc();
}

/// A DAG coprocessor read finished (Go `selectResult.fetchResp`: total time spent waiting for
/// the responses, labelled `paging` or `common`).
pub fn distsql_query(d: Duration, paging: bool) {
    if paging {
        DISTSQL_DAG_PAGING.observe(d.as_secs_f64());
    } else {
        DISTSQL_DAG_COMMON.observe(d.as_secs_f64());
    }
}

#[cfg(test)]
mod tests;
