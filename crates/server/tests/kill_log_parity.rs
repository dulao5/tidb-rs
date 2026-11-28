//! Whole-line golden log parity tests for `tidb-server`:
//! - `[kill]` (`pkg/server/server.go:933`),
//! - `"kill initiated"` & `"kill finished"` (`pkg/util/sqlkiller/sqlkiller.go`),
//! - `"write query result panic"` (`pkg/server/conn.go:2364`) across `1317`, `3024`, `8175`, `8176`, `8253`.
//!
//! Dedicated integration test binary (installs global `MemorySink`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::config::LogConfig;
use tidb_base::mysql::TypeLonglong;
use tidb_infoschema::InfoSchemaV2;
use tidb_kv::Context;
use tidb_kv::key::Key;
use tidb_kv::kv::{
    BatchGetOption, BatchGetter, Client, ClientSendOption, GetOption, Getter,
    Iterator as KvIterator, KvFuture, OptionValue, Request, Response, ResultSubset, Retriever,
    Snapshot, Storage, Transaction, TxnOption, ValueEntry,
};
use tidb_kv::version::Version;
use tidb_log::sink::{LogSink, MemorySink};
use tidb_memory::sqlkiller::{
    KillSignal, MaxExecTimeExceeded, QueryInterrupted, QueryMemoryExceeded, RunawayQueryExceeded,
    ServerMemoryExceeded, SqlKiller,
};
use tidb_model::{ColumnInfo, DBInfo, StatePublic, TableInfo};
use tidb_parser::model::CIStr;
use tidb_server::column::ResultEncoder;
use tidb_server::conn::{ResultPackets, write_result_set};
use tidb_server::resultset::ResultSet;
use tidb_server::session_mgr::ServerSessionManager;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::session_mgr::SessionManager;
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_sessiontxn::sctx::{Oracle, TsFuture};

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

type KillHook = Arc<Mutex<Option<(Arc<SqlKiller>, KillSignal)>>>;

struct HookCloseResponse {
    hook: KillHook,
}

impl Response for HookCloseResponse {
    fn next<'a>(&'a mut self, _: &'a Context) -> KvFuture<'a, Option<Box<dyn ResultSubset>>> {
        Box::pin(async { Ok(None) })
    }
    fn close(&mut self) -> Result<(), Error> {
        if let Some((killer, sig)) = self.hook.lock().take() {
            killer.send_kill_signal(sig);
        }
        Ok(())
    }
}

struct MockClient(KillHook);

impl Client for MockClient {
    fn send(
        &self,
        _: &Context,
        _: &Request,
        _: Option<OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        Box::new(HookCloseResponse {
            hook: Arc::clone(&self.0),
        })
    }
    fn is_request_type_supported(&self, _: i64, _: i64) -> bool {
        true
    }
}

struct MockSnapshot;

impl Getter for MockSnapshot {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        Box::pin(async { Err(tidb_kv::error::ErrNotExist.clone()) })
    }
}
impl Retriever for MockSnapshot {
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
impl BatchGetter for MockSnapshot {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        Box::pin(async { Ok(HashMap::new()) })
    }
}
impl Snapshot for MockSnapshot {
    fn set_option(&self, _: isize, _: OptionValue) {}
}

struct MockTxn(u64);

impl Getter for MockTxn {
    fn get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a Key,
        _: &'a [GetOption],
    ) -> KvFuture<'a, ValueEntry> {
        unreachable!()
    }
}
impl Retriever for MockTxn {
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
impl BatchGetter for MockTxn {
    fn batch_get<'a>(
        &'a self,
        _: &'a Context,
        _: &'a [Key],
        _: &'a [BatchGetOption],
    ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
        unreachable!()
    }
}
impl Transaction for MockTxn {
    fn size(&self) -> isize {
        0
    }
    fn len(&self) -> isize {
        0
    }
    fn commit<'a>(&'a mut self, _: &'a Context) -> KvFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn rollback(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn string(&self) -> String {
        format!("txn {}", self.0)
    }
    fn set_option(&mut self, _: isize, _: OptionValue) {}
    fn get_option(&self, _: isize) -> Option<&(dyn std::any::Any + Send + Sync)> {
        None
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn start_ts(&self) -> u64 {
        self.0
    }
    fn valid(&self) -> bool {
        true
    }
    fn get_snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(MockSnapshot)
    }
    fn is_pessimistic(&self) -> bool {
        false
    }
}

