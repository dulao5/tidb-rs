//! The executors' tasks on the session's `ExecPool` (#363; N3-A1 §3.1 items 3 and 5, §3.6;
//! spec `specs/D05.3-session/spec.md` §9.4, `specs/D12.5-sort/spec.md` §7.2).
//!
//! A statement whose plan is a sort over the mock plan of `tidb_executor::testutil` runs
//! through the session (`runStmt`, the record set, `Close`) on an instrumented runtime. The
//! sort takes its parallel path, so the mock's `Next` runs inside the sort's fetcher task,
//! and each `Next` that returns a row does a long busy loop ([`MockWork`]): with time-sliced
//! checkpoints, or with none.
//!
//! The stall count is process-wide: no other test of this crate runs tasks on an
//! instrumented runtime, and the three runs below are one test, so they do not overlap.

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use tidb_executor::testutil::{MockCounters, MockWork, mock_plan_with_work};
use tidb_expression::Expr;
use tidb_planner::core::physical_plans::PhysicalSort;
use tidb_planner::physicalop::BasePhysicalPlan;
use tidb_planner::util::byitem::ByItems;
use tidb_runtime::slice::StallCheck;
use tidb_runtime::{Runtime, RuntimeConfig, SliceConfig, Topology};

use super::{MockStore, compile, session};
use crate::Session;
use crate::session::run_stmt;

/// Rows of the mock child; each costs one busy loop in the child's `Next`.
const ROWS: i64 = 3;
/// The busy loop of each `Next`: twice the stall threshold below.
const WORK: Duration = Duration::from_millis(100);

/// An instrumented runtime: the default 1 ms time slice; a stall threshold above OS
/// scheduling noise on a busy CI runner (as the runtime's own stall test), and stalls
/// classified, so that a stall caused by machine load is not taken for the code's.
fn runtime(topology: Topology) -> Runtime {
    Runtime::new(RuntimeConfig {
        topology,
        worker_threads: 2,
        slice: SliceConfig {
            stall_threshold: Duration::from_millis(50),
            classify_stalls: true,
            ..SliceConfig::default()
        },
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// `SELECT a FROM t ORDER BY a DESC` over the mock child doing `work` in each `Next`, run
/// through the session on `rt`: the sorted rows and the child's counters.
fn run_sort(rt: &Runtime, s: Session, work: MockWork) -> (Session, Vec<i64>, Arc<MockCounters>) {
    let mut s = s;
    let mut stmt = compile(&mut s, "SELECT a FROM t ORDER BY a DESC", 0);
    let (child, counters) = mock_plan_with_work(ROWS, work);
    let key = Expr::Column((*child.schema().columns[0]).clone()).into_ref();
    let mut base = BasePhysicalPlan::default();
    base.children = vec![child];
    stmt.plan = Some(Arc::new(PhysicalSort {
        base,
        by_items: vec![ByItems {
            expr: key,
            desc: true,
        }],
        is_partial_sort: false,
    }));
    let (s, rows) = rt.block_on(async move {
        let mut s = s;
        let mut rs = run_stmt(&mut s, stmt).await.unwrap().expect("rows");
        let mut chk = rs.new_chunk().unwrap();
        let mut rows = Vec::new();
        loop {
            rs.next(&mut s, &mut chk).await.unwrap();
            if chk.num_rows() == 0 {
                break;
            }
            rows.extend((0..chk.num_rows()).map(|i| chk.get_row(i).get_int64(0)));
        }
        rs.close(&mut s).await.unwrap();
        (s, rows)
    });
    assert!(s.is_idle(), "{}", s.state_snapshot());
    (s, rows, counters)
}

/// The session gives its `ExecPool` to the executors (`ExecCtx.pool`), and the sort starts
/// its tasks there, so the instrumentation of the pool covers them: a long loop with
/// time-sliced checkpoints yields (and does not stall), a long loop without them is reported
/// as a stall. Without a pool (the session before #363) the same tasks run uninstrumented:
/// the checkpoints never yield and the stall goes unnoticed. In both topologies.
#[test]
fn executor_tasks_run_on_the_session_exec_pool() {
    let with_checkpoints = MockWork::Loop {
        per_next: WORK,
        checkpoints: true,
    };
    let without_checkpoints = MockWork::Loop {
        per_next: WORK,
        checkpoints: false,
    };
    for topology in [Topology::Steal, Topology::NoSteal] {
        let rt = runtime(topology);
        let store = MockStore::new(10);
        let mut s = session(&store);
        s.set_exec_pool(Some(rt.pool().clone()));

        // A long loop with checkpoints, in a task on the session's pool: it yields at its
        // checkpoints (many polls per loop), and no poll is the code's stall.
        let check = StallCheck::start();
        let (s2, rows, c) = run_sort(&rt, s, with_checkpoints);
        s = s2;
        assert_eq!(rows, vec![2, 1, 0], "{topology:?}");
        let nexts = c.nexts.load(SeqCst);
        assert!(nexts > ROWS as usize, "{topology:?}");
        assert_eq!(
            c.instrumented_nexts.load(SeqCst),
            nexts,
            "{topology:?}: every Next of the child ran in a task of the ExecPool"
        );
        assert_eq!(c.work_loops.load(SeqCst), ROWS as usize, "{topology:?}");
        assert!(
            c.work_polls.load(SeqCst) > 5 * ROWS as usize,
            "{topology:?}: the loops yielded only {} times",
            c.work_polls.load(SeqCst) - ROWS as usize
        );
        check.assert_no_code_stalls();

        // The same loop without checkpoints: the loops never yield (the fetcher may even run
        // several of them in one poll, as long as the chunk channel has room), so the polls
        // run past the threshold and are reported as stalls.
        let check = StallCheck::start();
        let (s2, rows, c) = run_sort(&rt, s, without_checkpoints);
        s = s2;
        assert_eq!(rows, vec![2, 1, 0], "{topology:?}");
        assert_eq!(
            c.instrumented_nexts.load(SeqCst),
            c.nexts.load(SeqCst),
            "{topology:?}"
        );
        assert_eq!(c.work_polls.load(SeqCst), ROWS as usize, "{topology:?}");
        assert!(
            check.stalls() >= 1,
            "{topology:?}: no stall reported for {ROWS} non-yielding loops"
        );

        // Without a pool (the session before #363): the sort's tasks go to the current tokio
        // runtime, outside the instrumentation. The checkpoints never yield and the polls of
        // twice the threshold are not seen.
        s.set_exec_pool(None);
        let check = StallCheck::start();
        let (_, rows, c) = run_sort(&rt, s, with_checkpoints);
        assert_eq!(rows, vec![2, 1, 0], "{topology:?}");
        assert_eq!(c.instrumented_nexts.load(SeqCst), 0, "{topology:?}");
        assert_eq!(c.work_polls.load(SeqCst), ROWS as usize, "{topology:?}");
        assert_eq!(check.stalls(), 0, "{topology:?}");
    }
}
