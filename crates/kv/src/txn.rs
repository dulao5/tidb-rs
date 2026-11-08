//! go-file: pkg/kv/txn.go
//!
//! `RunInNewTxn` (internal transactions, used on the read path by meta reads such as
//! `SHOW BDR ROLE` and `ADMIN` checks), its back-off, and the registry of their start
//! timestamps that GC safe-point reporting reads (`GetMinInnerTxnStartTS`).
//!
//! The failpoints (`mockCommitErrorInNewTxn`, `TxnResourceGroupChecker`) are test hooks and
//! are not ported. `BackOff` sleeps with the async timer (M-BLOCK): the runtime thread is
//! not blocked.

// Go identifiers for the package variables (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use rand::Rng;
use tidb_base::Error;

use crate::context::Context;
use crate::error::ErrTxnRetryable;
use crate::error::is_txn_retryable_error;
use crate::kv::Storage;
use crate::kv::Transaction;
use crate::option::ExplicitRequestSourceType;
use crate::option::RequestSourceInternal;
use crate::option::RequestSourceType;
use crate::option::ResourceGroupName;

/// TimeToPrintLongTimeInternalTxn is the duration if the internal transaction lasts more
/// than it, TiDB prints a log message.
pub const TimeToPrintLongTimeInternalTxn: Duration = Duration::from_secs(5 * 60);

static globalInnerTxnTsBox: LazyLock<InnerTxnStartTsBox> = LazyLock::new(|| InnerTxnStartTsBox {
    inner_txn_start_ts_map: Mutex::new(HashSet::with_capacity(256)),
});

struct InnerTxnStartTsBox {
    inner_txn_start_ts_map: Mutex<HashSet<u64>>,
}

impl InnerTxnStartTsBox {
    // go: innerTxnStartTsBox.storeInnerTxnTS L1
    fn store_inner_txn_ts(&self, start_ts: u64) {
        self.inner_txn_start_ts_map.lock().insert(start_ts);
    }

    // go: innerTxnStartTsBox.deleteInnerTxnTS L1
    fn delete_inner_txn_ts(&self, start_ts: u64) {
        self.inner_txn_start_ts_map.lock().remove(&start_ts);
    }

    // go: innerTxnStartTsBox.getMinStartTS L1 [M-MAPORDER]
    fn get_min_start_ts(
        &self,
        now: SystemTime,
        start_ts_lower_limit: u64,
        cur_min_start_ts: u64,
    ) -> u64 {
        let mut min_start_ts = cur_min_start_ts;
        let map = self.inner_txn_start_ts_map.lock();
        for &inner_ts in map.iter() {
            print_long_time_internal_txn(now, inner_ts, true);
            if inner_ts > start_ts_lower_limit && inner_ts < min_start_ts {
                min_start_ts = inner_ts;
            }
        }
        min_start_ts
    }
}

/// GetMinInnerTxnStartTS get the min StartTS between startTSLowerLimit and curMinStartTS in
/// globalInnerTxnTsBox.
// go: GetMinInnerTxnStartTS L1
pub fn get_min_inner_txn_start_ts(
    now: SystemTime,
    start_ts_lower_limit: u64,
    cur_min_start_ts: u64,
) -> u64 {
    globalInnerTxnTsBox.get_min_start_ts(now, start_ts_lower_limit, cur_min_start_ts)
}

