//! `ab`: run the same statements on Go TiDB (A) and tidb-rs (B) and compare rows, column
//! metadata and errors, via the text protocol and server-side prepared statements.
//! See tools/ab/README.md.

#![forbid(unsafe_code)]

mod census;
mod compare;
mod exec;
mod integration;
mod mysqltest;
mod rng;
mod rulings;
mod sql;
mod workload;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::{Value as Json, json};

use crate::census::{Census, CensusPath, Control, Record, RouteProbe};
use crate::compare::{
    Observed, Verdict, charset_name, classify, compare, execute_metadata_diffs, flag_names,
    mismatch_kind, needs_warnings, prepare_diffs, strip_limit, type_name,
    without_optimizer_skip_warnings,
};
use crate::exec::{ColumnMeta, Outcome, PrepareMeta, Side, TlsMode};
use crate::rulings::Ledger;
use crate::sql::Kind;
use crate::workload::{Case, Param, SysbenchSpec};

#[derive(Debug, Parser)]
#[command(
    name = "ab",
    about = "Compare query results of Go TiDB (A) and tidb-rs (B) on the same cluster"
)]
struct Cli {
    /// Endpoint A (reference: Go TiDB), host:port.
    #[arg(long, short = 'a', global = true, default_value = "127.0.0.1:27000")]
    a: String,
    /// Endpoint B (under test: tidb-rs), host:port.
    #[arg(long, short = 'b', global = true, default_value = "127.0.0.1:4001")]
    b: String,
    #[arg(long, short = 'u', global = true, default_value = "root")]
    user: String,
    #[arg(long, short = 'p', global = true, default_value = "")]
    password: String,
    /// Initial database (default: `sbtest` for the sysbench workload, none for files).
    #[arg(long, short = 'D', global = true)]
    db: Option<String>,
    /// TLS use (self-signed certificates are always accepted).
    #[arg(long, global = true, value_enum, default_value_t = TlsMode::Prefer)]
    tls: TlsMode,
    /// Seed for the random parameters.
    #[arg(long, global = true, default_value_t = 1)]
    seed: u64,
    /// Protocols to compare.
    #[arg(long, global = true, value_enum, default_value_t = Protocols::Both)]
    protocol: Protocols,
    /// Directory for mismatches.jsonl / unsupported.jsonl.
    #[arg(long, global = true, default_value = "target/ab")]
    out_dir: PathBuf,
    /// Per-statement timeout in seconds.
    #[arg(long, global = true, default_value_t = 30)]
    timeout: u64,
    /// Rows per side included in JSONL records (the diff always covers all rows).
    #[arg(long, global = true, default_value_t = 20)]
    report_rows: usize,
    /// Print every mismatch as it happens.
    #[arg(long, short = 'v', global = true)]
    verbose: bool,
    /// B's status address (host:port) for the coverage census: tidb-rs must run with
    /// `--route-trace`. Without it every statement B ran is census path `error`.
    #[arg(long, global = true)]
    b_status: Option<String>,
    /// Census layer name (integrationtest, workload, workload-strict, tpch; default:
    /// integrationtest / workload by subcommand). Unknown names are refused.
    #[arg(long, global = true)]
    census_layer: Option<String>,
    /// Fault injection proving the gate goes red: drop-census-record, skip-negative-control,
    /// untampered-negative-control.
    #[arg(long, global = true, hide = true)]
    gate_fault: Vec<String>,
    /// The divergence ledger (default: docs/testing/rulings.toml as built into this binary).
    /// Mismatches an approved ruling covers are `ruled:<id>`; hit counts go to
    /// rulings-hits.tsv in --out-dir.
    #[arg(long, global = true)]
    rulings: Option<PathBuf>,
    /// After every prepared EXECUTE, read `@@last_plan_from_cache` on both sides (right
    /// after it, as Go resets it with the next statement) and compare it (the plan cache,
    /// A3). The `SHOW WARNINGS` of such a run follows that read, so the warning texts are
    /// those of the read: compare warnings in a run without this flag.
    #[arg(long, global = true)]
    plan_cache: bool,
    #[command(subcommand)]
    workload: Workload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Protocols {
    Both,
    Text,
    Ps,
}

#[derive(Debug, Subcommand)]
enum Workload {
    /// The five oltp_read_only statements with random and edge-case parameters.
    Sysbench {
        /// Number of sbtest tables.
        #[arg(long, default_value_t = 1)]
        tables: u32,
        /// Rows per table (default: MAX(id) of sbtest1, read from A).
        #[arg(long)]
        table_size: Option<i64>,
        /// sysbench --range_size.
        #[arg(long, default_value_t = 100)]
        range_size: i64,
        /// Random cases per template (edge cases come on top).
        #[arg(long, short = 'n', default_value_t = 100)]
        per_template: usize,
        /// Skip the edge cases (bounds, empty ranges, region boundaries...).
        #[arg(long)]
        no_edge_cases: bool,
    },
    /// Statements from a file (`;`-terminated, or one per line).
    File {
        path: PathBuf,
        /// Statements to run on A first (writes allowed; SET/USE also run on B).
        #[arg(long)]
        setup: Option<PathBuf>,
        /// Do not also run SELECTs as prepared statements.
        #[arg(long)]
        no_prepared: bool,
    },
    /// TiDB's integration tests (mysql-tester `.test` files) in hybrid mode: every statement
    /// on A, reads also on B and compared. Writes statements.tsv, files.tsv,
    /// mismatches.jsonl and results.jsonl to --out-dir.
    Integrationtest {
        /// `tests/integrationtest/t` of a TiDB checkout.
        #[arg(long)]
        dir: PathBuf,
        /// Tests to run (`executor/aggregate`, or a prefix ending in `/`); default all.
        tests: Vec<String>,
    },
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    matched: u64,
    mismatch: u64,
    ruled: u64,
    unsupported: u64,
    both_error: u64,
}

impl Counts {
    fn add(&mut self, v: &Verdict) {
        match v {
            Verdict::Match => self.matched += 1,
            Verdict::BothError => self.both_error += 1,
            Verdict::Unsupported => self.unsupported += 1,
            Verdict::Mismatch(_) => self.mismatch += 1,
            Verdict::Ruled { .. } => self.ruled += 1,
        }
    }

