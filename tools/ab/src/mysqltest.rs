//! Reading mysql-tester `.test` files (TiDB's `tests/integrationtest/t`) and classifying their
//! statements for the hybrid run of `ab integrationtest`.
//!
//! The file format follows pingcap/mysql-tester: lines are trimmed; `#` lines are comments;
//! `--cmd args` lines are directives; other lines are SQL, joined until a line ends with `;`.
//! A comment or directive line also ends a statement that has no `;` yet. `connect (...)`,
//! `connection name` and `disconnect name` are written without `--`.

use crate::sql::{Word, words};

/// One item of a test file, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// `--cmd arg` (cmd lower-cased, `;` stripped from the end).
    Directive {
        line: usize,
        cmd: String,
        arg: String,
    },
    /// A SQL statement without its trailing `;`.
    Query { line: usize, sql: String },
    /// `connect (name, host, user, password, db)`.
    Connect {
        line: usize,
        name: String,
        user: String,
        password: String,
        db: String,
    },
    /// `connection name`.
    Connection { line: usize, name: String },
    /// `disconnect name`.
    Disconnect { line: usize, name: String },
}

/// Directives mysql-tester knows (and whether `ab integrationtest` acts on them). Lines such as
/// `-- text` or `--` alone are comments.
pub const DIRECTIVES: [&str; 15] = [
    "error",
    "echo",
    "sorted_result",
    "replace_regex",
    "replace_column",
    "enable_warnings",
    "disable_warnings",
    "enable_info",
    "disable_info",
    "enable_result_log",
    "disable_result_log",
    "remove_file",
    "begin_concurrent",
    "end_concurrent",
    "sleep",
];

pub fn parse(src: &str) -> Vec<Item> {
    // (first line, text) of the raw queries, as mysql-tester's loadQueries builds them.
    let mut raw: Vec<(usize, String)> = Vec::new();
    let mut new_stmt = true;
    let mut out = Vec::new();
    let flush = |raw: &mut Vec<(usize, String)>, out: &mut Vec<Item>| {
        for (line, q) in raw.drain(..) {
            if let Some(item) = query_item(line, &q) {
                out.push(item);
            }
        }
    };
    for (i, l) in src.lines().enumerate() {
        let line = i + 1;
        let s = l.trim();
        if s.starts_with('#') {
            new_stmt = true;
            continue;
        }
        if let Some(d) = s.strip_prefix("--") {
            flush(&mut raw, &mut out);
            new_stmt = true;
            // `--error 1146` and `-- error 1146` are both directives; `-- text` is a comment.
            let d = d.trim().trim_end_matches(';').trim_end();
            let (cmd, arg) = match d.find(char::is_whitespace) {
                Some(p) => (&d[..p], d[p..].trim()),
                None => (d, ""),
            };
            let cmd = cmd.to_ascii_lowercase();
            if DIRECTIVES.contains(&cmd.as_str()) {
                out.push(Item::Directive {
                    line,
                    cmd,
                    arg: arg.to_string(),
                });
            }
            continue;
        }
        if s.is_empty() {
            continue;
        }
        if new_stmt {
            flush(&mut raw, &mut out);
            raw.push((line, s.to_string()));
        } else if let Some(last) = raw.last_mut() {
            last.1.push('\n');
            last.1.push_str(s);
        }
        new_stmt = s.ends_with(';');
    }
    flush(&mut raw, &mut out);
    out
}

fn query_item(line: usize, q: &str) -> Option<Item> {
    let sql = q.trim().trim_end_matches(';').trim_end();
    if sql.is_empty() {
        return None;
    }
    let lower = sql.to_ascii_lowercase();
    let first = lower
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()?;
    let rest = sql[first.len()..].trim();
    match first {
        "connect" => {
            let inner = rest.trim_start_matches('(').trim_end_matches(')');
            let f: Vec<String> = inner.split(',').map(|x| x.trim().to_string()).collect();
            let get = |i: usize| f.get(i).cloned().unwrap_or_default();
            Some(Item::Connect {
                line,
                name: get(0),
                user: get(2),
                password: get(3),
                db: get(4),
            })
        }
        "connection" => Some(Item::Connection {
            line,
            name: rest.to_string(),
        }),
        "disconnect" => Some(Item::Disconnect {
            line,
            name: rest.to_string(),
        }),
        _ => Some(Item::Query {
            line,
            sql: sql.to_string(),
        }),
    }
}

