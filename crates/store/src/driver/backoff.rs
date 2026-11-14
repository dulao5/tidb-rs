//! go-file: pkg/store/driver/backoff/backoff.go
//!
//! TiDB's `Backoffer`: client-go's `tikv.Backoffer` with its errors converted to TiDB
//! errors. client-go's backoffer is client-rust's [`tikv_client::Backoff`] time budget (ADR
//! 0008, client-rust#12): one budget of total sleep per read, one curve per kind of error.

use std::collections::HashMap;

use tidb_base::Error;
use tidb_kv::Context;
use tikv_client::metrics::BackoffType;

use super::error::{TiKVError, to_tidb_err};
use crate::copr::tikv::Variables;

/// Backoffer wraps tikv.Backoffer and converts the error which returns by the functions of
/// tikv.Backoffer to tidb error.
pub struct Backoffer {
    b: tikv_client::Backoff,
    /// `tikv.Backoffer`'s context (M-GOCTX).
    ctx: Context,
    /// `tikv.Backoffer`'s vars; `None` is Go's nil.
    vars: Option<Variables>,
}

/// client-go `retry.NewBackofferWithVars`: the budget is `maxSleep` ms, times
/// `vars.BackoffWeight` when vars are given and the weight is positive.
// go-l3: ADR-0008 client-go config/retry NewBackofferWithVars
fn tikv_new_backoffer_with_vars(
    max_sleep: isize,
    vars: Option<&Variables>,
) -> tikv_client::Backoff {
    let mut max_sleep = max_sleep as i64;
    if let Some(vars) = vars
        && vars.backoff_weight > 0
    {
        max_sleep = max_sleep.wrapping_mul(vars.backoff_weight);
    }
    tikv_client::Backoff::time_budget(max_sleep.max(0) as u64).start()
}

/// NewBackofferWithVars creates a Backoffer with maximum sleep time(in ms) and kv.Variables.
// go: NewBackofferWithVars L1 [M-GOCTX]
pub fn new_backoffer_with_vars(
    ctx: &Context,
    max_sleep: isize,
    vars: Option<&Variables>,
) -> Backoffer {
    let b = tikv_new_backoffer_with_vars(max_sleep, vars);
    Backoffer {
        b,
        ctx: ctx.clone(),
        vars: vars.cloned(),
    }
}

/// NewBackofferWithTikvBo creates a Backoffer with tikv.Backoffer
// go: NewBackofferWithTikvBo L1 [M-GOCTX]
pub fn new_backoffer_with_tikv_bo(bo: tikv_client::Backoff, ctx: &Context) -> Backoffer {
    Backoffer {
        b: bo,
        ctx: ctx.clone(),
        vars: None,
    }
}

/// NewBackoffer creates a Backoffer with maximum sleep time(in ms).
// go: NewBackoffer L1 [M-GOCTX]
pub fn new_backoffer(ctx: &Context, max_sleep: isize) -> Backoffer {
    // client-go `tikv.NewBackoffer(ctx, maxSleep)` is NewBackofferWithVars with nil vars.
    let b = tikv_new_backoffer_with_vars(max_sleep, None);
    Backoffer {
        b,
        ctx: ctx.clone(),
        vars: None,
    }
}

impl Backoffer {
    /// TiKVBackoffer returns tikv.Backoffer.
    // go: Backoffer.TiKVBackoffer L1
    pub fn tikv_backoffer(&mut self) -> &mut tikv_client::Backoff {
        &mut self.b
    }

    /// Backoff sleeps a while base on the BackoffConfig and records the error message.
    /// It returns a retryable error if total sleep time exceeds maxSleep.
    // go: Backoffer.Backoff L1
    pub async fn backoff(
        &mut self,
        cfg: BackoffType,
        err: tikv_client::Error,
    ) -> Result<(), Error> {
        let e = self.b.backoff(cfg, err).await;
        e.map_err(|e| to_tidb_err(TiKVError::from(e)))
    }

