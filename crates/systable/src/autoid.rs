//! go-file: pkg/meta/autoid/autoid.go
//!
//! The row-ID allocator of a NONCLUSTERED table (`allocator` with `RowIDAllocType`, signed),
//! as `TableCommon.AllocHandle` uses it: a local `[base, end]` cache refilled from the meta
//! counter (`m` / `DB:<dbID>` / `TID:<tableID>`) in a separate meta transaction. The counter
//! is shared with the Go instances, so the ranges of both never overlap (spec §6.3).
//!
//! The meta transaction is the caller's: `alloc` hands it a closure over an [`IdAccessor`]
//! (`meta.AutoIDAccessor`'s `Get` / `Inc`), to run with `kv.RunInNewTxn` semantics
//! (retried as a whole on a write conflict).
//!
//! Block D06.7 holds `alloc4Signed`; the helpers it calls (`Alloc`'s checks,
//! `CalcNeededBatchSize`, `SeekToFirstAutoID*`, `NextStep`) are D06.2 functions ported with
//! it (tracker `rules.py` moves them to D06.7). `rebase4Signed` (reached only when
//! `offset > 1`, which `AllocHandle` never passes) is not ported: such a call is refused.

#![allow(non_upper_case_globals)]

use std::time::{Duration, Instant};

use tidb_base::Error;
use tidb_base::dbterror::ClassAutoid;
use tidb_base::errno;
use tidb_base::gofmt::Arg;

/// `minStep`.
const minStep: i64 = 30000;
/// `maxStep`.
const maxStep: i64 = 2000000;
/// `defaultConsumeTime`.
const defaultConsumeTime: Duration = Duration::from_secs(10);
/// `minIncrement`.
const minIncrement: i64 = 1;
/// `maxIncrement`.
const maxIncrement: i64 = 65535;
/// The default `step` (`var step = int64(30000)`).
pub const DefaultStep: i64 = 30000;

/// `errInvalidTableID`.
// go-dep: github.com/pingcap/tidb/pkg/meta/autoid.errInvalidTableID L1
fn err_invalid_table_id() -> Error {
    ClassAutoid
        .new_std(errno::ErrInvalidTableID)
        .gen_with_stack_by_args(vec![Arg::from("Invalid tableID".to_owned())])
}

/// `errInvalidIncrementAndOffset`.
// go-dep: github.com/pingcap/tidb/pkg/meta/autoid.errInvalidIncrementAndOffset L1
fn err_invalid_increment_and_offset(increment: i64, offset: i64) -> Error {
    ClassAutoid
        .new_std(errno::ErrInvalidIncrementAndOffset)
        .gen_with_stack_by_args(vec![Arg::from(increment), Arg::from(offset)])
}

/// `ErrAutoincReadFailed`.
// go-dep: github.com/pingcap/tidb/pkg/meta/autoid.ErrAutoincReadFailed L1
pub fn err_autoinc_read_failed() -> Error {
    ClassAutoid.new_std(errno::ErrAutoincReadFailed)
}

/// The meta row-ID counter of one table inside a meta transaction (`meta.AutoIDAccessor`).
pub trait IdAccessor {
    /// `Get`: the counter, 0 if unset.
    // go-dep: github.com/pingcap/tidb/pkg/meta.AutoIDAccessor.Get L1
    fn get(&mut self) -> Result<i64, Error>;
    /// `Inc`: adds step and returns the new value.
    // go-dep: github.com/pingcap/tidb/pkg/meta.AutoIDAccessor.Inc L1
    fn inc(&mut self, step: i64) -> Result<i64, Error>;
}

/// Runs f in a new meta transaction (`kv.RunInNewTxn(ctx, store, true, ...)`).
pub type RunInNewTxn<'a> =
    dyn FnMut(&mut dyn FnMut(&mut dyn IdAccessor) -> Result<(), Error>) -> Result<(), Error> + 'a;