/// What the hybrid run does with a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// Read-only: runs on A and B and is compared. The tag is `select`, `show` or `explain`.
    Read(&'static str),
    /// Session state (session SET, USE, PREPARE, DEALLOCATE, SELECT ... INTO @var): runs on A
    /// and is mirrored to B; not compared.
    Session,
    /// Everything else (DML, DDL, transactions, global settings, locking reads, ADMIN, ...):
    /// A only.
    Write,
}

fn has_seq(w: &[Word], seq: &[&str]) -> bool {
    w.windows(seq.len())
        .any(|win| win.iter().zip(seq).all(|(a, b)| a.text == *b))
}

/// Classifies a statement. `prepared_read(name)` tells whether a prepared statement of that
/// name is a read (for EXECUTE).
pub fn classify(sql: &str, prepared_read: impl Fn(&str) -> Option<bool>) -> Class {
    let w = words(sql);
    let Some(first) = w.first() else {
        return Class::Write;
    };
    let second = w.get(1).map_or("", |x| x.text.as_str());
    match first.text.as_str() {
        "SELECT" | "TABLE" | "VALUES" | "WITH" => select_class(&w),
        "SHOW" => Class::Read("show"),
        "EXPLAIN" | "DESC" | "DESCRIBE" => {
            // EXPLAIN ANALYZE executes the statement: a read only if that statement is one.
            if let Some(a) = w.iter().skip(1).take(3).position(|x| x.text == "ANALYZE") {
                let stmt = w[a + 2..].iter().position(|x| {
                    matches!(
                        x.text.as_str(),
                        "SELECT"
                            | "TABLE"
                            | "VALUES"
                            | "WITH"
                            | "INSERT"
                            | "UPDATE"
                            | "DELETE"
                            | "REPLACE"
                            | "LOAD"
                            | "IMPORT"
                    )
                });
                return match stmt.map(|p| select_class(&w[a + 2 + p..])) {
                    Some(Class::Read(_)) => Class::Read("explain"),
                    _ => Class::Write,
                };
            }
            Class::Read("explain")
        }
        "USE" => Class::Session,
        "PREPARE" => Class::Session,
        "DEALLOCATE" => Class::Session,
        "DROP" if second == "PREPARE" => Class::Session,
        "EXECUTE" => {
            let name = sql.split_whitespace().nth(1).unwrap_or("");
            match prepared_read(&name.to_ascii_lowercase()) {
                Some(true) => Class::Read("select"),
                _ => Class::Write,
            }
        }
        "SET" => {
            if matches!(
                second,
                "PASSWORD" | "DEFAULT" | "ROLE" | "RESOURCE" | "CONFIG" | "BINDING"
            ) {
                return Class::Write;
            }
            if w.iter().any(|x| {
                matches!(
                    x.text.as_str(),
                    "GLOBAL" | "PERSIST" | "PERSIST_ONLY" | "INSTANCE"
                ) || x.text.starts_with("@@GLOBAL.")
                    || x.text.starts_with("@@PERSIST")
                    || x.text.starts_with("@@INSTANCE.")
            }) {
                return Class::Write;
            }
            Class::Session
        }
        _ => Class::Write,
    }
}

/// `w` starts at the statement keyword.
fn select_class(w: &[Word]) -> Class {
    if !matches!(
        w.first().map(|x| x.text.as_str()),
        Some("SELECT" | "TABLE" | "VALUES" | "WITH")
    ) {
        return Class::Write;
    }
    for bad in [
        &["FOR", "UPDATE"][..],
        &["FOR", "SHARE"],
        &["LOCK", "IN", "SHARE", "MODE"],
        &["INTO", "OUTFILE"],
        &["INTO", "DUMPFILE"],
    ] {
        if has_seq(w, bad) {
            return Class::Write;
        }
    }
    // WITH ... UPDATE/DELETE/INSERT (function names such as INSERT() are followed by a
    // parenthesis, which keywords are not, but a CTE body is in parentheses anyway).
    if w[0].text == "WITH"
        && w.iter().any(|x| {
            x.depth == 0 && matches!(x.text.as_str(), "UPDATE" | "DELETE" | "INSERT" | "REPLACE")
        })
    {
        return Class::Write;
    }
    if w.windows(2)
        .any(|p| p[0].text == "INTO" && p[1].text.starts_with('@'))
    {
        return Class::Session;
    }
    Class::Read("select")
}

