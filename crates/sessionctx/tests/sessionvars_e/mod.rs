//! Replay of the D05.2 PR E ops of the govec domain "sessionvars" (`stmtvars`, `stmthints`;
//! tracker govec/domains/sessionvars_e.go). Included by `govec_sessionvars.rs`.

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_base::contextutil::SQLWarn;
use tidb_chunk::alloc::{Allocator, init_chunk_alloc_size, new_allocator};
use tidb_sessionctx::hint::StmtHints;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::session_methods as sm;

fn list_or_null(v: Vec<Value>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        Value::Array(v)
    }
}

fn err_str(r: Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => json!(String::from_utf8_lossy(&e.error())),
    }
}

fn warns(ws: Vec<SQLWarn>) -> Value {
    list_or_null(
        ws.into_iter()
            .map(|w| json!([w.level, String::from_utf8_lossy(&w.err.error())]))
            .collect(),
    )
}

fn last_sql_use_alloc(vars: &mut SessionVars) -> Value {
    match vars.get_session_or_global_system_var("last_sql_use_alloc") {
        Ok(v) => json!([v, Value::Null]),
        Err(e) => json!([Value::Null, String::from_utf8_lossy(&e.error())]),
    }
}

fn fts() -> Vec<tidb_types::etc::FieldType> {
    vec![
        tidb_types::field_type::new_field_type(tidb_base::mysql::TypeLonglong),
        tidb_types::field_type::new_field_type(tidb_base::mysql::TypeVarchar),
    ]
}

/// The connection's allocator shared with the session, as Go shares the interface value
/// (server/conn.go keeps cc.chunkAlloc; SetAlloc wraps the same object).
struct Shared(Arc<std::sync::Mutex<Box<dyn Allocator>>>);

impl Allocator for Shared {
    fn alloc(
        &mut self,
        fields: &[tidb_types::etc::FieldType],
        capacity: usize,
        max_chunk_size: usize,
    ) -> tidb_chunk::chunk::Chunk {
        self.0
            .lock()
            .unwrap()
            .alloc(fields, capacity, max_chunk_size)
    }
    fn check_reuse_alloc_size(&self) -> bool {
        self.0.lock().unwrap().check_reuse_alloc_size()
    }
    fn reset(&mut self) {
        self.0.lock().unwrap().reset();
    }
}

