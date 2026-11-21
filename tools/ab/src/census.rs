//! The coverage census (anti-false-green gate, `docs/testing/coverage-census.md`, #260).
//!
//! Every statement a run is supposed to execute gets exactly one census path:
//!
//! - `new_stack`: B ran it on the ported stack (route trace engine `new`);
//! - `fallback`: B ran it on the prototype, or refused it with 1235 because only the
//!   prototype could have run it (engine `poc` / `none`); the reason is the router's (`kind`,
//!   `ast`, `session`, a `blocked-by` block, `new_stack_off`, ...);
//! - `error`: it was sent to B but no route can be attributed (connection or timeout error,
//!   rejected before routing, the route trace unreadable);
//! - `skipped`: it was never sent to B, with the runner's reason.
//!
//! The four must add up to the number of statements the run was given, computed before the
//! run from its input; otherwise the run fails with INCOMPLETE CENSUS. Only `new_stack` with a
//! comparator-equal verdict counts as new-stack coverage. Each run also executes one negative
//! control (a statement whose B answer is deliberately falsified before the comparator sees
//! it): if the comparator judges it equal, or it did not run, the run fails.
//!
//! The engine that ran a statement comes from tidb-rs's route trace (`--route-trace`,
//! `GET /debug/route?conn=<id>` on its status port), read before and after the statement on
//! B's own connection.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::exec::{Outcome, Side};

/// Census layers: the test layers that report a census. Anything else is refused.
pub const LAYERS: &[&str] = &["integrationtest", "workload", "workload-strict", "tpch"];

/// Fault injections that prove the gate goes red (`--gate-fault`, hidden).
pub const FAULTS: &[&str] = &[
    "drop-census-record",
    "skip-negative-control",
    "untampered-negative-control",
];

/// Exit status of a run whose census or negative control failed.
pub const EXIT_GATE: u8 = 3;

/// A statement's census path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CensusPath {
    NewStack,
    Fallback,
    Error,
    Skipped,
}

impl CensusPath {
    pub const ALL: [CensusPath; 4] = [
        CensusPath::NewStack,
        CensusPath::Fallback,
        CensusPath::Error,
        CensusPath::Skipped,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CensusPath::NewStack => "new_stack",
            CensusPath::Fallback => "fallback",
            CensusPath::Error => "error",
            CensusPath::Skipped => "skipped",
        }
    }
}

/// What the route trace says about one statement on B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Engine (`new`, `poc`, `none`) and reason.
    Routed(String, String),
    /// Sent to B, but no route can be attributed (and why).
    Unknown(String),
}

impl Route {
    /// The census path and reason of a statement B ran with outcome `ob`.
    pub fn path(&self, ob: &Outcome) -> (CensusPath, String) {
        if let Outcome::Client(m) = ob {
            return (CensusPath::Error, format!("client error: {m}"));
        }
        match self {
            Route::Routed(engine, reason) if engine == "new" => {
                (CensusPath::NewStack, reason.clone())
            }
            Route::Routed(_, reason) => (CensusPath::Fallback, reason.clone()),
            Route::Unknown(why) => (CensusPath::Error, why.clone()),
        }
    }
}

/// B's connection id and route sequence number before a statement.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    conn: Option<u32>,
    seq: u64,
}

/// Reads tidb-rs's route trace.
pub struct RouteProbe {
    /// B's status address (host:port); `None`: no trace, every statement on B is `error`.
    addr: Option<String>,
}

impl RouteProbe {
    pub fn new(addr: Option<String>) -> Self {
        RouteProbe { addr }
    }

    /// Checks that the trace answers (tidb-rs started with `--route-trace`).
    pub async fn check(&self) -> Result<(), String> {
        match &self.addr {
            None => Ok(()),
            Some(a) => get(a, 0).await.map(|_| ()),
        }
    }

    /// Before a statement on `side`.
    pub async fn mark(&self, side: &Side) -> Mark {
        let conn = side.connection_id();
        let seq = match (&self.addr, conn) {
            (Some(a), Some(c)) => get(a, c).await.map_or(0, |(s, _, _)| s),
            _ => 0,
        };
        Mark { conn, seq }
    }

