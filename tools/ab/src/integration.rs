//! `ab integrationtest`: TiDB's integration tests (mysql-tester `.test` files) in hybrid mode.
//!
//! Every statement of a file runs on Go TiDB (A) in that file's sessions, as mysql-tester would
//! run it. Read statements also run on tidb-rs (B), in B sessions that follow A's current
//! database and session statements, and B's answer is compared with A's (same cluster, same
//! data, same moment). See tools/ab/README.md, "Integration tests".

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::json;

use crate::compare::{ER_NOT_SUPPORTED_YET, Verdict, compare};
use crate::exec::{Outcome, Side, TlsMode};
use crate::mysqltest::{self, Class, Item, TxnEffect};
use crate::outcome_json;

/// Connection settings shared by all sessions.
pub struct Endpoints {
    pub a: String,
    pub b: String,
    pub user: String,
    pub password: String,
    pub tls: TlsMode,
    pub timeout: Duration,
}

pub struct Options {
    /// `tests/integrationtest/t` of a TiDB checkout.
    pub dir: PathBuf,
    /// Test names (`executor/aggregate`) or prefixes (`executor/`); empty = all.
    pub only: Vec<String>,
    pub out_dir: PathBuf,
    pub report_rows: usize,
    pub verbose: bool,
}

/// Outcome of one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StmtOutcome {
    Match,
    Mismatch,
    Unsupported,
    BError,
    AError,
    Write,
    Skipped,
}

impl StmtOutcome {
    fn name(self) -> &'static str {
        match self {
            StmtOutcome::Match => "match",
            StmtOutcome::Mismatch => "mismatch",
            StmtOutcome::Unsupported => "unsupported",
            StmtOutcome::BError => "b_error",
            StmtOutcome::AError => "a_error",
            StmtOutcome::Write => "write",
            StmtOutcome::Skipped => "skipped",
        }
    }
}

/// Per-file counts (one row of files.tsv).
#[derive(Debug, Default, Clone)]
struct FileCounts {
    statements: u64,
    reads: u64,
    by: BTreeMap<&'static str, u64>,
    b_set_rejected: u64,
    retried: u64,
}

impl FileCounts {
    fn get(&self, k: &str) -> u64 {
        self.by.get(k).copied().unwrap_or(0)
    }
}

/// One session pair: A and B connected as the same user to the same database.
struct Conn {
    a: Side,
    b: Side,
    /// Why B could not log in, if it could not.
    b_login: Option<String>,
    in_txn: bool,
    autocommit_off: bool,
    /// Prepared statement name -> whether it is a read.
    prepared: BTreeMap<String, bool>,
    /// Local temporary tables created in this session.
    temp_tables: BTreeSet<String>,
    /// Session statements B rejected (still possibly in effect on A).
    rejected: Vec<String>,
}

/// Directives that apply to the next statement only.
#[derive(Default)]
struct Pending {
    error: Option<String>,
    sorted: bool,
    replace_regex: Vec<(Regex, String)>,
    replace_column: Vec<(usize, String)>,
    bad_directive: Option<String>,
}

struct Writers {
    statements: BufWriter<File>,
    mismatches: BufWriter<File>,
    files: BufWriter<File>,
}

/// Escapes a TSV field (backslash, tab, newline, carriage return).
fn tsv(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n).collect();
    t.push('…');
    t
}

/// mysql-tester's database for a test: the name with `/` replaced by `__`.
pub fn db_name(test: &str) -> String {
    test.replace('/', "__")
}

/// Test names (relative path without `.test`) under `dir`, sorted.
pub fn list_tests(dir: &Path) -> Result<Vec<String>, String> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for e in std::fs::read_dir(d)? {
            let p = e?.path();
            if p.is_dir() {
                walk(base, &p, out)?;
            } else if p.extension().is_some_and(|x| x == "test")
                && let Ok(rel) = p.strip_prefix(base)
            {
                let s = rel.with_extension("").to_string_lossy().replace('\\', "/");
                out.push(s);
            }
        }
        Ok(())
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v).map_err(|e| format!("{}: {e}", dir.display()))?;
    v.sort();
    Ok(v)
}

