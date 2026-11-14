//! Tests of `ExecStmt` / `recordSet` and the shared test doubles of the crate: a session
//! ([`MockSession`]), a physical plan that builds its own executor ([`MockPhysicalPlan`], Go's
//! `testutil.MockPhysicalPlan`), and executors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::{HighPriority, TypeLonglong};
use tidb_chunk::chunk::Chunk;
use tidb_expression::Column;
use tidb_expression::schema::new_schema;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::model::CIStr;
use tidb_planner::{
    PhysicalKind, PhysicalPlan, PhysicalPlanRef, Schema, physical_plan_boilerplate,
};
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_name::FieldName;
use tidb_types::field_type::new_field_type;

use super::*;
use crate::exec::ExecCtx;
use crate::glue::{InfoSchemaRef, Plan, PlanCacheStmt, PointGetExecutor, SessionContext};
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor_v2,
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

/// A session whose transaction manager reads at `read_ts` (`u64::MAX`: the MaxTS of an
/// autocommit point get).
pub(crate) struct MockSession {
    pub vars: SessionVars,
    pub killer: Arc<SqlKiller>,
    pub read_ts: u64,
    pub process_info: Vec<String>,
}

impl MockSession {
    pub(crate) fn new(read_ts: u64) -> MockSession {
        MockSession {
            vars: SessionVars::new(Box::new(Acc)),
            killer: SqlKiller::new(),
            read_ts,
            process_info: Vec::new(),
        }
    }
}

impl SessionContext for MockSession {
    fn get_session_vars(&self) -> &SessionVars {
        &self.vars
    }
    fn get_session_vars_mut(&mut self) -> &mut SessionVars {
        &mut self.vars
    }
    fn sql_killer(&self) -> Arc<SqlKiller> {
        self.killer.clone()
    }
    fn get_stmt_read_ts(&mut self) -> futures::future::BoxFuture<'_, Result<u64, Error>> {
        let ts = self.read_ts;
        Box::pin(async move { Ok(ts) })
    }
    fn get_txn_info_schema(&self) -> Option<InfoSchemaRef> {
        None
    }
    fn get_txn_scope(&self) -> String {
        "global".to_owned()
    }
    fn get_read_replica_scope(&self) -> String {
        "global".to_owned()
    }
    fn is_stmt_staleness(&self) -> bool {
        false
    }
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error> {
        Ok(Some(self.read_ts))
    }
    fn set_process_info(&mut self, sql: &str, _: SystemTime, _: u8, _: u64) {
        self.process_info.push(sql.to_owned());
    }
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, _: &Plan) -> Result<(), Error> {
        Ok(())
    }
}

fn col(id: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: id,
        ret_type: new_field_type(TypeLonglong),
        ..Column::default()
    })
}

/// Counts executor builds and point get re-inits.
#[derive(Default)]
pub(crate) struct Counters {
    pub built: AtomicUsize,
    pub inits: AtomicUsize,
}

/// Go's `testutil.MockPhysicalPlan`: builds its executor itself (`GetExecutor`). `kind` is
/// what the plan reports.
#[derive(Clone)]
pub(crate) struct MockPhysicalPlan {
    id: i32,
    kind: PhysicalKind,
    schema: Schema,
    children: Vec<PhysicalPlanRef>,
    rows: i64,
    point_get: bool,
    counters: Arc<Counters>,
}

impl MockPhysicalPlan {
    pub(crate) fn get_executor(&self) -> Box<dyn Executor> {
        self.counters.built.fetch_add(1, Ordering::SeqCst);
        let vars = SessionVars::new(Box::new(Acc));
        Box::new(Numbers {
            base: new_base_executor_v2(
                &vars,
                &SqlKiller::new(),
                Some(self.schema.clone()),
                self.id as isize,
                Vec::new(),
            ),
            next: 0,
            n: self.rows,
            point_get: self.point_get,
            counters: self.counters.clone(),
        })
    }
}

