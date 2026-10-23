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
    /// Rows are compared in order (top-level ORDER BY) rather than as multisets.
    pub ordered: bool,
    pub kind: Kind,
    /// Where it came from (file:line), if from a file.
    pub source: Option<String>,
    /// Also run as a server-side prepared statement.
    pub prepared: bool,
    /// Setup statement: runs on A only (session statements on both); not compared.
    pub setup: bool,
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
            ordered: sql::has_top_level_order_by(&sql),
            sql,
            params,
            kind: Kind::Read,
            source: None,
            prepared: true,
            setup: false,
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
    cases
}

/// Cases from a workload file (see [`sql::split_statements`]). `extra_setup` statements (from
/// `--setup`) run first, as setup. SELECT-like statements also run as prepared statements
/// (without parameters) unless `prepared` is false.
pub fn file_cases(
    path: &str,
    src: &str,
    extra_setup: Option<(&str, &str)>,
    prepared: bool,
) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut add = |path: &str, src: &str, force_setup: bool| {
        for st in sql::split_statements(src) {
            let kind = sql::classify(&st.sql);
            let first = sql::words(&st.sql)
                .first()
                .map_or_else(|| "?".to_string(), |w| w.text.to_ascii_lowercase());
            let setup = force_setup || st.section == Section::Setup;
            let ps = prepared
                && kind == Kind::Read
                && matches!(first.as_str(), "select" | "with" | "table" | "values");
            cases.push(Case {
                template: first,
                ordered: !st.sorted && sql::has_top_level_order_by(&st.sql),
                sql: st.sql,
                params: Vec::new(),
                kind,
                source: Some(format!("{path}:{}", st.line)),
                prepared: ps,
                setup,
            });
        }
    };
    if let Some((p, s)) = extra_setup {
        add(p, s, true);
    }
    add(path, src, false);
    cases
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
        let ordered: Vec<_> = a
            .iter()
            .filter(|c| c.ordered)
            .map(|c| &c.template)
            .collect();
        assert!(
            ordered
                .iter()
                .all(|t| *t == "order_range" || *t == "distinct_range")
        );
    }

    #[test]
    fn file_workload() {
        let src = "-- ab:setup\ncreate table t (a int);\ninsert into t values (1);\n-- ab:test\n\
                   use test;\nselect a from t order by a;\nshow tables;\ndelete from t;\n";
        let cases = file_cases("w.sql", src, Some(("s.sql", "set @x = 1")), true);
        let summary: Vec<_> = cases
            .iter()
            .map(|c| (c.template.as_str(), c.setup, c.prepared, c.ordered))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("set", true, false, false),
                ("create", true, false, false),
                ("insert", true, false, false),
                ("use", false, false, false),
                ("select", false, true, true),
                ("show", false, false, false),
                ("delete", false, false, false),
            ]
        );
        assert!(matches!(cases[6].kind, Kind::Other(_)));
        assert_eq!(cases[4].source.as_deref(), Some("w.sql:6"));
    }

    #[test]
    fn literals() {
        assert_eq!(Param::Str("a'b\\".into()).sql_literal(), "'a''b\\\\'");
        assert_eq!(Param::UInt(u64::MAX).sql_literal(), "18446744073709551615");
    }
}
