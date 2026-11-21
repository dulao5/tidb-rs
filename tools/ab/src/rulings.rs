//! The divergence ledger (`docs/testing/rulings.toml`, spec `docs/testing/rulings.md`).
//!
//! A ruling is an accepted, user-visible difference between tidb-rs and Go TiDB. A comparator
//! mismatch (or a `b_error`) that a ruling's match rule covers is recorded as `ruled:<id>`:
//! still written to mismatches.jsonl and counted, never turned into a match. A ruling whose
//! approver is `pending-owner` suppresses nothing (the statement stays a mismatch, marked with
//! the ruling it would fall under). Every run writes `rulings-hits.tsv`, the hit count of each
//! matched ruling; the weekly audit (`scripts/check_rulings.py --audit`) fails a ruling with no
//! hit in the last N full runs.
//!
//! The file is read with a small parser for the TOML subset the ledger uses (tables
//! `[audit]` / `[[ruling]]`, `key = "basic string"`, `key = 'literal string'`, `key = 123`,
//! comments). `scripts/check_rulings.py` checks the schema with a full TOML parser and refuses
//! anything outside this subset, so both read the same thing.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use regex::Regex;

/// The ledger shipped with the repository.
pub const DEFAULT_PATH: &str = "docs/testing/rulings.toml";

/// The ledger as of this build (the default; `ab --rulings` reads another file).
pub const BUILT_IN: &str = include_str!("../../../docs/testing/rulings.toml");

/// The per-run hit counts, in the run's output directory.
pub const HITS_FILE: &str = "rulings-hits.tsv";

/// Approver value of a ruling that waits for the owner: it suppresses nothing.
pub const PENDING: &str = "pending-owner";

/// Planes the comparator reports (the integrationtest mismatch kinds, plus `b_error`, plus
/// `explain`: a rows / order / columns / metadata mismatch of an EXPLAIN / DESC statement).
pub const MATCHED_PLANES: &[&str] = &[
    "rows",
    "order",
    "columns",
    "metadata",
    "error",
    "warnings",
    "affected_rows",
    "shape",
    "b_error",
    "explain",
];

/// Test layers whose statements the runners match against the ledger.
pub const LAYERS: &[&str] = &["integrationtest", "workload"];

/// One ruling the comparator can match.
#[derive(Debug)]
pub struct Ruling {
    pub id: String,
    pub plane: String,
    pub layer: String,
    unit: Regex,
    stmt: Regex,
    detail: Option<Regex>,
    pub approved: bool,
}

impl Ruling {
    fn covers(&self, layer: &str, unit: &str, sql: &str, kind: &str, detail: &str) -> bool {
        let plane_ok = match self.plane.as_str() {
            "explain" => {
                matches!(kind, "rows" | "order" | "columns" | "metadata") && is_explain(sql)
            }
            p => p == kind,
        };
        plane_ok
            && self.layer == layer
            && self.unit.is_match(unit)
            && self.stmt.is_match(sql)
            && self.detail.as_ref().is_none_or(|d| d.is_match(detail))
    }
}

fn is_explain(sql: &str) -> bool {
    let head = sql
        .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(head.as_str(), "EXPLAIN" | "DESC" | "DESCRIBE")
}

/// A ruling's match on one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    /// Approved: the statement is `ruled:<id>`. Pending: it stays a mismatch.
    pub approved: bool,
}

impl Hit {
    /// The mismatch kind to record: `ruled:<id>`, or the kind with the pending ruling named.
    pub fn kind(&self, kind: &str) -> String {
        if self.approved {
            format!("ruled:{}", self.id)
        } else {
            format!("{kind} (pending ruling {})", self.id)
        }
    }
}

