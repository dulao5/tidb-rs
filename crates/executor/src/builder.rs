//! go-file: pkg/executor/builder.go
//!
//! The executor builder: a physical plan to an executor tree. Part VS1-13 has the builder
//! itself and its dispatch (`build`); each `buildXxx` comes with its executor (VS1-12 point
//! get, VS1-34 table reader, VS1-35 selection / projection / limit / table dual, VS1-36 sort
//! / top-n) and is a boundary stub until then.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_planner::PhysicalKind;

use crate::adapter::TelemetryInfo;
use crate::glue::{InfoSchemaRef, Plan, SessionContext};
use crate::internal::exec::executor::Executor;

/// executorBuilder builds an Executor from a Plan.
/// The InfoSchema must not change during execution.
///
/// Go keeps the session (`sctx`) in the builder; here the build functions take it (the
/// builder lives for one `build` call on the session's task).
pub struct ExecutorBuilder {
    pub is: Option<InfoSchemaRef>,
    /// err is set when there is error happened during Executor building process.
    pub err: Option<Error>,
    pub has_lock: bool,
    pub ti: Arc<TelemetryInfo>,
    /// isStaleness means whether this statement use stale read.
    pub is_staleness: bool,
    pub txn_scope: String,
    pub read_replica_scope: String,
    pub in_update_stmt: bool,
    pub in_delete_stmt: bool,
    pub in_insert_stmt: bool,
    pub in_select_lock_stmt: bool,
    /// forDataReaderBuilder indicates whether the builder is used by a dataReaderBuilder.
    pub for_data_reader_builder: bool,
    pub data_reader_ts: u64,
    /// Used when building MPPGather.
    pub encounter_union_scan: bool,
}

// go: newExecutorBuilder L1 [M-GOCTX]
pub fn new_executor_builder(
    sctx: &dyn SessionContext,
    is: Option<InfoSchemaRef>,
    ti: Arc<TelemetryInfo>,
) -> ExecutorBuilder {
    ExecutorBuilder {
        is,
        ti,
        is_staleness: sctx.is_stmt_staleness(),
        txn_scope: sctx.get_txn_scope(),
        read_replica_scope: sctx.get_read_replica_scope(),
        err: None,
        has_lock: false,
        in_update_stmt: false,
        in_delete_stmt: false,
        in_insert_stmt: false,
        in_select_lock_stmt: false,
        for_data_reader_builder: false,
        data_reader_ts: 0,
        encounter_union_scan: false,
    }
}

impl ExecutorBuilder {
    /// The executor of `p`; on failure `b.err` is set and `None` returned, as Go.
    ///
    /// Plans other than the VS1 operators have no kind yet (`PhysicalKind` grows with the
    /// operators); in Go they reach the `default` branch's error the same way.
    // go: executorBuilder.build L1
    pub fn build(
        &mut self,
        sctx: &mut dyn SessionContext,
        p: Option<&Plan>,
    ) -> Option<Box<dyn Executor>> {
        let _ = &sctx;
        // case nil: return nil
        let v = p?;
        // Go's `default` branch first returns a test mock's executor
        // (`testutil.MockPhysicalPlan`), whatever kind the mock reports.
        #[cfg(test)]
        if let Some(mp) = v
            .as_any()
            .downcast_ref::<crate::adapter::tests::MockPhysicalPlan>()
        {
            return Some(mp.get_executor());
        }
        // glue: stub, blocked-by:D12.1, D12.2, D12.5, D12.0 (the buildXxx of each executor)
        let (block, what) = match v.kind() {
            // b.buildPointGet(v)
            PhysicalKind::PointGet => ("D12.1", "the point get executor (VS1-12)"),
            // b.buildLimit(v)
            PhysicalKind::Limit => ("D12.2", "the limit executor (VS1-35)"),
            // b.buildSort(v)
            PhysicalKind::Sort => ("D12.5", "the sort executor (VS1-36)"),
            // b.buildTopN(v)
            PhysicalKind::TopN => ("D12.5", "the top-n executor (VS1-36)"),
            // b.buildSelection(v)
            PhysicalKind::Selection => ("D12.2", "the selection executor (VS1-35)"),
            // b.buildProjection(v)
            PhysicalKind::Projection => ("D12.2", "the projection executor (VS1-35)"),
            // b.buildTableDual(v)
            PhysicalKind::TableDual => ("D12.2", "the table dual executor (VS1-35)"),
            // b.buildTableReader(v)
            PhysicalKind::TableReader => ("D12.1", "the table reader executor (VS1-34)"),
            // A table scan is pushed down inside a reader, never built on its own: Go's
            // `default` branch.
            PhysicalKind::TableScan => {
                self.err = Some(
                    tidb_base::dbterror::exeerrors::ErrUnknownPlan.gen_with_stack(
                        "Unknown Plan %T",
                        vec![tidb_base::gofmt::Arg::Stringer(
                            Vec::new(),
                            "*core.PhysicalTableScan",
                        )],
                    ),
                );
                return None;
            }
            _ => ("D12.0", "an executor for this plan"),
        };
        self.err = Some(blocked(block, what));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tests::{MockSession, test_plan};

    #[test]
    fn dispatch_reports_the_executor_block() {
        let mut s = MockSession::new(u64::MAX);
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        assert!(b.build(&mut s, None).is_none());
        assert!(b.err.is_none());
        for (kind, block) in [
            (PhysicalKind::PointGet, "D12.1"),
            (PhysicalKind::TableReader, "D12.1"),
            (PhysicalKind::Selection, "D12.2"),
            (PhysicalKind::TopN, "D12.5"),
        ] {
            let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
            assert!(b.build(&mut s, Some(&test_plan(kind))).is_none());
            let err = b.err.expect("blocked");
            assert_eq!(
                tidb_base::blocked::blocked_by(&err),
                Some(block),
                "{kind:?}"
            );
        }
        let mut b = new_executor_builder(&s, None, Arc::new(TelemetryInfo::default()));
        assert!(
            b.build(&mut s, Some(&test_plan(PhysicalKind::TableScan)))
                .is_none()
        );
        assert_eq!(
            b.err.unwrap().to_string(),
            "[executor:8114]Unknown Plan *core.PhysicalTableScan"
        );
    }
}