    fn total(&self) -> u64 {
        self.matched + self.mismatch + self.ruled + self.unsupported + self.both_error
    }

    fn merge(&mut self, o: &Counts) {
        self.matched += o.matched;
        self.mismatch += o.mismatch;
        self.ruled += o.ruled;
        self.unsupported += o.unsupported;
        self.both_error += o.both_error;
    }
}

struct Report {
    /// `--plan-cache`: per template, (equal, A from cache, B from cache, compared).
    plan_cache: BTreeMap<String, [u64; 4]>,
    /// `--plan-cache`: the statements whose `@@last_plan_from_cache` differs.
    plan_cache_diffs: Vec<String>,
    counts: BTreeMap<(String, &'static str), Counts>,
    skipped: BTreeMap<String, u64>,
    mismatches: BufWriter<File>,
    unsupported: BufWriter<File>,
    /// Every compared statement and its verdict, without rows (for the weekly report's
    /// per-operator agreement, `tools/weekly`).
    results: BufWriter<File>,
    report_rows: usize,
    verbose: bool,
    ledger: Ledger,
}

fn column_json(c: &ColumnMeta) -> Json {
    json!({
        "name": c.name, "org_name": c.org_name, "schema": c.schema, "table": c.table,
        "org_table": c.org_table, "type": type_name(c.column_type), "length": c.length,
        "decimals": c.decimals, "flags": flag_names(c.flags), "charset": charset_name(c.charset),
    })
}

fn outcome_json(o: &Outcome, max_rows: usize) -> Json {
    match o {
        Outcome::Rows {
            columns,
            rows,
            warnings,
        } => json!({
            "columns": columns.iter().map(column_json).collect::<Vec<_>>(),
            "row_count": rows.len(),
            "rows": rows.iter().take(max_rows).collect::<Vec<_>>(),
            "warnings": warnings,
        }),
        Outcome::Ok {
            affected_rows,
            warnings,
        } => json!({"ok": {"affected_rows": affected_rows, "warnings": warnings}}),
        Outcome::Error {
            code,
            state,
            message,
        } => json!({"error": {"code": code, "state": state, "message": message}}),
        Outcome::Client(m) => json!({"client_error": m}),
    }
}

fn prepare_json(p: Option<&PrepareMeta>) -> Json {
    p.map_or(Json::Null, |p| {
        json!({
            "param_count": p.param_count,
            "columns": p.columns.iter().map(column_json).collect::<Vec<_>>(),
        })
    })
}

/// One side's result for the record.
struct SideResult<'a> {
    prepare: Option<&'a PrepareMeta>,
    outcome: &'a Outcome,
}

impl Report {
    /// The divergence ledger on a mismatch: an approved ruling makes it `ruled:<id>`; a
    /// pending one stays a mismatch with a diff line naming it.
    fn rule(&mut self, case: &Case, query: &str, a: &Outcome, b: &Outcome, v: Verdict) -> Verdict {
        let Verdict::Mismatch(mut diff) = v else {
            return v;
        };
        let kind = mismatch_kind(a, b, &diff);
        match self
            .ledger
            .rule("workload", &case.template, query, kind, &diff.join("; "))
        {
            Some(hit) if hit.approved => Verdict::Ruled { id: hit.id, diff },
            Some(hit) => {
                diff.push(hit.kind(kind));
                Verdict::Mismatch(diff)
            }
            None => Verdict::Mismatch(diff),
        }
    }

