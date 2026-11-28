//! Whole-line golden log parity tests for `KILL` and `* panic` boundaries in `tidb-executor`
//! (`simple.go:2708`, `compiler.go:62`, `adapter.go:539`, `adapter.go:164`) across all five runtime
//! termination errors (`1317`, `3024`, `8175`, `8176`, `8253`).
//! Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;
use std::time::SystemTime;

use tidb_base::config::LogConfig;
use tidb_base::dbterror::exeerrors;
use tidb_base::{Error, go_args};
use tidb_executor::compiler::Compiler;
use tidb_executor::glue::{InfoSchemaRef, Plan, PlanReplayerHandle, SessionContext};
use tidb_executor::simple::execute_kill_stmt;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, ClientSendOption, GetOption, Getter,
    Iterator as KvIterator, KvFuture, OptionValue, Request, Response, Retriever, Snapshot,
    ValueEntry,
};
use tidb_kv::{Context, Key};
use tidb_log::sink::{LogSink, MemorySink};
use tidb_memory::sqlkiller::{
    KillSignal, MaxExecTimeExceeded, QueryInterrupted, QueryMemoryExceeded, RunawayQueryExceeded,
    ServerMemoryExceeded, SqlKiller,
};
use tidb_planner::core::common_plans::SimpleKillStmt;
use tidb_planner::planctx::SessionPlanCtx;
use tidb_sessionctx::distsql_ctx::DistSQLContext;
use tidb_sessionctx::exprctx::ExprContextRef;
use tidb_sessionctx::exprstatic::new_expr_context;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

struct NoGlobals;