struct MockStore(KillHook);

impl Oracle for MockStore {
    fn get_timestamp_async(&self, _: &str) -> TsFuture {
        Box::pin(async { Ok(100) })
    }
    fn get_low_resolution_timestamp_async(&self, s: &str) -> TsFuture {
        self.get_timestamp_async(s)
    }
}

impl Storage for MockStore {
    fn begin<'a>(&'a self, opts: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
        let start_ts = opts
            .iter()
            .find_map(|o| match o {
                TxnOption::StartTS(ts) => Some(*ts),
                _ => None,
            })
            .unwrap_or(100);
        Box::pin(async move { Ok(Box::new(MockTxn(start_ts)) as Box<dyn Transaction>) })
    }
    fn get_snapshot(&self, _: Version) -> Arc<dyn Snapshot> {
        Arc::new(MockSnapshot)
    }
    fn get_client(&self) -> Arc<dyn Client> {
        Arc::new(MockClient(Arc::clone(&self.0)))
    }
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn uuid(&self) -> String {
        "mock".into()
    }
    fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
        unreachable!()
    }
    fn name(&self) -> String {
        "mock".into()
    }
    fn describe(&self) -> String {
        "mock".into()
    }
    fn get_min_safe_ts(&self, _: &str) -> u64 {
        0
    }
}

async fn test_info_schema() -> Arc<InfoSchemaV2> {
    let col = Arc::new(ColumnInfo {
        id: 1,
        name: CIStr::new("a"),
        offset: 0,
        field_type: tidb_types::field_type::new_field_type(TypeLonglong),
        state: StatePublic,
        ..Default::default()
    });
    let tbl = Arc::new(TableInfo {
        id: 10,
        name: CIStr::new("t"),
        columns: Some(vec![col]),
        state: StatePublic,
        ..Default::default()
    });
    let mut db = DBInfo {
        id: 1,
        name: CIStr::new("test"),
        state: StatePublic,
        ..Default::default()
    };
    db.deprecated.tables = Some(vec![tbl]);
    let mut b = tidb_infoschema::new_builder(None, None, tidb_infoschema::new_data());
    b.init_with_db_infos(vec![db], vec![], vec![], 1)
        .await
        .unwrap();
    Arc::new(b.build(1).await)
}

