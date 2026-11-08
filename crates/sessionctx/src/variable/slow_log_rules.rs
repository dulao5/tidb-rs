//! go-file: pkg/sessionctx/variable/slow_log.go (the rule parser, L918–L1095)
//! go-file: pkg/sessionctx/slowlogrule/rules.go (types)
//!
//! Parsing of `tidb_slow_log_rules` (D05.2 T7). The `Setter` / `Match` side of
//! `SlowLogRuleFieldAccessors` evaluates rules against a finished statement and belongs to
//! the slow log writer (D14.2); only `Parse` is here.
//!
//! Deviation (M-MAPORDER, L2): Go builds the conditions of a rule by ranging over a map and
//! `ParseGlobalSlowLogRules` joins the per-connection rule sets by ranging over a map, so
//! Go's `RawRules` (the value `SHOW VARIABLES` returns) and `RawRulesHash` change between
//! runs when a rule has two or more conditions or the global value has two or more
//! connection groups. Rust emits conditions sorted by field name and groups sorted by
//! connection ID; the replay compares Go's output after the same canonicalisation.

// Go names for the package-level values (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::LazyLock;

use parking_lot::RwLock;
use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gostd;

/// A parsed threshold (`any` in Go: `uint64`, `int64`, `float64`, `bool` or `string`).
#[derive(Clone, Debug, PartialEq)]
pub enum Threshold {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
}

impl Threshold {
    /// `fmt.Sprintf("%v", threshold)`.
    // glue: fmt %v of the dynamic threshold value
    pub fn format_v(&self) -> String {
        match self {
            Threshold::U64(v) => v.to_string(),
            Threshold::I64(v) => v.to_string(),
            Threshold::F64(v) => tidb_types::gostd::format_float_g(*v),
            Threshold::Bool(v) => v.to_string(),
            Threshold::Str(v) => v.clone(),
        }
    }
}

/// `slowlogrule.SlowLogCondition`.
#[derive(Clone, Debug, PartialEq)]
pub struct SlowLogCondition {
    /// Name of the slow log field to check (e.g., "Conn_ID", "Query_time").
    pub field: String,
    /// Threshold value for triggering the condition.
    pub threshold: Threshold,
}

/// `slowlogrule.SlowLogRule`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlowLogRule {
    /// List of conditions combined with logical AND.
    pub conditions: Vec<SlowLogCondition>,
}

/// `slowlogrule.SlowLogRules`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlowLogRules {
    /// Raw rule string before parsing.
    pub raw_rules: String,
    /// All unique fields used in this rule set.
    pub fields: BTreeSet<String>,
    /// List of rules combined with logical OR.
    pub rules: Vec<SlowLogRule>,
}

/// `slowlogrule.GlobalSlowLogRules`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlobalSlowLogRules {
    /// Raw rule string before parsing.
    pub raw_rules: String,
    /// Hash of RawRules for fast comparison.
    pub raw_rules_hash: u64,
    /// Mapping of ConnID → SlowLogRules.
    pub rules_map: BTreeMap<i64, SlowLogRules>,
}

/// Go `GlobalSlowLogRules` (`atomic.Pointer`, initialised to an empty rule set).
// go-dep: github.com/pingcap/tidb/pkg/sessionctx/variable.GlobalSlowLogRules L1
pub static GlobalSlowLogRules: LazyLock<RwLock<Arc<GlobalSlowLogRules>>> =
    LazyLock::new(|| RwLock::new(Arc::new(GlobalSlowLogRules::default())));

/// UnsetConnID is a sentinel value (-1) for slow log rules without an explicit connection
/// binding.
pub const UnsetConnID: i64 = -1;

/// SlowLogConnIDStr is slow log field name.
pub const SlowLogConnIDStr: &str = "Conn_ID";

/// The parser of a field's threshold (`SlowLogFieldAccessor.Parse`).
#[derive(Clone, Copy)]
enum ParseKind {
    Uint64,
    Int64,
    Float64,
    Bool,
    String,
}