    /// After the statement: the route it took.
    pub async fn route(&self, side: &Side, before: Mark) -> Route {
        let Some(addr) = &self.addr else {
            return Route::Unknown("no route trace (--b-status not given)".into());
        };
        let Some(conn) = side.connection_id() else {
            return Route::Unknown("B not connected".into());
        };
        match get(addr, conn).await {
            Err(e) => Route::Unknown(format!("route trace: {e}")),
            Ok((seq, engine, reason)) => {
                let advanced = if before.conn == Some(conn) {
                    seq > before.seq
                } else {
                    seq > 0
                };
                if advanced {
                    Route::Routed(engine, reason)
                } else {
                    Route::Unknown("not routed (rejected before routing)".into())
                }
            }
        }
    }
}

/// `GET /debug/route?conn=<c>`: (seq, engine, reason).
async fn get(addr: &str, conn: u32) -> Result<(u64, String, String), String> {
    let fut = async {
        let mut s = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("{addr}: {e}"))?;
        let req = format!("GET /debug/route?conn={conn} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
        s.write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(String::from_utf8_lossy(&buf).into_owned())
    };
    let text = tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .map_err(|_| "timeout".to_string())??;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        let status = head.lines().next().unwrap_or("");
        return Err(format!(
            "{status}: {} (is tidb-rs running with --route-trace?)",
            body.trim()
        ));
    }
    let mut f = body.trim_end_matches('\n').splitn(3, '\t');
    let seq = f
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad route trace answer {body:?}"))?;
    Ok((
        seq,
        f.next().unwrap_or("").to_string(),
        f.next().unwrap_or("").to_string(),
    ))
}

/// One statement's census record.
#[derive(Debug, Clone)]
pub struct Record {
    /// Test file or workload template.
    pub unit: String,
    pub line: usize,
    /// `text` / `ps`.
    pub protocol: &'static str,
    pub path: CensusPath,
    pub reason: String,
    /// The comparator's verdict (or the runner's outcome name for statements not compared).
    pub verdict: String,
}

impl Record {
    /// New-stack coverage: ran on the new stack and the comparator judged it equal.
    pub fn covered(&self) -> bool {
        self.path == CensusPath::NewStack && matches!(self.verdict.as_str(), "match" | "both_error")
    }
}

/// The negative control's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    NotRun,
    /// The comparator's verdict on the falsified answer.
    Judged(String),
}

impl Control {
    pub fn ok(&self) -> bool {
        matches!(self, Control::Judged(v) if v == "mismatch")
    }

    fn name(&self) -> String {
        match self {
            Control::NotRun => "NOT RUN".into(),
            Control::Judged(v) if v == "mismatch" => "failed as required".into(),
            Control::Judged(v) => format!("PASSED ({v})"),
        }
    }
}

/// The statement of the negative control, and how its B answer is falsified.
pub const CONTROL_SQL: &str = "SELECT 'negative control' AS c";

/// Falsifies B's answer to the control (unless `untampered`).
pub fn tamper(ob: &mut Outcome, untampered: bool) {
    if untampered {
        return;
    }
    match ob {
        Outcome::Rows { rows, .. } => {
            for r in rows.iter_mut() {
                for v in r.iter_mut() {
                    v.push_str(" (falsified)");
                }
            }
            if rows.is_empty() {
                rows.push(vec!["falsified".into()]);
            }
        }
        other => {
            *other = Outcome::Rows {
                columns: Vec::new(),
                rows: vec![vec!["falsified".into()]],
                warnings: 0,
            }
        }
    }
}

/// A run's census.
pub struct Census {
    pub layer: String,
    /// Statements the run was given (from its input, before running).
    pub expected: u64,
    pub records: Vec<Record>,
    pub control: Control,
    pub faults: Vec<String>,
}

/// The census verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub total: u64,
    pub by_path: BTreeMap<CensusPath, u64>,
    pub covered: u64,
    /// `ok`, or why the gate fails.
    pub problems: Vec<String>,
}