/// One call of op "stmtvars" (sessionvars_e.go sveCall).
fn call(
    vars: &mut SessionVars,
    conn_alloc: &mut Option<Arc<std::sync::Mutex<Box<dyn Allocator>>>>,
    c: &[Value],
) -> Value {
    let b = |k: usize| c[k].as_bool().unwrap();
    let u = |k: usize| c[k].as_u64().unwrap();
    let s = |k: usize| c[k].as_str().unwrap().to_owned();
    match c[0].as_str().unwrap() {
        "sc" => {
            let field = s(1);
            if field == "warn" {
                vars.stmt_ctx().append_warning(tidb_base::errors::new(s(2)));
                return Value::Null;
            }
            let sc = vars.stmt_ctx_mut();
            let h = &mut sc.stmt_hints;
            match field.as_str() {
                "IsReadOnly" => sc.is_read_only = b(2),
                "RCCheckTS" => sc.rc_check_ts = b(2),
                "InExplainStmt" => sc.in_explain_stmt = b(2),
                "HasAllowInSubqToJoinAndAggHint" => h.has_allow_in_subq_to_join_and_agg_hint = b(2),
                "AllowInSubqToJoinAndAgg" => h.allow_in_subq_to_join_and_agg = b(2),
                "HasEnableCascadesPlannerHint" => h.has_enable_cascades_planner_hint = b(2),
                "EnableCascadesPlanner" => h.enable_cascades_planner = b(2),
                "HasReplicaReadHint" => h.has_replica_read_hint = b(2),
                "ReplicaRead" => h.replica_read = u(2) as u8,
                "HasMaxExecutionTime" => h.has_max_execution_time = b(2),
                "MaxExecutionTime" => h.max_execution_time = u(2),
                f => panic!("bad sc field {f}"),
            }
            Value::Null
        }
        "GetAllowInSubqToJoinAndAgg" => json!(vars.get_allow_in_subq_to_join_and_agg()),
        "GetEnableCascadesPlanner" => json!(vars.get_enable_cascades_planner()),
        "GetReplicaRead" => json!(vars.get_replica_read().0),
        "GetMaxExecutionTime" => json!(vars.get_max_execution_time()),
        "SetLastInsertID" => {
            vars.set_last_insert_id(u(1));
            Value::Null
        }
        "RaiseWarningWhenMPPEnforced" => {
            vars.raise_warning_when_mpp_enforced(&s(1));
            Value::Null
        }
        "NewAllocator" => {
            *conn_alloc = Some(Arc::new(std::sync::Mutex::new(new_allocator())));
            Value::Null
        }
        "SetAlloc" => {
            let a = conn_alloc
                .as_ref()
                .map(|a| Box::new(Shared(Arc::clone(a))) as Box<dyn Allocator>);
            vars.set_alloc(a);
            Value::Null
        }
        "ClearAlloc" => {
            if b(2) {
                // Go: ClearAlloc(&cc.chunkAlloc, hasErr): a pointer to the connection's
                // variable; with hasErr the variable gets a new allocator.
                let mut slot: Box<dyn Allocator> = match conn_alloc {
                    Some(a) => Box::new(Shared(Arc::clone(a))),
                    None => tidb_chunk::alloc::new_empty_allocator(),
                };
                vars.clear_alloc(Some(&mut slot), b(1));
                if b(1) {
                    *conn_alloc = Some(Arc::new(std::sync::Mutex::new(slot)));
                }
                return match conn_alloc {
                    Some(a) => json!(a.lock().unwrap().check_reuse_alloc_size()),
                    None => Value::Null,
                };
            }
            vars.clear_alloc(None, false);
            Value::Null
        }
        "Alloc" => vars.get_chunk_allocator(|a| {
            let chk = a.alloc(&fts(), 1, 32);
            json!([chk.capacity(), chk.required_rows(), chk.num_cols()])
        }),
        "ExchangeChunkStatus" => {
            vars.exchange_chunk_status();
            Value::Null
        }
        "IsAllocValid" => json!(vars.is_alloc_valid()),
        "GetUseChunkAlloc" => json!(vars.get_use_chunk_alloc()),
        "InitChunkAllocSize" => {
            init_chunk_alloc_size(u(1) as u32, u(2) as u32);
            Value::Null
        }
        "InitStatementContext" => {
            let sc = vars.init_statement_context();
            vars.set_stmt_ctx(sc);
            Value::Null
        }
        "last_sql_use_alloc" => last_sql_use_alloc(vars),
        n => panic!("bad call {n}"),
    }
}

fn hints_in(m: &serde_json::Map<String, Value>) -> StmtHints {
    let mut h = StmtHints::default();
    let bo = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    h.mem_quota_query = m.get("MemQuotaQuery").and_then(Value::as_i64).unwrap_or(0);
    h.max_execution_time = m
        .get("MaxExecutionTime")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    h.replica_read = m.get("ReplicaRead").and_then(Value::as_u64).unwrap_or(0) as u8;
    h.allow_in_subq_to_join_and_agg = bo("AllowInSubqToJoinAndAgg");
    h.no_index_merge_hint = bo("NoIndexMergeHint");
    h.straight_join_order = bo("StraightJoinOrder");
    h.enable_cascades_planner = bo("EnableCascadesPlanner");
    h.force_nth_plan = m.get("ForceNthPlan").and_then(Value::as_i64).unwrap_or(0);
    h.resource_group = m
        .get("ResourceGroup")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    h.write_slow_log = bo("WriteSlowLog");
    h.has_allow_in_subq_to_join_and_agg_hint = bo("HasAllowInSubqToJoinAndAggHint");
    h.has_mem_quota_hint = bo("HasMemQuotaHint");
    h.has_replica_read_hint = bo("HasReplicaReadHint");
    h.has_max_execution_time = bo("HasMaxExecutionTime");
    h.has_enable_cascades_planner_hint = bo("HasEnableCascadesPlannerHint");
    h.has_resource_group = bo("HasResourceGroup");
    if let Some(Value::Object(sv)) = m.get("SetVars") {
        h.set_vars = sv
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
    }
    if bo("Hypo") {
        h.hinted_hypo_indexes = Some(Arc::new(()) as Arc<dyn Any + Send + Sync>);
    }
    h
}

