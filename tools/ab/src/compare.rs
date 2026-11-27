//! THE result comparator: the one implementation of `docs/testing/result-comparison.md`.
//!
//! Every SQL-level test layer (the `ab` workloads, `ab integrationtest`, the weekly strict
//! new-stack round) judges B against A only through [`classify`] and [`compare`] (plus
//! [`compare_status_only`] for tolerance T4). Nothing else may decide that two answers are
//! "the same". The relaxations it applies are the numbered entries of
//! `docs/testing/tolerances.toml` ([`TOLERANCES`]); a new one is a new entry there first.
//!
//! The fixtures in `tools/ab/fixtures/compare/*.case` pin the verdicts, including cases that
//! must be judged different (negative fixtures); `cargo test -p tidb-ab` runs them.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

use crate::exec::{ColumnMeta, Outcome, PrepareMeta};
use crate::sql::{self, Token, TokenKind};

/// ER_NOT_SUPPORTED_YET: tidb-rs's "not implemented" answer.
pub const ER_NOT_SUPPORTED_YET: u16 = 1235;

/// The prototype's fixed "not supported" answers (T1), generated from its sources by
/// `scripts/prototype_not_supported.py`.
const PROTOTYPE_NOT_SUPPORTED: &str = include_str!("../prototype-not-supported.txt");

static PROTOTYPE_ANSWER: LazyLock<Regex> = LazyLock::new(|| {
    let alts: Vec<&str> = PROTOTYPE_NOT_SUPPORTED
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    Regex::new(&format!(
        "^This version of TiDB doesn't yet support '(?:{})'$",
        alts.join("|")
    ))
    .expect("tools/ab/prototype-not-supported.txt: bad pattern")
});

/// T1: whether a 1235 message is tidb-rs's conventional "not implemented yet" answer: a
/// boundary stub's (`blocked-by:<block>`, `tidb_base::blocked`) or one of the prototype's
/// fixed answers. Any other 1235 (a ported Go `ErrNotSupportedYet` branch) is a normal error.
pub fn is_not_implemented_answer(message: &str) -> bool {
    message.contains("blocked-by:") || PROTOTYPE_ANSWER.is_match(message)
}

/// T1: B's answer counts as `unsupported`: B returned a not-implemented 1235 (see
/// [`is_not_implemented_answer`]) and A did not return the very same 1235. When A also answers
/// 1235 with another text, neither side runs the statement: B's not-implemented answer is still
/// `unsupported`, not a mismatch (#510). The same text on both sides compares in full (equal),
/// and a 1235 that is not a not-implemented answer (a ported Go `ErrNotSupportedYet` branch)
/// always compares in full.
pub fn b_not_implemented(a: &Outcome, b: &Outcome) -> bool {
    let Outcome::Error { code, message, .. } = b else {
        return false;
    };
    let a_same = matches!(a, Outcome::Error { code, message: m, .. }
        if *code == ER_NOT_SUPPORTED_YET && m == message);
    *code == ER_NOT_SUPPORTED_YET && !a_same && is_not_implemented_answer(message)
}

/// The numbered tolerances (`docs/testing/tolerances.toml`; a unit test keeps both in step,
/// and every fixture's `tolerance:` must name one of them).
#[cfg(test)]
pub const TOLERANCES: &[(&str, &str)] = &[
    ("T1", "b-not-supported"),
    ("T2", "sorted-result"),
    ("T3", "replace-directives"),
    ("T4", "result-log-off"),
    ("T5", "go-execute-metadata-drift"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Match,
    /// Both failed with the same error (code, SQLSTATE and message).
    BothError,
    /// B answered a not-implemented 1235 (T1: a boundary stub's or the prototype's fixed
    /// answer). Never equal, never coverage.
    Unsupported,
    Mismatch(Vec<String>),
    /// A mismatch an approved ruling of the divergence ledger covers (`ruled:<id>`,
    /// `docs/testing/rulings.toml`): never equal, still counted and reported with its diff.
    /// [`compare`] never returns it; the runners turn a [`Verdict::Mismatch`] into it through
    /// `rulings::Ledger::rule`.
    Ruled {
        id: String,
        diff: Vec<String>,
    },
}

impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::BothError => "both_error",
            Verdict::Unsupported => "unsupported",
            Verdict::Mismatch(_) => "mismatch",
            Verdict::Ruled { .. } => "ruled",
        }
    }
}

/// The order class of a statement (spec §2), from its text and A's column names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtClass {
    /// Rows compare as lists (EXPLAIN, SHOW, ..., or an ORDER BY that does not resolve to
    /// result columns: the stricter reading).
    Exact,
    /// Rows compare as multisets (no top-level ORDER BY).
    Unordered,
    /// Top-level ORDER BY resolved to these result columns (0-based): runs of equal keys
    /// compare as multisets, the runs in order.
    Ordered(Vec<usize>),
    /// A top-level LIMIT cuts the result; the inner class without the cut. With A's result
    /// without the LIMIT, B may return other members of it (spec §2.4).
    Limited(Box<StmtClass>),
}

impl fmt::Display for StmtClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StmtClass::Exact => write!(f, "exact"),
            StmtClass::Unordered => write!(f, "unordered"),
            StmtClass::Ordered(k) => {
                let k: Vec<String> = k.iter().map(|i| (i + 1).to_string()).collect();
                write!(f, "ordered {}", k.join(","))
            }
            StmtClass::Limited(inner) => write!(f, "limited {inner}"),
        }
    }
}