    fn record(
        &mut self,
        case: &Case,
        protocol: &'static str,
        query: &str,
        a: SideResult<'_>,
        b: SideResult<'_>,
        verdict: &Verdict,
    ) -> std::io::Result<()> {
        self.counts
            .entry((case.template.clone(), protocol))
            .or_default()
            .add(verdict);
        writeln!(
            self.results,
            "{}",
            match verdict {
                Verdict::Ruled { id, .. } => json!({
                    "template": case.template,
                    "protocol": protocol,
                    "verdict": verdict.name(),
                    "query": query,
                    "ruling": id,
                }),
                _ => json!({
                    "template": case.template,
                    "protocol": protocol,
                    "verdict": verdict.name(),
                    "query": query,
                }),
            }
        )?;
        let out = match verdict {
            Verdict::Mismatch(diff) | Verdict::Ruled { diff, .. } => {
                if self.verbose {
                    eprintln!("{} [{protocol}] {query}", verdict.name().to_uppercase());
                    for d in diff {
                        eprintln!("    {d}");
                    }
                }
                &mut self.mismatches
            }
            Verdict::Unsupported => &mut self.unsupported,
            _ => return Ok(()),
        };
        let mut rec = json!({
            "template": case.template,
            "protocol": protocol,
            "verdict": verdict.name(),
            "query": query,
            "a": outcome_json(a.outcome, self.report_rows),
            "b": outcome_json(b.outcome, self.report_rows),
        });
        match verdict {
            Verdict::Mismatch(diff) => rec["diff"] = json!(diff),
            Verdict::Ruled { id, diff } => {
                rec["diff"] = json!(diff);
                rec["ruling"] = json!(id);
            }
            _ => {}
        }
        if let Some(s) = &case.source {
            rec["source"] = json!(s);
        }
        if protocol == "ps" {
            rec["params"] = json!(case.params.iter().map(Param::to_json).collect::<Vec<_>>());
            if case.execute > 0 {
                rec["execute"] = json!(case.execute);
            }
            rec["a"]["prepare"] = prepare_json(a.prepare);
            rec["b"]["prepare"] = prepare_json(b.prepare);
        }
        writeln!(out, "{rec}")
    }

