//! Replays the govec `resetstmt` vectors (tidb-rs-tracker `govec/domains/resetstmt.go`):
//! `executor.ResetContextOfStmt` (D12.2, ported by VS1-14) after a previous statement with
//! warnings, notes, errors, a last insert id and a plan-cache hit, under random `sql_mode`s.
//! The session and statement-context fields it writes must be Go's: the "warning counts
//! equal to Go" of VS1-14 at the function level (spec `specs/D05.3-session/spec.md` §5).

use std::io::Read;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_executor::select::reset_context_of_stmt;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::variable::session::GlobalVarAccessor;

struct Defaults;

impl GlobalVarAccessor for Defaults {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        Ok(tidb_sessionctx::variable::get_sys_var(name)
            .map(|v| v.value.clone())
            .unwrap_or_default())
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

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn parse(sql: &str) -> tidb_parser::ast::Stmt {
    tidb_parser::parse(sql, &Default::default())
        .unwrap()
        .stmts
        .remove(0)
}

/// The `reset` op.
fn reset(cfg: &Value, prev: &str, sql: &str) -> Value {
    let mut s = Session::new(Dialect::MySQL, Box::new(Defaults));
    let vars = s.vars_mut();
    vars.connection_id = 1;
    vars.set_system_var("sql_mode", cfg["sql_mode"].as_str().unwrap())
        .unwrap();
    vars.skip_utf8_check = cfg["skip_utf8_check"].as_bool().unwrap();
    if !prev.is_empty() {
        reset_context_of_stmt(s.core_mut(), &parse(prev), prev).unwrap();
    }
    let sc = s.vars().stmt_ctx_arc();
    for i in 0..cfg["warnings"].as_u64().unwrap() {
        sc.append_warning(Error::plain(format!("w{i}")));
    }
    for i in 0..cfg["notes"].as_u64().unwrap() {
        sc.append_note(Error::plain(format!("n{i}")));
    }
    for i in 0..cfg["errors"].as_u64().unwrap() {
        sc.append_error(Error::plain(format!("e{i}")));
    }
    sc.set_last_insert_id(cfg["last_insert_id"].as_u64().unwrap());
    drop(sc);
    s.vars_mut().found_in_plan_cache = cfg["found_in_plan_cache"].as_bool().unwrap();
    reset_context_of_stmt(s.core_mut(), &parse(sql), sql).unwrap();
    let vars = s.vars();
    let sc = vars.stmt_ctx();
    let levels: Vec<u8> = sc.err_levels().iter().map(|l| l.0).collect();
    json!({
        "sys_warning_count": vars.sys_warning_count,
        "sys_error_count": vars.sys_error_count,
        "prev_affected_rows": sc.prev_affected_rows,
        "prev_last_insert_id": sc.prev_last_insert_id,
        "prev_found_in_plan_cache": vars.prev_found_in_plan_cache,
        "found_in_plan_cache": vars.found_in_plan_cache,
        "in_select": sc.in_select_stmt,
        "in_show_warning": sc.in_show_warning,
        "warnings": sc.get_warnings().len(),
        "warning_count": sc.warning_count(),
        "type_flags": sc.type_flags().0,
        "err_levels": levels,
        "priority": sc.priority.0,
        "not_fill_cache": sc.not_fill_cache,
        "weak_consistency": sc.weak_consistency,
        "original_sql": sc.original_sql,
        "in_restricted_sql": sc.in_restricted_sql,
    })
}

#[test]
fn govec_resetstmt() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/resetstmt.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "resetstmt");
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        assert_eq!(rec["op"], "reset");
        assert!(rec["err"].is_null());
        let a = rec["args"].as_array().unwrap();
        let got = reset(&a[0], a[1].as_str().unwrap(), a[2].as_str().unwrap());
        assert_eq!(got, rec["out"], "{line}");
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}