/// For `PREPARE name FROM 'sql'`: the name (lower-cased) and the statement text, if literal.
pub fn prepare_parts(sql: &str) -> Option<(String, Option<String>)> {
    let mut it = sql.split_whitespace();
    if !it.next()?.eq_ignore_ascii_case("prepare") {
        return None;
    }
    let name = it.next()?.trim_matches('`').to_ascii_lowercase();
    let from = sql.to_ascii_lowercase().find(" from")? + 5;
    let rest = sql[from..].trim();
    let q = rest.chars().next()?;
    if q != '\'' && q != '"' {
        return Some((name, None));
    }
    let body = &rest[1..];
    let mut text = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                text.push(n);
            }
        } else if c == q {
            if chars.peek() == Some(&q) {
                chars.next();
                text.push(q);
            } else {
                return Some((name, Some(text)));
            }
        } else {
            text.push(c);
        }
    }
    Some((name, None))
}

/// Transaction effect of a statement that succeeded on A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnEffect {
    None,
    Begin,
    /// COMMIT, ROLLBACK, and statements with an implicit commit (DDL, ...).
    End,
    AutocommitOff,
    AutocommitOn,
}

pub fn txn_effect(sql: &str) -> TxnEffect {
    let w = words(sql);
    let Some(first) = w.first() else {
        return TxnEffect::None;
    };
    let second = w.get(1).map_or("", |x| x.text.as_str());
    match first.text.as_str() {
        "BEGIN" => TxnEffect::Begin,
        "START" if second == "TRANSACTION" => TxnEffect::Begin,
        // ROLLBACK TO [SAVEPOINT] s keeps the transaction open.
        "ROLLBACK" if second == "TO" => TxnEffect::None,
        "COMMIT" | "ROLLBACK" => TxnEffect::End,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "LOCK"
        | "UNLOCK" | "FLUSH" => {
            // CREATE/DROP TEMPORARY TABLE do not commit.
            if second == "TEMPORARY" {
                TxnEffect::None
            } else {
                TxnEffect::End
            }
        }
        "SET" => {
            let i = w.iter().position(|x| {
                let t = x
                    .text
                    .trim_start_matches("@@SESSION.")
                    .trim_start_matches("@@");
                t == "AUTOCOMMIT"
            });
            match i.and_then(|i| w.get(i + 1)).map(|x| x.text.as_str()) {
                Some("0" | "OFF" | "FALSE") => TxnEffect::AutocommitOff,
                Some("1" | "ON" | "TRUE") => TxnEffect::AutocommitOn,
                _ => TxnEffect::None,
            }
        }
        _ => TxnEffect::None,
    }
}

/// Table names created by `CREATE TEMPORARY TABLE` (local temporary tables live in A's session
/// only; a read of such a name on B would see a different table or none).
pub fn local_temporary_table(sql: &str) -> Option<String> {
    let w = words(sql);
    let t: Vec<&str> = w.iter().map(|x| x.text.as_str()).collect();
    if t.len() < 4 || t[0] != "CREATE" || t[1] != "TEMPORARY" || t[2] != "TABLE" {
        return None;
    }
    let mut i = 3;
    if t.get(3..6) == Some(&["IF", "NOT", "EXISTS"][..]) {
        i = 6;
    }
    let name = w.get(i)?.text.rsplit('.').next()?.to_ascii_lowercase();
    Some(name)
}

/// Statements that would break the shared cluster for the rest of the run, with the reason.
/// They are not run. `SET GLOBAL tidb_enable_ddl = OFF` is expected to fail on a single-server
/// test cluster (error 8246), but tidb-rs registers as a TiDB server, so Go TiDB accepts it and
/// the cluster has no DDL owner any more.
pub fn cluster_hazard(sql: &str) -> Option<&'static str> {
    let w = words(sql);
    if w.first().is_none_or(|x| x.text != "SET") {
        return None;
    }
    let i = w.iter().position(|x| {
        x.text
            .trim_start_matches("@@GLOBAL.")
            .trim_start_matches("@@")
            == "TIDB_ENABLE_DDL"
    })?;
    match w.get(i + 1).map(|x| x.text.as_str()) {
        Some("0" | "OFF" | "FALSE") => {
            Some("would disable DDL on the shared cluster (tidb-rs counts as a TiDB server)")
        }
        _ => None,
    }
}