/// Parses `--replace_regex /a/b/ /c/d/`.
fn parse_replace_regex(arg: &str) -> Result<Vec<(Regex, String)>, String> {
    let mut out = Vec::new();
    let mut rest = arg.trim();
    while !rest.is_empty() {
        let mut chars = rest.char_indices();
        let Some((_, delim)) = chars.next() else {
            break;
        };
        let mut parts = Vec::new();
        let mut cur = String::new();
        let mut end = rest.len();
        let mut escaped = false;
        for (i, c) in chars {
            if escaped {
                if c != delim {
                    cur.push('\\');
                }
                cur.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == delim {
                parts.push(std::mem::take(&mut cur));
                if parts.len() == 2 {
                    end = i + c.len_utf8();
                    break;
                }
            } else {
                cur.push(c);
            }
        }
        if parts.len() != 2 {
            return Err(format!("bad replace_regex {arg:?}"));
        }
        let re = Regex::new(&parts[0]).map_err(|e| format!("replace_regex {:?}: {e}", parts[0]))?;
        out.push((re, parts[1].clone()));
        rest = rest[end..].trim_start();
    }
    Ok(out)
}

/// Parses `--replace_column 1 <x> 3 <y>`.
fn parse_replace_column(arg: &str) -> Result<Vec<(usize, String)>, String> {
    let f: Vec<&str> = arg.split_whitespace().collect();
    if !f.len().is_multiple_of(2) {
        return Err(format!("bad replace_column {arg:?}"));
    }
    f.chunks(2)
        .map(|c| {
            let n: usize = c[0]
                .parse()
                .map_err(|_| format!("bad replace_column {arg:?}"))?;
            Ok((n, c[1].to_string()))
        })
        .collect()
}

/// Applies the result rewrites to rendered values (in place).
fn rewrite(o: &mut Outcome, p: &Pending) {
    let Outcome::Rows { rows, .. } = o else {
        return;
    };
    for row in rows {
        for (re, to) in &p.replace_regex {
            for v in row.iter_mut() {
                *v = re.replace_all(v, to.as_str()).into_owned();
            }
        }
        for (n, to) in &p.replace_column {
            if let Some(v) = n.checked_sub(1).and_then(|i| row.get_mut(i)) {
                *v = to.clone();
            }
        }
    }
}

/// Mismatch kind from the comparison's diff lines.
fn mismatch_kind(d: &[String]) -> &'static str {
    let rows = d.iter().any(|s| {
        s.starts_with("row count") || s.contains(" only in ") || s.starts_with("first differing")
    });
    let order_only = d.iter().any(|s| s == "same rows in a different order");
    if rows && !order_only {
        "rows"
    } else if order_only {
        "order"
    } else if d
        .iter()
        .any(|s| s.starts_with("column count") || s.contains("].name:"))
    {
        "columns"
    } else {
        "metadata"
    }
}

fn error_code(o: &Outcome) -> Option<u16> {
    match o {
        Outcome::Error { code, .. } => Some(*code),
        _ => None,
    }
}

fn brief(o: &Outcome) -> String {
    match o {
        Outcome::Rows { columns, rows, .. } => {
            format!("{} cols, {} rows", columns.len(), rows.len())
        }
        Outcome::Ok { affected_rows, .. } => format!("OK ({affected_rows})"),
        Outcome::Error { code, message, .. } => format!("{code}: {message}"),
        Outcome::Client(m) => format!("client: {m}"),
    }
}