    fn print_summary(&self) -> Counts {
        println!();
        println!(
            "{:<20} {:<5} {:>8} {:>8} {:>6} {:>11} {:>10} {:>7}",
            "template", "proto", "match", "mismatch", "ruled", "unsupported", "both_error", "total"
        );
        let mut all = Counts::default();
        for ((t, p), c) in &self.counts {
            println!(
                "{:<20} {:<5} {:>8} {:>8} {:>6} {:>11} {:>10} {:>7}",
                t,
                p,
                c.matched,
                c.mismatch,
                c.ruled,
                c.unsupported,
                c.both_error,
                c.total()
            );
            all.merge(c);
        }
        println!(
            "{:<20} {:<5} {:>8} {:>8} {:>6} {:>11} {:>10} {:>7}",
            "TOTAL",
            "",
            all.matched,
            all.mismatch,
            all.ruled,
            all.unsupported,
            all.both_error,
            all.total()
        );
        for (why, n) in &self.skipped {
            println!("skipped {n} statement(s): {why}");
        }
        if !self.plan_cache.is_empty() {
            println!();
            println!(
                "{:<20} {:>8} {:>8} {:>8} {:>8}   (@@last_plan_from_cache after EXECUTE)",
                "template", "equal", "A=1", "B=1", "compared"
            );
            for (t, c) in &self.plan_cache {
                println!("{:<20} {:>8} {:>8} {:>8} {:>8}", t, c[0], c[1], c[2], c[3]);
            }
            for d in &self.plan_cache_diffs {
                println!("last_plan_from_cache differs: {d}");
            }
        }
        all
    }
}

/// How to fetch A's result without the LIMIT (spec §2.4).
enum Rerun<'a> {
    Text,
    Prepared(&'a [mysql_async::Value]),
}

/// Compares through the comparator: fetches `SHOW WARNINGS` on both sides when either
/// reports warnings, and A's result without the LIMIT when the class asks for it.
/// `b_on_prototype`: B answered on the prototype (route trace engine `poc`), so tolerance T6
/// leaves out A's optimizer-time plan-cache skip warnings that B does not report.
#[allow(clippy::too_many_arguments)]
async fn judge(
    case: &Case,
    sql: &str,
    rerun: Rerun<'_>,
    a: &mut Side,
    b: &mut Side,
    oa: &Outcome,
    ob: &Outcome,
    b_on_prototype: bool,
) -> Verdict {
    let (wa, wb) = if needs_warnings(oa, ob) {
        (a.warnings().await, b.warnings().await)
    } else {
        (None, None)
    };
    let cols = match oa {
        Outcome::Rows { columns, .. } => columns.as_slice(),
        _ => &[],
    };
    let class = classify(sql, cols, case.sorted);
    let obs_b = Observed::new(ob, wb.as_deref());
    // T6: on the prototype path only.
    let tolerated = b_on_prototype
        .then(|| without_optimizer_skip_warnings(Observed::new(oa, wa.as_deref()), obs_b))
        .flatten();
    let obs_a = match &tolerated {
        Some((o, w)) => Observed::new(o, Some(w)),
        None => Observed::new(oa, wa.as_deref()),
    };
    let v = compare(&class, obs_a, obs_b, None);
    if !matches!(v, Verdict::Mismatch(_)) || !class.wants_full() {
        return v;
    }
    let Some(full_sql) = strip_limit(sql) else {
        return v;
    };
    let full = match rerun {
        Rerun::Text => a.query(&full_sql).await,
        Rerun::Prepared(params) => a.prepared(&full_sql, params, false).await.1,
    };
    match full {
        Outcome::Rows { rows, .. } => compare(&class, obs_a, obs_b, Some(&rows)),
        _ => v,
    }
}

/// The protocols a case runs with (the census expects one record per protocol).
fn case_protocols(case: &Case, cli: &Cli) -> Vec<&'static str> {
    if case.setup || matches!(case.kind, Kind::Other(_)) {
        return vec!["text"];
    }
    let mut v = Vec::new();
    if cli.protocol != Protocols::Ps || !case.prepared {
        v.push("text");
    }
    if cli.protocol != Protocols::Text && case.prepared {
        v.push("ps");
    }
    v
}