impl StmtClass {
    /// Whether a mismatch under this class may still turn into a match given A's result
    /// without the LIMIT (the runner then fetches it with [`strip_limit`]).
    pub fn wants_full(&self) -> bool {
        matches!(self, StmtClass::Limited(inner) if **inner != StmtClass::Exact)
    }
}

/// First words of statements whose rows always compare as lists.
const EXACT_HEADS: &[&str] = &[
    "EXPLAIN", "DESC", "DESCRIBE", "SHOW", "TRACE", "ADMIN", "HELP", "PLAN",
];

/// Classifies a statement (spec §2). `columns`: A's result columns (ORDER BY items resolve
/// against them). `sorted`: mysqltest `--sorted_result` precedes it (T2).
pub fn classify(sql: &str, columns: &[ColumnMeta], sorted: bool) -> StmtClass {
    let class = classify_text(sql, columns);
    if !sorted {
        return class;
    }
    match class {
        StmtClass::Limited(_) => StmtClass::Limited(Box::new(StmtClass::Unordered)),
        _ => StmtClass::Unordered,
    }
}

fn classify_text(sql: &str, columns: &[ColumnMeta]) -> StmtClass {
    let toks = sql::tokens(sql);
    let Some(first) = toks.first() else {
        return StmtClass::Exact;
    };
    // `(SELECT ... ORDER BY a LIMIT 3)`: the whole statement is one parenthesised query, whose
    // ORDER BY / LIMIT are the statement's own.
    if first.text(sql) == "(" {
        let end = toks.iter().rposition(|t| t.text(sql) != ";").unwrap_or(0);
        let close = toks
            .iter()
            .skip(1)
            .position(|t| t.depth == 0 && t.text(sql) == ")")
            .map(|i| i + 1);
        if close == Some(end) && end > 1 {
            return classify_text(&sql[toks[1].start..toks[end - 1].end], columns);
        }
    }
    let head = first.text(sql).to_ascii_uppercase();
    let query = matches!(head.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES") || head == "(";
    if !query || EXACT_HEADS.contains(&head.as_str()) {
        return StmtClass::Exact;
    }
    let top = |i: usize, kw: &str| toks[i].depth == 0 && toks[i].is_word(sql, kw);
    let order_at = (0..toks.len().saturating_sub(1))
        .rev()
        .find(|&i| top(i, "ORDER") && toks[i + 1].is_word(sql, "BY"));
    let limited = (0..toks.len()).any(|i| top(i, "LIMIT"));
    let inner = match order_at {
        None => StmtClass::Unordered,
        Some(i) => {
            let end = (i + 2..toks.len())
                .find(|&j| {
                    toks[j].depth == 0
                        && ([
                            "LIMIT",
                            "FOR",
                            "LOCK",
                            "INTO",
                            "UNION",
                            "EXCEPT",
                            "INTERSECT",
                        ]
                        .iter()
                        .any(|kw| toks[j].is_word(sql, kw))
                            || toks[j].text(sql) == ";")
                })
                .unwrap_or(toks.len());
            order_keys(sql, &toks[i + 2..end], columns)
        }
    };
    if limited {
        StmtClass::Limited(Box::new(inner))
    } else {
        inner
    }
}

/// The class of the ORDER BY items `items` (tokens after ORDER BY).
fn order_keys(sql: &str, items: &[Token], columns: &[ColumnMeta]) -> StmtClass {
    let mut keys = Vec::new();
    let mut parts: Vec<&[Token]> = Vec::new();
    let mut from = 0;
    for (j, t) in items.iter().enumerate() {
        if t.depth == 0 && t.kind == TokenKind::Punct && t.text(sql) == "," {
            parts.push(&items[from..j]);
            from = j + 1;
        }
    }
    parts.push(&items[from..]);
    for mut item in parts {
        if let Some((last, rest)) = item.split_last()
            && (last.is_word(sql, "ASC") || last.is_word(sql, "DESC"))
        {
            item = rest;
        }
        if item.is_empty() {
            return StmtClass::Exact;
        }
        // MySQL: ORDER BY NULL asks for no order.
        if item.len() == 1 && item[0].is_word(sql, "NULL") {
            if keys.is_empty() {
                return StmtClass::Unordered;
            }
            continue;
        }
        match resolve_item(sql, item, columns) {
            Some(k) => {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            None => return StmtClass::Exact,
        }
    }
    if keys.is_empty() {
        StmtClass::Unordered
    } else {
        StmtClass::Ordered(keys)
    }
}

/// The result column an ORDER BY item names: an ordinal, a (qualified) column name or alias,
/// or an expression whose text is a column's name. `None`: does not resolve (or ambiguous).
fn resolve_item(sql: &str, item: &[Token], columns: &[ColumnMeta]) -> Option<usize> {
    if let [t] = item
        && t.kind == TokenKind::Number
    {
        let n: usize = t.text(sql).parse().ok()?;
        return (1..=columns.len()).contains(&n).then(|| n - 1);
    }
    let unique = |it: &mut dyn Iterator<Item = usize>| {
        let v: Vec<usize> = it.collect();
        (v.len() == 1).then(|| v[0])
    };
    if let Some(name) = ident_chain(sql, item) {
        let (col, qual) = match name.as_slice() {
            [.., q, c] => (c.as_str(), Some(q.as_str())),
            [c] => (c.as_str(), None),
            [] => return None,
        };
        let qual_ok = |t: &str| qual.is_none_or(|q| t.eq_ignore_ascii_case(q));
        let by_name = unique(&mut columns.iter().enumerate().filter_map(|(i, c)| {
            (c.name.eq_ignore_ascii_case(col) && qual_ok(&c.table)).then_some(i)
        }));
        if by_name.is_some() {
            return by_name;
        }
        // `SELECT a AS x ... ORDER BY a`: the column whose origin is `a`.
        if !columns.iter().any(|c| c.name.eq_ignore_ascii_case(col)) {
            return unique(&mut columns.iter().enumerate().filter_map(|(i, c)| {
                (c.org_name.eq_ignore_ascii_case(col)
                    && (qual_ok(&c.table) || qual_ok(&c.org_table)))
                .then_some(i)
            }));
        }
        return None;
    }
    // An expression: Go names an unaliased select item by its text.
    let text = &sql[item[0].start..item[item.len() - 1].end];
    unique(
        &mut columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.name.eq_ignore_ascii_case(text).then_some(i)),
    )
}

/// `a`, `t.a`, `` `t`.`a` ``, `db.t.a` as name parts; `None` if `item` is anything else.
fn ident_chain(sql: &str, item: &[Token]) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut want_name = true;
    for t in item {
        match t.kind {
            TokenKind::Word if want_name => {
                let text = t.text(sql);
                if text.starts_with('@') {
                    return None;
                }
                let mut it = text.split('.').peekable();
                while let Some(p) = it.next() {
                    if !p.is_empty() {
                        parts.push(p.to_string());
                    } else if it.peek().is_some() {
                        return None;
                    }
                }
                want_name = text.ends_with('.');
            }
            TokenKind::Quoted if want_name => {
                parts.push(t.ident(sql));
                want_name = false;
            }
            TokenKind::Punct if !want_name && t.text(sql) == "." => want_name = true,
            _ => return None,
        }
    }
    (!want_name && !parts.is_empty()).then_some(parts)
}

/// The statement without its top-level `LIMIT` clause, to fetch A's full result (spec §2.4).
/// `None` when the clause is not just `LIMIT n [, m] | LIMIT n OFFSET m` at the end (with
/// literal numbers), so the comparison stays strict.
pub fn strip_limit(sql: &str) -> Option<String> {
    if has_side_effects(sql) {
        return None;
    }
    let toks = sql::tokens(sql);
    let at = toks
        .iter()
        .rposition(|t| t.depth == 0 && t.is_word(sql, "LIMIT"))?;
    let rest = &toks[at + 1..];
    let plain = !rest.is_empty()
        && rest.iter().all(|t| {
            t.kind == TokenKind::Number
                || t.is_word(sql, "OFFSET")
                || (t.kind == TokenKind::Punct && matches!(t.text(sql), "," | ";"))
        });
    plain.then(|| sql[..toks[at].start].trim_end().to_string())
}

/// Whether running the statement again on A would change A's state (sequences, user
/// variables, locks, sleeps, `LAST_INSERT_ID(expr)`, locking reads): such a statement is never
/// re-run without its LIMIT, so its comparison stays strict (spec §2.4).
pub fn has_side_effects(sql: &str) -> bool {
    let toks = sql::tokens(sql);
    let text = |i: usize| toks.get(i).map_or("", |t: &Token| t.text(sql));
    (0..toks.len()).any(|i| {
        let t = &toks[i];
        let call = text(i + 1) == "(";
        (t.kind == TokenKind::Word
            && call
            && [
                "NEXTVAL",
                "SETVAL",
                "GET_LOCK",
                "RELEASE_LOCK",
                "RELEASE_ALL_LOCKS",
                "SLEEP",
            ]
            .iter()
            .any(|f| t.is_word(sql, f)))
            || (t.is_word(sql, "LAST_INSERT_ID") && call && text(i + 2) != ")")
            || (text(i) == ":" && text(i + 1) == "=")
            || (t.is_word(sql, "FOR")
                && (toks
                    .get(i + 1)
                    .is_some_and(|n| n.is_word(sql, "UPDATE") || n.is_word(sql, "SHARE"))))
            || (t.is_word(sql, "LOCK") && toks.get(i + 1).is_some_and(|n| n.is_word(sql, "IN")))
    })
}

/// What one side answered, as the comparator sees it.
#[derive(Debug, Clone, Copy)]
pub struct Observed<'a> {
    pub outcome: &'a Outcome,
    /// `SHOW WARNINGS` rows right after the statement; the runner fetches them on both sides
    /// whenever either side reports a warning count above zero (spec §4).
    pub warnings: Option<&'a [Vec<String>]>,
}