/// The ledger's matched rulings and their hit counts in this run.
#[derive(Debug, Default)]
pub struct Ledger {
    rulings: Vec<Ruling>,
    /// Ids of every ruling in the file (also those no test layer sees), for the hits file.
    pub all_ids: Vec<String>,
    hits: BTreeMap<String, u64>,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger, String> {
        let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ledger::parse(&src).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(src: &str) -> Result<Ledger, String> {
        let mut ledger = Ledger::default();
        for (n, t) in parse_tables(src)?.into_iter().enumerate() {
            if t.name != "ruling" {
                continue;
            }
            let get = |k: &str| t.fields.get(k).map(String::as_str);
            let id = get("id").ok_or(format!("ruling #{} has no id", n + 1))?;
            ledger.all_ids.push(id.to_string());
            let layer = get("layer").unwrap_or("none");
            if layer == "none" {
                continue;
            }
            if !LAYERS.contains(&layer) {
                return Err(format!("{id}: unknown layer {layer:?}"));
            }
            let plane = get("plane").ok_or(format!("{id}: no plane"))?;
            if !MATCHED_PLANES.contains(&plane) {
                return Err(format!(
                    "{id}: plane {plane:?} is not one the comparator reports"
                ));
            }
            let re = |k: &str| -> Result<Option<Regex>, String> {
                get(k)
                    .map(|p| Regex::new(p).map_err(|e| format!("{id}: {k}: {e}")))
                    .transpose()
            };
            let approver = get("approver").ok_or(format!("{id}: no approver"))?;
            ledger.rulings.push(Ruling {
                id: id.to_string(),
                plane: plane.to_string(),
                layer: layer.to_string(),
                unit: re("unit")?.ok_or(format!("{id}: no unit"))?,
                stmt: re("stmt")?.ok_or(format!("{id}: no stmt"))?,
                detail: re("detail")?,
                approved: approver != PENDING,
            });
        }
        Ok(ledger)
    }

    /// The first ruling covering a mismatch (`kind` = the integrationtest mismatch kind, or
    /// `b_error`) of `sql` in `unit` (test file or workload template) of `layer`; counts the
    /// hit.
    pub fn rule(
        &mut self,
        layer: &str,
        unit: &str,
        sql: &str,
        kind: &str,
        detail: &str,
    ) -> Option<Hit> {
        let r = self
            .rulings
            .iter()
            .find(|r| r.covers(layer, unit, sql, kind, detail))?;
        *self.hits.entry(r.id.clone()).or_default() += 1;
        Some(Hit {
            id: r.id.clone(),
            approved: r.approved,
        })
    }

    pub fn hits(&self, id: &str) -> u64 {
        self.hits.get(id).copied().unwrap_or(0)
    }

    /// `rulings-hits.tsv`: one row per matched ruling (`id`, `status`, `layer`, `hits`), zero
    /// hits included, so the audit can tell "no hit" from "not in the ledger then".
    pub fn write_hits(&self, path: &Path) -> Result<(), String> {
        let mut out = String::from("id\tstatus\tlayer\thits\n");
        for r in &self.rulings {
            let status = if r.approved { "approved" } else { PENDING };
            out.push_str(&format!(
                "{}\t{status}\t{}\t{}\n",
                r.id,
                r.layer,
                self.hits(&r.id)
            ));
        }
        std::fs::File::create(path)
            .and_then(|mut f| f.write_all(out.as_bytes()))
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// One `[name]` / `[[name]]` table of the file.
struct Table {
    name: String,
    fields: BTreeMap<String, String>,
}

/// The TOML subset of the ledger. Values are kept as strings (integers as written).
fn parse_tables(src: &str) -> Result<Vec<Table>, String> {
    let mut tables: Vec<Table> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix("[[")
            .and_then(|l| l.strip_suffix("]]"))
            .or_else(|| line.strip_prefix('[').and_then(|l| l.strip_suffix(']')))
        {
            tables.push(Table {
                name: name.trim().to_string(),
                fields: BTreeMap::new(),
            });
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .ok_or(format!("line {n}: expected key = value"))?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("line {n}: bad key {key:?}"));
        }
        let value = parse_value(rest.trim()).map_err(|e| format!("line {n}: {e}"))?;
        let t = tables
            .last_mut()
            .ok_or(format!("line {n}: key outside a table"))?;
        if t.fields.insert(key.to_string(), value).is_some() {
            return Err(format!("line {n}: duplicate key {key}"));
        }
    }
    Ok(tables)
}

fn parse_value(v: &str) -> Result<String, String> {
    if let Some(body) = v.strip_prefix('\'') {
        let end = body.find('\'').ok_or("unterminated literal string")?;
        trailing(&body[end + 1..])?;
        return Ok(body[..end].to_string());
    }
    if let Some(body) = v.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    trailing(chars.as_str())?;
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    other => return Err(format!("unsupported escape \\{other:?}")),
                },
                c => out.push(c),
            }
        }
        return Err("unterminated string".into());
    }
    let (num, rest) = v.split_once('#').unwrap_or((v, ""));
    let _ = rest;
    let num = num.trim();
    if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
        return Ok(num.to_string());
    }
    Err(format!(
        "unsupported value {v:?} (strings and integers only)"
    ))
}

