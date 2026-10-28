//! The metric families of the dashboard panels tidb-rs feeds, as Go defines them, and a
//! checker for a `/metrics` exposition. Sources (`R:` = tidb `v8.5.8-20261021-849a85c`,
//! `CG:` = client-go `v2.0.8-0.20261020042752-6ea7c1662c95`, `PD:` = pd/client
//! `v0.0.0-20260804033407-85a975a5ca78`, both pinned by that tidb's go.mod; client-go metrics
//! get namespace `tidb`, subsystem `tikvclient` from `R:pkg/metrics/metrics.go:315`).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::encode::go_float;

/// Metric type as written on the `# TYPE` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// One Go metric family.
#[derive(Clone, Copy, Debug)]
pub struct GoMetric {
    pub name: &'static str,
    pub kind: Kind,
    pub labels: &'static [&'static str],
    /// `prometheus.ExponentialBuckets(start, factor, count)`.
    pub buckets: Option<(f64, f64, usize)>,
    /// Where Go defines it.
    pub source: &'static str,
}

const fn m(
    name: &'static str,
    kind: Kind,
    labels: &'static [&'static str],
    buckets: Option<(f64, f64, usize)>,
    source: &'static str,
) -> GoMetric {
    GoMetric {
        name,
        kind,
        labels,
        buckets,
        source,
    }
}

use Kind::Counter;
use Kind::Gauge;
use Kind::Histogram;

/// Families recorded by tidb-rs itself (crate `tidb-metrics`).
pub const TIDB: &[GoMetric] = &[
    m(
        "tidb_server_info",
        Gauge,
        &["version", "hash"],
        None,
        "R:pkg/metrics/server.go:311",
    ),
    m(
        "tidb_server_maxprocs",
        Gauge,
        &[],
        None,
        "R:pkg/metrics/server.go:286",
    ),
    m(
        "tidb_server_connections",
        Gauge,
        &["resource_group"],
        None,
        "R:pkg/metrics/server.go:127",
    ),
    m(
        "tidb_server_tokens",
        Gauge,
        &[],
        None,
        "R:pkg/metrics/server.go:319",
    ),
    m(
        "tidb_server_query_total",
        Counter,
        &["type", "result", "resource_group"],
        None,
        "R:pkg/metrics/server.go:119",
    ),
    m(
        "tidb_server_handle_query_duration_seconds",
        Histogram,
        &["sql_type", "db", "resource_group"],
        Some((0.0005, 2.0, 29)),
        "R:pkg/metrics/server.go:92",
    ),
    m(
        "tidb_server_conn_idle_duration_seconds",
        Histogram,
        &["in_txn"],
        Some((0.0005, 2.0, 29)),
        "R:pkg/metrics/server.go:302",
    ),
    m(
        "tidb_server_execute_error_total",
        Counter,
        &["type", "db", "resource_group"],
        None,
        "R:pkg/metrics/server.go:150",
    ),
    m(
        "tidb_server_multi_query_num",
        Histogram,
        &[],
        Some((1.0, 2.0, 20)),
        "R:pkg/metrics/server.go:241",
    ),
    m(
        "tidb_server_disconnection_total",
        Counter,
        &["result"],
        None,
        "R:pkg/metrics/server.go:135",
    ),
    m(
        "tidb_server_get_token_duration_seconds",
        Histogram,
        &[],
        Some((1.0, 2.0, 30)),
        "R:pkg/metrics/server.go:232",
    ),
    m(
        "tidb_server_packet_io_bytes",
        Counter,
        &["type"],
        None,
        "R:pkg/metrics/server.go:84",
    ),
    m(
        "tidb_server_plan_cache_total",
        Counter,
        &["type"],
        None,
        "R:pkg/metrics/server.go:182",
    ),
    m(
        "tidb_server_plan_cache_miss_total",
        Counter,
        &["type"],
        None,
        "R:pkg/metrics/server.go:190",
    ),
    m(
        "tidb_server_slow_query_process_duration_seconds",
        Histogram,
        &["sql_type"],
        Some((0.001, 2.0, 28)),
        "R:pkg/metrics/server.go:250",
    ),
    m(
        "tidb_session_parse_duration_seconds",
        Histogram,
        &["sql_type"],
        Some((0.00004, 2.0, 28)),
        "R:pkg/metrics/session.go:60",
    ),
    m(
        "tidb_session_compile_duration_seconds",
        Histogram,
        &["sql_type"],
        Some((0.00004, 2.0, 28)),
        "R:pkg/metrics/session.go:69",
    ),
    m(
        "tidb_session_execute_duration_seconds",
        Histogram,
        &["sql_type"],
        Some((0.0001, 2.0, 30)),
        "R:pkg/metrics/session.go:79",
    ),
    m(
        "tidb_session_transaction_duration_seconds",
        Histogram,
        &["txn_mode", "type", "scope"],
        Some((0.001, 2.0, 28)),
        "R:pkg/metrics/session.go:130",
    ),
    m(
        "tidb_executor_statement_total",
        Counter,
        &["type", "db", "resource_group"],
        None,
        "R:pkg/metrics/executor.go:95",
    ),
    m(
        "tidb_executor_expensive_total",
        Counter,
        &["type"],
        None,
        "R:pkg/metrics/executor.go:86",
    ),
    m(
        "tidb_distsql_handle_query_duration_seconds",
        Histogram,
        &["type", "sql_type", "copr_type"],
        Some((0.0005, 2.0, 29)),
        "R:pkg/metrics/distsql.go:38",
    ),
    // client_golang's process collector (prometheus/client_golang v1.23.2).
    m(
        "process_start_time_seconds",
        Gauge,
        &[],
        None,
        "client_golang process_collector.go",
    ),
    m(
        "process_cpu_seconds_total",
        Counter,
        &[],
        None,
        "client_golang process_collector.go",
    ),
    m(
        "process_resident_memory_bytes",
        Gauge,
        &[],
        None,
        "client_golang process_collector.go",
    ),
    m(
        "process_virtual_memory_bytes",
        Gauge,
        &[],
        None,
        "client_golang process_collector.go",
    ),
    m(
        "process_open_fds",
        Gauge,
        &[],
        None,
        "client_golang process_collector.go",
    ),
    m(
        "process_max_fds",
        Gauge,
        &[],
        None,
        "client_golang process_collector.go",
    ),
];

