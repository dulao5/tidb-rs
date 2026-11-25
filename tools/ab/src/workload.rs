//! Workloads: the cases the harness runs on both servers.

use crate::rng::Rng;
use crate::sql::{self, Kind, Section};

/// A bound parameter (prepared statements) / literal (text protocol).
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Int(i64),
    UInt(u64),
    Str(String),
}

impl Param {
    pub fn sql_literal(&self) -> String {
        match self {
            Param::Int(v) => v.to_string(),
            Param::UInt(v) => v.to_string(),
            Param::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
        }
    }

    pub fn to_value(&self) -> mysql_async::Value {
        match self {
            Param::Int(v) => mysql_async::Value::Int(*v),
            Param::UInt(v) => mysql_async::Value::UInt(*v),
            Param::Str(s) => mysql_async::Value::Bytes(s.clone().into_bytes()),
        }
    }

    /// Parses the parameter list of an `-- ab:execute` line: comma-separated values, each
    /// bound with its own binary type. `42`, `-7`: signed 64-bit (`MYSQL_TYPE_LONGLONG`);
    /// `18446744073709551615u`: unsigned 64-bit (`MYSQL_TYPE_LONGLONG` with the unsigned flag);
    /// `'abc'`: a string (`MYSQL_TYPE_VAR_STRING`), where `''` is a quote and a backslash
    /// takes the next character as it is. An empty list is an EXECUTE without parameters.
    pub fn parse_list(src: &str) -> Result<Vec<Param>, String> {
        let b = src.as_bytes();
        let mut out = Vec::new();
        let skip_ws = |mut i: usize| {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            i
        };
        let mut i = skip_ws(0);
        if i == b.len() {
            return Ok(out);
        }
        loop {
            let (p, next) = Param::parse_one(src, i)?;
            out.push(p);
            i = skip_ws(next);
            match b.get(i) {
                None => return Ok(out),
                Some(b',') => i = skip_ws(i + 1),
                Some(_) => return Err(format!("expected `,` at {:?}", &src[i..])),
            }
        }
    }

    /// One value of [`Param::parse_list`] starting at byte `i`, and the index after it.
    fn parse_one(src: &str, i: usize) -> Result<(Param, usize), String> {
        let b = src.as_bytes();
        if b.get(i) == Some(&b'\'') {
            let mut s = Vec::new();
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => return Err(format!("unterminated string {:?}", &src[i..])),
                    Some(b'\\') if j + 1 < b.len() => {
                        s.push(b[j + 1]);
                        j += 2;
                    }
                    Some(b'\'') if b.get(j + 1) == Some(&b'\'') => {
                        s.push(b'\'');
                        j += 2;
                    }
                    Some(b'\'') => break,
                    Some(&c) => {
                        s.push(c);
                        j += 1;
                    }
                }
            }
            let s = String::from_utf8(s).map_err(|e| e.to_string())?;
            return Ok((Param::Str(s), j + 1));
        }
        let end = b[i..]
            .iter()
            .position(|&c| c == b',' || c.is_ascii_whitespace())
            .map_or(b.len(), |n| i + n);
        let tok = &src[i..end];
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit());
        let p = if let Some(u) = tok.strip_suffix('u') {
            if !digits(u) {
                return Err(format!("bad unsigned value {tok:?}"));
            }
            Param::UInt(
                u.parse()
                    .map_err(|_| format!("{tok:?} is out of the unsigned 64-bit range"))?,
            )
        } else {
            if !digits(tok.strip_prefix('-').unwrap_or(tok)) {
                return Err(format!(
                    "bad value {tok:?} (an integer, an integer with suffix u, or a quoted string)"
                ));
            }
            Param::Int(tok.parse().map_err(|_| {
                format!("{tok:?} is out of the signed 64-bit range (suffix u for unsigned)")
            })?)
        };
        Ok((p, end))
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Param::Int(v) => serde_json::json!(v),
            Param::UInt(v) => serde_json::json!(v),
            Param::Str(s) => serde_json::json!(s),
        }
    }
}