/// Judges a read: (verdict, kind, detail).
fn judge(
    a: &Outcome,
    b: &Outcome,
    ordered: bool,
    status_only: bool,
) -> (StmtOutcome, String, String) {
    if let Outcome::Error { code, message, .. } = b
        && *code == ER_NOT_SUPPORTED_YET
    {
        return (StmtOutcome::Unsupported, String::new(), message.clone());
    }
    match (a, b) {
        (_, Outcome::Client(m)) => (StmtOutcome::BError, "client".into(), m.clone()),
        (Outcome::Error { code: ca, .. }, Outcome::Error { code: cb, .. }) => {
            if ca == cb {
                (StmtOutcome::Match, String::new(), String::new())
            } else {
                (
                    StmtOutcome::Mismatch,
                    "error".into(),
                    format!("A {} vs B {}", brief(a), brief(b)),
                )
            }
        }
        (Outcome::Error { .. }, _) => (
            StmtOutcome::Mismatch,
            "error".into(),
            format!("A {} vs B {}", brief(a), brief(b)),
        ),
        (_, Outcome::Error { code, message, .. }) => {
            (StmtOutcome::BError, code.to_string(), message.clone())
        }
        (Outcome::Rows { .. }, Outcome::Rows { .. }) | (Outcome::Ok { .. }, Outcome::Ok { .. })
            if status_only =>
        {
            (StmtOutcome::Match, String::new(), String::new())
        }
        _ => match compare(a, b, ordered) {
            Verdict::Match | Verdict::BothError => {
                (StmtOutcome::Match, String::new(), String::new())
            }
            Verdict::Unsupported => (StmtOutcome::Unsupported, String::new(), String::new()),
            Verdict::Mismatch(d) => {
                let kind = if matches!((a, b), (Outcome::Rows { .. }, Outcome::Rows { .. })) {
                    mismatch_kind(&d)
                } else {
                    "shape"
                };
                (StmtOutcome::Mismatch, kind.into(), d.join("; "))
            }
        },
    }
}

struct Runner<'a> {
    ep: &'a Endpoints,
    opts: &'a Options,
    w: Writers,
    /// A session for creating/dropping the per-file databases.
    admin: Side,
    totals: FileCounts,
}

/// One statement's record.
struct Rec<'a> {
    file: &'a str,
    line: usize,
    conn: &'a str,
    class: &'a str,
    verdict: StmtOutcome,
    kind: String,
    detail: String,
    sql: &'a str,
    ms: u128,
}