fn trailing(rest: &str) -> Result<(), String> {
    let rest = rest.trim();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err(format!("unexpected text after the value: {rest:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEDGER: &str = r#"
[audit]
runs = 4

[[ruling]]
id = "R90"
class = "divergence"
plane = "error"
layer = "integrationtest"
unit = '^executor/foo$'
stmt = '(?i)^select @@x'
detail = 'code A=1238 B=1193'
approver = "zhigang-du"

[[ruling]]
id = "R91"
plane = "columns"
layer = "integrationtest"
unit = '.'
stmt = "(?i)^select \\+"
approver = "pending-owner"   # waits for the owner

[[ruling]]
id = "R92"
plane = "explain"
layer = "workload"
unit = '^t$'
stmt = '.'
approver = "maintainer"

[[ruling]]
id = "R93"
plane = "behaviour"
layer = "none"
approver = "pending-owner"
"#;

    #[test]
    fn rules_and_counts() {
        let mut l = Ledger::parse(LEDGER).unwrap();
        assert_eq!(l.rulings.len(), 3);
        assert_eq!(l.all_ids, ["R90", "R91", "R92", "R93"]);
        let d = "error: code A=1238 B=1193; error: message ...";
        let hit = l.rule("integrationtest", "executor/foo", "SELECT @@x", "error", d);
        assert_eq!(
            hit.as_ref().map(|h| h.kind("error")).as_deref(),
            Some("ruled:R90")
        );
        // Another file, another plane, another detail, another layer: no ruling.
        assert_eq!(
            l.rule("integrationtest", "executor/bar", "SELECT @@x", "error", d),
            None
        );
        assert_eq!(
            l.rule("integrationtest", "executor/foo", "SELECT @@x", "rows", d),
            None
        );
        assert_eq!(
            l.rule(
                "integrationtest",
                "executor/foo",
                "SELECT @@x",
                "error",
                "x"
            ),
            None
        );
        assert_eq!(
            l.rule("workload", "executor/foo", "SELECT @@x", "error", d),
            None
        );
        // Pending: matched and counted, but it suppresses nothing.
        let p = l
            .rule("integrationtest", "a", "select +1", "columns", "")
            .unwrap();
        assert!(!p.approved);
        assert_eq!(p.kind("columns"), "columns (pending ruling R91)");
        // explain: only rows-like kinds of EXPLAIN / DESC statements.
        assert!(
            l.rule("workload", "t", "EXPLAIN SELECT 1", "rows", "")
                .is_some()
        );
        assert!(l.rule("workload", "t", "SELECT 1", "rows", "").is_none());
        assert!(l.rule("workload", "t", "desc t", "error", "").is_none());
        assert_eq!((l.hits("R90"), l.hits("R91"), l.hits("R92")), (1, 1, 1));
        let dir = std::env::temp_dir().join(format!("ab-rulings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rulings-hits.tsv");
        l.write_hits(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "id\tstatus\tlayer\thits\nR90\tapproved\tintegrationtest\t1\n\
             R91\tpending-owner\tintegrationtest\t1\nR92\tapproved\tworkload\t1\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        for bad in [
            "[[ruling]]\nid = \"R1\"\nlayer = \"integrationtest\"\nplane = \"rows\"\nunit = '('\nstmt = 'x'\napprover = \"x\"",
            "[[ruling]]\nid = \"R1\"\nlayer = \"integrationtest\"\nplane = \"session\"\nunit = 'a'\nstmt = 'x'\napprover = \"x\"",
            "[[ruling]]\nid = \"R1\"\nlayer = \"tpcc\"\n",
            "[[ruling]]\nid = \"R1\"\nid = \"R2\"\n",
            "id = \"R1\"\n",
            "[[ruling]]\nid = [\"R1\"]\n",
            "[[ruling]]\nid = \"R1\" x\n",
        ] {
            assert!(Ledger::parse(bad).is_err(), "{bad}");
        }
    }

    /// The shipped ledger parses and every regex compiles (its schema is checked by
    /// scripts/check_rulings.py in CI).
    #[test]
    fn shipped_ledger_parses() {
        let l = Ledger::parse(BUILT_IN).unwrap();
        assert!(!l.all_ids.is_empty());
    }
}