/// One statement to run on both servers.
#[derive(Debug, Clone)]
pub struct Case {
    /// Summary group (sysbench template name, or the statement's first keyword).
    pub template: String,
    /// SQL with `?` placeholders when `params` is non-empty.
    pub sql: String,
    pub params: Vec<Param>,
    /// mysqltest `--sorted_result` preceded it: rows compare as a multiset (tolerance T2).
    /// Otherwise the comparator classifies the statement from its text.
    pub sorted: bool,
    pub kind: Kind,
    /// Where it came from (file:line), if from a file.
    pub source: Option<String>,
    /// Also run as a server-side prepared statement.
    pub prepared: bool,
    /// Setup statement: runs on A only (session statements on both); not compared.
    pub setup: bool,
    /// Which EXECUTE (1-based) of its statement's `-- ab:execute` lines this is. The first
    /// one prepares the statement afresh on each side (an earlier server-side statement with
    /// the same text on that connection is closed first); the later ones execute that same
    /// statement again with their own parameters (so the plan cache can hit, and the
    /// statement keeps what its first EXECUTE left behind). 0: not from `ab:execute`; an
    /// earlier statement with the same text on the connection is reused (the driver's
    /// statement cache), as the sysbench workload always did.
    pub execute: usize,
}

impl Case {
    /// The statement for the text protocol: placeholders replaced by literals.
    pub fn text_sql(&self) -> String {
        if self.params.is_empty() {
            return self.sql.clone();
        }
        let lits: Vec<String> = self.params.iter().map(Param::sql_literal).collect();
        sql::substitute(&self.sql, &lits)
    }
}

/// The five oltp_read_only statements (sysbench 1.0 oltp_common.lua), `{}` = table number.
pub const SYSBENCH_TEMPLATES: [(&str, &str); 5] = [
    ("point_select", "SELECT c FROM sbtest{} WHERE id=?"),
    (
        "simple_range",
        "SELECT c FROM sbtest{} WHERE id BETWEEN ? AND ?",
    ),
    (
        "sum_range",
        "SELECT SUM(k) FROM sbtest{} WHERE id BETWEEN ? AND ?",
    ),
    (
        "order_range",
        "SELECT c FROM sbtest{} WHERE id BETWEEN ? AND ? ORDER BY c",
    ),
    (
        "distinct_range",
        "SELECT DISTINCT c FROM sbtest{} WHERE id BETWEEN ? AND ? ORDER BY c",
    ),
];

#[derive(Debug, Clone)]
pub struct SysbenchSpec {
    pub tables: u32,
    pub table_size: i64,
    pub range_size: i64,
    /// Random cases per template (edge cases come on top).
    pub per_template: usize,
    pub edge_cases: bool,
    /// Region boundaries (row handles) per table, index 0 = sbtest1.
    pub boundaries: Vec<Vec<i64>>,
}

/// Point ids that are interesting regardless of the data.
fn edge_points(size: i64) -> Vec<Param> {
    vec![
        Param::Int(1),
        Param::Int(size),
        Param::Int(size / 2),
        Param::Int(0),
        Param::Int(size + 1),
        Param::Int(-1),
        Param::Int(i64::MIN),
        Param::Int(i64::MAX),
        Param::UInt(u64::MAX),
        Param::UInt(1 << 63),
        // A string bound to an integer column (sysbench binds ints; drivers often send strings).
        Param::Str((size / 3).to_string()),
        Param::Str("abc".into()),
    ]
}

/// Ranges that are interesting regardless of the data, plus ranges around region boundaries.
fn edge_ranges(size: i64, r: i64, boundaries: &[i64]) -> Vec<(i64, i64)> {
    let mid = size / 2;
    let mut v = vec![
        (1, r),                       // first rows
        (mid, mid),                   // single row
        (mid + 10, mid),              // a > b: empty
        (size - r / 2, size + r / 2), // crosses max id
        (size + 1, size + r),         // beyond max id: empty
        (-r, r / 2),                  // negative start
        (-r, -1),                     // all negative: empty
        (0, 0),                       // id 0: empty
        (i64::MIN, i64::MIN + r),     // i64 lower bound
        (i64::MAX - r, i64::MAX),     // i64 upper bound
        (i64::MIN, i64::MAX),         // whole table
        (i64::MAX, i64::MIN),         // a > b at the bounds
    ];
    for &h in boundaries {
        v.push((h - r / 2, h + r / 2)); // straddles the boundary
        v.push((h - r, h - 1)); // ends just before it
        v.push((h, h + r - 1)); // starts exactly on it
        v.push((h - 1, h)); // two rows, one per region
    }
    v
}