impl Runner<'_> {
    fn write_stmt(&mut self, r: &Rec<'_>, counts: &mut FileCounts) {
        counts.statements += 1;
        *counts.by.entry(r.verdict.name()).or_default() += 1;
        let _ = writeln!(
            self.w.statements,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            tsv(r.file),
            r.line,
            tsv(r.conn),
            r.class,
            r.verdict.name(),
            tsv(&r.kind),
            tsv(&truncate(&r.detail, 300)),
            r.ms,
            tsv(&truncate(r.sql, 1000)),
        );
        if self.opts.verbose && matches!(r.verdict, StmtOutcome::Mismatch) {
            eprintln!(
                "MISMATCH {}:{} [{}] {}\n    {}",
                r.file,
                r.line,
                r.kind,
                truncate(r.sql, 200),
                truncate(&r.detail, 500)
            );
        }
    }

    async fn new_conn(&self, user: &str, password: &str, db: Option<&str>) -> Result<Conn, String> {
        let mk = |label, ep: &str| {
            Side::new(label, ep, user, password, db, self.ep.tls, self.ep.timeout)
        };
        let mut a = mk("A", &self.ep.a)?;
        a.ensure_connected().await?;
        let mut b = mk("B", &self.ep.b)?;
        // B learns about a new database through schema sync; give it a moment.
        let mut b_login = None;
        for attempt in 0..3 {
            match b.ensure_connected().await {
                Ok(()) => {
                    b_login = None;
                    break;
                }
                Err(e) => {
                    b_login = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        Ok(Conn {
            a,
            b,
            b_login,
            in_txn: false,
            autocommit_off: false,
            prepared: BTreeMap::new(),
            temp_tables: BTreeSet::new(),
            rejected: Vec::new(),
        })
    }

    async fn run_file(&mut self, test: &str) -> Result<FileCounts, String> {
        let path = self.opts.dir.join(format!("{test}.test"));
        let src = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let items = mysqltest::parse(&src);
        let db = db_name(test);
        let mut counts = FileCounts::default();
        for sql in [
            format!("DROP DATABASE IF EXISTS `{db}`"),
            format!("CREATE DATABASE `{db}`"),
        ] {
            if let o @ (Outcome::Error { .. } | Outcome::Client(_)) = self.admin.query(&sql).await {
                return Err(format!("{sql}: {}", brief(&o)));
            }
        }
        let mut conns: BTreeMap<String, Conn> = BTreeMap::new();
        conns.insert(
            "default".into(),
            self.new_conn(&self.ep.user, &self.ep.password, Some(&db))
                .await?,
        );
        let mut cur = "default".to_string();
        let mut pending = Pending::default();
        let mut warnings = false;
        let mut result_log = true;

        for item in items {
            match item {
                Item::Directive { line, cmd, arg } => match cmd.as_str() {
                    "error" => pending.error = Some(arg),
                    "sorted_result" => pending.sorted = true,
                    "replace_regex" => match parse_replace_regex(&arg) {
                        Ok(mut v) => pending.replace_regex.append(&mut v),
                        Err(e) => pending.bad_directive = Some(format!("line {line}: {e}")),
                    },
                    "replace_column" => match parse_replace_column(&arg) {
                        Ok(mut v) => pending.replace_column.append(&mut v),
                        Err(e) => pending.bad_directive = Some(format!("line {line}: {e}")),
                    },
                    "enable_warnings" => warnings = true,
                    "disable_warnings" => warnings = false,
                    "enable_result_log" => result_log = true,
                    "disable_result_log" => result_log = false,
                    // echo, enable/disable_info (affected-rows info of writes), remove_file,
                    // sleep: no effect on what is compared.
                    _ => {}
                },
                Item::Connect {
                    line,
                    name,
                    user,
                    password,
                    db: cdb,
                } => {
                    let cdb = (!cdb.is_empty()).then_some(cdb);
                    match self.new_conn(&user, &password, cdb.as_deref()).await {
                        Ok(c) => {
                            conns.insert(name.clone(), c);
                            cur = name;
                        }
                        Err(e) => {
                            // mysql-tester would abort the file here.
                            eprintln!(
                                "{test}:{line}: connect {name} failed on A: {e}; rest of file skipped"
                            );
                            break;
                        }
                    }
                }
                Item::Connection { line, name } => {
                    if conns.contains_key(&name) {
                        cur = name;
                    } else {
                        eprintln!("{test}:{line}: unknown connection {name}; rest of file skipped");
                        break;
                    }
                }
                Item::Disconnect { name, .. } => {
                    if let Some(mut c) = conns.remove(&name) {
                        c.a.disconnect().await;
                        c.b.disconnect().await;
                    }
                    // mysql-tester falls back to the default connection.
                    if cur == name {
                        cur = "default".into();
                    }
                }
                Item::Query { line, sql } => {
                    let p = std::mem::take(&mut pending);
                    let Some(conn) = conns.get_mut(&cur) else {
                        eprintln!("{test}:{line}: no current connection; rest of file skipped");
                        break;
                    };
                    let t0 = Instant::now();
                    let rec = self
                        .run_stmt(
                            test,
                            line,
                            &cur,
                            conn,
                            &sql,
                            &p,
                            warnings,
                            result_log,
                            &mut counts,
                        )
                        .await;
                    let ms = t0.elapsed().as_millis();
                    self.write_stmt(&rec.as_rec(test, line, &cur, &sql, ms), &mut counts);
                }
            }
        }
        for (_, mut c) in conns {
            c.a.disconnect().await;
            c.b.disconnect().await;
        }
        let _ = self
            .admin
            .query(&format!("DROP DATABASE IF EXISTS `{db}`"))
            .await;
        Ok(counts)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_stmt(
        &mut self,
        test: &str,
        line: usize,
        conn_name: &str,
        conn: &mut Conn,
        sql: &str,
        p: &Pending,
        warnings: bool,
        result_log: bool,
        counts: &mut FileCounts,
    ) -> Owned {
        let class = mysqltest::classify(sql, |n| conn.prepared.get(n).copied());
        let class_name = match &class {
            Class::Read(t) => *t,
            Class::Session => "session",
            Class::Write => "write",
        };
        let mut out = Owned {
            class: class_name,
            verdict: StmtOutcome::Write,
            kind: String::new(),
            detail: String::new(),
        };
        if matches!(class, Class::Read(_)) {
            counts.reads += 1;
        }
        let expect_error = p.error.is_some();

        let oa = conn.a.query(sql).await;
        let a_failed = matches!(oa, Outcome::Error { .. } | Outcome::Client(_));
        if !a_failed {
            match mysqltest::txn_effect(sql) {
                TxnEffect::Begin => conn.in_txn = true,
                TxnEffect::End => conn.in_txn = false,
                TxnEffect::AutocommitOff => conn.autocommit_off = true,
                TxnEffect::AutocommitOn => {
                    conn.autocommit_off = false;
                    conn.in_txn = false;
                }
                TxnEffect::None => {}
            }
            if let Some(t) = mysqltest::local_temporary_table(sql) {
                conn.temp_tables.insert(t);
            }
        }
        if a_failed && !expect_error {
            out.verdict = StmtOutcome::AError;
            out.detail = brief(&oa);
            return out;
        }

        match class {
            Class::Write => out,
            Class::Session => {
                out.kind = "session".into();
                if a_failed {
                    return out;
                }
                if let Some((name, text)) = mysqltest::prepare_parts(sql) {
                    let is_read = text.is_some_and(|t| {
                        matches!(mysqltest::classify(&t, |_| None), Class::Read(_))
                    });
                    conn.prepared.insert(name, is_read);
                }
                conn.a.log_session(sql);
                if conn.b_login.is_some() {
                    return out;
                }
                let ob = conn.b.query(sql).await;
                match &ob {
                    Outcome::Error { .. } | Outcome::Client(_) => {
                        counts.b_set_rejected += 1;
                        out.detail = format!("B rejected: {}", brief(&ob));
                        conn.rejected.push(truncate(sql, 120));
                    }
                    _ => conn.b.log_session(sql),
                }
                out
            }
            Class::Read(_) => {
                if let Some(why) = &p.bad_directive {
                    out.verdict = StmtOutcome::Skipped;
                    out.kind = "directive".into();
                    out.detail = why.clone();
                    return out;
                }
                if conn.in_txn || conn.autocommit_off {
                    out.verdict = StmtOutcome::Skipped;
                    out.kind = "in transaction".into();
                    return out;
                }
                if mysqltest::mentions(sql, &conn.temp_tables) {
                    out.verdict = StmtOutcome::Skipped;
                    out.kind = "local temporary table".into();
                    return out;
                }
                if let Some(e) = &conn.b_login {
                    out.verdict = StmtOutcome::Unsupported;
                    out.kind = "login".into();
                    out.detail = format!("B login as this connection's user failed: {e}");
                    return out;
                }
                let mut oa = oa;
                let mut ob = conn.b.query(sql).await;
                // A read right after DDL: B's schema sync should already have the new schema
                // (the DDL owner waits for it). Retry once to detect lag.
                if !a_failed && matches!(error_code(&ob), Some(1146 | 1049 | 1054 | 1051 | 1046)) {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let again = conn.b.query(sql).await;
                    if error_code(&again) != error_code(&ob) {
                        counts.retried += 1;
                        eprintln!(
                            "{test}:{line}: B answered {} then {} (schema lag?)",
                            brief(&ob),
                            brief(&again)
                        );
                        out.kind = "retried".into();
                    }
                    ob = again;
                }
                rewrite(&mut oa, p);
                rewrite(&mut ob, p);
                let ordered = !p.sorted && crate::sql::has_top_level_order_by(sql);
                let (mut v, kind, mut detail) = judge(&oa, &ob, ordered, !result_log);
                let mut kind = if out.kind.is_empty() {
                    kind
                } else {
                    format!("{kind} retried")
                };
                // --enable_warnings: mysql-tester prints SHOW WARNINGS after the statement.
                if warnings && v == StmtOutcome::Match && !a_failed {
                    let wa = conn.a.query("SHOW WARNINGS").await;
                    let wb = conn.b.query("SHOW WARNINGS").await;
                    if let Verdict::Mismatch(d) = compare(&wa, &wb, true) {
                        v = StmtOutcome::Mismatch;
                        kind = "warnings".into();
                        detail = d.join("; ");
                    }
                }
                if matches!(v, StmtOutcome::Mismatch | StmtOutcome::BError) {
                    let rec = json!({
                        "file": test,
                        "line": line,
                        "conn": conn_name,
                        "verdict": v.name(),
                        "kind": kind,
                        "sql": sql,
                        "ordered": ordered,
                        "detail": detail,
                        "b_rejected_session_statements": conn.rejected,
                        "a": outcome_json(&oa, self.opts.report_rows),
                        "b": outcome_json(&ob, self.opts.report_rows),
                    });
                    let _ = writeln!(self.w.mismatches, "{rec}");
                }
                out.verdict = v;
                out.kind = kind;
                out.detail = detail;
                out
            }
        }
    }
}

/// A statement's result before it is written.
struct Owned {
    class: &'static str,
    verdict: StmtOutcome,
    kind: String,
    detail: String,
}

impl Owned {
    fn as_rec<'a>(
        &'a self,
        file: &'a str,
        line: usize,
        conn: &'a str,
        sql: &'a str,
        ms: u128,
    ) -> Rec<'a> {
        Rec {
            file,
            line,
            conn,
            class: self.class,
            verdict: self.verdict,
            kind: self.kind.clone(),
            detail: self.detail.clone(),
            sql,
            ms,
        }
    }
}

const FILE_COLUMNS: &str = "file\tstatements\treads\tmatch\tmismatch\tunsupported\tb_error\ta_error\tskipped\twrite\tb_set_rejected\tretried\tseconds";

/// Runs the selected test files; returns whether there were no mismatches.
pub async fn run(ep: &Endpoints, opts: &Options) -> Result<bool, String> {
    let mut tests = list_tests(&opts.dir)?;
    if !opts.only.is_empty() {
        tests.retain(|t| {
            opts.only
                .iter()
                .any(|o| t == o || (o.ends_with('/') && t.starts_with(o.as_str())))
        });
    }
    if tests.is_empty() {
        return Err(format!(
            "no .test files selected under {}",
            opts.dir.display()
        ));
    }
    std::fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("{}: {e}", opts.out_dir.display()))?;
    let open = |name: &str| {
        let p = opts.out_dir.join(name);
        File::create(&p)
            .map(BufWriter::new)
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let mut w = Writers {
        statements: open("statements.tsv")?,
        mismatches: open("mismatches.jsonl")?,
        files: open("files.tsv")?,
    };
    writeln!(
        w.statements,
        "file\tline\tconn\tclass\toutcome\tkind\tdetail\tms\tsql"
    )
    .map_err(|e| e.to_string())?;
    writeln!(w.files, "{FILE_COLUMNS}").map_err(|e| e.to_string())?;

    let mut admin = Side::new("A", &ep.a, &ep.user, &ep.password, None, ep.tls, ep.timeout)?;
    admin
        .ensure_connected()
        .await
        .map_err(|e| format!("cannot connect to A: {e}"))?;
    let mut b_probe = Side::new("B", &ep.b, &ep.user, &ep.password, None, ep.tls, ep.timeout)?;
    b_probe
        .ensure_connected()
        .await
        .map_err(|e| format!("cannot connect to B: {e}"))?;
    b_probe.disconnect().await;

    let mut r = Runner {
        ep,
        opts,
        w,
        admin,
        totals: FileCounts::default(),
    };
    let started = Instant::now();
    let n = tests.len();
    for (i, t) in tests.iter().enumerate() {
        let t0 = Instant::now();
        let c = match r.run_file(t).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{t}: {e}");
                FileCounts::default()
            }
        };
        let secs = t0.elapsed().as_secs_f64();
        let _ = writeln!(
            r.w.files,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}",
            t,
            c.statements,
            c.reads,
            c.get("match"),
            c.get("mismatch"),
            c.get("unsupported"),
            c.get("b_error"),
            c.get("a_error"),
            c.get("skipped"),
            c.get("write"),
            c.b_set_rejected,
            c.retried,
            secs
        );
        let _ = r.w.files.flush();
        let _ = r.w.statements.flush();
        let _ = r.w.mismatches.flush();
        println!(
            "[{}/{n}] {t}: {} stmts, {} reads: match {} mismatch {} unsupported {} b_error {} a_error {} skipped {} ({secs:.1}s)",
            i + 1,
            c.statements,
            c.reads,
            c.get("match"),
            c.get("mismatch"),
            c.get("unsupported"),
            c.get("b_error"),
            c.get("a_error"),
            c.get("skipped"),
        );
        r.totals.statements += c.statements;
        r.totals.reads += c.reads;
        r.totals.b_set_rejected += c.b_set_rejected;
        r.totals.retried += c.retried;
        for (k, v) in c.by {
            *r.totals.by.entry(k).or_default() += v;
        }
    }
    r.admin.disconnect().await;
    let t = &r.totals;
    println!(
        "TOTAL {n} files, {} statements, {} reads: match {} mismatch {} unsupported {} b_error {} | a_error {} skipped {} write {} | B rejected {} session statements, {} schema-lag retries; {:.0}s",
        t.statements,
        t.reads,
        t.get("match"),
        t.get("mismatch"),
        t.get("unsupported"),
        t.get("b_error"),
        t.get("a_error"),
        t.get("skipped"),
        t.get("write"),
        t.b_set_rejected,
        t.retried,
        started.elapsed().as_secs_f64()
    );
    Ok(t.get("mismatch") == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&str]) -> Outcome {
        Outcome::Rows {
            columns: Vec::new(),
            rows: v.iter().map(|s| vec![s.to_string()]).collect(),
            warnings: 0,
        }
    }

    fn err(code: u16) -> Outcome {
        Outcome::Error {
            code,
            state: "HY000".into(),
            message: "m".into(),
        }
    }

    #[test]
    fn judges() {
        assert_eq!(
            judge(&rows(&["1"]), &rows(&["1"]), true, false).0,
            StmtOutcome::Match
        );
        let (v, k, _) = judge(&rows(&["1"]), &rows(&["2"]), false, false);
        assert_eq!((v, k.as_str()), (StmtOutcome::Mismatch, "rows"));
        let (v, k, _) = judge(&rows(&["1", "2"]), &rows(&["2", "1"]), true, false);
        assert_eq!((v, k.as_str()), (StmtOutcome::Mismatch, "order"));
        assert_eq!(
            judge(&rows(&["1"]), &rows(&["2"]), false, true).0,
            StmtOutcome::Match
        );
        assert_eq!(
            judge(&rows(&[]), &err(1235), false, false).0,
            StmtOutcome::Unsupported
        );
        assert_eq!(
            judge(&rows(&[]), &err(1105), false, false).0,
            StmtOutcome::BError
        );
        assert_eq!(
            judge(&err(1146), &err(1146), false, false).0,
            StmtOutcome::Match
        );
        let (v, k, _) = judge(&err(1146), &err(1105), false, false);
        assert_eq!((v, k.as_str()), (StmtOutcome::Mismatch, "error"));
        let (v, k, _) = judge(&err(1146), &rows(&[]), false, false);
        assert_eq!((v, k.as_str()), (StmtOutcome::Mismatch, "error"));
    }

    #[test]
    fn rewrites() {
        let p = Pending {
            replace_regex: parse_replace_regex(r"/[0-9]+ Bytes/<num> Bytes/ /a\/b/c/").unwrap(),
            replace_column: parse_replace_column("2 <x>").unwrap(),
            ..Pending::default()
        };
        let mut o = Outcome::Rows {
            columns: Vec::new(),
            rows: vec![vec!["\"12 Bytes a/b\"".into(), "\"y\"".into()]],
            warnings: 0,
        };
        rewrite(&mut o, &p);
        let Outcome::Rows { rows, .. } = o else {
            panic!()
        };
        assert_eq!(
            rows[0],
            vec!["\"<num> Bytes c\"".to_string(), "<x>".to_string()]
        );
        assert_eq!(db_name("executor/aggregate"), "executor__aggregate");
        assert_eq!(tsv("a\tb\nc\\"), "a\\tb\\nc\\\\");
    }
}