impl<'a> Observed<'a> {
    pub fn new(outcome: &'a Outcome, warnings: Option<&'a [Vec<String>]>) -> Self {
        Observed { outcome, warnings }
    }
}

/// The warning count an outcome reports (0 for errors: the error itself is compared).
pub fn warning_count(o: &Outcome) -> u16 {
    match o {
        Outcome::Rows { warnings, .. } | Outcome::Ok { warnings, .. } => *warnings,
        _ => 0,
    }
}

/// Whether the runner must fetch `SHOW WARNINGS` on both sides before comparing.
pub fn needs_warnings(a: &Outcome, b: &Outcome) -> bool {
    warning_count(a) > 0 || warning_count(b) > 0
}

/// Compares B's answer with A's under `class` (spec §3–§5). `a_full`: A's rows without the
/// LIMIT, for a [`StmtClass::Limited`] statement, when the runner fetched them.
pub fn compare(
    class: &StmtClass,
    a: Observed<'_>,
    b: Observed<'_>,
    a_full: Option<&[Vec<String>]>,
) -> Verdict {
    if b_not_implemented(a.outcome, b.outcome) {
        return Verdict::Unsupported;
    }
    let mut d = match (a.outcome, b.outcome) {
        (Outcome::Error { .. }, Outcome::Error { .. }) => {
            let d = error_diffs(a.outcome, b.outcome);
            return if d.is_empty() {
                Verdict::BothError
            } else {
                Verdict::Mismatch(d)
            };
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
            d.extend(row_diffs(class, ra, rb, a_full));
            d
        }
        (
            Outcome::Ok {
                affected_rows: x, ..
            },
            Outcome::Ok {
                affected_rows: y, ..
            },
        ) => {
            if x == y {
                Vec::new()
            } else {
                vec![format!("affected rows A={x} B={y}")]
            }
        }
        _ => vec![format!("A {} vs B {}", brief(a.outcome), brief(b.outcome))],
    };
    if d.is_empty() {
        d.extend(warning_diffs(a, b));
    }
    if d.is_empty() {
        Verdict::Match
    } else {
        Verdict::Mismatch(d)
    }
}