/// The keys of `SlowLogRuleFieldAccessors` (lower-cased field names) and their `Parse`.
// go-part: SlowLogRuleFieldAccessors L1 (Parse only; Setter / Match belong to the slow log writer)
fn accessor(field: &str) -> Option<ParseKind> {
    use ParseKind::*;
    Some(match field {
        "backoff_time"
        | "commit_time"
        | "compile_time"
        | "cop_mvcc_read_amplification"
        | "kv_total"
        | "optimize_time"
        | "parse_time"
        | "pd_total"
        | "prewrite_time"
        | "process_time"
        | "query_time"
        | "rewrite_time"
        | "wait_ts"
        | "write_sql_response_total" => Float64,
        "conn_id" | "exec_retry_count" | "prewrite_region" | "process_keys" | "total_keys"
        | "write_keys" | "write_size" => Uint64,
        "disk_max" | "mem_max" | "num_cop_tasks" => Int64,
        "is_internal" | "succ" => Bool,
        "db" | "digest" | "resource_group" | "session_alias" => String,
        _ => return None,
    })
}

// go: parseInt64 L1
fn parse_int64(v: &str) -> Result<Threshold, Error> {
    let (n, err) = tidb_types::gostd::parse_int(v.as_bytes(), 64);
    err.map_err(|e| e.into_base())?;
    if n < 0 {
        return Err(errors::new(format!(
            "threshold value must be non-negative, got {n}"
        )));
    }
    Ok(Threshold::I64(n))
}

// go: parseUint64 L1
fn parse_uint64(v: &str) -> Result<Threshold, Error> {
    let (n, err) = tidb_types::gostd::parse_uint(v.as_bytes(), 64);
    err.map_err(|e| e.into_base())?;
    Ok(Threshold::U64(n))
}

// go: parseFloat64 L1
fn parse_float64(v: &str) -> Result<Threshold, Error> {
    let (f, err) = tidb_types::gostd::parse_float(v);
    if let Err(e) = err {
        return Err(errors::new(e.to_string()));
    }
    if f.is_nan() || f.is_infinite() {
        return Err(errors::new(format!(
            "threshold value must be finite, got {}",
            tidb_types::gostd::format_float_g(f)
        )));
    }
    if f < 0.0 {
        return Err(errors::new(format!(
            "threshold value must be non-negative, got {}",
            tidb_types::gostd::format_float_g(f)
        )));
    }
    Ok(Threshold::F64(f))
}

// go: parseBool L1
// go-std: strconv.ParseBool L1
fn parse_bool(v: &str) -> Result<Threshold, Error> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(Threshold::Bool(true)),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(Threshold::Bool(false)),
        _ => {
            let quoted = tidb_base::gofmt::sprintf(
                "%q",
                &[tidb_base::gofmt::Arg::go_string(v.as_bytes().to_vec())],
            );
            let mut m = b"strconv.ParseBool: parsing ".to_vec();
            m.extend(quoted);
            m.extend_from_slice(b": invalid syntax");
            Err(errors::new(m))
        }
    }
}

/// ParseSlowLogFieldValue is exporting for testing.
// go: ParseSlowLogFieldValue L1
pub fn parse_slow_log_field_value(field_name: &str, value: &str) -> Result<Threshold, Error> {
    let Some(kind) = accessor(&gostd::to_lower(field_name)) else {
        return Err(errors::new(format!(
            "unknown slow log field name:{field_name}"
        )));
    };
    match kind {
        ParseKind::Uint64 => parse_uint64(value),
        ParseKind::Int64 => parse_int64(value),
        ParseKind::Float64 => parse_float64(value),
        ParseKind::Bool => parse_bool(value),
        ParseKind::String => Ok(Threshold::Str(value.to_owned())),
    }
}