/// Reads that must not be sent to B because they break the shared cluster. A LIMIT count of
/// 2^63 or more from tidb-rs makes TiKV v8.5.8 panic (`assertion failed: scan_rows > 0` in its
/// scan executor) and the whole cluster stops; Go TiDB pushes the same Limit without harm.
pub fn b_hazard(sql: &str) -> Option<&'static str> {
    let w = words(sql);
    for (i, x) in w.iter().enumerate() {
        if x.text != "LIMIT" {
            continue;
        }
        for y in w.iter().skip(i + 1).take(3) {
            if y.text.parse::<u128>().is_ok_and(|n| n >= 1 << 63) {
                return Some("not sent: a LIMIT >= 2^63 from tidb-rs crashes TiKV");
            }
        }
    }
    None
}

/// Whether a statement's answer depends on the session's history, which differs between A and
/// B because writes run on A only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum History {
    None,
    /// Depends on the previous statement only (SHOW WARNINGS, @@warning_count, ROW_COUNT(),
    /// FOUND_ROWS(), @@last_plan_from_cache, ...): comparable if that one also ran on B.
    Previous,
    /// Depends on earlier statements of the session (LAST_INSERT_ID(), @@tidb_last_txn_info,
    /// @@tidb_last_query_info): never comparable.
    Session,
}

pub fn history(sql: &str) -> History {
    let w = words(sql);
    let t = |i: usize| w.get(i).map_or("", |x| x.text.as_str());
    if t(0) == "SHOW"
        && (matches!(t(1), "WARNINGS" | "ERRORS")
            || (t(1) == "COUNT" && matches!(t(2), "WARNINGS" | "ERRORS")))
    {
        return History::Previous;
    }
    let mut h = History::None;
    for x in &w {
        let v = x
            .text
            .trim_start_matches("@@SESSION.")
            .trim_start_matches("@@");
        match v {
            "LAST_INSERT_ID" | "TIDB_LAST_TXN_INFO" | "TIDB_LAST_QUERY_INFO" | "IDENTITY" => {
                return History::Session;
            }
            "WARNING_COUNT"
            | "ERROR_COUNT"
            | "ROW_COUNT"
            | "FOUND_ROWS"
            | "LAST_PLAN_FROM_CACHE"
            | "LAST_PLAN_FROM_BINDING"
            | "LAST_SQL_USE_ALLOC"
            | "TIDB_LAST_PLAN_REPLAYER_TOKEN" => h = History::Previous,
            _ => {}
        }
    }
    h
}

