//! go-file: pkg/executor/aggregate/agg_util.go
//!
//! The helpers of the aggregation executors (D12.3): `closeBaseExecutor` (Part A),
//! `GetGroupKey`, `ActionSpill` / `actionSpillForUnparallel`, and `murmur3::sum32`
//! (`github.com/twmb/murmur3` v1.1.6). Parallel helpers (`getBuffer`, `tryRecycleBuffer`,
//! `recoveryHashAgg`, `getGroupKeyMemUsage`, `HashAggRuntimeStats`, `AggWorkerStat`,
//! `actionSpillForParallel`, `updateWaitTime` / `updateWorkerTime` / `updateExecTime`) live
//! here as well.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gostd::time::Location;
use tidb_base::mysql;
use tidb_base::terror;
use tidb_chunk::chunk::Chunk;
use tidb_codec::codec::hash_group_key;
use tidb_expression::ExprRef;
use tidb_expression::context::EvalContext;
use tidb_expression::eval::RpnState;
use tidb_expression::eval::rt::empty_column;
use tidb_expression::expression::{EvalState, eval_expr};
use tidb_memory::action::ActionRef;
use tidb_sessionctx::errctx::Context as ErrCtx;

use crate::internal::exec::executor::BaseExecutor;

use super::agg_hash_executor::HashAggExec;
use super::agg_spill::AggSpillDiskAction;

/// Go's `github.com/twmb/murmur3@v1.1.6` `Sum32` (used to route group keys to final workers
/// and spill partitions).
// go-dep: github.com/twmb/murmur3@v1.1.6 Sum32 L1
#[allow(dead_code)] // Used by B2 parallel workers.
pub(crate) fn murmur3_sum32(data: &[u8]) -> u32 {
    const C1_32: u32 = 0xcc9e_2d51;
    const C2_32: u32 = 0x1b87_3593;

    let mut h1: u32 = 0;
    let clen = data.len() as u32;
    let (chunks, rem) = data.as_chunks::<4>();
    for &chunk in chunks {
        let mut k1 = u32::from_le_bytes(chunk);
        k1 = k1.wrapping_mul(C1_32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2_32);

        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe654_6b64);
    }

    let mut k1: u32 = 0;
    if rem.len() == 3 {
        k1 ^= u32::from(rem[2]) << 16;
    }
    if rem.len() >= 2 {
        k1 ^= u32::from(rem[1]) << 8;
    }
    if !rem.is_empty() {
        k1 ^= u32::from(rem[0]);
        k1 = k1.wrapping_mul(C1_32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2_32);
        h1 ^= k1;
    }

    h1 ^= clen;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85eb_ca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2_ae35);
    h1 ^= h1 >> 16;

    h1
}

/// Go's deferred `closeBaseExecutor` recovers a panic of `OpenSelf`, closes the base
/// executor (the children) and panics again. A Go panic is an error here (M-PANIC), so the
/// caller calls it on the error path of `OpenSelf` and returns what it returns: `err`
/// itself, after the children were closed (spec §4.5).
// go: closeBaseExecutor L2 (recover → the error of OpenSelf, M-PANIC; async close)
pub(crate) async fn close_base_executor(b: &mut BaseExecutor, err: Error) -> Error {
    // Release the resource, but throw the panic again and let the top level handle it.
    terror::log(b.base.close().await.err().as_ref());
    log::warn!("panic in Open(), close base executor and throw exception again");
    err
}

/// GetGroupKey evaluates the group items and encodes them as group-key byte strings.
///
/// Takes the specific context values Go reads from `sessionctx.Context` (N2): evaluation
/// context, error context, time zone, vectorized switch, and the executor's evaluation and
/// RPN scratch state (N4 §3.4). Temporary columns are allocated directly (M-POOL).
// go: GetGroupKey L2 (N2 explicit context fields; M-POOL no column pool; N4 EvalState/RpnState)
#[allow(clippy::too_many_arguments)]
pub fn get_group_key(
    eval_ctx: &dyn EvalContext,
    err_ctx: &ErrCtx,
    loc: &Arc<Location>,
    vec_enabled: bool,
    st: &EvalState,
    rpn: &mut RpnState,
    input: &Chunk,
    mut group_key: Vec<Vec<u8>>,
    group_by_items: &[ExprRef],
) -> Result<Vec<Vec<u8>>, Error> {
    let num_rows = input.num_rows();
    let avl_group_key_len = group_key.len().min(num_rows);
    for key in &mut group_key[..avl_group_key_len] {
        key.clear();
    }
    for _ in avl_group_key_len..num_rows {
        group_key.push(Vec::with_capacity(10 * group_by_items.len()));
    }

    for item in group_by_items {
        let mut tp = item.get_type(eval_ctx).into_owned();
        let mut buf = empty_column();

        // In strict sql mode like 'STRICT_TRANS_TABLES', cannot insert an invalid enum value
        // like 0. While in sql mode like '', can insert an invalid enum value like 0, then
        // the enum value 0 will have the enum name '', which may conflict with user defined
        // enum ''. Use enum value as groupKey instead of enum name.
        if tp.get_type() == mysql::TypeEnum {
            tp.add_flag(mysql::EnumSetAsIntFlag);
        }

        eval_expr(
            eval_ctx,
            st,
            rpn,
            vec_enabled,
            item,
            tp.eval_type(),
            input,
            &mut buf,
        )?;

        // Avoid error during the execution of `EncodeDecimal`.
        if tp.get_type() == mysql::TypeNewDecimal {
            tp.set_flen(0);
        }

        if let Err(err) = hash_group_key(loc, num_rows, &buf, &mut group_key[..num_rows], &tp) {
            err_ctx.handle_error(err)?;
        }
    }

    group_key.truncate(num_rows);
    Ok(group_key)
}

impl HashAggExec {
    // go: HashAggExec.actionSpillForUnparallel L1
    pub(crate) fn action_spill_for_unparallel(&mut self) -> ActionRef {
        let tracker = Arc::clone(
            self.mem_tracker
                .as_ref()
                .expect("Open initialized memTracker"),
        );
        let action = Arc::new(AggSpillDiskAction::new(
            Arc::clone(&self.in_spill_mode),
            tracker,
        ));
        self.spill_action = Some(Arc::clone(&action));
        action
    }

    /// ActionSpill returns an action for spilling intermediate data for hashAgg.
    // go: HashAggExec.ActionSpill L1
    pub fn action_spill(&mut self) -> ActionRef {
        if self.is_unparallel_exec {
            return self.action_spill_for_unparallel();
        }
        // Part B2 wires `actionSpillForParallel`; unreachable while parallel execution is not
        // yet enabled in this B1 draft.
        unimplemented!("HashAggExec.actionSpillForParallel lands with B2")
    }
}