impl GlobalVarAccessor for NoGlobals {
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

struct StubKvClient;

impl Client for StubKvClient {
    fn send(
        &self,
        _: &Context,
        _: &Request,
        _: Option<OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("no KV RPCs run in Select 1 / Compile / ExecStmt parity checks")
    }

    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

struct StubSnapshot;

impl Getter for StubSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        unreachable!()
    }
}
impl Retriever for StubSnapshot {
    fn iter<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unreachable!()
    }
    fn iter_reverse<'a: 'f, 'f>(
        &'a self,
        _: &'f Key,
        _: &'f Key,
    ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
        unreachable!()
    }
}
impl BatchGetter for StubSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, std::collections::HashMap<Vec<u8>, ValueEntry>> {
        unreachable!()
    }
}
impl Snapshot for StubSnapshot {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

struct KillLogSession {
    vars: SessionVars,
    killer: Arc<SqlKiller>,
    expr_ctx: ExprContextRef,
    is: InfoSchemaRef,
    advise_err: Option<Error>,
    txn_start_ts_err: Option<Error>,
}

impl KillLogSession {
    fn new(conn_id: u64, alias: &str, is: InfoSchemaRef) -> Self {
        let mut vars = SessionVars::new(Box::new(NoGlobals));
        vars.connection_id = conn_id;
        vars.session_alias = alias.to_owned();
        vars.stmt_ctx_mut().original_sql = "SELECT 1".to_owned();
        let killer = SqlKiller::new();
        killer
            .conn_id
            .store(conn_id, std::sync::atomic::Ordering::SeqCst);
        Self {
            vars,
            killer,
            expr_ctx: new_expr_context(Vec::new()),
            is,
            advise_err: None,
            txn_start_ts_err: None,
        }
    }
}

impl SessionContext for KillLogSession {
    fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }
    fn sql_killer(&self) -> Arc<SqlKiller> {
        Arc::clone(&self.killer)
    }
    fn get_stmt_read_ts(&mut self) -> futures::future::BoxFuture<'_, Result<u64, Error>> {
        Box::pin(async { Ok(100) })
    }
    fn get_txn_info_schema(&mut self) -> Option<InfoSchemaRef> {
        Some(Arc::clone(&self.is))
    }
    fn get_txn_scope(&self) -> String {
        "global".to_owned()
    }
    fn get_plan_replayer_handle(&self) -> Option<Arc<dyn PlanReplayerHandle>> {
        None
    }
    fn get_read_replica_scope(&self) -> String {
        "global".to_owned()
    }
    fn is_stmt_staleness(&self) -> bool {
        false
    }
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error> {
        match self.txn_start_ts_err.take() {
            Some(err) => Err(err),
            None => Ok(Some(100)),
        }
    }
    fn set_process_info(&mut self, _: &str, _: SystemTime, _: u8, _: u64) {}
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, _: &Plan) -> Result<(), Error> {
        match self.advise_err.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
    fn get_expr_ctx(&self) -> ExprContextRef {
        Arc::clone(&self.expr_ctx)
    }
    fn get_dist_sql_ctx(&self) -> Arc<DistSQLContext> {
        let sc = self.vars.stmt_ctx();
        Arc::new(DistSQLContext {
            enable_chunk_rpc: self.vars.enable_chunk_rpc,
            dist_sql_concurrency: self.vars.dist_sql_scan_concurrency() as isize,
            replica_read_type: self.vars.get_replica_read(),
            enable_paging: self.vars.enable_paging,
            min_paging_size: self.vars.min_paging_size as isize,
            max_paging_size: self.vars.max_paging_size as isize,
            replica_closest_read_threshold: self.vars.replica_closest_read_threshold,
            runtime_stats_coll: sc.runtime_stats_coll.clone(),
            location: self.vars.location(),
            err_ctx: sc.err_ctx(),
            ..Default::default()
        })
    }
    fn get_build_pb_ctx(&self) -> Result<Arc<BuildPBContext>, Error> {
        let sc = self.vars.stmt_ctx();
        Ok(Arc::new(BuildPBContext {
            expr_ctx: Arc::clone(&self.expr_ctx) as tidb_sessionctx::exprctx::BuildContextRef,
            client: Arc::new(StubKvClient),
            tiflash_fast_scan: self.vars.tiflash_fast_scan,
            tiflash_fine_grained_shuffle_batch_size: self
                .vars
                .tiflash_fine_grained_shuffle_batch_size,
            group_concat_max_len: self.vars.group_concat_max_len,
            in_explain_stmt: sc.in_explain_stmt,
            warn_handler: Some(sc.warn_handler() as tidb_base::contextutil::WarnAppenderRef),
            extra_warnghandler: Some(
                Arc::clone(&sc.extra_warn_handler) as tidb_base::contextutil::WarnAppenderRef
            ),
        }))
    }
    fn get_ranger_ctx(&self) -> Arc<RangerContext> {
        let sc = self.vars.stmt_ctx();
        Arc::new(RangerContext {
            type_ctx: sc.type_ctx(),
            err_ctx: sc.err_ctx(),
            expr_ctx: Arc::clone(&self.expr_ctx) as tidb_sessionctx::exprctx::BuildContextRef,
            range_fallback_handler: Arc::clone(&sc.range_fallback_handler),
            plan_cache_tracker: Arc::clone(&sc.plan_cache_tracker),
            optimizer_fix_control: self.vars.get_optimizer_fix_control_map().clone(),
            use_cache: false,
            in_prepared_plan_building: sc.in_prepared_plan_building,
            regard_null_as_point: self.vars.regard_null_as_point,
            opt_prefix_index_single_scan: self.vars.opt_prefix_index_single_scan,
        })
    }
    fn get_info_schema(&self) -> Option<InfoSchemaRef> {
        Some(Arc::clone(&self.is))
    }
    fn get_plan_ctx(&self) -> SessionPlanCtx {
        SessionPlanCtx {
            expr_ctx: Arc::clone(&self.expr_ctx),
            client: None,
            has_dirty_content: None,
            advise_txn_warmup: None,
            domain_info_schema: Some(Arc::clone(&self.is) as _),
            instance_plan_cache: None,
            privilege_manager: None,
            session_manager: None,
        }
    }
    fn get_snapshot_with_stmt_read_ts(
        &mut self,
    ) -> futures::future::BoxFuture<'_, Result<Arc<dyn Snapshot>, Error>> {
        Box::pin(async { Ok(Arc::new(StubSnapshot) as Arc<dyn Snapshot>) })
    }
}