/// Random and edge-case instances of the five oltp_read_only templates. Every case runs via
/// the text protocol and as a prepared statement.
pub fn sysbench_cases(spec: &SysbenchSpec, rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    let size = spec.table_size.max(1);
    let r = spec.range_size.max(1);
    let mk = |template: &str, fmt: &str, table: u32, params: Vec<Param>| {
        let sql = fmt.replace("{}", &table.to_string());
        Case {
            template: template.to_string(),
            sorted: false,
            sql,
            params,
            kind: Kind::Read,
            source: None,
            prepared: true,
            setup: false,
            execute: 0,
        }
    };
    for (name, fmt) in SYSBENCH_TEMPLATES {
        let point = !fmt.contains("BETWEEN");
        for _ in 0..spec.per_template {
            let table = rng.range(1, spec.tables.max(1) as i64) as u32;
            // sysbench draws ids from 1..table_size; ranges are [a, a + range_size - 1].
            let a = rng.range(1, size);
            let params = if point {
                vec![Param::Int(a)]
            } else {
                vec![Param::Int(a), Param::Int(a + r - 1)]
            };
            cases.push(mk(name, fmt, table, params));
        }
        if !spec.edge_cases {
            continue;
        }
        for table in 1..=spec.tables.max(1) {
            let boundaries = spec
                .boundaries
                .get(table as usize - 1)
                .map_or(&[][..], Vec::as_slice);
            if point {
                let mut ps = edge_points(size);
                for &h in boundaries {
                    ps.push(Param::Int(h - 1));
                    ps.push(Param::Int(h));
                }
                for p in ps {
                    cases.push(mk(name, fmt, table, vec![p]));
                }
            } else {
                for (lo, hi) in edge_ranges(size, r, boundaries) {
                    cases.push(mk(name, fmt, table, vec![Param::Int(lo), Param::Int(hi)]));
                }
            }
        }
    }
    // A parameter in the select list (VS1 plan §4.7, prepared-statement planning): Go's
    // PREPARE describes `?` with a placeholder type and each EXECUTE with the bound value's,
    // so the PREPARE-vs-EXECUTE metadata check (`execute_metadata_diffs`) sees a column whose
    // definition comes from the parameter.
    for params in [
        vec![Param::Int(7), Param::Int(1)],
        vec![Param::UInt(u64::MAX), Param::Int(1)],
        vec![Param::Str("x".into()), Param::Int(1)],
    ] {
        cases.push(mk(
            "param_in_select_list",
            "SELECT ?, c FROM sbtest{} WHERE id=?",
            1,
            params,
        ));
    }
    cases
}