/// RE2 `\s`: `[\t\n\f\r ]`.
// glue: RE2 \s class
fn is_re_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// RE2 `\w`: `[0-9A-Za-z_]`.
// glue: RE2 \w class
fn is_re_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `slowLogFieldRe.FindAllStringSubmatch(s, -1)` for `\s*(\w+)\s*:\s*([^,]+)\s*`: the two
/// capture groups of every match, leftmost-first.
// go-part: slowLogFieldRe (regexp.MustCompile(`\s*(\w+)\s*:\s*([^,]+)\s*`).FindAllStringSubmatch) L2 (hand-written matcher of this one pattern)
fn find_all_field_matches(s: &str) -> Vec<(&str, &str)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut p = 0;
    while p < b.len() {
        match match_at(b, p) {
            Some((g1, g2, end)) => {
                out.push((&s[g1.0..g1.1], &s[g2.0..g2.1]));
                p = end;
            }
            None => p += 1,
        }
    }
    out
}

type Span = (usize, usize);

/// One match of the pattern starting at `p`, with backtracking where the pattern needs it:
/// `\s*` before the value gives back spaces when no non-comma byte follows them.
// glue: the matcher behind find_all_field_matches
fn match_at(b: &[u8], p: usize) -> Option<(Span, Span, usize)> {
    let mut i = p;
    while i < b.len() && is_re_space(b[i]) {
        i += 1;
    }
    let w = i;
    while i < b.len() && is_re_word(b[i]) {
        i += 1;
    }
    if i == w {
        return None;
    }
    let g1 = (w, i);
    while i < b.len() && is_re_space(b[i]) {
        i += 1;
    }
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i += 1;
    let spaces_from = i;
    while i < b.len() && is_re_space(b[i]) {
        i += 1;
    }
    // [^,]+ needs one byte; if only spaces precede a ',' or the end, `\s*` gives back its last
    // space and [^,]+ matches it.
    let start = if i < b.len() && b[i] != b',' {
        i
    } else if i > spaces_from {
        i - 1
    } else {
        return None;
    };
    let mut j = start;
    while j < b.len() && b[j] != b',' {
        j += 1;
    }
    // [^,]+ is greedy and also takes trailing spaces, so the final `\s*` matches nothing.
    Some((g1, (start, j), j))
}

/// `strings.Trim(s, "\"'")`. (`strings.TrimSpace` is `str::trim`: both trim Unicode White_Space.)
// go-std: strings.Trim L1 (ASCII cutset)
fn trim_quotes(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'')
}

// go: parseSlowLogRuleEntry L2 (M-MAPORDER: conditions sorted by field name)
fn parse_slow_log_rule_entry(
    raw_rule: &str,
    allow_conn_id: bool,
) -> Result<(i64, Option<SlowLogRule>), Error> {
    let mut conn_id = UnsetConnID;
    let raw_rule = raw_rule.trim();
    if raw_rule.is_empty() {
        return Ok((conn_id, None));
    }
    let matches = find_all_field_matches(raw_rule);
    if matches.is_empty() {
        return Err(errors::new(format!(
            "invalid slow log rule format:{raw_rule}"
        )));
    }
    let mut field_map: BTreeMap<String, Threshold> = BTreeMap::new();
    for (g1, g2) in matches {
        let field_name = gostd::to_lower(g1.trim());
        let value = g2.trim();
        let field_value = match parse_slow_log_field_value(&field_name, trim_quotes(value)) {
            Ok(v) => v,
            Err(err) => {
                let mut m = format!("invalid slow log format, value:{value}, err:").into_bytes();
                m.extend(err.error());
                return Err(errors::new(m));
            }
        };
        if gostd::equal_fold(&field_name, SlowLogConnIDStr) {
            if !allow_conn_id {
                return Err(errors::new(format!("do not allow ConnID value:{value}")));
            }
            let Threshold::U64(id) = field_value else {
                unreachable!("conn_id parses as uint64")
            };
            conn_id = id as i64;
        }
        field_map.insert(field_name, field_value);
    }
    let rule = SlowLogRule {
        conditions: field_map
            .into_iter()
            .map(|(field, threshold)| SlowLogCondition { field, threshold })
            .collect(),
    };
    Ok((conn_id, Some(rule)))
}