/// Go marshals a nil map as null and an empty map as {}: `set_vars_nil` says which.
fn hints_dump(h: &StmtHints, set_vars_nil: bool) -> Value {
    let set_vars = if set_vars_nil {
        Value::Null
    } else {
        let m: serde_json::Map<String, Value> = h
            .set_vars
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        Value::Object(m)
    };
    json!({
        "MemQuotaQuery": h.mem_quota_query, "MaxExecutionTime": h.max_execution_time,
        "ReplicaRead": h.replica_read, "AllowInSubqToJoinAndAgg": h.allow_in_subq_to_join_and_agg,
        "NoIndexMergeHint": h.no_index_merge_hint, "StraightJoinOrder": h.straight_join_order,
        "EnableCascadesPlanner": h.enable_cascades_planner, "ForceNthPlan": h.force_nth_plan,
        "ResourceGroup": h.resource_group, "WriteSlowLog": h.write_slow_log,
        "HasAllowInSubqToJoinAndAggHint": h.has_allow_in_subq_to_join_and_agg_hint,
        "HasMemQuotaHint": h.has_mem_quota_hint, "HasReplicaReadHint": h.has_replica_read_hint,
        "HasMaxExecutionTime": h.has_max_execution_time,
        "HasEnableCascadesPlannerHint": h.has_enable_cascades_planner_hint,
        "HasResourceGroup": h.has_resource_group, "SetVars": set_vars,
        "Hypo": h.hinted_hypo_indexes.is_some(), "TaskMapNeedBackUp": h.task_map_need_back_up(),
    })
}

pub fn exec_e(op: &str, args: &[Value]) -> Option<(Value, Option<Error>)> {
    Some(match op {
        "stmtvars" => {
            sm::set_enable_adaptive_replica_read(true);
            init_chunk_alloc_size(64, 256);
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut vars = super::fresh_vars();
                let none = vec![];
                let sets: Vec<Value> = args[0]
                    .as_array()
                    .unwrap_or(&none)
                    .iter()
                    .map(|p| {
                        err_str(vars.set_system_var(p[0].as_str().unwrap(), p[1].as_str().unwrap()))
                    })
                    .collect();
                let mut conn_alloc: Option<Arc<std::sync::Mutex<Box<dyn Allocator>>>> = None;
                let calls: Vec<Value> = args[1]
                    .as_array()
                    .unwrap_or(&none)
                    .iter()
                    .map(|c| call(&mut vars, &mut conn_alloc, c.as_array().unwrap()))
                    .collect();
                let w = warns(vars.stmt_ctx().get_warnings());
                let x = warns(vars.stmt_ctx().get_extra_warnings());
                json!({"set": list_or_null(sets), "calls": list_or_null(calls), "warns": w,
                    "extra": x, "lastInsertID": vars.stmt_ctx().last_insert_id(),
                    "useChunkAlloc": vars.get_use_chunk_alloc(),
                    "isAllocValid": vars.is_alloc_valid(),
                    "last_sql_use_alloc": last_sql_use_alloc(&mut vars)})
            }));
            sm::set_enable_adaptive_replica_read(true);
            init_chunk_alloc_size(64, 256);
            match res {
                Ok(v) => (v, None),
                Err(p) => std::panic::resume_unwind(p),
            }
        }
        "stmthints" => {
            let m = args[0].as_object().cloned().unwrap_or_default();
            let mut h = hints_in(&m);
            let c = h.clone_hints();
            // Go: the clone has a nil SetVars map unless the original had entries.
            let clone_nil = h.set_vars.is_empty();
            let orig_had_map = m.contains_key("SetVars");
            if orig_had_map {
                h.set_vars.insert("later".to_owned(), "x".to_owned());
            }
            h.force_nth_plan = 99;
            let _: &HashMap<String, String> = &h.set_vars;
            (
                json!({"orig": [h.task_map_need_back_up(), h.hinted_hypo_indexes.is_some()],
                    "clone": hints_dump(&c, clone_nil), "cloneMap": !clone_nil}),
                None,
            )
        }
        _ => return None,
    })
}