/// Cases from a workload file (see [`sql::split_statements`]). `extra_setup` statements (from
/// `--setup`) run first, as setup. SELECT-like statements also run as prepared statements
/// (without parameters) unless `prepared` is false. A statement with `-- ab:execute` lines
/// becomes one case per line, with that line's parameters (see [`Param::parse_list`]): via the
/// text protocol with the parameters as literals and, unless `prepared` is false, as EXECUTEs
/// of one prepared statement ([`Case::execute`]). Such a statement must be a SELECT-like read
/// in the test section, and every list must have one value per `?`.
pub fn file_cases(
    path: &str,
    src: &str,
    extra_setup: Option<(&str, &str)>,
    prepared: bool,
) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    let mut add = |path: &str, src: &str, force_setup: bool| -> Result<(), String> {
        for st in sql::split_statements(src).map_err(|e| format!("{path}: {e}"))? {
            let kind = sql::classify(&st.sql);
            let first = sql::words(&st.sql)
                .first()
                .map_or_else(|| "?".to_string(), |w| w.text.to_ascii_lowercase());
            let setup = force_setup || st.section == Section::Setup;
            let select_like = kind == Kind::Read
                && matches!(first.as_str(), "select" | "with" | "table" | "values");
            let template = st.template.clone().unwrap_or(first);
            let source = format!("{path}:{}", st.line);
            let case = |params: Vec<Param>, prepared: bool, execute: usize| Case {
                template: template.clone(),
                sorted: st.sorted,
                sql: st.sql.clone(),
                params,
                kind: kind.clone(),
                source: Some(source.clone()),
                prepared,
                setup,
                execute,
            };
            if st.executes.is_empty() {
                cases.push(case(Vec::new(), prepared && select_like, 0));
                continue;
            }
            if setup || !select_like {
                return Err(format!(
                    "{source}: ab:execute needs a SELECT-like read in the test section"
                ));
            }
            let markers = sql::placeholders(&st.sql);
            for (n, (line, list)) in st.executes.iter().enumerate() {
                let params = Param::parse_list(list).map_err(|e| format!("{path}:{line}: {e}"))?;
                if params.len() != markers {
                    return Err(format!(
                        "{path}:{line}: {} parameter(s) for the {markers} `?` of line {}",
                        params.len(),
                        st.line
                    ));
                }
                cases.push(case(params, prepared, n + 1));
            }
        }
        Ok(())
    };
    if let Some((p, s)) = extra_setup {
        add(p, s, true)?;
    }
    add(path, src, false)?;
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SysbenchSpec {
        SysbenchSpec {
            tables: 2,
            table_size: 1000,
            range_size: 100,
            per_template: 10,
            edge_cases: true,
            boundaries: vec![vec![500], vec![]],
        }
    }

    #[test]
    fn sysbench_deterministic() {
        let a = sysbench_cases(&spec(), &mut Rng::new(7));
        let b = sysbench_cases(&spec(), &mut Rng::new(7));
        let c = sysbench_cases(&spec(), &mut Rng::new(8));
        let key = |v: &[Case]| v.iter().map(Case::text_sql).collect::<Vec<_>>();
        assert_eq!(key(&a), key(&b));
        assert_ne!(key(&a), key(&c));
        assert!(a.iter().all(|c| c.prepared && c.kind == Kind::Read));
        assert!(
            a.iter()
                .any(|c| c.text_sql() == "SELECT c FROM sbtest1 WHERE id=500")
        );
        assert!(
            a.iter()
                .any(|c| c.text_sql() == "SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN 450 AND 550")
        );
        assert!(a.iter().any(|c| c.text_sql()
            == "SELECT c FROM sbtest2 WHERE id BETWEEN -9223372036854775808 AND 9223372036854775807 ORDER BY c"));
        assert!(a.iter().all(|c| !c.sorted));
    }

    #[test]
    fn file_workload() {
        let src = "-- ab:setup\ncreate table t (a int);\ninsert into t values (1);\n-- ab:test\n\
                   use test;\nselect a from t order by a;\nshow tables;\ndelete from t;\n";
        let cases = file_cases("w.sql", src, Some(("s.sql", "set @x = 1")), true).unwrap();
        let summary: Vec<_> = cases
            .iter()
            .map(|c| (c.template.as_str(), c.setup, c.prepared, c.sorted))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("set", true, false, false),
                ("create", true, false, false),
                ("insert", true, false, false),
                ("use", false, false, false),
                ("select", false, true, false),
                ("show", false, false, false),
                ("delete", false, false, false),
            ]
        );
        assert!(matches!(cases[6].kind, Kind::Other(_)));
        assert_eq!(cases[4].source.as_deref(), Some("w.sql:6"));
    }

    #[test]
    fn execute_lines_become_cases_of_one_statement() {
        let src = "-- ab:template two_params\n\
                   -- ab:execute 1, 10\n\
                   -- ab:execute 18446744073709551615u, 'it''s'\n\
                   SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ?;\n\
                   -- ab:template\n\
                   SELECT 1;\n";
        let cases = file_cases("p.sql", src, None, true).unwrap();
        let got: Vec<_> = cases
            .iter()
            .map(|c| {
                (
                    c.template.as_str(),
                    c.source.as_deref().unwrap(),
                    c.execute,
                    c.prepared,
                    c.text_sql(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "two_params",
                    "p.sql:4",
                    1,
                    true,
                    "SELECT id FROM sbtest1 WHERE id BETWEEN 1 AND 10".to_string()
                ),
                (
                    "two_params",
                    "p.sql:4",
                    2,
                    true,
                    "SELECT id FROM sbtest1 WHERE id BETWEEN 18446744073709551615 AND 'it''s'"
                        .to_string()
                ),
                ("select", "p.sql:6", 0, true, "SELECT 1".to_string()),
            ]
        );
        assert_eq!(
            cases[1].params,
            vec![Param::UInt(u64::MAX), Param::Str("it's".into())]
        );
        assert_eq!(
            cases[1]
                .params
                .iter()
                .map(Param::to_value)
                .collect::<Vec<_>>(),
            vec![
                mysql_async::Value::UInt(u64::MAX),
                mysql_async::Value::Bytes(b"it's".to_vec())
            ]
        );
        // --no-prepared: the text protocol only, still once per parameter list.
        let text_only = file_cases("p.sql", src, None, false).unwrap();
        assert_eq!(text_only.len(), 3);
        assert!(text_only.iter().all(|c| !c.prepared));
    }

    #[test]
    fn bad_execute_lines_are_errors() {
        for (src, err) in [
            (
                "-- ab:execute 1\nSELECT ?, ?;",
                "p.sql:1: 1 parameter(s) for the 2 `?` of line 2",
            ),
            (
                "-- ab:execute 1\nSHOW TABLES;",
                "p.sql:2: ab:execute needs a SELECT-like read in the test section",
            ),
            (
                "-- ab:setup\n-- ab:execute 1\nSELECT ?;",
                "p.sql:3: ab:execute needs a SELECT-like read in the test section",
            ),
            (
                "-- ab:execute 1x\nSELECT ?;",
                "p.sql:1: bad value \"1x\" (an integer, an integer with suffix u, or a quoted string)",
            ),
            (
                "SELECT 1;\n-- ab:execute 1\n",
                "p.sql: line 2: ab:execute without a statement after it",
            ),
        ] {
            assert_eq!(
                file_cases("p.sql", src, None, true).unwrap_err(),
                err,
                "{src:?}"
            );
        }
    }

    /// The type of a bound parameter as the binary protocol sends it.
    fn binary_type(p: &Param) -> &'static str {
        match p {
            Param::Int(_) => "signed",
            Param::UInt(_) => "unsigned",
            Param::Str(_) => "string",
        }
    }

    /// The statements of a workload file with `ab:execute` lines, each with its EXECUTEs.
    fn executions(cases: &[Case]) -> Vec<(&str, Vec<&Case>)> {
        let mut out: Vec<(&str, Vec<&Case>)> = Vec::new();
        for c in cases {
            assert!(c.execute > 0, "{:?} has no ab:execute line", c.source);
            if c.execute == 1 {
                out.push((c.source.as_deref().unwrap(), Vec::new()));
            }
            let last = out.last_mut().unwrap();
            assert_eq!(last.1.len() + 1, c.execute);
            last.1.push(c);
        }
        out
    }

    #[test]
    fn shipped_prepared_workload() {
        let src = include_str!("../workloads/prepared.sql");
        let cases = file_cases("prepared.sql", src, None, true).unwrap();
        assert!(
            cases
                .iter()
                .all(|c| c.prepared && c.kind == Kind::Read && !c.setup)
        );
        let stmts = executions(&cases);
        for (src, execs) in &stmts {
            // At least two EXECUTEs, the second with other values (a plan cache hit), and the
            // same binary types on each (another type is prepared_type_change.sql's).
            assert!(execs.len() >= 2, "{src}");
            assert_ne!(execs[0].params, execs[1].params, "{src}");
            let types = |c: &Case| c.params.iter().map(binary_type).collect::<Vec<_>>();
            assert!(execs.iter().all(|c| types(c) == types(execs[0])), "{src}");
        }
        let sqls: Vec<&str> = stmts.iter().map(|(_, e)| e[0].sql.as_str()).collect();
        for want in [
            "SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?",
            "SELECT id, k FROM sbtest1 WHERE id >= ? AND id <= ?",
            "SELECT id, k FROM sbtest1 WHERE k = ? AND id = ?",
            "SELECT id, k FROM sbtest1 WHERE id IN (?, ?, ?)",
            "SELECT id FROM sbtest1 WHERE id BETWEEN ? AND ?",
            "SELECT ?, id FROM sbtest1 WHERE id = ?",
            "SELECT c FROM sbtest1 WHERE id = ? LIMIT ?",
        ] {
            assert!(sqls.contains(&want), "{want}");
        }
        assert!(stmts.iter().any(|(_, e)| e[0].params.len() >= 4));
        let templates: std::collections::BTreeSet<_> =
            cases.iter().map(|c| c.template.as_str()).collect();
        assert_eq!(
            templates.into_iter().collect::<Vec<_>>(),
            vec![
                "prepared_limit",
                "prepared_pk_only",
                "prepared_range",
                "prepared_select_list",
                "prepared_two_columns"
            ]
        );
    }

    #[test]
    fn shipped_type_change_workload() {
        let src = include_str!("../workloads/prepared_type_change.sql");
        let cases = file_cases("prepared_type_change.sql", src, None, true).unwrap();
        // One template, the file's name, so that a ruling can match these by its unit.
        assert!(
            cases
                .iter()
                .all(|c| c.template == "prepared_type_change" && c.prepared)
        );
        let stmts = executions(&cases);
        let mut changes = Vec::new();
        for (src, execs) in &stmts {
            // The first parameter (the selected column) changes its binary type after the
            // first EXECUTE; the others keep theirs.
            assert_eq!(execs.len(), 2, "{src}");
            let (a, b) = (&execs[0].params, &execs[1].params);
            assert_ne!(binary_type(&a[0]), binary_type(&b[0]), "{src}");
            assert!(a[1..].iter().zip(&b[1..]).all(|(x, y)| x == y), "{src}");
            changes.push((binary_type(&a[0]), binary_type(&b[0])));
            assert!(execs[0].sql.starts_with("SELECT ?"), "{src}");
        }
        for want in [
            ("signed", "unsigned"),
            ("unsigned", "signed"),
            ("signed", "string"),
            ("string", "signed"),
        ] {
            assert!(changes.contains(&want), "{want:?}");
        }
        assert!(
            cases
                .iter()
                .any(|c| c.params.first() == Some(&Param::UInt(u64::MAX)))
        );
        assert!(
            cases
                .iter()
                .any(|c| c.sql == "SELECT ? FROM DUAL ORDER BY RAND()" && c.execute == 2)
        );
    }

    #[test]
    fn parses_parameter_lists() {
        assert_eq!(Param::parse_list("").unwrap(), vec![]);
        assert_eq!(Param::parse_list("  ").unwrap(), vec![]);
        assert_eq!(
            Param::parse_list(
                " -9223372036854775808 ,9223372036854775807,0u, 18446744073709551615u"
            )
            .unwrap(),
            vec![
                Param::Int(i64::MIN),
                Param::Int(i64::MAX),
                Param::UInt(0),
                Param::UInt(u64::MAX)
            ]
        );
        assert_eq!(
            Param::parse_list("'', 'a, b', 'x''y', 'back\\\\slash', '7'").unwrap(),
            vec![
                Param::Str(String::new()),
                Param::Str("a, b".into()),
                Param::Str("x'y".into()),
                Param::Str("back\\slash".into()),
                Param::Str("7".into())
            ]
        );
        // The text protocol gets the same value back as a literal.
        let s = Param::Str("x'y\\z".into());
        assert_eq!(
            Param::parse_list(&s.sql_literal()).unwrap(),
            vec![s.clone()],
            "{}",
            s.sql_literal()
        );
        for bad in [
            "9223372036854775808",
            "18446744073709551616u",
            "-1u",
            "u",
            "1.5",
            "NULL",
            "1,",
            ",1",
            "1 2",
            "'abc",
        ] {
            assert!(Param::parse_list(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn literals() {
        assert_eq!(Param::Str("a'b\\".into()).sql_literal(), "'a''b\\\\'");
        assert_eq!(Param::UInt(u64::MAX).sql_literal(), "18446744073709551615");
    }
}