impl Census {
    pub fn new(layer: &str, faults: &[String]) -> Result<Census, String> {
        if !LAYERS.contains(&layer) {
            return Err(format!(
                "unknown census layer {layer:?} (known: {})",
                LAYERS.join(", ")
            ));
        }
        for f in faults {
            if !FAULTS.contains(&f.as_str()) {
                return Err(format!(
                    "unknown --gate-fault {f:?} (known: {})",
                    FAULTS.join(", ")
                ));
            }
        }
        Ok(Census {
            layer: layer.to_string(),
            expected: 0,
            records: Vec::new(),
            control: Control::NotRun,
            faults: faults.to_vec(),
        })
    }

    pub fn fault(&self, f: &str) -> bool {
        self.faults.iter().any(|x| x == f)
    }

    pub fn add(&mut self, r: Record) {
        self.records.push(r);
    }

    /// Counts and the gate's verdict.
    pub fn summary(&mut self) -> Summary {
        if self.fault("drop-census-record") && !self.records.is_empty() {
            self.records.remove(0);
        }
        let mut by_path: BTreeMap<CensusPath, u64> =
            CensusPath::ALL.iter().map(|p| (*p, 0)).collect();
        for r in &self.records {
            *by_path.entry(r.path).or_default() += 1;
        }
        let total: u64 = by_path.values().sum();
        let covered = self.records.iter().filter(|r| r.covered()).count() as u64;
        let mut problems = Vec::new();
        if total != self.expected || self.expected == 0 {
            problems.push(format!(
                "INCOMPLETE CENSUS: {} = {total} of {} statements",
                CensusPath::ALL
                    .iter()
                    .map(|p| p.name())
                    .collect::<Vec<_>>()
                    .join(" + "),
                self.expected
            ));
        }
        if !self.control.ok() {
            problems.push(format!(
                "NEGATIVE CONTROL {}: the falsified answer must be judged a mismatch",
                self.control.name()
            ));
        }
        Summary {
            total,
            by_path,
            covered,
            problems,
        }
    }

    /// Writes census.tsv, census-summary.tsv and census-reasons.tsv to `dir` and prints the
    /// summary. Returns whether the gate passed.
    pub fn write(&mut self, dir: &Path) -> Result<bool, String> {
        let s = self.summary();
        let open = |name: &str| {
            let p = dir.join(name);
            File::create(&p)
                .map(BufWriter::new)
                .map_err(|e| format!("{}: {e}", p.display()))
        };
        let io = |e: std::io::Error| e.to_string();
        let mut f = open("census.tsv")?;
        writeln!(
            f,
            "layer\tunit\tline\tprotocol\tpath\treason\tverdict\tcovered"
        )
        .map_err(io)?;
        for r in &self.records {
            writeln!(
                f,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                self.layer,
                field(&r.unit),
                r.line,
                r.protocol,
                r.path.name(),
                field(&r.reason),
                r.verdict,
                u8::from(r.covered())
            )
            .map_err(io)?;
        }
        f.flush().map_err(io)?;
        let status = if s.problems.is_empty() {
            "ok".to_string()
        } else {
            s.problems.join("; ")
        };
        let mut f = open("census-summary.tsv")?;
        writeln!(
            f,
            "layer\texpected\ttotal\tnew_stack\tfallback\terror\tskipped\tnew_stack_equal\tnegative_control\tstatus"
        )
        .map_err(io)?;
        let n = |p| s.by_path.get(&p).copied().unwrap_or(0);
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.layer,
            self.expected,
            s.total,
            n(CensusPath::NewStack),
            n(CensusPath::Fallback),
            n(CensusPath::Error),
            n(CensusPath::Skipped),
            s.covered,
            self.control.name(),
            field(&status)
        )
        .map_err(io)?;
        f.flush().map_err(io)?;
        let mut reasons: BTreeMap<(CensusPath, &str), u64> = BTreeMap::new();
        for r in &self.records {
            *reasons.entry((r.path, r.reason.as_str())).or_default() += 1;
        }
        let mut f = open("census-reasons.tsv")?;
        writeln!(f, "layer\tpath\treason\tcount").map_err(io)?;
        for ((p, why), c) in &reasons {
            writeln!(f, "{}\t{}\t{}\t{c}", self.layer, p.name(), field(why)).map_err(io)?;
        }
        f.flush().map_err(io)?;
        println!(
            "CENSUS {}: {} statements: new_stack {} (equal to Go: {}) fallback {} error {} skipped {}; negative control: {}",
            self.layer,
            s.total,
            n(CensusPath::NewStack),
            s.covered,
            n(CensusPath::Fallback),
            n(CensusPath::Error),
            n(CensusPath::Skipped),
            self.control.name()
        );
        for p in &s.problems {
            println!("GATE FAILED: {p}");
        }
        Ok(s.problems.is_empty())
    }
}