/// T4 (`--disable_result_log`): only the answer's shape (rows / OK / error) is compared, and
/// errors in full.
pub fn compare_status_only(a: &Outcome, b: &Outcome) -> Verdict {
    match (a, b) {
        _ if b_not_implemented(a, b) => Verdict::Unsupported,
        (Outcome::Error { .. }, Outcome::Error { .. }) => {
            let d = error_diffs(a, b);
            if d.is_empty() {
                Verdict::BothError
            } else {
                Verdict::Mismatch(d)
            }
        }
        (Outcome::Rows { .. }, Outcome::Rows { .. }) | (Outcome::Ok { .. }, Outcome::Ok { .. }) => {
            Verdict::Match
        }
        _ => Verdict::Mismatch(vec![format!("A {} vs B {}", brief(a), brief(b))]),
    }
}

/// The kind of a mismatch (spec §8): `error` when both failed, `shape` when the answers have
/// different shapes, else from the diff lines: `warnings`, `affected_rows`, `rows`, `order`,
/// `columns` or `metadata`. Rulings match on it (their plane).
pub fn mismatch_kind(a: &Outcome, b: &Outcome, d: &[String]) -> &'static str {
    match (a, b) {
        (Outcome::Error { .. }, Outcome::Error { .. }) => return "error",
        (Outcome::Rows { .. }, Outcome::Rows { .. }) | (Outcome::Ok { .. }, Outcome::Ok { .. }) => {
        }
        _ => return "shape",
    }
    if d.iter().all(|s| s.starts_with("warnings:")) {
        return "warnings";
    }
    if d.iter().all(|s| s.starts_with("affected rows")) {
        return "affected_rows";
    }
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

fn error_diffs(a: &Outcome, b: &Outcome) -> Vec<String> {
    let (
        Outcome::Error {
            code: ca,
            state: sa,
            message: ma,
        },
        Outcome::Error {
            code: cb,
            state: sb,
            message: mb,
        },
    ) = (a, b)
    else {
        return Vec::new();
    };
    let mut d = Vec::new();
    if ca != cb {
        d.push(format!("error: code A={ca} B={cb}"));
    }
    if sa != sb {
        d.push(format!("error: sqlstate A={sa} B={sb}"));
    }
    if ma != mb {
        d.push(format!("error: message A={ma:?} B={mb:?}"));
    }
    d
}

fn warning_diffs(a: Observed<'_>, b: Observed<'_>) -> Vec<String> {
    let (na, nb) = (warning_count(a.outcome), warning_count(b.outcome));
    if na != nb {
        return vec![format!("warnings: count A={na} B={nb}")];
    }
    if na == 0 {
        return Vec::new();
    }
    let (Some(wa), Some(wb)) = (a.warnings, b.warnings) else {
        return vec!["warnings: contents not fetched (a runner bug)".into()];
    };
    match exact_diff(wa, wb) {
        None => Vec::new(),
        Some(d) => d.into_iter().map(|s| format!("warnings: {s}")).collect(),
    }
}

/// At most this many differing rows are listed per side.
const MAX_ROW_DIFFS: usize = 5;

fn row_diffs(
    class: &StmtClass,
    a: &[Vec<String>],
    b: &[Vec<String>],
    a_full: Option<&[Vec<String>]>,
) -> Vec<String> {
    match class {
        StmtClass::Exact => exact_diff(a, b).unwrap_or_default(),
        StmtClass::Unordered => multiset_diffs(a, b),
        StmtClass::Ordered(keys) => ordered_diffs(keys, a, b),
        StmtClass::Limited(inner) => {
            let d = row_diffs(inner, a, b, None);
            match (d.is_empty(), a_full, &**inner) {
                (true, _, _) | (false, None, _) | (false, _, StmtClass::Exact) => d,
                (false, Some(full), inner) => limited_diffs(inner, a, b, full),
            }
        }
    }
}

/// Lists: `None` if equal.
fn exact_diff(a: &[Vec<String>], b: &[Vec<String>]) -> Option<Vec<String>> {
    if a == b {
        return None;
    }
    let mut d = Vec::new();
    if a.len() != b.len() {
        d.push(format!("row count A={} B={}", a.len(), b.len()));
    }
    let i = a
        .iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let show = |r: Option<&Vec<String>>| r.map_or("(none)".to_string(), |r| r.join(", "));
    d.push(format!(
        "first differing row #{i}: A={} B={}",
        show(a.get(i)),
        show(b.get(i))
    ));
    if multiset_diff(a, b).1 == 0 && multiset_diff(b, a).1 == 0 {
        d.push("same rows in a different order".into());
    }
    Some(d)
}

