//! glue: test doubles for the crates above the executors (feature `test-util`): Go's
//! `testutil.MockPhysicalPlan`, a physical plan that builds its own executor, here one that
//! returns the integers `0..rows` in one `BIGINT` column, one row per `Next`. The session's
//! tests (VS1-14) run statements with it while the real executors are not ported.
//!
//! The mock's executor can also do some CPU work in each `Next` ([`MockWork`]), with or
//! without time-sliced checkpoints, and counts whether it ran inside a task of an `ExecPool`:
//! the session's tests of the executors' tasks on the session's pool (N3-A1 §3.1 item 5,
//! §3.6) use it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::chunk::Chunk;
use tidb_expression::Column;
use tidb_expression::schema::new_schema;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_planner::physicalop::BasePhysicalPlan;
use tidb_planner::{PhysicalKind, PhysicalPlan, PhysicalPlanRef, Schema};
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

use crate::exec::ExecCtx;
use crate::glue::Plan;
use crate::internal::exec::executor::{BaseExecutorV2, Executor, new_base_executor_v2};

/// Global variables for the executors' own `SessionVars` (empty values).
struct NoGlobals;

impl GlobalVarAccessor for NoGlobals {
    // glue: test double
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    // glue: test double
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    // glue: test double
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    // glue: test double
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Counts what the mock's executors did.
#[derive(Default, Debug)]
pub struct MockCounters {
    /// Executors built.
    pub built: AtomicUsize,
    /// `Next` calls.
    pub nexts: AtomicUsize,
    /// `Close` calls.
    pub closed: AtomicUsize,
    /// `Next` calls that ran inside a task started through an `ExecPool` (instrumented: its
    /// checkpoints can yield, its stalls are detected; N3-A1 §3.1 item 5, §3.6).
    pub instrumented_nexts: AtomicUsize,
    /// Polls of the [`MockWork`] loops, summed: more polls than loops means a loop yielded
    /// at its checkpoints.
    pub work_polls: AtomicUsize,
    /// [`MockWork`] loops run (one per `Next` that returns a row).
    pub work_loops: AtomicUsize,
}

/// CPU work the mock's executor does in each `Next` that returns a row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MockWork {
    /// None.
    #[default]
    None,
    /// A busy loop of `per_next`, with a time-sliced checkpoint (`CheckpointStride`, the
    /// clock read every 16 iterations; N3-A1 §3.1 item 5) in each iteration when
    /// `checkpoints`, or none at all.
    Loop {
        per_next: Duration,
        checkpoints: bool,
    },
}

/// Go's `testutil.MockPhysicalPlan`: the executor builder returns [`MockPhysicalPlan::get_executor`].
#[derive(Clone)]
pub struct MockPhysicalPlan {
    base: BasePhysicalPlan,
    schema: Schema,
    children: Vec<PhysicalPlanRef>,
    rows: i64,
    /// The kind the plan reports. Only a `TableReader` mock is built into the mock executor;
    /// any other kind reaches the builder's boundary stubs (`Blocked`), as a real plan of
    /// that kind would.
    pub kind: PhysicalKind,
    /// What the executors did.
    pub counters: Arc<MockCounters>,
    /// The work of each `Next` of the executors built from now on.
    pub work: MockWork,
}

/// A plan whose executor returns `0..rows`.
// glue: test double
pub fn mock_plan(rows: i64) -> (Plan, Arc<MockCounters>) {
    mock_plan_of_kind(rows, PhysicalKind::TableReader)
}

/// [`mock_plan`] reporting `kind`.
// glue: test double
pub fn mock_plan_of_kind(rows: i64, kind: PhysicalKind) -> (Plan, Arc<MockCounters>) {
    let counters = Arc::new(MockCounters::default());
    let col = Arc::new(Column {
        unique_id: 1,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    });
    let plan: Plan = Arc::new(MockPhysicalPlan {
        base: BasePhysicalPlan::default(),
        schema: new_schema(vec![col]),
        children: Vec::new(),
        rows,
        kind,
        counters: Arc::clone(&counters),
        work: MockWork::None,
    });
    (plan, counters)
}