/// A TSV field: tabs and newlines replaced by spaces.
fn field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: CensusPath, verdict: &str) -> Record {
        Record {
            unit: "t".into(),
            line: 1,
            protocol: "text",
            path,
            reason: "ok".into(),
            verdict: verdict.into(),
        }
    }

    fn census(n: u64) -> Census {
        let mut c = Census::new("integrationtest", &[]).unwrap();
        c.expected = n;
        c.add(rec(CensusPath::NewStack, "match"));
        c.add(rec(CensusPath::NewStack, "mismatch"));
        c.add(rec(CensusPath::Fallback, "match"));
        c.add(rec(CensusPath::Skipped, "write"));
        c.control = Control::Judged("mismatch".into());
        c
    }

    #[test]
    fn complete_census_passes() {
        let s = census(4).summary();
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        assert_eq!(s.total, 4);
        // Only new stack + comparator-equal is coverage.
        assert_eq!(s.covered, 1);
    }

    #[test]
    fn missing_statement_fails() {
        let s = census(5).summary();
        assert!(
            s.problems[0].starts_with("INCOMPLETE CENSUS"),
            "{:?}",
            s.problems
        );
        let mut c = census(4);
        c.faults = vec!["drop-census-record".into()];
        assert!(c.summary().problems[0].starts_with("INCOMPLETE CENSUS"));
        // An empty run is never complete.
        let mut c = Census::new("workload", &[]).unwrap();
        c.control = Control::Judged("mismatch".into());
        assert!(!c.summary().problems.is_empty());
    }

    #[test]
    fn negative_control_must_fail_and_run() {
        let mut c = census(4);
        c.control = Control::NotRun;
        assert!(c.summary().problems[0].starts_with("NEGATIVE CONTROL NOT RUN"));
        c.control = Control::Judged("match".into());
        assert!(c.summary().problems[0].starts_with("NEGATIVE CONTROL PASSED"));
    }

    #[test]
    fn unknown_names_fail() {
        assert!(Census::new("integration", &[]).is_err());
        assert!(Census::new("workload", &["drop".into()]).is_err());
    }

    #[test]
    fn route_paths() {
        let ok = Outcome::Ok {
            affected_rows: 0,
            warnings: 0,
        };
        let r = |e: &str, why: &str| Route::Routed(e.into(), why.into()).path(&ok).0;
        assert_eq!(r("new", "ok"), CensusPath::NewStack);
        assert_eq!(r("poc", "D05.3"), CensusPath::Fallback);
        assert_eq!(r("none", "D05.3"), CensusPath::Fallback);
        assert_eq!(Route::Unknown("x".into()).path(&ok).0, CensusPath::Error);
        let client = Outcome::Client("lost".into());
        assert_eq!(
            Route::Routed("new".into(), "ok".into()).path(&client).0,
            CensusPath::Error
        );
    }

    #[test]
    fn tampering_changes_the_answer() {
        let mut o = Outcome::Rows {
            columns: Vec::new(),
            rows: vec![vec!["\"negative control\"".into()]],
            warnings: 0,
        };
        let orig = o.clone();
        tamper(&mut o, true);
        assert_eq!(o, orig);
        tamper(&mut o, false);
        assert_ne!(o, orig);
    }
}