fn census_record(
    case: &Case,
    protocol: &'static str,
    path: (CensusPath, String),
    v: &str,
) -> Record {
    let (unit, line) = match case.source.as_deref().and_then(|s| s.rsplit_once(':')) {
        Some((f, l)) => (f.to_string(), l.parse().unwrap_or(0)),
        None => (case.template.clone(), 0),
    };
    Record {
        unit,
        line,
        protocol,
        path: path.0,
        reason: path.1,
        verdict: v.to_string(),
    }
}

/// The negative control (census): the control statement on both sides, B's answer falsified,
/// judged by the comparator like any statement.
async fn run_control(a: &mut Side, b: &mut Side, census: &mut Census) {
    if census.fault("skip-negative-control") {
        return;
    }
    let case = Case {
        template: "negative-control".into(),
        sql: census::CONTROL_SQL.into(),
        params: Vec::new(),
        sorted: false,
        kind: Kind::Read,
        source: None,
        prepared: false,
        setup: false,
        execute: 0,
    };
    let oa = a.query(&case.sql).await;
    let mut ob = b.query(&case.sql).await;
    census::tamper(&mut ob, census.fault("untampered-negative-control"));
    let v = judge(&case, &case.sql, Rerun::Text, a, b, &oa, &ob, false).await;
    census.control = Control::Judged(v.name().to_string());
}

async fn run_case(
    case: &Case,
    cli: &Cli,
    a: &mut Side,
    b: &mut Side,
    report: &mut Report,
    probe: &RouteProbe,
    census: &mut Census,
) {
    let text_sql = case.text_sql();
    if cli.protocol != Protocols::Ps || !case.prepared {
        let oa = a.query(&text_sql).await;
        let mark = probe.mark(b).await;
        let ob = b.query(&text_sql).await;
        let route = probe.route(b, mark).await;
        if case.kind == Kind::Session {
            if !matches!(oa, Outcome::Error { .. } | Outcome::Client(_)) {
                a.log_session(&text_sql);
            }
            if !matches!(ob, Outcome::Error { .. } | Outcome::Client(_)) {
                b.log_session(&text_sql);
            }
        }
        let v = judge(
            case,
            &text_sql,
            Rerun::Text,
            a,
            b,
            &oa,
            &ob,
            route.on_prototype(),
        )
        .await;
        let v = report.rule(case, &text_sql, &oa, &ob, v);
        census.add(census_record(case, "text", route.path(&ob), v.name()));
        let ra = SideResult {
            prepare: None,
            outcome: &oa,
        };
        let rb = SideResult {
            prepare: None,
            outcome: &ob,
        };
        if let Err(e) = report.record(case, "text", &text_sql, ra, rb, &v) {
            eprintln!("cannot write report: {e}");
        }
    }
    if cli.protocol != Protocols::Text && case.prepared {
        let params: Vec<_> = case.params.iter().map(Param::to_value).collect();
        let fresh = case.execute == 1;
        let (pa, oa) = a.prepared(&case.sql, &params, fresh).await;
        let mark = probe.mark(b).await;
        let (pb, ob) = b.prepared(&case.sql, &params, fresh).await;
        let route = probe.route(b, mark).await;
        if cli.plan_cache {
            plan_cache_check(case, &params, a, b, report).await;
        }
        let mut v = judge(
            case,
            &case.sql,
            Rerun::Prepared(&params),
            a,
            b,
            &oa,
            &ob,
            route.on_prototype(),
        )
        .await;
        if let (Some(x), Some(y)) = (&pa, &pb) {
            let mut d = prepare_diffs(x, y);
            d.extend(execute_metadata_diffs((Some(x), &oa), (Some(y), &ob)));
            if !d.is_empty() && v != Verdict::Unsupported {
                v = match v {
                    Verdict::Mismatch(mut prev) => {
                        prev.splice(0..0, d);
                        Verdict::Mismatch(prev)
                    }
                    _ => Verdict::Mismatch(d),
                };
            }
        }
        let v = report.rule(case, &case.sql, &oa, &ob, v);
        census.add(census_record(case, "ps", route.path(&ob), v.name()));
        let ra = SideResult {
            prepare: pa.as_ref(),
            outcome: &oa,
        };
        let rb = SideResult {
            prepare: pb.as_ref(),
            outcome: &ob,
        };
        if let Err(e) = report.record(case, "ps", &case.sql, ra, rb, &v) {
            eprintln!("cannot write report: {e}");
        }
    }
}