fn multiset_diffs(a: &[Vec<String>], b: &[Vec<String>]) -> Vec<String> {
    let mut d = Vec::new();
    if a.len() != b.len() {
        d.push(format!("row count A={} B={}", a.len(), b.len()));
    }
    for (side, (only, n)) in [("A", multiset_diff(a, b)), ("B", multiset_diff(b, a))] {
        if n > 0 {
            d.push(format!(
                "{n} row(s) only in {side}, e.g. {}",
                rows_brief(&only)
            ));
        }
    }
    d
}

fn rows_brief(rows: &[&Vec<String>]) -> String {
    rows.iter()
        .map(|r| format!("({})", r.join(", ")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_of<'r>(keys: &[usize], r: &'r [String]) -> Vec<Option<&'r String>> {
    keys.iter().map(|&k| r.get(k)).collect()
}

/// Ordered by `keys`: A's maximal runs of equal keys; B's rows at the same positions must be
/// the same multiset run by run.
fn ordered_diffs(keys: &[usize], a: &[Vec<String>], b: &[Vec<String>]) -> Vec<String> {
    if a.len() != b.len() {
        return multiset_diffs(a, b);
    }
    let mut start = 0;
    while start < a.len() {
        let k = key_of(keys, &a[start]);
        let mut end = start + 1;
        while end < a.len() && key_of(keys, &a[end]) == k {
            end += 1;
        }
        let (only_a, na) = multiset_diff(&a[start..end], &b[start..end]);
        if na > 0 {
            let (only_b, _) = multiset_diff(&b[start..end], &a[start..end]);
            let mut d = vec![format!(
                "first differing row #{start}: run of rows {start}..{end} with key ({}): \
                 {na} row(s) only in A, e.g. {}; only in B, e.g. {}",
                k.iter()
                    .map(|v| v.map_or("?", |s| s.as_str()))
                    .collect::<Vec<_>>()
                    .join(", "),
                rows_brief(&only_a),
                rows_brief(&only_b)
            )];
            if multiset_diff(a, b).1 == 0 && multiset_diff(b, a).1 == 0 {
                d.push("same rows in a different order".into());
            }
            return d;
        }
        start = end;
    }
    Vec::new()
}

/// Spec §2.4: B's rows are as many as A's, each a member of A's result without the LIMIT
/// (multiset containment), and with ORDER BY keys the key sequence is A's row by row.
fn limited_diffs(
    inner: &StmtClass,
    a: &[Vec<String>],
    b: &[Vec<String>],
    full: &[Vec<String>],
) -> Vec<String> {
    let mut d = Vec::new();
    if a.len() != b.len() {
        d.push(format!("row count A={} B={}", a.len(), b.len()));
    }
    let (extra, n) = multiset_diff(b, full);
    if n > 0 {
        d.push(format!(
            "{n} row(s) only in B (not in A's result without LIMIT), e.g. {}",
            rows_brief(&extra)
        ));
    }
    if let StmtClass::Ordered(keys) = inner
        && let Some(i) = a
            .iter()
            .zip(b)
            .position(|(x, y)| key_of(keys, x) != key_of(keys, y))
    {
        d.push(format!(
            "first differing row #{i}: ORDER BY key differs: A={} B={}",
            a[i].join(", "),
            b[i].join(", ")
        ));
    }
    d
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

/// The router check of the vertical-slice plan (§4.7), seen from the client: the column
/// definitions of an EXECUTE must be those its COM_STMT_PREPARE returned. tidb-rs with
/// `--new-stack=fallback` sends the new stack's definitions at PREPARE and may run an
/// EXECUTE on the PoC; a difference there is a bug. Go itself can answer an EXECUTE with
/// other definitions than its PREPARE (a `?` in the select list), so a difference is reported
/// when A has none in that column and field, or when A's EXECUTE has another value there.
pub fn execute_metadata_diffs(
    a: (Option<&PrepareMeta>, &Outcome),
    b: (Option<&PrepareMeta>, &Outcome),
) -> Vec<String> {
    // (key, message, the EXECUTE's value) per drifted field.
    let drift = |(p, o): (Option<&PrepareMeta>, &Outcome)| match (p, o) {
        (Some(p), Outcome::Rows { columns, .. }) => column_diffs("", &p.columns, columns)
            .into_iter()
            .map(|d| {
                // "col[i].field: A=.. B=.." or "column count A=.. B=..".
                let key = d.split([':', '=']).next().unwrap_or_default().to_owned();
                let executed = d.rsplit_once(" B=").map(|(_, v)| v.to_owned());
                let d = d
                    .replacen("A=", "prepare=", 1)
                    .replacen(" B=", " execute=", 1);
                (key, format!("B execute vs its prepare: {d}"), executed)
            })
            .collect(),
        _ => Vec::<(String, String, Option<String>)>::new(),
    };
    let on_a = drift(a);
    drift(b)
        .into_iter()
        .filter_map(|(k, d, executed)| {
            match on_a.iter().find(|(ka, _, _)| *ka == k) {
                None => Some(d),
                // Go drifted this field too: B must drift to Go's value.
                Some((_, _, a_executed)) if *a_executed != executed => Some(format!(
                    "{d} (A's execute={})",
                    a_executed.as_deref().unwrap_or("?")
                )),
                Some(_) => None,
            }
        })
        .collect()
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

    fn obs(o: &Outcome) -> Observed<'_> {
        Observed::new(o, None)
    }

    fn cmp(a: &Outcome, b: &Outcome, class: &StmtClass) -> Verdict {
        compare(class, obs(a), obs(b), None)
    }

    #[test]
    fn verdicts() {
        let u = StmtClass::Unordered;
        assert_eq!(
            cmp(&rows(&["a", "b"]), &rows(&["b", "a"]), &u),
            Verdict::Match
        );
        let Verdict::Mismatch(d) = cmp(&rows(&["a", "b"]), &rows(&["b", "a"]), &StmtClass::Exact)
        else {
            panic!()
        };
        assert!(
            d.contains(&"same rows in a different order".to_string()),
            "{d:?}"
        );
        let Verdict::Mismatch(d) = cmp(&rows(&["a", "a"]), &rows(&["a", "b"]), &u) else {
            panic!()
        };
        assert_eq!(
            d,
            vec![
                "1 row(s) only in A, e.g. (a)".to_string(),
                "1 row(s) only in B, e.g. (b)".to_string()
            ]
        );
        assert_eq!(cmp(&err(1146), &err(1146), &u), Verdict::BothError);
        assert!(matches!(
            cmp(&err(1146), &err(1105), &u),
            Verdict::Mismatch(_)
        ));
        // T1: only a not-implemented 1235 (boundary stub or prototype answer).
        let stub = Outcome::Error {
            code: 1235,
            state: "42000".into(),
            message: "This version of TiDB doesn't yet support 'x (blocked-by:D09.1)'".into(),
        };
        let poc = Outcome::Error {
            code: 1235,
            state: "42000".into(),
            message: "This version of TiDB doesn't yet support 'joins'".into(),
        };
        assert_eq!(cmp(&rows(&["a"]), &stub, &u), Verdict::Unsupported);
        assert_eq!(cmp(&rows(&["a"]), &poc, &u), Verdict::Unsupported);
        assert_eq!(cmp(&err(1146), &stub, &u), Verdict::Unsupported);
        assert!(matches!(
            cmp(&rows(&["a"]), &err(1235), &u),
            Verdict::Mismatch(_)
        ));
        // A also refuses with another 1235 text: still unsupported (#510); the same text on
        // both sides is equal; B's unmarked 1235 against A's 1235 compares in full.
        assert_eq!(cmp(&err(1235), &stub, &u), Verdict::Unsupported);
        assert_eq!(cmp(&stub, &stub, &u), Verdict::BothError);
        assert!(matches!(cmp(&stub, &err(1235), &u), Verdict::Mismatch(_)));
        assert!(matches!(
            cmp(&rows(&[]), &err(1105), &u),
            Verdict::Mismatch(_)
        ));
        assert!(matches!(
            cmp(&rows(&[]), &Outcome::Client("x".into()), &u),
            Verdict::Mismatch(_)
        ));
        let ok = Outcome::Ok {
            affected_rows: 0,
            warnings: 0,
        };
        assert_eq!(cmp(&ok, &ok, &u), Verdict::Match);
    }

    #[test]
    fn tolerances_match_the_ledger() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/testing/tolerances.toml"
        );
        let src = std::fs::read_to_string(path).expect("docs/testing/tolerances.toml");
        let field = |k: &str| -> Vec<String> {
            src.lines()
                .filter_map(|l| {
                    let (key, v) = l.split_once('=')?;
                    (key.trim() == k).then(|| v.trim().trim_matches('"').to_string())
                })
                .collect()
        };
        let ids = field("id");
        let names = field("name");
        let ledger: Vec<(&str, &str)> = ids
            .iter()
            .map(String::as_str)
            .zip(names.iter().map(String::as_str))
            .collect();
        assert_eq!(ids.len(), names.len(), "every entry needs an id and a name");
        assert_eq!(ledger, TOLERANCES, "tolerances.toml and TOLERANCES differ");
    }

    #[test]
    fn side_effects_keep_it_strict() {
        for q in [
            "select nextval(s) from t limit 1",
            "select @v := a from t limit 1",
            "select get_lock('x', 1) from t limit 1",
            "select sleep(0) from t limit 1",
            "select last_insert_id(a) from t limit 1",
            "select a from t limit 1 for update",
            "select a from t limit 1 lock in share mode",
        ] {
            assert!(has_side_effects(q), "{q}");
            assert_eq!(strip_limit(q), None, "{q}");
        }
        assert!(!has_side_effects("select last_insert_id() from t limit 1"));
        assert!(!has_side_effects("select 'sleep(1)', a from t limit 1"));
    }

    #[test]
    fn outer_parentheses() {
        let c = parse_cols("a");
        assert_eq!(
            classify("(SELECT a FROM t ORDER BY a LIMIT 3)", &c, false).to_string(),
            "limited ordered 1"
        );
        assert_eq!(
            classify("((select a from t order by a));", &c, false).to_string(),
            "ordered 1"
        );
        assert_eq!(
            classify(
                "(select a from t order by a) union (select a from u)",
                &c,
                false
            )
            .to_string(),
            "unordered"
        );
    }

    #[test]
    fn prototype_list_compiles() {
        assert!(is_not_implemented_answer(
            "This version of TiDB doesn't yet support 'aggregate function FOO'"
        ));
        assert!(!is_not_implemented_answer(
            "This version of TiDB doesn't yet support 'something Go says'"
        ));
        // The read-only node's reasons a table cannot be read (crates/meta `Unsupported`,
        // passed on as `unsupported(e.to_string())`): fixed answers too (#510).
        for what in [
            "table `tp` is partitioned; partitioned tables are not supported",
            "`cte.v1` is a view",
            "table `t` has a clustered non-integer primary key (common handle)",
        ] {
            assert!(
                is_not_implemented_answer(&format!(
                    "This version of TiDB doesn't yet support '{what}'"
                )),
                "{what}"
            );
        }
    }

    #[test]
    fn both_1235() {
        let e = |m: &str| Outcome::Error {
            code: ER_NOT_SUPPORTED_YET,
            state: "42000".into(),
            message: format!("This version of TiDB doesn't yet support '{m}'"),
        };
        // Both refuse with different texts, B's is a not-implemented answer: unsupported.
        assert!(b_not_implemented(&e("this feature"), &e("joins")));
        assert!(b_not_implemented(
            &e("this feature"),
            &e("x (blocked-by:D09.1)")
        ));
        // The same text compares in full (both_error).
        assert!(!b_not_implemented(&e("joins"), &e("joins")));
        // B's 1235 is not a not-implemented answer (a ported Go branch): compared in full.
        assert!(!b_not_implemented(
            &e("this feature"),
            &e("something Go says")
        ));
    }

    #[test]
    fn strips_limit() {
        assert_eq!(
            strip_limit("select a from t order by a limit 2").as_deref(),
            Some("select a from t order by a")
        );
        assert_eq!(
            strip_limit("select a from t limit 1, 2;").as_deref(),
            Some("select a from t")
        );
        assert_eq!(
            strip_limit("select a from t limit 2 offset 1").as_deref(),
            Some("select a from t")
        );
        assert_eq!(strip_limit("select a from t limit ?"), None);
        assert_eq!(
            strip_limit("select a from (select a from t limit 1) x"),
            None
        );
        assert_eq!(strip_limit("select a from t limit 1 for update"), None);
    }

    /// The fixtures (`tools/ab/fixtures/compare/*.case`, format in its README).
    #[test]
    fn fixtures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/compare");
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .expect("fixtures/compare")
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "case"))
            .collect();
        paths.sort();
        let mut seen: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
        let mut failures = Vec::new();
        for p in &paths {
            let src = std::fs::read_to_string(p).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            match run_fixture(&src) {
                Ok((family, verdict)) => {
                    *seen.entry((family, verdict)).or_default() += 1;
                }
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
        assert!(
            failures.is_empty(),
            "fixture failures:\n{}",
            failures.join("\n")
        );
        // Every class needs a fixture judged equal and one that MUST be judged different.
        for family in [
            "exact",
            "unordered",
            "ordered",
            "limited",
            "error",
            "warnings",
        ] {
            for want in ["equal", "different"] {
                assert!(
                    seen.get(&(family.to_string(), want)).copied().unwrap_or(0) > 0,
                    "no `{want}` fixture for `{family}` (have {seen:?})"
                );
            }
        }
    }

    fn parse_cols(spec: &str) -> Vec<ColumnMeta> {
        spec.split_whitespace()
            .map(|c| {
                let (table, name) = c.rsplit_once('.').unwrap_or(("t", c));
                // `name=org`: an alias of column `org`.
                let (name, org) = name.split_once('=').unwrap_or((name, name));
                ColumnMeta {
                    table: table.into(),
                    org_table: table.into(),
                    name: name.into(),
                    org_name: org.into(),
                    ..col("c")
                }
            })
            .collect()
    }

    /// Runs one fixture: (class family, "equal" | "different" | "unsupported").
    fn run_fixture(src: &str) -> Result<(String, &'static str), String> {
        let mut kv: BTreeMap<String, String> = BTreeMap::new();
        let mut diffs: Vec<String> = Vec::new();
        let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut cur: Option<String> = None;
        for line in src.lines() {
            if line.starts_with('#') || (cur.is_none() && line.trim().is_empty()) {
                continue;
            }
            if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                const KNOWN: [&str; 9] = [
                    "a",
                    "a_full",
                    "b",
                    "a_error",
                    "b_error",
                    "a_ok",
                    "b_ok",
                    "a_warnings",
                    "b_warnings",
                ];
                if !KNOWN.contains(&sec) {
                    return Err(format!("unknown section [{sec}]"));
                }
                sections.insert(sec.to_string(), Vec::new());
                cur = Some(sec.to_string());
                continue;
            }
            if let Some(c) = &cur {
                if !line.is_empty() {
                    sections.get_mut(c).unwrap().push(line.to_string());
                }
                continue;
            }
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| format!("bad line {line:?}"))?;
            let (k, v) = (k.trim(), v.trim());
            match k {
                "diff" => diffs.push(v.to_string()),
                "sql" | "cols" | "b_cols" | "sorted" | "result_log" | "class" | "verdict"
                | "tolerance" => {
                    kv.insert(k.to_string(), v.to_string());
                }
                _ => return Err(format!("unknown key {k:?}")),
            }
        }
        let get = |k: &str| kv.get(k).cloned().ok_or(format!("missing `{k}:`"));
        let sql = get("sql")?;
        let cols_a = parse_cols(kv.get("cols").map_or("", String::as_str));
        let cols_b = kv
            .get("b_cols")
            .map_or_else(|| cols_a.clone(), |c| parse_cols(c));
        if let Some(t) = kv.get("tolerance")
            && !TOLERANCES.iter().any(|(id, _)| id == t)
        {
            return Err(format!("unknown tolerance {t}"));
        }
        let table = |sec: &str| -> Option<Vec<Vec<String>>> {
            sections.get(sec).map(|v| {
                v.iter()
                    .map(|l| l.split('|').map(str::to_string).collect())
                    .collect()
            })
        };
        let side = |p: &str, cols: &[ColumnMeta]| -> Result<Outcome, String> {
            let warnings = sections.get(&format!("{p}_warnings")).map_or(0, Vec::len) as u16;
            if let Some(r) = table(p) {
                return Ok(Outcome::Rows {
                    columns: cols.to_vec(),
                    rows: r,
                    warnings,
                });
            }
            if let Some(e) = sections.get(&format!("{p}_error")) {
                let l = e.first().ok_or("empty error section")?;
                let mut f = l.splitn(3, '|');
                let code = f
                    .next()
                    .unwrap_or("")
                    .parse()
                    .map_err(|_| "bad error code")?;
                return Ok(Outcome::Error {
                    code,
                    state: f.next().unwrap_or("").to_string(),
                    message: f.next().unwrap_or("").to_string(),
                });
            }
            if let Some(o) = sections.get(&format!("{p}_ok")) {
                let n = o
                    .first()
                    .map_or(Ok(0), |l| l.parse())
                    .map_err(|_| "bad ok")?;
                return Ok(Outcome::Ok {
                    affected_rows: n,
                    warnings,
                });
            }
            Err(format!("no [{p}] / [{p}_error] / [{p}_ok] section"))
        };
        let a = side("a", &cols_a)?;
        let b = side("b", &cols_b)?;
        let sorted = kv.get("sorted").is_some_and(|v| v == "yes");
        let class = classify(&sql, &cols_a, sorted);
        let want_class = get("class")?;
        if class.to_string() != want_class {
            return Err(format!("class {class} (fixture says {want_class})"));
        }
        let wa = table("a_warnings");
        let wb = table("b_warnings");
        let full = table("a_full");
        let v = if kv.get("result_log").is_some_and(|v| v == "off") {
            compare_status_only(&a, &b)
        } else {
            compare(
                &class,
                Observed::new(&a, wa.as_deref()),
                Observed::new(&b, wb.as_deref()),
                full.as_deref(),
            )
        };
        let want = get("verdict")?;
        if v.name() != want {
            return Err(format!("verdict {v:?} (fixture says {want})"));
        }
        if let Verdict::Mismatch(d) = &v {
            for want in &diffs {
                if !d.iter().any(|x| x.contains(want.as_str())) {
                    return Err(format!("no diff containing {want:?} in {d:?}"));
                }
            }
        } else if !diffs.is_empty() {
            return Err("`diff:` lines on a fixture that is not a mismatch".into());
        }
        let family = if matches!(a, Outcome::Error { .. }) || matches!(b, Outcome::Error { .. }) {
            "error".to_string()
        } else if wa.is_some() || wb.is_some() {
            "warnings".to_string()
        } else {
            class
                .to_string()
                .split(' ')
                .next()
                .unwrap_or("")
                .to_string()
        };
        let verdict = match v {
            Verdict::Match | Verdict::BothError => "equal",
            Verdict::Mismatch(_) | Verdict::Ruled { .. } => "different",
            Verdict::Unsupported => "unsupported",
        };
        Ok((family, verdict))
    }

    /// Counter-proof of the §4.7 check: B's EXECUTE returns another column definition than
    /// its PREPARE (a fallback whose PoC metadata differs) and A's does not.
    #[test]
    fn execute_metadata_differs_from_prepare() {
        let prep = PrepareMeta {
            param_count: 0,
            columns: vec![col("c")],
        };
        let same = rows(&[]);
        let mut other = col("c");
        other.length = 11;
        let drifted = Outcome::Rows {
            columns: vec![other],
            rows: vec![],
            warnings: 0,
        };
        assert!(execute_metadata_diffs((Some(&prep), &same), (Some(&prep), &same)).is_empty());
        assert_eq!(
            execute_metadata_diffs((Some(&prep), &same), (Some(&prep), &drifted)),
            vec!["B execute vs its prepare: col[0].length: prepare=480 execute=11".to_string()]
        );
        // Go drifted the same field to another value: still B's fault.
        let mut other_len = col("c");
        other_len.length = 12;
        let other_len = Outcome::Rows {
            columns: vec![other_len],
            rows: vec![],
            warnings: 0,
        };
        assert_eq!(
            execute_metadata_diffs((Some(&prep), &other_len), (Some(&prep), &drifted)),
            vec![
                "B execute vs its prepare: col[0].length: prepare=480 execute=11 (A's execute=12)"
                    .to_string()
            ]
        );
        // Go's own PREPARE/EXECUTE difference is not B's fault, another one is.
        assert!(
            execute_metadata_diffs((Some(&prep), &drifted), (Some(&prep), &drifted)).is_empty()
        );
        let mut flags = col("c");
        flags.flags = 0;
        let flags = Outcome::Rows {
            columns: vec![flags],
            rows: vec![],
            warnings: 0,
        };
        assert_eq!(
            execute_metadata_diffs((Some(&prep), &drifted), (Some(&prep), &flags)).len(),
            1
        );
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
        let Verdict::Mismatch(d) = compare(&StmtClass::Unordered, obs(&a), obs(&bo), None) else {
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
