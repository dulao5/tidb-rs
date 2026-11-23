//! Replays the govec `plancache` vectors (tidb-rs-tracker `govec/domains/plancache.go`):
//! block D13.1 (the plan cache), the cacheability check of a prepared statement
//! (`plan_cacheable_checker.go` `IsASTCacheable`), against Go TiDB v8.5.8. The cases are
//! those of `plan_cacheable_checker_test.go` `TestCacheable` / `TestFixControl44823`
//! written as SQL, and every reason of the checker.
//!
//! Each record: the session switches, the tables (TableInfo JSON, database `test`) and the
//! SQL; the output is the cacheable flag and the reason (the text of the
//! `skip prepared plan-cache: ...` warning), compared byte for byte. Records whose Rust result
//! is a boundary stub are counted per block and pinned below (constitution §7.3): DML and
//! statements the Rust AST does not have (D02.2), optimizer hints (D13.3). Records Go parses
//! and the Rust parser does not are counted and pinned too (the Rust grammar's SELECT subset).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::gofmt::Arg;
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::plan_cache_utils::{InfoSchemaLookup, check_types_compatibility4_pc};
use tidb_planner::core::plan_cacheable_checker::is_ast_cacheable;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

/// The number of records answered by each boundary stub.
const EXPECTED_BLOCKED: [(&str, usize); 2] = [("D02.2", 16), ("D13.3", 2)];
/// The number of records Go parses and the Rust parser does not.
const EXPECTED_UNPARSED: usize = 0;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Go's `infoschema.MockInfoSchema(tables)`: every table in database `test`.
struct MockIs(HashMap<String, Arc<TableInfo>>);

impl InfoSchemaLookup for MockIs {
    fn table_by_name(&self, schema: &CIStr, table: &CIStr) -> Result<Arc<TableInfo>, Error> {
        if schema.l == "test"
            && let Some(t) = self.0.get(&table.l)
        {
            return Ok(Arc::clone(t));
        }
        // ErrTableNotExists.FastGenByArgs(schema, table)
        Err(
            tidb_infoschema::error::ErrTableNotExists.fast_gen_by_args(vec![
                Arg::Stringer(schema.o.clone().into_bytes(), "model.CIStr"),
                Arg::Stringer(table.o.clone().into_bytes(), "model.CIStr"),
            ]),
        )
    }
    fn table_by_id(&self, _: i64) -> Result<Option<Arc<TableInfo>>, Error> {
        Ok(None)
    }
    fn schema_name_by_id(&self, _: i64) -> Option<CIStr> {
        None
    }
    fn schema_meta_version(&self) -> i64 {
        0
    }
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(MockIs(self.0.clone()))
    }
}

enum Outcome {
    Value(Value),
    Unparsed,
    Blocked(String),
}

/// The field types of a `types_compat` record (`{tp, flag, flen, dec, cs, coll}` each).
fn field_types(v: &Value) -> Vec<tidb_parser::types::FieldType> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let mut t = tidb_parser::types::FieldType::new(f["tp"].as_u64().unwrap() as u8);
            t.set_flag(f["flag"].as_u64().unwrap() as usize);
            t.set_flen(f["flen"].as_i64().unwrap() as isize);
            t.set_decimal(f["dec"].as_i64().unwrap() as isize);
            t.set_charset(f["cs"].as_str().unwrap());
            t.set_collate(f["coll"].as_str().unwrap());
            t
        })
        .collect()
}

fn run(args: &[Value]) -> Outcome {
    let sess = &args[0];
    let sql = args[2].as_str().unwrap();
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let tables = args[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut ti = TableInfo::default();
            ti.unmarshal_json(t.as_str().unwrap().as_bytes()).unwrap();
            (ti.name.l.clone(), Arc::new(ti))
        })
        .collect();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = sess["db"].as_str().unwrap().to_owned();
    vars.enable_plan_cache_for_param_limit = sess["param_limit"].as_bool().unwrap();
    vars.enable_plan_cache_for_subquery = sess["subquery"].as_bool().unwrap();
    vars.state.partition_prune_mode = if sess
        .get("static_prune")
        .is_some_and(|v| v.as_bool().unwrap())
    {
        "static".to_owned()
    } else {
        "dynamic".to_owned()
    };
    if let Some(fix) = sess.get("fix").and_then(Value::as_object) {
        for (k, v) in fix {
            vars.state
                .optimizer_fix_control
                .insert(k.parse().unwrap(), v.as_str().unwrap().to_owned());
        }
    }
    match is_ast_cacheable(&vars, &stmt, Some(&MockIs(tables))) {
        Ok((c, r)) => Outcome::Value(json!({"c": c, "r": r})),
        Err(e) => match blocked_by(&e) {
            Some(b) => Outcome::Blocked(b.to_owned()),
            None => panic!("{sql}: {e}"),
        },
    }
}

#[test]
fn govec_plancache() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/plancache.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "plancache");
    let (mut matched, mut unparsed, mut fails) = (0, Vec::new(), Vec::new());
    let mut blocked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut n = 0;
    let (mut types_matched, mut types_fails) = (0, Vec::new());
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().unwrap();
        if rec["op"] == "types_compat" {
            let got = check_types_compatibility4_pc(&field_types(&args[0]), &field_types(&args[1]));
            if Value::Bool(got) == rec["out"] {
                types_matched += 1;
            } else {
                types_fails.push(format!("{args:?}: go {}, got {got}", rec["out"]));
            }
            continue;
        }
        let sql = args[2].as_str().unwrap();
        if rec["out"].get("parse").is_some() {
            // Go does not parse it either: nothing to compare.
            continue;
        }
        match run(args) {
            Outcome::Unparsed => unparsed.push(sql.to_string()),
            Outcome::Blocked(b) => blocked.entry(b).or_default().push(sql.to_string()),
            Outcome::Value(got) if got == rec["out"] => matched += 1,
            Outcome::Value(got) => fails.push(format!(
                "{sql} {}\n    go  {}\n    got {got}",
                args[0], rec["out"]
            )),
        }
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    eprintln!("plancache types_compat: {types_matched} matched");
    assert!(
        types_fails.is_empty(),
        "{} types_compat records differ:\n{}",
        types_fails.len(),
        types_fails.join("\n")
    );
    eprintln!(
        "plancache: {matched} matched, {} blocked ({}), {} not parsed by the Rust parser",
        blocked.values().map(Vec::len).sum::<usize>(),
        blocked
            .iter()
            .map(|(b, v)| format!("{b}: {}", v.len()))
            .collect::<Vec<_>>()
            .join(", "),
        unparsed.len()
    );
    assert!(
        fails.is_empty(),
        "{} of {n} records differ:\n{}",
        fails.len(),
        fails.join("\n")
    );
    assert_eq!(
        unparsed.len(),
        EXPECTED_UNPARSED,
        "Go parses, Rust does not: {unparsed:?}"
    );
    let counts: BTreeMap<&str, usize> =
        blocked.iter().map(|(b, v)| (b.as_str(), v.len())).collect();
    assert_eq!(
        counts,
        BTreeMap::from(EXPECTED_BLOCKED),
        "blocked records: {blocked:?}"
    );
}