    /// BackoffWithMaxSleepTxnLockFast sleeps a while for the operation TxnLockFast and records
    /// the error message and never sleep more than maxSleepMs for each sleep.
    // go: Backoffer.BackoffWithMaxSleepTxnLockFast L1
    pub async fn backoff_with_max_sleep_txn_lock_fast(
        &mut self,
        max_sleep_ms: isize,
        err: tikv_client::Error,
    ) -> Result<(), Error> {
        let e = self
            .b
            .backoff_with_max_sleep_txn_lock_fast(max_sleep_ms.max(0) as u64, err)
            .await;
        e.map_err(|e| to_tidb_err(TiKVError::from(e)))
    }

    /// GetBackoffTimes returns a map contains backoff time count by type.
    ///
    /// client-rust's budget does not expose its per-kind counts (it keeps them for the
    /// metrics only): the map is empty (L3, ADR 0008; these feed the slow log and
    /// `EXPLAIN ANALYZE`, D14.2).
    // go: Backoffer.GetBackoffTimes L3
    pub fn get_backoff_times(&self) -> HashMap<String, isize> {
        HashMap::new()
    }

    /// GetCtx returns the binded context.
    // go: Backoffer.GetCtx L1 [M-GOCTX]
    pub fn get_ctx(&self) -> &Context {
        &self.ctx
    }

    /// GetVars returns the binded vars.
    // go: Backoffer.GetVars L1
    pub fn get_vars(&self) -> Option<&Variables> {
        self.vars.as_ref()
    }

    /// GetBackoffSleepMS returns a map contains backoff sleep time by type.
    ///
    /// Empty, as [`Backoffer::get_backoff_times`] (L3).
    // go: Backoffer.GetBackoffSleepMS L3
    pub fn get_backoff_sleep_ms(&self) -> HashMap<String, isize> {
        HashMap::new()
    }

    /// GetTotalSleep returns total sleep time.
    // go: Backoffer.GetTotalSleep L1
    pub fn get_total_sleep(&self) -> isize {
        self.b.total_sleep_ms() as isize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget_ms(mut b: tikv_client::Backoff) -> u128 {
        let mut total = std::time::Duration::ZERO;
        while let Some(d) = b.next_delay_duration() {
            total += d;
        }
        total.as_millis()
    }

    /// The weight multiplies the budget (client-go `NewBackofferWithVars`); without vars or
    /// with a non-positive weight it does not.
    #[test]
    fn budget_follows_the_weight() {
        let ctx = Context::background();
        let two = Variables::default();
        let b = new_backoffer_with_vars(&ctx, 20_000, Some(&two));
        assert!((38_000..=42_000).contains(&budget_ms(b.b)));
        let b = new_backoffer(&ctx, 20_000);
        assert!((18_000..=22_000).contains(&budget_ms(b.b)));
        let zero = Variables {
            backoff_weight: 0,
            killed: None,
        };
        let b = new_backoffer_with_vars(&ctx, 20_000, Some(&zero));
        assert!((18_000..=22_000).contains(&budget_ms(b.b)));
        assert_eq!(new_backoffer(&ctx, 1).get_total_sleep(), 0);
    }

    #[tokio::test]
    async fn exhaustion_is_a_tidb_error() {
        // A budget of 1 ms: the first backoff sleeps (2 ms, regionMiss), the second finds the
        // budget spent (client-go checks before sleeping; a budget of 0 is unlimited).
        let mut b = new_backoffer(&Context::background(), 1);
        b.backoff(
            BackoffType::RegionMiss,
            tikv_client::Error::StringError("x".into()),
        )
        .await
        .unwrap();
        assert!(b.get_total_sleep() > 0);
        let err = b
            .backoff(
                BackoffType::RegionMiss,
                tikv_client::Error::StringError("x".into()),
            )
            .await
            .unwrap_err();
        assert!(super::super::error::ErrRegionUnavailable.equal(&err));
    }
}