/// [`mock_plan`] whose executors do `work` in each `Next` that returns a row.
// glue: test double
pub fn mock_plan_with_work(rows: i64, work: MockWork) -> (Plan, Arc<MockCounters>) {
    let (plan, counters) = mock_plan(rows);
    let mut mock = plan
        .as_any()
        .downcast_ref::<MockPhysicalPlan>()
        .expect("mock_plan builds a MockPhysicalPlan")
        .clone();
    mock.work = work;
    (Arc::new(mock), counters)
}

impl MockPhysicalPlan {
    /// `GetExecutor`.
    // glue: test double (Go testutil.MockPhysicalPlan.GetExecutor)
    pub fn get_executor(&self) -> Box<dyn Executor> {
        self.counters.built.fetch_add(1, Ordering::SeqCst);
        let vars = SessionVars::new(Box::new(NoGlobals));
        Box::new(Rows {
            base: new_base_executor_v2(
                &vars,
                &SqlKiller::new(),
                Some(self.schema.clone()),
                7,
                Vec::new(),
            ),
            next: 0,
            n: self.rows,
            counters: Arc::clone(&self.counters),
            work: self.work,
        })
    }
}

impl PhysicalPlan for MockPhysicalPlan {
    // glue: test double
    fn kind(&self) -> PhysicalKind {
        self.kind
    }
    // glue: test double
    fn id(&self) -> i32 {
        7
    }
    // glue: test double
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    // glue: test double
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    // glue: test double
    fn schema(&self) -> &Schema {
        &self.schema
    }
    // glue: test double
    fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }
    // glue: test double
    fn children(&self) -> &[PhysicalPlanRef] {
        &self.children
    }
    // glue: test double
    fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef> {
        &mut self.children
    }
    // glue: test double
    fn as_dyn(&self) -> &dyn PhysicalPlan {
        self
    }
    // glue: test double
    fn base_physical(&self) -> &BasePhysicalPlan {
        &self.base
    }
    // glue: test double
    fn base_physical_mut(&mut self) -> &mut BasePhysicalPlan {
        &mut self.base
    }
    // glue: test double
    fn clone_node(&self) -> PhysicalPlanRef {
        Arc::new(self.clone())
    }
}

/// Returns `0..n`, one row per `Next`.
struct Rows {
    base: BaseExecutorV2,
    next: i64,
    n: i64,
    counters: Arc<MockCounters>,
    work: MockWork,
}

/// A busy loop of `per_next`, with a time-sliced checkpoint in each iteration when
/// `checkpoints` (the hook is `()`: only the time slice).
// glue: test double
async fn busy_loop(per_next: Duration, checkpoints: bool) {
    let mut stride = tidb_runtime::CheckpointStride::new(16);
    let hook = crate::exec::ExecCtx::new(Arc::new(tidb_sessionctx::stmtctx::new_stmt_ctx()));
    let until = Instant::now() + per_next;
    while Instant::now() < until {
        std::hint::spin_loop();
        if checkpoints {
            let _ = stride.checkpoint(&hook).await;
        }
    }
}

/// Counts the polls of the future it wraps.
struct PollCount<F> {
    inner: Pin<Box<F>>,
    polls: usize,
}

impl<F: Future> Future for PollCount<F> {
    type Output = usize;

    // glue: test double
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
        self.polls += 1;
        match self.inner.as_mut().poll(cx) {
            Poll::Ready(_) => Poll::Ready(self.polls),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[async_trait]
impl Executor for Rows {
    // glue: test double
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    // glue: test double
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    // glue: test double
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        self.counters.nexts.fetch_add(1, Ordering::SeqCst);
        if tidb_runtime::slice::task_cpu_time().is_some() {
            self.counters
                .instrumented_nexts
                .fetch_add(1, Ordering::SeqCst);
        }
        req.reset();
        if self.next < self.n {
            if let MockWork::Loop {
                per_next,
                checkpoints,
            } = self.work
            {
                let polls = PollCount {
                    inner: Box::pin(busy_loop(per_next, checkpoints)),
                    polls: 0,
                }
                .await;
                self.counters.work_polls.fetch_add(polls, Ordering::SeqCst);
                self.counters.work_loops.fetch_add(1, Ordering::SeqCst);
            }
            req.append_int64(0, self.next);
        }
        self.next += 1;
        Ok(())
    }
    // glue: test double
    async fn close(&mut self) -> Result<(), Error> {
        self.counters.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
