//! Comparing what A and B returned.

use std::collections::BTreeMap;

use crate::exec::{ColumnMeta, Outcome, PrepareMeta};

/// ER_NOT_SUPPORTED_YET: tidb-rs's "not implemented" answer.
pub const ER_NOT_SUPPORTED_YET: u16 = 1235;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// Both failed with the same error code.
    BothError,
    /// B answered 1235 (not supported yet).
    Unsupported,
    Mismatch(Vec<String>),
}

impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::BothError => "both_error",
            Verdict::Unsupported => "unsupported",
            Verdict::Mismatch(_) => "mismatch",
        }
    }
}

/// Compares two outcomes. `ordered`: rows must come in the same order; otherwise they are
/// compared as multisets. Warning counts are not compared.
pub fn compare(a: &Outcome, b: &Outcome, ordered: bool) -> Verdict {
    if let Outcome::Error { code, .. } = b
        && *code == ER_NOT_SUPPORTED_YET
    {
        return Verdict::Unsupported;
    }
    let diffs = match (a, b) {
        (Outcome::Error { code: ca, .. }, Outcome::Error { code: cb, .. }) if ca == cb => {
            return Verdict::BothError;
        }
        (
            Outcome::Rows {
                columns: ca,
                rows: ra,
                ..
            },
            Outcome::Rows {
                columns: cb,
                rows: rb,
                ..
            },
        ) => {
            let mut d = column_diffs("", ca, cb);
            d.extend(row_diffs(ra, rb, ordered));
            d
        }
        (Outcome::Ok { .. }, Outcome::Ok { .. }) => Vec::new(),
        _ => vec![format!("A {} vs B {}", brief(a), brief(b))],
    };
    if diffs.is_empty() {
        Verdict::Match
    } else {
        Verdict::Mismatch(diffs)
    }
}

/// Differences between the COM_STMT_PREPARE answers (parameter count and column definitions).
pub fn prepare_diffs(a: &PrepareMeta, b: &PrepareMeta) -> Vec<String> {
    let mut d = Vec::new();
    if a.param_count != b.param_count {
        d.push(format!(
            "prepare: param count A={} B={}",
            a.param_count, b.param_count
        ));
    }
    d.extend(column_diffs("prepare: ", &a.columns, &b.columns));
    d
}

fn brief(o: &Outcome) -> String {
    match o {
        Outcome::Rows { columns, rows, .. } => {
            format!("result set ({} cols, {} rows)", columns.len(), rows.len())
        }
        Outcome::Ok { affected_rows, .. } => format!("OK (affected {affected_rows})"),
        Outcome::Error { code, message, .. } => format!("error {code} ({message})"),
        Outcome::Client(m) => format!("client error ({m})"),
    }
}