// go: parseSlowLogRuleSet L1
fn parse_slow_log_rule_set(
    raw_rules: &str,
    allow_conn_id: bool,
) -> Result<Option<BTreeMap<i64, SlowLogRules>>, Error> {
    let raw_rules = raw_rules.trim();
    if raw_rules.is_empty() {
        return Ok(None);
    }
    let rules: Vec<&str> = raw_rules.split(';').collect();
    if rules.len() > 10 {
        return Err(errors::new(format!(
            "invalid slow log rules count:{}, limit is 10",
            rules.len()
        )));
    }
    let mut result: BTreeMap<i64, SlowLogRules> = BTreeMap::new();
    for raw in rules {
        let (conn_id, rule) = parse_slow_log_rule_entry(raw, allow_conn_id)?;
        let Some(rule) = rule else {
            continue;
        };
        let rs = result.entry(conn_id).or_default();
        for cond in &rule.conditions {
            rs.fields.insert(cond.field.clone());
        }
        rs.rules.push(rule);
    }
    Ok(Some(result))
}

/// ParseSessionSlowLogRules parses raw rules into the default (UnsetConnID) slow log rules.
/// Returns nil if no rules for UnsetConnID are found.
// go: ParseSessionSlowLogRules L1
pub fn parse_session_slow_log_rules(raw_rules: &str) -> Result<Option<SlowLogRules>, Error> {
    let Some(mut global_rules) = parse_slow_log_rule_set(raw_rules, false)? else {
        return Ok(None);
    };
    let Some(mut rules) = global_rules.remove(&UnsetConnID) else {
        return Ok(None);
    };
    rules.raw_rules = encode_rules(&rules);
    Ok(Some(rules))
}

// go: encodeRules L1
fn encode_rules(rules: &SlowLogRules) -> String {
    if rules.rules.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    for (i, rule) in rules.rules.iter().enumerate() {
        for (j, cond) in rule.conditions.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&cond.field);
            s.push(':');
            s.push_str(&cond.threshold.format_v());
        }
        if i < rules.rules.len() - 1 {
            s.push(';');
        }
    }
    s
}

/// ParseGlobalSlowLogRules parses raw rules and constructs a GlobalSlowLogRules object.
/// The result contains both the raw string and the rules map keyed by ConnID.
/// allowConnID = true is used here to support both ConnID-bound and default rules.
// go: ParseGlobalSlowLogRules L2 (M-MAPORDER: groups joined in connection-ID order)
pub fn parse_global_slow_log_rules(raw_rules: &str) -> Result<GlobalSlowLogRules, Error> {
    let rules_map = parse_slow_log_rule_set(raw_rules, true)?.unwrap_or_default();
    let raw_slice: Vec<String> = rules_map.values().map(encode_rules).collect();
    let raw_rules = raw_slice.join(";");
    Ok(GlobalSlowLogRules {
        raw_rules_hash: crc64_ecma(raw_rules.as_bytes()),
        raw_rules,
        rules_map,
    })
}

/// `crc64.Checksum(data, crc64.MakeTable(crc64.ECMA))`.
// go-std: hash/crc64.Checksum L1 (ECMA table; Go's slicing-by-8 computes the same value)
pub fn crc64_ecma(data: &[u8]) -> u64 {
    const ECMA: u64 = 0xC96C5795D7870F42;
    static TABLE: LazyLock<[u64; 256]> = LazyLock::new(|| {
        let mut t = [0u64; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut crc = i as u64;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ ECMA
                } else {
                    crc >> 1
                };
            }
            *slot = crc;
        }
        t
    });
    let mut crc = !0u64;
    for &b in data {
        crc = TABLE[((crc as u8) ^ b) as usize] ^ (crc >> 8);
    }
    !crc
}