/// `--plan-cache`: `@@last_plan_from_cache` right after the EXECUTE, on both sides.
async fn plan_cache_check(
    case: &Case,
    params: &[mysql_async::Value],
    a: &mut Side,
    b: &mut Side,
    report: &mut Report,
) {
    const SQL: &str = "SELECT @@last_plan_from_cache";
    let value = |o: Outcome| match o {
        Outcome::Rows { rows, .. } => rows.first().and_then(|r| r.first()).cloned(),
        _ => None,
    };
    let (va, vb) = (value(a.query(SQL).await), value(b.query(SQL).await));
    let c = report.plan_cache.entry(case.template.clone()).or_default();
    c[3] += 1;
    let one = |v: &Option<String>| v.as_deref().is_some_and(|v| v.trim_matches('"') == "1");
    c[1] += u64::from(one(&va));
    c[2] += u64::from(one(&vb));
    if va == vb {
        c[0] += 1;
    } else {
        report
            .plan_cache_diffs
            .push(format!("{} {:?}: A {va:?}, B {vb:?}", case.sql, params));
    }
}

/// Runs a setup statement: on A always, on B too for session statements.
async fn run_setup(case: &Case, a: &mut Side, b: &mut Side) {
    let sql = case.text_sql();
    let oa = a.query(&sql).await;
    let src = case.source.as_deref().unwrap_or("setup");
    match &oa {
        Outcome::Error { code, message, .. } => {
            eprintln!("setup {src}: A error {code}: {message}")
        }
        Outcome::Client(m) => eprintln!("setup {src}: A client error: {m}"),
        _ if case.kind == Kind::Session => a.log_session(&sql),
        _ => {}
    }
    if case.kind == Kind::Session {
        let ob = b.query(&sql).await;
        match &ob {
            Outcome::Error { code, message, .. } => {
                eprintln!("setup {src}: B error {code}: {message}")
            }
            Outcome::Client(m) => eprintln!("setup {src}: B client error: {m}"),
            _ => b.log_session(&sql),
        }
    }
}