fn normalize_caller(line: &str) -> String {
    assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
    let Some((level_part, rest)) = line.split_once("] [") else {
        return line.to_owned();
    };
    let Some((caller, tail)) = rest.split_once("] ") else {
        return line.to_owned();
    };
    let Some((file, digits)) = caller.rsplit_once(':') else {
        return line.to_owned();
    };
    assert!(!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
    format!("{level_part}] [{file}:<LINE>] {tail}")
}

fn normalize_panic_line(line: &str) -> String {
    let norm = normalize_caller(line);
    let (head, after) = norm
        .split_once(" [stack=\"")
        .unwrap_or_else(|| panic!("missing stack in {norm}"));
    let (stack_body, tail) = after
        .split_once('"')
        .unwrap_or_else(|| panic!("unterminated stack quote in {norm}"));
    assert!(!stack_body.is_empty() && stack_body.contains("\\n"));
    format!("{head} [stack=\"<STACK>\"{tail}")
}

#[tokio::test]
async fn server_kill_and_write_result_set_panic_logs_match_go_whole_line() {
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

    // 1. Verify [kill], "kill initiated", and "kill finished" whole-line parity.
    let mgr = ServerSessionManager::new_standalone();
    let ctrl = mgr.register_new_conn();
    assert_eq!(ctrl.conn_id, 2_097_154);

    let cases: &[(KillSignal, bool, bool, bool, u16, &str)] = &[
        (
            QueryInterrupted,
            true,
            false,
            false,
            1317,
            "[executor:1317]Query execution was interrupted",
        ),
        (
            MaxExecTimeExceeded,
            true,
            true,
            false,
            3024,
            "[executor:3024]Query execution was interrupted, maximum statement execution time exceeded",
        ),
        (
            QueryMemoryExceeded,
            false,
            false,
            false,
            8175,
            "[executor:8175]Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=2097154]",
        ),
        (
            ServerMemoryExceeded,
            false,
            false,
            false,
            8176,
            "[executor:8176]Your query has been cancelled due to exceeding the allowed memory limit for the tidb-server instance and this query is currently using the most memory. Please try narrowing your query scope or increase the tidb_server_memory_limit and try again.[conn=2097154]",
        ),
        (
            RunawayQueryExceeded,
            true,
            false,
            true,
            8253,
            "[executor:8253]Query execution was interrupted, identified as runaway query [runaway exceed tidb side]",
        ),
    ];

    for &(sig, via_mgr_query, max_exec, runaway, code, err_text) in cases {
        let _ = sink.take();
        if sig == QueryInterrupted || sig == MaxExecTimeExceeded || sig == RunawayQueryExceeded {
            mgr.kill(ctrl.conn_id, via_mgr_query, max_exec, runaway);
            ctrl.killer.reset();
            let lines: Vec<String> = String::from_utf8(sink.take())
                .unwrap()
                .lines()
                .map(normalize_caller)
                .collect();
            assert_eq!(
                lines,
                vec![
                    format!(
                        "[INFO] [session_mgr.rs:<LINE>] [kill] [conn=2097154] [query={via_mgr_query}] [maxExecutionTime={max_exec}] [runawayExceed={runaway}]"
                    ),
                    format!(
                        "[WARN] [sqlkiller.rs:<LINE>] [\"kill initiated\"] [\"connection ID\"=2097154] [reason=\"{err_text}\"]"
                    ),
                    "[WARN] [sqlkiller.rs:<LINE>] [\"kill finished\"] [conn=2097154]".to_owned(),
                ]
            );
        } else {
            ctrl.killer.send_kill_signal(sig);
            ctrl.killer.reset();
            let lines: Vec<String> = String::from_utf8(sink.take())
                .unwrap()
                .lines()
                .map(normalize_caller)
                .collect();
            assert_eq!(
                lines,
                vec![
                    format!(
                        "[WARN] [sqlkiller.rs:<LINE>] [\"kill initiated\"] [\"connection ID\"=2097154] [reason=\"{err_text}\"]"
                    ),
                    "[WARN] [sqlkiller.rs:<LINE>] [\"kill finished\"] [conn=2097154]".to_owned(),
                ]
            );
        }
        let _ = code;
    }

    // 2. Verify write_result_set ERROR "write query result panic" across all 5 signals (conn.go:2364).
    let hook: KillHook = Arc::new(Mutex::new(None));
    let store = Arc::new(MockStore(Arc::clone(&hook)));
    let mut se = Session::with_store(
        Dialect::MySQL,
        Box::new(Defaults),
        Arc::clone(&store) as Arc<dyn Storage>,
        store,
    );
    se.vars_mut().connection_id = 2_097_154;
    se.sql_killer().conn_id.store(2_097_154, Ordering::SeqCst);
    se.vars_mut()
        .set_system_var("tidb_session_alias", "alias1")
        .unwrap();
    let is = test_info_schema().await;
    se.set_domain_info_schema(is);

    for &(sig, _, _, _, code, err_text) in cases {
        let sql = "SELECT a FROM test.t";
        let stmt = tidb_parser::parse(sql, &Default::default())
            .unwrap()
            .stmts
            .remove(0);
        let exec_rs = se
            .execute_stmt(stmt, sql)
            .await
            .unwrap()
            .expect("result set");
        let mut rs = ResultSet::new(exec_rs);
        let mut enc: Option<ResultEncoder> = None;
        let mut out = ResultPackets::default();
        *hook.lock() = Some((se.sql_killer(), sig));
        let _ = sink.take();
        let err = write_result_set(&mut se, &mut rs, false, &mut enc, &mut out)
            .await
            .expect_err("must report runtime kill/oom/runaway error");
        assert_eq!(err.code(), code as isize);
        let raw_logs = String::from_utf8(sink.take()).unwrap();
        let panic_lines: Vec<String> = raw_logs
            .lines()
            .filter(|l| l.contains("write query result panic"))
            .map(normalize_panic_line)
            .collect();
        assert_eq!(
            panic_lines,
            vec![format!(
                "[ERROR] [conn.rs:<LINE>] [\"write query result panic\"] [conn=2097154] [session_alias=alias1] [lastSQL=\"SELECT a FROM test.t\"] [stack=\"<STACK>\"] [recover=\"{err_text}\"]"
            )]
        );
        rs.close(&mut se).await.unwrap();
        se.sql_killer().reset();
        let _ = sink.take();
        let _ = AtomicBool::new(false).load(Ordering::SeqCst);
    }
}