/// `oracle.GetTimeFromTS`: the physical part of a TSO (milliseconds since the epoch).
// go-dep: client-go oracle.GetTimeFromTS L1
fn get_time_from_ts(ts: u64) -> SystemTime {
    const PHYSICAL_SHIFT_BITS: u64 = 18;
    let ms = (ts >> PHYSICAL_SHIFT_BITS) as i64;
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

/// PrintLongTimeInternalTxn print the internal transaction information.
/// runByFunction    true means the transaction is run by `RunInNewTxn`,
///
///    false means the transaction is run by internal session.
// go: PrintLongTimeInternalTxn L1
pub fn print_long_time_internal_txn(now: SystemTime, start_ts: u64, run_by_function: bool) {
    if start_ts > 0 {
        let inner_txn_start_time = get_time_from_ts(start_ts);
        let elapsed = now.duration_since(inner_txn_start_time).unwrap_or_default();
        if elapsed > TimeToPrintLongTimeInternalTxn {
            let mut caller_name = "internal session";
            if run_by_function {
                caller_name = "RunInNewTxn";
            }
            let info_header =
                format!("An internal transaction running by {caller_name} lasts long time");
            log::info!(
                "{info_header} time={elapsed:?} startTS={start_ts} start time={:?}",
                inner_txn_start_time
            );
        }
    }
}

/// The function `RunInNewTxn` runs: `f(ctx, txn)`.
pub type TxnFn<'f> = dyn for<'a> FnMut(&'a Context, &'a mut dyn Transaction) -> BoxFuture<'a, Result<(), Error>>
    + Send
    + 'f;

/// Removes the original start TS from the registry when `RunInNewTxn` returns (Go: defer).
struct DeleteOnDrop(u64);

impl Drop for DeleteOnDrop {
    // glue: defer globalInnerTxnTsBox.deleteInnerTxnTS(originalTxnTS) (M-DEFER)
    fn drop(&mut self) {
        globalInnerTxnTsBox.delete_inner_txn_ts(self.0);
    }
}

/// RunInNewTxn will run the f in a new transaction environment, should be used by inner
/// txn only.
// go: RunInNewTxn L1 [M-DEFER]
pub async fn run_in_new_txn(
    ctx: &Context,
    store: &dyn Storage,
    retryable: bool,
    f: &mut TxnFn<'_>,
) -> Result<(), Error> {
    let mut err: Option<Error> = None;
    let mut guard = DeleteOnDrop(0);
    let max = MaxRetryCnt.load(Ordering::Relaxed);
    let mut i: usize = 0;
    while i < max {
        let mut txn = match store.begin(&[]).await {
            Ok(txn) => txn,
            Err(e) => {
                log::error!("RunInNewTxn error={}", String::from_utf8_lossy(&e.error()));
                return Err(e);
            }
        };
        set_request_source_for_inner_txn(ctx, &mut *txn);

        // originalTxnTS is used to trace the original transaction when the function is
        // retryable.
        if i == 0 {
            guard.0 = txn.start_ts();
            globalInnerTxnTsBox.store_inner_txn_ts(guard.0);
        }

        err = f(ctx, &mut *txn).await.err();
        if let Some(e) = &err {
            let err1 = txn.rollback();
            tidb_base::terror::log(err1.err().as_ref());
            if retryable && is_txn_retryable_error(Some(e)) {
                log::warn!(
                    "RunInNewTxn retry txn={} original txn={} error={}",
                    txn.start_ts(),
                    guard.0,
                    String::from_utf8_lossy(&e.error())
                );
                i += 1;
                continue;
            }
            return Err(e.clone());
        }

        if err.is_none() {
            err = txn.commit(ctx).await.err();
            if err.is_none() {
                break;
            }
        }
        if retryable && is_txn_retryable_error(err.as_ref()) {
            log::warn!(
                "RunInNewTxn retry txn={} original txn={} error={}",
                txn.start_ts(),
                guard.0,
                err.as_ref()
                    .map(|e| String::from_utf8_lossy(&e.error()).into_owned())
                    .unwrap_or_default()
            );
            back_off(i).await;
            i += 1;
            continue;
        }
        return match err {
            None => Ok(()),
            Some(e) => Err(e),
        };
    }
    match err {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// MaxRetryCnt represents maximum retry times.
pub static MaxRetryCnt: AtomicUsize = AtomicUsize::new(100);
/// retryBackOffBase is the initial duration, in microsecond, a failed transaction stays
/// dormancy before it retries
const retryBackOffBase: isize = 1;
/// retryBackOffCap is the max amount of duration, in microsecond, a failed transaction
/// stays dormancy before it retries
const retryBackOffCap: isize = 100;

/// The bound `BackOff` draws its sleep from (`math.Min` over float64).
// glue: the upper bound computation of BackOff
pub fn back_off_upper(attempts: usize) -> isize {
    (retryBackOffCap as f64).min(retryBackOffBase as f64 * 2.0f64.powf(attempts as f64)) as isize
}

/// BackOff Implements exponential backoff with full jitter.
/// Returns real back off time in microsecond.
/// See http://www.awsarchitectureblog.com/2015/03/backoff.html.
// go: BackOff L2 (async sleep, M-BLOCK)
pub async fn back_off(attempts: usize) -> isize {
    let upper = back_off_upper(attempts);
    let sleep = Duration::from_millis(rand::thread_rng().gen_range(0..upper) as u64);
    tokio::time::sleep(sleep).await;
    sleep.as_nanos() as isize
}

// go: setRequestSourceForInnerTxn L1
fn set_request_source_for_inner_txn(ctx: &Context, txn: &mut dyn Transaction) {
    if let Some(request_source) = ctx.request_source()
        && !request_source.request_source_type.is_empty()
    {
        if !request_source.request_source_internal {
            log::warn!("`RunInNewTxn` should be used by inner txn only");
        }
        txn.set_option(
            RequestSourceInternal,
            Box::new(request_source.request_source_internal),
        );
        txn.set_option(
            RequestSourceType,
            Box::new(request_source.request_source_type.clone()),
        );
        if !request_source.explicit_request_source_type.is_empty() {
            txn.set_option(
                ExplicitRequestSourceType,
                Box::new(request_source.explicit_request_source_type.clone()),
            );
        }
        return;
    }
    // panic in test mode in case there are requests without source in the future.
    // log warnings in production mode.
    // (Go: intest.Assert(true, ...), which never fires.)
    log::warn!(
        "unexpected no source type context, if you see this warning, the `RequestSourceTypeKey` is missing in the context"
    );
}

/// SetTxnResourceGroup update the resource group name of target txn.
// go: SetTxnResourceGroup L1
pub fn set_txn_resource_group(txn: &mut dyn Transaction, name: &str) {
    txn.set_option(ResourceGroupName, Box::new(name.to_owned()));
}

/// `ErrTxnRetryable`, re-exported for the failpoint-free callers that inject it in tests.
// glue: test access to ErrTxnRetryable
pub fn err_txn_retryable() -> Error {
    ErrTxnRetryable.clone()
}