impl PhysicalPlan for MockPhysicalPlan {
    // A mock is matched before the kinds (Go's `default` branch).
    fn kind(&self) -> PhysicalKind {
        self.kind
    }
    fn id(&self) -> i32 {
        self.id
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
    fn children(&self) -> &[PhysicalPlanRef] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef> {
        &mut self.children
    }
    fn clone_node(&self) -> PhysicalPlanRef {
        Arc::new(self.clone())
    }
}

/// A plain plan of `kind` (no executor of its own).
#[derive(Clone)]
struct KindPlan {
    id: i32,
    kind: PhysicalKind,
    schema: Schema,
    children: Vec<PhysicalPlanRef>,
}

impl PhysicalPlan for KindPlan {
    fn kind(&self) -> PhysicalKind {
        self.kind
    }
    fn id(&self) -> i32 {
        self.id
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn schema(&self) -> &Schema {
        &self.schema
    }
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
    fn children(&self) -> &[PhysicalPlanRef] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef> {
        &mut self.children
    }
    fn clone_node(&self) -> PhysicalPlanRef {
        Arc::new(self.clone())
    }
}

#[allow(dead_code)]
// glue: keeps the boilerplate macro in use for a plain operator
fn _boilerplate_compiles() {
    #[derive(Clone)]
    struct P {
        id: i32,
        schema: Schema,
        children: Vec<PhysicalPlanRef>,
    }
    impl PhysicalPlan for P {
        physical_plan_boilerplate!(TableDual);
    }
}

pub(crate) fn test_plan(kind: PhysicalKind) -> Plan {
    Arc::new(KindPlan {
        id: 1,
        kind,
        schema: new_schema(vec![col(1)]),
        children: Vec::new(),
    })
}

fn mock_plan(rows: i64, columns: usize, point_get: bool, counters: &Arc<Counters>) -> Plan {
    Arc::new(MockPhysicalPlan {
        id: 3,
        kind: PhysicalKind::PointGet,
        schema: new_schema((0..columns as i64).map(col).collect()),
        children: Vec::new(),
        rows,
        point_get,
        counters: counters.clone(),
    })
}

/// Produces 0..n, one row per Next; a point get executor when `point_get`.
struct Numbers {
    base: BaseExecutorV2,
    next: i64,
    n: i64,
    point_get: bool,
    counters: Arc<Counters>,
}

impl PointGetExecutor for Numbers {
    fn init(&mut self, _plan: &Plan) -> Result<(), Error> {
        self.counters.inits.fetch_add(1, Ordering::SeqCst);
        self.next = 0;
        Ok(())
    }
    fn is_partitioned(&self) -> bool {
        false
    }
}

#[async_trait]
impl Executor for Numbers {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        if self.next < self.n && !self.base.meta.ret_field_types().is_empty() {
            req.append_int64(0, self.next);
        }
        self.next += 1;
        Ok(())
    }
    fn as_point_get(&mut self) -> Option<&mut dyn PointGetExecutor> {
        if self.point_get { Some(self) } else { None }
    }
}

async fn drain(rs: &mut RecordSet, stmt: &mut ExecStmt, s: &mut MockSession) -> Vec<i64> {
    let mut out = Vec::new();
    let mut chk = rs.new_chunk(None);
    loop {
        rs.next(stmt, s, &mut chk).await.unwrap();
        if chk.num_rows() == 0 {
            return out;
        }
        out.push(chk.get_row(0).get_int64(0));
    }
}

fn point_stmt(plan: Plan) -> ExecStmt {
    let mut stmt = ExecStmt::new("select c from t where id = 1".to_owned(), Some(plan), None);
    stmt.ps_stmt = Some(Arc::new(parking_lot::Mutex::new(PlanCacheStmt::default())));
    stmt
}

/// The MaxTS decision of `ExecStmt.PointGet` (`useMaxTS := startTs == math.MaxUint64`): with
/// MaxTS the point get executor is cached in the prepared statement and reused (re-init) by
/// the next execution; the statement runs at high priority.
#[tokio::test]
async fn point_get_with_max_ts_caches_the_executor() {
    let counters = Arc::new(Counters::default());
    let plan = mock_plan(2, 1, true, &counters);
    let mut s = MockSession::new(u64::MAX);
    let mut stmt = point_stmt(plan);

    let mut rs = stmt.point_get(&mut s).await.unwrap();
    assert_eq!(s.vars.stmt_ctx().priority, HighPriority);
    assert_eq!(rs.txn_start_ts(), u64::MAX);
    assert_eq!(drain(&mut rs, &mut stmt, &mut s).await, vec![0, 1]);
    assert_eq!(s.vars.last_found_rows, 2);
    rs.close(&mut stmt, &mut s).await.unwrap();
    assert!(
        stmt.ps_stmt
            .as_ref()
            .unwrap()
            .lock()
            .point_get_executor
            .is_some()
    );
    assert_eq!(counters.built.load(Ordering::SeqCst), 1);

    // The second execution reuses the cached executor.
    let mut rs = stmt.point_get(&mut s).await.unwrap();
    assert_eq!(drain(&mut rs, &mut stmt, &mut s).await, vec![0, 1]);
    rs.close(&mut stmt, &mut s).await.unwrap();
    assert_eq!(counters.built.load(Ordering::SeqCst), 1);
    assert_eq!(counters.inits.load(Ordering::SeqCst), 1);
    assert_eq!(s.process_info.len(), 2);
}

/// With a TSO read ts the executor is neither cached nor reused, even if the cache has one.
#[tokio::test]
async fn point_get_with_a_tso_does_not_cache() {
    let counters = Arc::new(Counters::default());
    let plan = mock_plan(1, 1, true, &counters);
    let mut s = MockSession::new(449_000_000_000_000_000);
    let mut stmt = point_stmt(plan.clone());
    stmt.ps_stmt.as_ref().unwrap().lock().point_get_executor = Some(
        plan.as_any()
            .downcast_ref::<MockPhysicalPlan>()
            .unwrap()
            .get_executor(),
    );
    let built_before = counters.built.load(Ordering::SeqCst);

    let mut rs = stmt.point_get(&mut s).await.unwrap();
    assert_eq!(drain(&mut rs, &mut stmt, &mut s).await, vec![0]);
    rs.close(&mut stmt, &mut s).await.unwrap();
    assert_eq!(counters.built.load(Ordering::SeqCst), built_before + 1);
    assert_eq!(counters.inits.load(Ordering::SeqCst), 0);
    // The cache entry is left alone (it is only read at MaxTS).
    assert!(
        stmt.ps_stmt
            .as_ref()
            .unwrap()
            .lock()
            .point_get_executor
            .is_some()
    );
}

/// A table dual (not a point get executor) is never cached, even at MaxTS.
#[tokio::test]
async fn point_get_does_not_cache_other_executors() {
    let counters = Arc::new(Counters::default());
    let plan = mock_plan(0, 1, false, &counters);
    let mut s = MockSession::new(u64::MAX);
    let mut stmt = point_stmt(plan);
    let mut rs = stmt.point_get(&mut s).await.unwrap();
    assert!(drain(&mut rs, &mut stmt, &mut s).await.is_empty());
    rs.close(&mut stmt, &mut s).await.unwrap();
    assert!(
        stmt.ps_stmt
            .as_ref()
            .unwrap()
            .lock()
            .point_get_executor
            .is_none()
    );
}

#[tokio::test]
async fn exec_returns_a_record_set_or_runs_without_delay() {
    let counters = Arc::new(Counters::default());
    let mut s = MockSession::new(42);
    let mut stmt = ExecStmt::new(
        "select".to_owned(),
        Some(mock_plan(3, 1, false, &counters)),
        None,
    );
    let mut rs = stmt.exec(&mut s).await.unwrap().expect("a record set");
    assert_eq!(rs.txn_start_ts(), 42);
    assert_eq!(drain(&mut rs, &mut stmt, &mut s).await, vec![0, 1, 2]);
    assert_eq!(s.vars.stmt_ctx().found_rows(), 3);
    rs.close(&mut stmt, &mut s).await.unwrap();
    assert!(
        stmt.phase_build_durations[0] > std::time::Duration::ZERO
            || counters.built.load(Ordering::SeqCst) == 1
    );

    // An executor without columns runs in Exec and returns no record set.
    let mut stmt = ExecStmt::new(
        "do".to_owned(),
        Some(mock_plan(3, 0, false, &counters)),
        None,
    );
    assert!(stmt.exec(&mut s).await.unwrap().is_none());

    // A plan without an executor yet fails with its block.
    let mut stmt = ExecStmt::new(
        "select".to_owned(),
        Some(test_plan(PhysicalKind::Sort)),
        None,
    );
    let err = stmt.exec(&mut s).await.err().unwrap();
    assert_eq!(tidb_base::blocked::blocked_by(&err), Some("D12.5"));
}

#[tokio::test]
async fn killed_statement_stops_next() {
    let counters = Arc::new(Counters::default());
    let mut s = MockSession::new(1);
    let mut stmt = ExecStmt::new(
        "select".to_owned(),
        Some(mock_plan(3, 1, false, &counters)),
        None,
    );
    let mut rs = stmt.exec(&mut s).await.unwrap().unwrap();
    s.killer
        .send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    let mut chk = rs.new_chunk(None);
    let err = rs.next(&mut stmt, &mut s, &mut chk).await.unwrap_err();
    assert!(
        tidb_base::dbterror::exeerrors::ErrQueryInterrupted.equal(&err),
        "{err}"
    );
}

#[test]
fn fast_plans() {
    assert!(is_fast_plan(&test_plan(PhysicalKind::PointGet)));
    assert!(is_fast_plan(&test_plan(PhysicalKind::TableDual)));
    assert!(!is_fast_plan(&test_plan(PhysicalKind::TableReader)));
    let proj: Plan = Arc::new(KindPlan {
        id: 2,
        kind: PhysicalKind::Projection,
        schema: new_schema(vec![col(1)]),
        children: vec![test_plan(PhysicalKind::PointGet)],
    });
    assert!(is_fast_plan(&proj));
    assert!(!is_no_result_plan(&proj));
    let empty: Plan = Arc::new(KindPlan {
        id: 3,
        kind: PhysicalKind::TableDual,
        schema: new_schema(Vec::new()),
        children: Vec::new(),
    });
    assert!(is_no_result_plan(&empty));
}

#[test]
fn result_fields() {
    let schema = new_schema(vec![col(1), col(2)]);
    let long = "c".repeat(300);
    let names = vec![
        FieldName {
            tbl_name: CIStr::new("t"),
            orig_tbl_name: CIStr::new("T0"),
            col_name: CIStr::new("A"),
            orig_col_name: CIStr::new("a"),
            ..Default::default()
        },
        FieldName {
            col_name: CIStr::new(&long),
            ..Default::default()
        },
    ];
    let rfs = col_names2_result_fields(&schema, &names, "Test");
    assert_eq!(rfs[0].db_name, CIStr::new("Test"));
    assert_eq!(rfs[0].column_name, CIStr::new("a"));
    assert!(!rfs[0].empty_org_name);
    assert_eq!(rfs[0].table_name, CIStr::new("T0"));
    assert_eq!(rfs[1].db_name, CIStr::default());
    assert!(rfs[1].empty_org_name);
    assert_eq!(rfs[1].column_name.o.len(), 300);
    assert_eq!(rfs[1].column_as_name.o.len(), 256);
    assert_eq!(rfs[1].column_as_name.l.len(), 256);
}

#[test]
fn format_sql_truncates_and_replaces() {
    use tidb_sessionctx::variable::tidb_vars::QueryLogMaxLen;
    assert_eq!(format_sql("select\t1\r\n"), "select 1  ");
    let old = QueryLogMaxLen.swap(6, Ordering::SeqCst);
    assert_eq!(format_sql("select 1\n"), "select(len:9)");
    QueryLogMaxLen.store(0, Ordering::SeqCst);
    assert_eq!(format_sql("select 1\n"), "select 1 ");
    QueryLogMaxLen.store(old, Ordering::SeqCst);
    assert_eq!(format_sql_lazy("a\tb".to_owned())(), "a b");
}

#[test]
fn phase_durations() {
    let mut stmt = ExecStmt::new(String::new(), None, None);
    stmt.phase_open_durations[0] = std::time::Duration::from_millis(3);
    stmt.reset_phase_durations();
    assert_eq!(
        stmt.phase_open_durations,
        [
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(3)
        ]
    );
    assert_eq!(
        get_phase_duration_observer("open:final", true).labels,
        vec!["open:final", "1"]
    );
    assert_eq!(
        get_phase_duration_observer("x", false).labels,
        vec!["unknown", "0"]
    );
}

/// The base executor wrappers: Next records the runtime stats and checks the killer before
/// and after the call.
#[tokio::test]
async fn next_wrapper_records_stats() {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.stmt_ctx_mut().runtime_stats_coll =
        Some(tidb_execdetails::execdetails::RuntimeStatsColl::new(None));
    let killer = SqlKiller::new();
    let mut e = Numbers {
        base: new_base_executor_v2(
            &vars,
            &killer,
            Some(new_schema(vec![col(1)])),
            5,
            Vec::new(),
        ),
        next: 0,
        n: 2,
        point_get: false,
        counters: Arc::new(Counters::default()),
    };
    let ctx = ExecCtx::new(vars.stmt_ctx_arc());
    let mut chk = exec::new_first_chunk(&e);
    exec::open(&ctx, &mut e).await.unwrap();
    exec::next(&ctx, &mut e, &mut chk).await.unwrap();
    assert_eq!(chk.num_rows(), 1);
    assert!(e.runtime_stats().is_some());
    killer.send_kill_signal(tidb_memory::sqlkiller::QueryInterrupted);
    assert!(exec::next(&ctx, &mut e, &mut chk).await.is_err());
    exec::close(&mut e).await.unwrap();
    assert_eq!(e.base().meta.id(), 5);
    assert_eq!(e.init_cap(), vars.init_chunk_size as isize);
    assert!(e.detach().is_none());
}
