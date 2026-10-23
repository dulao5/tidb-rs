//! `ab`: run the same statements on Go TiDB (A) and tidb-rs (B) and compare rows, column
//! metadata and errors, via the text protocol and server-side prepared statements.
//! See tools/ab/README.md.

#![forbid(unsafe_code)]

mod compare;
mod exec;
mod rng;
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

use crate::compare::{Verdict, charset_name, compare, flag_names, prepare_diffs, type_name};
use crate::exec::{ColumnMeta, Outcome, PrepareMeta, Side, TlsMode};
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
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    matched: u64,
    mismatch: u64,
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
        }
    }

    fn total(&self) -> u64 {
        self.matched + self.mismatch + self.unsupported + self.both_error
    }

    fn merge(&mut self, o: &Counts) {
        self.matched += o.matched;
        self.mismatch += o.mismatch;
        self.unsupported += o.unsupported;
        self.both_error += o.both_error;
    }
}

struct Report {
    counts: BTreeMap<(String, &'static str), Counts>,
    skipped: BTreeMap<String, u64>,
    mismatches: BufWriter<File>,
    unsupported: BufWriter<File>,
    report_rows: usize,
    verbose: bool,
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
        let out = match verdict {
            Verdict::Mismatch(diff) => {
                if self.verbose {
                    eprintln!("MISMATCH [{protocol}] {query}");
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
        if let Verdict::Mismatch(diff) = verdict {
            rec["diff"] = json!(diff);
        }
        if let Some(s) = &case.source {
            rec["source"] = json!(s);
        }
        if protocol == "ps" {
            rec["params"] = json!(case.params.iter().map(Param::to_json).collect::<Vec<_>>());
            rec["a"]["prepare"] = prepare_json(a.prepare);
            rec["b"]["prepare"] = prepare_json(b.prepare);
        }
        writeln!(out, "{rec}")
    }

    fn print_summary(&self) -> Counts {
        println!();
        println!(
            "{:<20} {:<5} {:>8} {:>8} {:>11} {:>10} {:>7}",
            "template", "proto", "match", "mismatch", "unsupported", "both_error", "total"
        );
        let mut all = Counts::default();
        for ((t, p), c) in &self.counts {
            println!(
                "{:<20} {:<5} {:>8} {:>8} {:>11} {:>10} {:>7}",
                t,
                p,
                c.matched,
                c.mismatch,
                c.unsupported,
                c.both_error,
                c.total()
            );
            all.merge(c);
        }
        println!(
            "{:<20} {:<5} {:>8} {:>8} {:>11} {:>10} {:>7}",
            "TOTAL",
            "",
            all.matched,
            all.mismatch,
            all.unsupported,
            all.both_error,
            all.total()
        );
        for (why, n) in &self.skipped {
            println!("skipped {n} statement(s): {why}");
        }
        all
    }
}

async fn run_case(case: &Case, cli: &Cli, a: &mut Side, b: &mut Side, report: &mut Report) {
    let text_sql = case.text_sql();
    if cli.protocol != Protocols::Ps || !case.prepared {
        let oa = a.query(&text_sql).await;
        let ob = b.query(&text_sql).await;
        if case.kind == Kind::Session {
            if !matches!(oa, Outcome::Error { .. } | Outcome::Client(_)) {
                a.log_session(&text_sql);
            }
            if !matches!(ob, Outcome::Error { .. } | Outcome::Client(_)) {
                b.log_session(&text_sql);
            }
        }
        let v = compare(&oa, &ob, case.ordered);
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
        let (pa, oa) = a.prepared(&case.sql, &params).await;
        let (pb, ob) = b.prepared(&case.sql, &params).await;
        let mut v = compare(&oa, &ob, case.ordered);
        if let (Some(x), Some(y)) = (&pa, &pb) {
            let d = prepare_diffs(x, y);
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

async fn run(cli: Cli) -> Result<bool, String> {
    let default_db = match &cli.workload {
        Workload::Sysbench { .. } => Some("sbtest".to_string()),
        Workload::File { .. } => None,
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
            workload::file_cases(&path.display().to_string(), &src, extra, !no_prepared)
        }
    };

    std::fs::create_dir_all(&cli.out_dir).map_err(|e| format!("{}: {e}", cli.out_dir.display()))?;
    let open = |name: &str| {
        let p = cli.out_dir.join(name);
        File::create(&p)
            .map(BufWriter::new)
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut report = Report {
        counts: BTreeMap::new(),
        skipped: BTreeMap::new(),
        mismatches: open("mismatches.jsonl")?,
        unsupported: open("unsupported.jsonl")?,
        report_rows: cli.report_rows,
        verbose: cli.verbose,
    };

    let started = Instant::now();
    for case in &cases {
        if case.setup {
            run_setup(case, &mut a, &mut b).await;
            continue;
        }
        if let Kind::Other(why) = &case.kind {
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
        run_case(case, &cli, &mut a, &mut b, &mut report).await;
    }
    a.disconnect().await;
    b.disconnect().await;
    report
        .mismatches
        .flush()
        .and_then(|_| report.unsupported.flush())
        .map_err(|e| e.to_string())?;

    let all = report.print_summary();
    println!(
        "{} statement(s) in {:.1}s; mismatches: {}, unsupported: {}",
        cases.len(),
        started.elapsed().as_secs_f64(),
        cli.out_dir.join("mismatches.jsonl").display(),
        cli.out_dir.join("unsupported.jsonl").display()
    );
    Ok(all.mismatch == 0)
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
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            println!("RESULT: MISMATCH");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("ab: {e}");
            ExitCode::from(2)
        }
    }
}