/// Families recorded by tikv-client (our fork, feature `client-go-metrics`).
pub const CLIENT: &[GoMetric] = &[
    m(
        "tidb_tikvclient_request_seconds",
        Histogram,
        &["type", "store", "stale_read", "scope"],
        Some((0.0005, 2.0, 24)),
        "CG:metrics/metrics.go:183",
    ),
    m(
        "tidb_tikvclient_rpc_net_latency_seconds",
        Histogram,
        &["store", "scope"],
        Some((0.0001, 2.0, 20)),
        "CG:metrics/metrics.go:204",
    ),
    m(
        "tidb_tikvclient_backoff_seconds",
        Histogram,
        &["type"],
        Some((0.0005, 2.0, 29)),
        "CG:metrics/metrics.go:173",
    ),
    m(
        "tidb_tikvclient_region_err_total",
        Counter,
        &["type", "store"],
        None,
        "CG:metrics/metrics.go:224",
    ),
    m(
        "tidb_tikvclient_region_cache_operations_total",
        Counter,
        &["type", "result"],
        None,
        "CG:metrics/metrics.go:312",
    ),
    m(
        "tidb_tikvclient_load_region_cache_seconds",
        Histogram,
        &["type"],
        Some((0.0001, 2.0, 20)),
        "CG:metrics/metrics.go:329",
    ),
    m(
        "tidb_tikvclient_lock_resolver_actions_total",
        Counter,
        &["type"],
        None,
        "CG:metrics/metrics.go:215",
    ),
    m(
        "tidb_tikvclient_replica_selector_failure_counter",
        Counter,
        &["type"],
        None,
        "CG:metrics/metrics.go:664",
    ),
    m(
        "pd_client_cmd_handle_cmds_duration_seconds",
        Histogram,
        &["type"],
        Some((0.0005, 2.0, 13)),
        "PD:metrics/metrics.go:97",
    ),
    m(
        "pd_client_cmd_handle_failed_cmds_duration_seconds",
        Histogram,
        &["type"],
        Some((0.0005, 2.0, 13)),
        "PD:metrics/metrics.go:107",
    ),
    m(
        "pd_client_request_handle_requests_duration_seconds",
        Histogram,
        &["type"],
        Some((0.0005, 2.0, 13)),
        "PD:metrics/metrics.go:117",
    ),
];

/// What an exposition says about one family.
#[derive(Debug, Default)]
struct Seen {
    kind: Option<String>,
    labels: BTreeSet<String>,
    les: BTreeSet<String>,
    samples: usize,
}

/// Check `text` (a `/metrics` body) against `specs`: every family present with at least one
/// sample, the right type, exactly the label names, and Go's bucket bounds spelled as Go
/// spells them. Returns the problems found.
pub fn check(text: &str, specs: &[GoMetric]) -> Vec<String> {
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').unwrap_or((rest, ""));
            seen.entry(name).or_default().kind = Some(kind.to_owned());
            continue;
        }
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let series = line.rsplit_once(' ').map_or(line, |(s, _)| s);
        let (name, labels) = match series.split_once('{') {
            Some((n, l)) => (n, l.trim_end_matches('}')),
            None => (series, ""),
        };
        let family = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| {
                name.strip_suffix(s)
                    .filter(|f| specs.iter().any(|sp| sp.name == *f && sp.kind == Histogram))
            })
            .unwrap_or(name);
        let e = seen.entry(family).or_default();
        e.samples += 1;
        for pair in split_labels(labels) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == "le" {
                e.les.insert(v.trim_matches('"').to_owned());
            } else {
                e.labels.insert(k.to_owned());
            }
        }
    }
    let mut problems = Vec::new();
    for spec in specs {
        let Some(s) = seen.get(spec.name) else {
            problems.push(format!("{}: missing ({})", spec.name, spec.source));
            continue;
        };
        if s.samples == 0 {
            problems.push(format!("{}: no samples", spec.name));
        }
        if s.kind.as_deref() != Some(spec.kind.as_str()) {
            problems.push(format!(
                "{}: type {:?}, want {}",
                spec.name,
                s.kind,
                spec.kind.as_str()
            ));
        }
        let want: BTreeSet<String> = spec.labels.iter().map(|l| (*l).to_owned()).collect();
        if s.labels != want && s.samples > 0 {
            problems.push(format!(
                "{}: labels {:?}, want {:?}",
                spec.name, s.labels, want
            ));
        }
        if let Some((start, factor, count)) = spec.buckets {
            let mut want: BTreeSet<String> = BTreeSet::new();
            let mut b = start;
            for _ in 0..count {
                want.insert(go_float(b));
                b *= factor;
            }
            want.insert("+Inf".to_owned());
            if s.les != want {
                problems.push(format!("{}: le {:?}, want {:?}", spec.name, s.les, want));
            }
        }
    }
    problems
}

/// Split `a="x",b="y,z"` at the commas between pairs (values may contain commas and escaped
/// quotes).
fn split_labels(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}