/// A signed row-ID allocator (`allocator`, `RowIDAllocType`, `isUnsigned = false`).
#[derive(Clone, Debug)]
pub struct RowIdAllocator {
    /// `tbID`.
    pub tb_id: i64,
    /// `base`: the last ID handed out.
    pub base: i64,
    /// `end`: the last ID of the cached range.
    pub end: i64,
    /// `step`.
    pub step: i64,
    /// `customStep`: `AUTO_ID_CACHE` was set (no dynamic step).
    pub custom_step: bool,
    /// `lastAllocTime`.
    pub last_alloc_time: Instant,
}

impl RowIdAllocator {
    /// `NewAllocator(r, dbID, tbID, false, RowIDAllocType, CustomAutoIncCacheOption(cache),
    /// AllocOptionTableInfoVersion(tb_version))` (cache = `TableInfo.AutoIDCache`).
    // go-part: NewAllocator (RowIDAllocType, signed) L1
    pub fn new(tb_id: i64, cache: i64, tb_version: u16, now: Instant) -> RowIdAllocator {
        let mut a = RowIdAllocator {
            tb_id,
            base: 0,
            end: 0,
            step: DefaultStep,
            custom_step: false,
            last_alloc_time: now,
        };
        // CustomAutoIncCacheOption.ApplyOn (D06.2)
        if cache != 0 {
            a.step = cache;
            a.custom_step = true;
        }
        // Use the MySQL compatible AUTO_INCREMENT mode: for the row-ID allocator,
        // AUTO_ID_CACHE 1 does not disable the cache.
        if a.custom_step && a.step == 1 && tb_version >= tidb_model::TableInfoVersion5 {
            a.custom_step = false;
            a.step = DefaultStep;
        }
        a
    }

    /// Alloc allocates n consecutive autoID for table with tableID, returning (min, max] of
    /// the allocated autoID batch.
    // go: allocator.Alloc L2 (signed only: alloc4Unsigned is D06.2)
    pub fn alloc(
        &mut self,
        n: u64,
        increment: i64,
        offset: i64,
        now: Instant,
        run_in_new_txn: &mut RunInNewTxn<'_>,
    ) -> Result<(i64, i64), Error> {
        if self.tb_id == 0 {
            return Err(err_invalid_table_id());
        }
        if n == 0 {
            return Ok((0, 0));
        }
        if !valid_increment_and_offset(increment, offset) {
            return Err(err_invalid_increment_and_offset(increment, offset));
        }
        self.alloc4_signed(n, increment, offset, now, run_in_new_txn)
    }

    // go: allocator.alloc4Signed L1
    fn alloc4_signed(
        &mut self,
        n: u64,
        increment: i64,
        offset: i64,
        now: Instant,
        run_in_new_txn: &mut RunInNewTxn<'_>,
    ) -> Result<(i64, i64), Error> {
        // Check offset rebase if necessary.
        if offset - 1 > self.base {
            // rebase4Signed (D06.2) is not ported: AllocHandle always passes offset 1.
            return Err(tidb_base::errors::new(format!(
                "row-ID allocator: offset {offset} above base {} needs rebase4Signed, not supported",
                self.base
            )));
        }
        // CalcNeededBatchSize calculates the total batch size needed.
        let mut n1 = calc_needed_batch_size(self.base, n as i64, increment, offset, false);

        // Condition alloc.base+N1 > alloc.end will overflow when alloc.base + N1 > MaxInt64. So need this.
        if i64::MAX - self.base <= n1 {
            return Err(err_autoinc_read_failed());
        }
        // The local rest is not enough for allocN, skip it.
        if self.base + n1 > self.end {
            let (mut new_base, mut new_end) = (0i64, 0i64);
            let start_time = now;
            let mut next_step = self.step;
            if !self.custom_step && self.end > 0 {
                // Although it may skip a segment here, we still think it is consumed.
                let consume_dur = start_time.saturating_duration_since(self.last_alloc_time);
                next_step = next_step_fn(self.step, consume_dur);
            }
            run_in_new_txn(&mut |id_acc: &mut dyn IdAccessor| {
                new_base = id_acc.get()?;
                // CalcNeededBatchSize calculates the total batch size needed on global base.
                n1 = calc_needed_batch_size(new_base, n as i64, increment, offset, false);
                // Although the step is customized by user, we still need to make sure nextStep is big enough for insert batch.
                if next_step < n1 {
                    next_step = n1;
                }
                let tmp_step = (i64::MAX - new_base).min(next_step);
                // The global rest is not enough for alloc.
                if tmp_step < n1 {
                    return Err(err_autoinc_read_failed());
                }
                new_end = id_acc.inc(tmp_step)?;
                Ok(())
            })?;
            // Store the step for non-customized-step allocator to calculate next dynamic step.
            if !self.custom_step {
                self.step = next_step;
            }
            self.last_alloc_time = now;
            if new_base == i64::MAX {
                return Err(err_autoinc_read_failed());
            }
            self.base = new_base;
            self.end = new_end;
        }
        let min = self.base;
        self.base += n1;
        Ok((min, self.base))
    }
}