/// Row handles where sbtestN's record regions start (from A's SHOW TABLE ... REGIONS).
async fn region_boundaries(a: &mut Side, table: u32) -> Vec<i64> {
    let Outcome::Rows { columns, rows, .. } =
        a.query(&format!("SHOW TABLE sbtest{table} REGIONS")).await
    else {
        return Vec::new();
    };
    let Some(idx) = columns.iter().position(|c| c.name == "START_KEY") else {
        return Vec::new();
    };
    let mut v: Vec<i64> = rows
        .iter()
        .filter_map(|r| {
            let k = r.get(idx)?.trim_matches('"');
            k.rsplit_once("_r_")?.1.parse().ok()
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

async fn table_size(a: &mut Side) -> Result<i64, String> {
    match a.query("SELECT MAX(id) FROM sbtest1").await {
        Outcome::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.trim_matches('"').parse().ok())
            .ok_or_else(|| "sbtest1 is empty".to_string()),
        other => Err(format!("cannot read MAX(id) of sbtest1 on A: {other:?}")),
    }
}

/// What a run found: no mismatches; the census and negative control passed.
struct RunResult {
    no_mismatch: bool,
    gate_ok: bool,
}

async fn run(cli: Cli) -> Result<RunResult, String> {
    let layer = cli.census_layer.clone().unwrap_or_else(|| {
        match cli.workload {
            Workload::Integrationtest { .. } => "integrationtest",
            _ => "workload",
        }
        .to_string()
    });
    let mut census = Census::new(&layer, &cli.gate_fault)?;
    let mut ledger = match &cli.rulings {
        Some(p) => Ledger::load(p)?,
        None => Ledger::parse(rulings::BUILT_IN)
            .map_err(|e| format!("{}: {e}", rulings::DEFAULT_PATH))?,
    };
    let probe = RouteProbe::new(cli.b_status.clone());
    probe
        .check()
        .await
        .map_err(|e| format!("route trace of B: {e}"))?;
    if let Workload::Integrationtest { dir, tests } = &cli.workload {
        let ep = integration::Endpoints {
            a: cli.a.clone(),
            b: cli.b.clone(),
            user: cli.user.clone(),
            password: cli.password.clone(),
            tls: cli.tls,
            timeout: Duration::from_secs(cli.timeout),
        };
        let opts = integration::Options {
            dir: dir.clone(),
            only: tests.clone(),
            out_dir: cli.out_dir.clone(),
            report_rows: cli.report_rows,
            verbose: cli.verbose,
        };
        let no_mismatch = integration::run(&ep, &opts, &probe, &mut census, &mut ledger).await?;
        ledger.write_hits(&cli.out_dir.join(rulings::HITS_FILE))?;
        let gate_ok = census.write(&cli.out_dir)?;
        return Ok(RunResult {
            no_mismatch,
            gate_ok,
        });
    }
    let default_db = match &cli.workload {
        Workload::Sysbench { .. } => Some("sbtest".to_string()),
        Workload::File { .. } | Workload::Integrationtest { .. } => None,
    };
    let db = cli.db.clone().or(default_db);
    let timeout = Duration::from_secs(cli.timeout);
    let mk = |label, ep: &str| {
        Side::new(
            label,
            ep,
            &cli.user,
            &cli.password,
            db.as_deref(),
            cli.tls,
            timeout,
        )
    };
    let mut a = mk("A", &cli.a)?;
    let mut b = mk("B", &cli.b)?;
    for side in [&mut a, &mut b] {
        side.ensure_connected()
            .await
            .map_err(|e| format!("cannot connect to {}: {e}", side.label))?;
    }
    println!(
        "A = {} (tls: {}), B = {} (tls: {}), db = {}, seed = {}",
        cli.a,
        a.using_tls,
        cli.b,
        b.using_tls,
        db.as_deref().unwrap_or("-"),
        cli.seed
    );

    let mut rng = rng::Rng::new(cli.seed);
    let cases = match &cli.workload {
        Workload::Sysbench {
            tables,
            table_size: size,
            range_size,
            per_template,
            no_edge_cases,
        } => {
            let size = match size {
                Some(s) => *s,
                None => table_size(&mut a).await?,
            };
            let mut boundaries = Vec::new();
            for t in 1..=*tables {
                boundaries.push(region_boundaries(&mut a, t).await);
            }
            println!(
                "sysbench: {tables} table(s) x {size} rows, range {range_size}, \
                 {per_template} random cases/template, region boundaries {boundaries:?}"
            );
            workload::sysbench_cases(
                &SysbenchSpec {
                    tables: *tables,
                    table_size: size,
                    range_size: *range_size,
                    per_template: *per_template,
                    edge_cases: !no_edge_cases,
                    boundaries,
                },
                &mut rng,
            )
        }
        Workload::File {
            path,
            setup,
            no_prepared,
        } => {
            let read = |p: &PathBuf| {
                std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
            };
            let src = read(path)?;
            let setup_src = setup.as_ref().map(read).transpose()?;
            let setup_name = setup.as_ref().map(|p| p.display().to_string());
            let extra = setup_name.as_deref().zip(setup_src.as_deref());
            workload::file_cases(&path.display().to_string(), &src, extra, !no_prepared)?
        }
        Workload::Integrationtest { .. } => unreachable!("handled above"),
    };

    std::fs::create_dir_all(&cli.out_dir).map_err(|e| format!("{}: {e}", cli.out_dir.display()))?;
    let open = |name: &str| {
        let p = cli.out_dir.join(name);
        File::create(&p)
            .map(BufWriter::new)
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut report = Report {
        plan_cache: BTreeMap::new(),
        plan_cache_diffs: Vec::new(),
        counts: BTreeMap::new(),
        skipped: BTreeMap::new(),
        mismatches: open("mismatches.jsonl")?,
        unsupported: open("unsupported.jsonl")?,
        results: open("results.jsonl")?,
        report_rows: cli.report_rows,
        verbose: cli.verbose,
        ledger,
    };

    let started = Instant::now();
    census.expected = cases
        .iter()
        .map(|c| case_protocols(c, &cli).len() as u64)
        .sum();
    for case in &cases {
        if case.setup {
            run_setup(case, &mut a, &mut b).await;
            census.add(census_record(
                case,
                "text",
                (CensusPath::Skipped, "setup (A only)".into()),
                "setup",
            ));
            continue;
        }
        if let Kind::Other(why) = &case.kind {
            census.add(census_record(
                case,
                "text",
                (CensusPath::Skipped, why.clone()),
                "skipped",
            ));
            *report.skipped.entry(why.clone()).or_default() += 1;
            if cli.verbose {
                eprintln!(
                    "skip {}: {}",
                    case.source.as_deref().unwrap_or("-"),
                    case.sql
                );
            }
            continue;
        }
        run_case(case, &cli, &mut a, &mut b, &mut report, &probe, &mut census).await;
    }
    run_control(&mut a, &mut b, &mut census).await;
    a.disconnect().await;
    b.disconnect().await;
    report
        .mismatches
        .flush()
        .and_then(|_| report.unsupported.flush())
        .and_then(|_| report.results.flush())
        .map_err(|e| e.to_string())?;

    let all = report.print_summary();
    println!(
        "{} statement(s) in {:.1}s; mismatches: {}, unsupported: {}",
        cases.len(),
        started.elapsed().as_secs_f64(),
        cli.out_dir.join("mismatches.jsonl").display(),
        cli.out_dir.join("unsupported.jsonl").display()
    );
    report
        .ledger
        .write_hits(&cli.out_dir.join(rulings::HITS_FILE))?;
    let gate_ok = census.write(&cli.out_dir)?;
    Ok(RunResult {
        no_mismatch: all.mismatch == 0,
        gate_ok,
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ab: {e}");
            return ExitCode::from(2);
        }
    };
    match rt.block_on(run(cli)) {
        Ok(r) if !r.gate_ok => {
            if !r.no_mismatch {
                println!("RESULT: MISMATCH");
            }
            println!("RESULT: GATE FAILED (census / negative control)");
            ExitCode::from(census::EXIT_GATE)
        }
        Ok(r) if r.no_mismatch => ExitCode::SUCCESS,
        Ok(_) => {
            println!("RESULT: MISMATCH");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("ab: {e}");
            ExitCode::from(2)
        }
    }
}