/// Whether `sql` mentions any of `names` (lower-case, unqualified) as a word.
pub fn mentions(sql: &str, names: &std::collections::BTreeSet<String>) -> bool {
    if names.is_empty() {
        return false;
    }
    words(sql).iter().any(|x| {
        x.text
            .split('.')
            .any(|p| names.contains(&p.to_ascii_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_mysql_tester() {
        let src = "# comment\n--error 1146\nselect * from\n  t;\n-- plain comment\n--sorted_result\n\
                   select 1; \nconnect (conn1, localhost, root,, db1);\nconnection conn1;\n\
                   select 2\n--enable_warnings;\nselect 3;\ndisconnect conn1;\n-- error 1142\n";
        let items = parse(src);
        assert_eq!(
            items,
            vec![
                Item::Directive {
                    line: 2,
                    cmd: "error".into(),
                    arg: "1146".into()
                },
                Item::Query {
                    line: 3,
                    sql: "select * from\nt".into()
                },
                Item::Directive {
                    line: 6,
                    cmd: "sorted_result".into(),
                    arg: String::new()
                },
                Item::Query {
                    line: 7,
                    sql: "select 1".into()
                },
                Item::Connect {
                    line: 8,
                    name: "conn1".into(),
                    user: "root".into(),
                    password: String::new(),
                    db: "db1".into()
                },
                Item::Connection {
                    line: 9,
                    name: "conn1".into()
                },
                Item::Query {
                    line: 10,
                    sql: "select 2".into()
                },
                Item::Directive {
                    line: 11,
                    cmd: "enable_warnings".into(),
                    arg: String::new()
                },
                Item::Query {
                    line: 12,
                    sql: "select 3".into()
                },
                Item::Disconnect {
                    line: 13,
                    name: "conn1".into()
                },
                Item::Directive {
                    line: 14,
                    cmd: "error".into(),
                    arg: "1142".into()
                },
            ]
        );
    }

    #[test]
    fn classifies() {
        let none = |_: &str| None;
        assert_eq!(classify("select * from t", none), Class::Read("select"));
        assert_eq!(
            classify("(select 1) union (select 2)", none),
            Class::Read("select")
        );
        assert_eq!(classify("show create table t", none), Class::Read("show"));
        assert_eq!(
            classify("explain format='brief' select 1", none),
            Class::Read("explain")
        );
        assert_eq!(classify("desc t", none), Class::Read("explain"));
        assert_eq!(
            classify("explain analyze select 1", none),
            Class::Read("explain")
        );
        assert_eq!(
            classify("explain analyze insert into t values (1)", none),
            Class::Write
        );
        assert_eq!(
            classify("explain insert into t values (1)", none),
            Class::Read("explain")
        );
        assert_eq!(classify("select * from t for update", none), Class::Write);
        assert_eq!(classify("select 1 into @a", none), Class::Session);
        assert_eq!(
            classify("with x as (select 1) delete from t", none),
            Class::Write
        );
        assert_eq!(classify("set @@tidb_x = 1", none), Class::Session);
        assert_eq!(classify("set global tidb_x = 1", none), Class::Write);
        assert_eq!(classify("use test", none), Class::Session);
        assert_eq!(classify("prepare s from 'select 1'", none), Class::Session);
        assert_eq!(
            classify("execute s using @a", |_| Some(true)),
            Class::Read("select")
        );
        assert_eq!(classify("execute s", |_| Some(false)), Class::Write);
        assert_eq!(classify("insert into t values (1)", none), Class::Write);
        assert_eq!(classify("admin show ddl jobs", none), Class::Write);
        assert_eq!(
            classify(
                "select insert('abc', 1, 1, 'x'), replace(a, 'b', 'c') from t",
                none
            ),
            Class::Read("select")
        );
        assert_eq!(
            classify("explain analyze format='brief' select 1", none),
            Class::Read("explain")
        );
    }

    #[test]
    fn prepare_and_txn() {
        assert_eq!(
            prepare_parts("prepare stmt from 'select ? from t where a=''x'''"),
            Some(("stmt".into(), Some("select ? from t where a='x'".into())))
        );
        assert_eq!(prepare_parts("PREPARE s FROM @q"), Some(("s".into(), None)));
        assert_eq!(txn_effect("begin"), TxnEffect::Begin);
        assert_eq!(txn_effect("start transaction read only"), TxnEffect::Begin);
        assert_eq!(txn_effect("commit"), TxnEffect::End);
        assert_eq!(txn_effect("rollback to s1"), TxnEffect::None);
        // TiDB's ANALYZE does not commit the open transaction.
        assert_eq!(txn_effect("analyze table t"), TxnEffect::None);
        assert_eq!(txn_effect("rollback to savepoint s1"), TxnEffect::None);
        assert_eq!(txn_effect("create table t (a int)"), TxnEffect::End);
        assert_eq!(
            txn_effect("create temporary table t (a int)"),
            TxnEffect::None
        );
        assert_eq!(txn_effect("set autocommit = 0"), TxnEffect::AutocommitOff);
        assert_eq!(
            txn_effect("set @@session.autocommit=ON"),
            TxnEffect::AutocommitOn
        );
        assert_eq!(
            local_temporary_table("create temporary table if not exists tmp1 (a int)"),
            Some("tmp1".into())
        );
        assert_eq!(
            local_temporary_table("create global temporary table g (a int)"),
            None
        );
        assert!(cluster_hazard("set @@global.tidb_enable_ddl=false").is_some());
        assert!(cluster_hazard("set global tidb_enable_ddl = on").is_none());
        assert!(b_hazard("select * from t limit 18446744073709551615 offset 1").is_some());
        assert!(b_hazard("select * from t limit 1, 9223372036854775808").is_some());
        assert!(b_hazard("select * from t limit 9223372036854775807").is_none());
        assert_eq!(history("show warnings"), History::Previous);
        assert_eq!(history("show count(*) errors"), History::Previous);
        assert_eq!(
            history("select @@warning_count, row_count()"),
            History::Previous
        );
        assert_eq!(history("select last_insert_id()"), History::Session);
        assert_eq!(history("select a from t"), History::None);
        let names = ["tmp1".to_string()].into_iter().collect();
        assert!(mentions("select * from test.tmp1", &names));
        assert!(!mentions("select 'tmp1' from t", &names));
    }
}