// go: validIncrementAndOffset L1
fn valid_increment_and_offset(increment: i64, offset: i64) -> bool {
    (minIncrement..=maxIncrement).contains(&increment)
        && (minIncrement..=maxIncrement).contains(&offset)
}

/// CalcNeededBatchSize is used to calculate batch size for autoID allocation.
/// It firstly seeks to the first valid position based on increment and offset,
/// then plus the length remained, which could be (n-1) * increment.
// go: CalcNeededBatchSize L1 [M-INT]
pub fn calc_needed_batch_size(
    base: i64,
    n: i64,
    increment: i64,
    offset: i64,
    is_unsigned: bool,
) -> i64 {
    if increment == 1 {
        return n;
    }
    if is_unsigned {
        // SeekToFirstAutoIDUnSigned seeks to the next unsigned valid position.
        let mut nr = seek_to_first_auto_id_unsigned(base as u64, increment as u64, offset as u64);
        // Calculate the total batch size needed.
        nr = nr.wrapping_add((n as u64).wrapping_sub(1).wrapping_mul(increment as u64));
        return nr.wrapping_sub(base as u64) as i64;
    }
    let mut nr = seek_to_first_auto_id_signed(base, increment, offset);
    // Calculate the total batch size needed.
    nr = nr.wrapping_add((n.wrapping_sub(1)).wrapping_mul(increment));
    nr.wrapping_sub(base)
}

/// SeekToFirstAutoIDSigned seeks to the next valid signed position.
// go: SeekToFirstAutoIDSigned L1 [M-INT]
pub fn seek_to_first_auto_id_signed(base: i64, increment: i64, offset: i64) -> i64 {
    let nr = (base.wrapping_add(increment).wrapping_sub(offset)).wrapping_div(increment);
    nr.wrapping_mul(increment).wrapping_add(offset)
}

/// SeekToFirstAutoIDUnSigned seeks to the next valid unsigned position.
// go: SeekToFirstAutoIDUnSigned L1 [M-INT]
pub fn seek_to_first_auto_id_unsigned(base: u64, increment: u64, offset: u64) -> u64 {
    let nr = (base.wrapping_add(increment).wrapping_sub(offset)) / increment;
    nr.wrapping_mul(increment).wrapping_add(offset)
}

/// NextStep return new auto id step according to previous step and consuming time.
// go: NextStep L1
pub fn next_step_fn(cur_step: i64, consume_dur: Duration) -> i64 {
    let consume_rate = defaultConsumeTime.as_secs_f64() / consume_dur.as_secs_f64();
    // Go's float64 -> int64 conversion of an out-of-range value is target-defined; the
    // result is clamped right after, and a finite rate times a step stays in range.
    let res = (cur_step as f64 * consume_rate) as i64;
    if res < minStep {
        return minStep;
    } else if res > maxStep {
        return maxStep;
    }
    res
}