async fn empty_info_schema() -> InfoSchemaRef {
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    b.init_with_db_infos(vec![], vec![], vec![], 1)
        .await
        .unwrap();
    Arc::new(b.build(1).await)
}

fn normalize_caller(line: &str, expected_file: &str) -> String {
    assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
    let pat = format!("[{expected_file}:");
    let Some((head, tail)) = line.split_once(&pat) else {
        panic!("expected caller file {expected_file} in {line:?}");
    };
    let (digits, rest) = tail.split_once(']').unwrap();
    assert!(!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
    format!("{head}[{expected_file}:<LINE>]{rest}")
}

fn normalize_stack_line(line: &str, expected_file: &str) -> String {
    let norm = normalize_caller(line, expected_file);
    let (head, after) = norm
        .split_once(" [stack=\"")
        .unwrap_or_else(|| panic!("missing [stack=\"...\"] in {norm}"));
    let (stack_body, tail) = after
        .split_once('"')
        .unwrap_or_else(|| panic!("unterminated stack quote in {norm}"));
    assert!(
        !stack_body.is_empty() && stack_body.contains("\\n"),
        "expected non-empty stack frames in {stack_body:?}"
    );
    format!("{head} [stack=\"<STACK>\"{tail}")
}

fn all_runtime_cases(conn_id: u64) -> [(KillSignal, Error, &'static str); 5] {
    [
        (
            QueryInterrupted,
            exeerrors::ErrQueryInterrupted.gen_with_stack_by_args(vec![]),
            "[executor:1317]Query execution was interrupted",
        ),
        (
            MaxExecTimeExceeded,
            exeerrors::ErrMaxExecTimeExceeded.gen_with_stack_by_args(vec![]),
            "[executor:3024]Query execution was interrupted, maximum statement execution time exceeded",
        ),
        (
            QueryMemoryExceeded,
            exeerrors::ErrMemoryExceedForQuery.gen_with_stack_by_args(go_args![conn_id]),
            "[executor:8175]Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=42]",
        ),
        (
            ServerMemoryExceeded,
            exeerrors::ErrMemoryExceedForInstance.gen_with_stack_by_args(go_args![conn_id]),
            "[executor:8176]Your query has been cancelled due to exceeding the allowed memory limit for the tidb-server instance and this query is currently using the most memory. Please try narrowing your query scope or increase the tidb_server_memory_limit and try again.[conn=42]",
        ),
        (
            RunawayQueryExceeded,
            exeerrors::ErrResourceGroupQueryRunawayInterrupted
                .fast_gen_by_args(go_args!["runaway exceed tidb side"]),
            "[executor:8253]Query execution was interrupted, identified as runaway query [runaway exceed tidb side]",
        ),
    ]
}

#[tokio::test]
async fn executor_kill_and_panic_logs_match_go_whole_line() {
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "info".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&cfg, "", Arc::clone(&sink) as Arc<dyn LogSink>, None)
        .unwrap();
    tidb_base::config::update_global_config(|c| c.enable_global_kill = true);

    // 1. Truncated 32-bit connection ID WARN (pkg/executor/simple.go:2708).
    let truncated_stmt = SimpleKillStmt {
        query: false,
        connection_id: 99_999_999,
        tidb_extension: false,
        expr_func_name: None,
        expr_invalid: false,
    };
    let sc = tidb_sessionctx::stmtctx::new_stmt_ctx();
    execute_kill_stmt(Some(&sc), 42, None, false, "127.0.0.1", &truncated_stmt)
        .await
        .unwrap();
    // Without a SessionManager, enable_global_kill returns early; pass a minimal SessionManager
    // to exercise the exact truncated-ID path.
    struct DummyMgr;
    impl tidb_sessionctx::session_mgr::SessionManager for DummyMgr {
        fn show_process_list(
            &self,
        ) -> std::collections::HashMap<u64, Arc<tidb_sessionctx::session_mgr::ProcessInfo>>
        {
            Default::default()
        }
        fn get_process_info(
            &self,
            _: u64,
        ) -> Option<Arc<tidb_sessionctx::session_mgr::ProcessInfo>> {
            None
        }
        fn kill(&self, _: u64, _: bool, _: bool, _: bool) {}
        fn kill_all_connections(&self) {}
        fn server_id(&self) -> u64 {
            1
        }
    }
    let mgr = DummyMgr;
    let _ = sink.take();
    execute_kill_stmt(
        Some(&sc),
        42,
        Some(&mgr),
        false,
        "127.0.0.1",
        &truncated_stmt,
    )
    .await
    .unwrap();
    let trunc_log = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(trunc_log.trim_end_matches('\n'), "simple.rs"),
        "[WARN] [simple.rs:<LINE>] [\"Kill failed: Received a 32bits truncated ConnectionID, expect 64bits. Please execute 'KILL [CONNECTION | QUERY] ConnectionID' to send a Kill without truncating ConnectionID.\"] [conn=99999999]"
    );

    let is = empty_info_schema().await;
    let parse_select = || {
        tidb_parser::parse("SELECT 1", &Default::default())
            .unwrap()
            .stmts
            .remove(0)
    };

    for (sig, err, err_text) in all_runtime_cases(42) {
        // 2. Compiler::compile panic log (pkg/executor/compiler.go:62).
        let mut s = KillLogSession::new(42, "alias1", Arc::clone(&is));
        s.advise_err = Some(err.clone());
        let _ = sink.take();
        let compile_err = Compiler
            .compile(&mut s, parse_select(), "SELECT 1")
            .err()
            .unwrap();
        assert_eq!(compile_err.code(), err.code());
        let line = String::from_utf8(sink.take()).unwrap();
        assert_eq!(
            normalize_stack_line(line.trim_end_matches('\n'), "compiler.rs"),
            format!(
                "[ERROR] [compiler.rs:<LINE>] [\"compile SQL panic\"] [conn=42] [session_alias=alias1] [SQL=\"SELECT 1\"] [stack=\"<STACK>\"] [recover=\"{err_text}\"]"
            )
        );

        // 3. ExecStmt::exec panic log (pkg/executor/adapter.go:539).
        let mut s = KillLogSession::new(42, "alias1", Arc::clone(&is));
        let mut stmt = Compiler
            .compile(&mut s, parse_select(), "SELECT 1")
            .unwrap();
        s.txn_start_ts_err = Some(err.clone());
        let _ = sink.take();
        let exec_err = stmt.exec(&mut s).await.err().unwrap();
        assert_eq!(exec_err.code(), err.code());
        let line = String::from_utf8(sink.take()).unwrap();
        assert_eq!(
            normalize_stack_line(line.trim_end_matches('\n'), "adapter.rs"),
            "[WARN] [adapter.rs:<LINE>] [\"execute sql panic\"] [conn=42] [session_alias=alias1] [sql=\"SELECT 1\"] [stack=\"<STACK>\"]"
        );

        // 4. RecordSet::next panic log (pkg/executor/adapter.go:164).
        let mut s = KillLogSession::new(42, "alias1", Arc::clone(&is));
        let mut stmt = Compiler
            .compile(&mut s, parse_select(), "SELECT 1")
            .unwrap();
        let mut rs = stmt.exec(&mut s).await.unwrap().expect("record set");
        let mut chk = rs.new_chunk(None);
        s.killer.send_kill_signal(sig);
        let _ = sink.take(); // isolate RecordSet::next output after kill initiated
        let next_err = rs.next(&mut stmt, &mut s, &mut chk).await.unwrap_err();
        assert_eq!(next_err.code(), err.code());
        let lines: Vec<String> = String::from_utf8(sink.take())
            .unwrap()
            .lines()
            .filter(|l| !l.contains("global memory controller, NeedKill"))
            .map(|l| normalize_stack_line(l, "adapter.rs"))
            .collect();
        assert_eq!(
            lines,
            vec![
                "[WARN] [adapter.rs:<LINE>] [\"execute sql panic\"] [conn=42] [session_alias=alias1] [sql=\"SELECT 1\"] [stack=\"<STACK>\"]"
            ]
        );
        rs.close(&mut stmt, &mut s).await.unwrap();
        s.killer.reset();
    }
}