fn column_diffs(prefix: &str, a: &[ColumnMeta], b: &[ColumnMeta]) -> Vec<String> {
    let mut d = Vec::new();
    if a.len() != b.len() {
        d.push(format!("{prefix}column count A={} B={}", a.len(), b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let mut field = |name: &str, va: String, vb: String| {
            if va != vb {
                d.push(format!("{prefix}col[{i}].{name}: A={va} B={vb}"));
            }
        };
        field("name", format!("{:?}", x.name), format!("{:?}", y.name));
        field(
            "org_name",
            format!("{:?}", x.org_name),
            format!("{:?}", y.org_name),
        );
        field(
            "schema",
            format!("{:?}", x.schema),
            format!("{:?}", y.schema),
        );
        field("table", format!("{:?}", x.table), format!("{:?}", y.table));
        field(
            "org_table",
            format!("{:?}", x.org_table),
            format!("{:?}", y.org_table),
        );
        field("type", type_name(x.column_type), type_name(y.column_type));
        field("length", x.length.to_string(), y.length.to_string());
        field("decimals", x.decimals.to_string(), y.decimals.to_string());
        field("flags", flag_names(x.flags), flag_names(y.flags));
        field("charset", charset_name(x.charset), charset_name(y.charset));
    }
    d
}

/// At most this many differing rows are listed per side.
const MAX_ROW_DIFFS: usize = 5;

fn row_diffs(a: &[Vec<String>], b: &[Vec<String>], ordered: bool) -> Vec<String> {
    let mut d = Vec::new();
    if a.len() != b.len() {
        d.push(format!("row count A={} B={}", a.len(), b.len()));
    }
    if ordered {
        if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
            d.push(format!(
                "first differing row #{i}: A={} B={}",
                a[i].join(", "),
                b[i].join(", ")
            ));
            // Tell order-only differences apart (e.g. ties in a non-unique ORDER BY key).
            if multiset_diff(a, b).0.is_empty() && multiset_diff(b, a).0.is_empty() {
                d.push("same rows in a different order".into());
            }
        }
        return d;
    }
    for (side, (only, n)) in [("A", multiset_diff(a, b)), ("B", multiset_diff(b, a))] {
        if n > 0 {
            d.push(format!(
                "{n} row(s) only in {side}, e.g. {}",
                only.iter()
                    .map(|r| format!("({})", r.join(", ")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }
    d
}

/// Rows of `a` not matched in `b` (multiset difference): up to [`MAX_ROW_DIFFS`] examples and
/// the total count.
fn multiset_diff<'r>(a: &'r [Vec<String>], b: &[Vec<String>]) -> (Vec<&'r Vec<String>>, usize) {
    let mut counts: BTreeMap<&Vec<String>, isize> = BTreeMap::new();
    for r in b {
        *counts.entry(r).or_default() += 1;
    }
    let mut examples = Vec::new();
    let mut n = 0;
    for r in a {
        let c = counts.entry(r).or_default();
        if *c > 0 {
            *c -= 1;
        } else {
            n += 1;
            if examples.len() < MAX_ROW_DIFFS {
                examples.push(r);
            }
        }
    }
    (examples, n)
}

pub fn type_name(t: u8) -> String {
    let name = match t {
        0 => "DECIMAL",
        1 => "TINY",
        2 => "SHORT",
        3 => "LONG",
        4 => "FLOAT",
        5 => "DOUBLE",
        6 => "NULL",
        7 => "TIMESTAMP",
        8 => "LONGLONG",
        9 => "INT24",
        10 => "DATE",
        11 => "TIME",
        12 => "DATETIME",
        13 => "YEAR",
        14 => "NEWDATE",
        15 => "VARCHAR",
        16 => "BIT",
        245 => "JSON",
        246 => "NEWDECIMAL",
        247 => "ENUM",
        248 => "SET",
        249 => "TINY_BLOB",
        250 => "MEDIUM_BLOB",
        251 => "LONG_BLOB",
        252 => "BLOB",
        253 => "VAR_STRING",
        254 => "STRING",
        255 => "GEOMETRY",
        _ => return format!("type#{t}"),
    };
    format!("{name}({t})")
}

pub fn flag_names(f: u16) -> String {
    const NAMES: [&str; 16] = [
        "NOT_NULL",
        "PRI_KEY",
        "UNIQUE_KEY",
        "MULTIPLE_KEY",
        "BLOB",
        "UNSIGNED",
        "ZEROFILL",
        "BINARY",
        "ENUM",
        "AUTO_INCREMENT",
        "TIMESTAMP",
        "SET",
        "NO_DEFAULT_VALUE",
        "ON_UPDATE_NOW",
        "PART_KEY",
        "NUM",
    ];
    let names: Vec<&str> = (0..16)
        .filter(|i| f & (1 << i) != 0)
        .map(|i| NAMES[i])
        .collect();
    format!("{f:#06x}[{}]", names.join("|"))
}

pub fn charset_name(c: u16) -> String {
    let name = match c {
        8 => "latin1_swedish_ci",
        33 => "utf8_general_ci",
        45 => "utf8mb4_general_ci",
        46 => "utf8mb4_bin",
        47 => "latin1_bin",
        63 => "binary",
        83 => "utf8_bin",
        224 => "utf8mb4_unicode_ci",
        255 => "utf8mb4_0900_ai_ci",
        309 => "utf8mb4_0900_bin",
        _ => return c.to_string(),
    };
    format!("{c}({name})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            schema: "sbtest".into(),
            table: "sbtest1".into(),
            org_table: "sbtest1".into(),
            name: name.into(),
            org_name: name.into(),
            column_type: 254,
            length: 480,
            decimals: 0,
            flags: 1,
            charset: 46,
        }
    }

    fn rows(v: &[&str]) -> Outcome {
        Outcome::Rows {
            columns: vec![col("c")],
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
    fn verdicts() {
        assert_eq!(
            compare(&rows(&["a", "b"]), &rows(&["b", "a"]), false),
            Verdict::Match
        );
        let Verdict::Mismatch(d) = compare(&rows(&["a", "b"]), &rows(&["b", "a"]), true) else {
            panic!()
        };
        assert!(
            d.contains(&"same rows in a different order".to_string()),
            "{d:?}"
        );
        let Verdict::Mismatch(d) = compare(&rows(&["a", "a"]), &rows(&["a", "b"]), false) else {
            panic!()
        };
        assert_eq!(
            d,
            vec![
                "1 row(s) only in A, e.g. (a)".to_string(),
                "1 row(s) only in B, e.g. (b)".to_string()
            ]
        );
        assert_eq!(compare(&err(1146), &err(1146), false), Verdict::BothError);
        assert!(matches!(
            compare(&err(1146), &err(1105), false),
            Verdict::Mismatch(_)
        ));
        assert_eq!(
            compare(&rows(&["a"]), &err(1235), false),
            Verdict::Unsupported
        );
        assert_eq!(compare(&err(1146), &err(1235), false), Verdict::Unsupported);
        assert!(matches!(
            compare(&rows(&[]), &err(1105), false),
            Verdict::Mismatch(_)
        ));
        assert!(matches!(
            compare(&rows(&[]), &Outcome::Client("x".into()), false),
            Verdict::Mismatch(_)
        ));
        let ok = Outcome::Ok {
            affected_rows: 0,
            warnings: 0,
        };
        assert_eq!(compare(&ok, &ok, false), Verdict::Match);
    }

    #[test]
    fn metadata() {
        let mut b = col("c");
        b.flags = 0x1081;
        b.charset = 63;
        let a = Outcome::Rows {
            columns: vec![col("c")],
            rows: vec![],
            warnings: 0,
        };
        let bo = Outcome::Rows {
            columns: vec![b],
            rows: vec![],
            warnings: 0,
        };
        let Verdict::Mismatch(d) = compare(&a, &bo, false) else {
            panic!()
        };
        assert_eq!(
            d,
            vec![
                "col[0].flags: A=0x0001[NOT_NULL] B=0x1081[NOT_NULL|BINARY|NO_DEFAULT_VALUE]"
                    .to_string(),
                "col[0].charset: A=46(utf8mb4_bin) B=63(binary)".to_string(),
            ]
        );
    }
}
