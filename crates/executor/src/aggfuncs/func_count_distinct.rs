//! go-file: pkg/executor/aggfuncs/func_count_distinct.go
//!
//! `COUNT(DISTINCT)` (part B1 of D12.4: the six `countOriginalWithDistinct*` implementations
//! and the encoding helpers `evalAndEncode`, `appendInt64`, `appendFloat64`, `appendDecimal`,
//! `WriteTime`, `appendTime`, `appendDuration`). `APPROX_COUNT_DISTINCT` (`intHash64` and
//! `approxCountDistinct*`) belongs to part B2.
//!
//! The DISTINCT sets are `util/set`'s sets in the allocator of [`AggFunc::set_alloc`] (A1,
//! spec §5.3); the memory deltas Go adds up are not returned (spec §9 item 1).

// Go identifiers for EvalType constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors::errorf;
use tidb_base::gofmt::Arg;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_codec::bytes::encode_compact_bytes;
use tidb_expression::context::EvalContext;
use tidb_expression::expression::{EvalState, ExprRef};
use tidb_kv::set::{
    Float64SetWithMemoryUsage, Int64SetWithMemoryUsage, StringSetWithMemoryUsage,
    new_float64_set_with_memory_usage, new_int64_set_with_memory_usage,
    new_string_set_with_memory_usage,
};
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32,
};
use tidb_types::MyDecimal;
use tidb_types::collate::{Collator, get_collator};
use tidb_types::time::{Duration, Time};

use super::aggfuncs::{AggFunc, AggFuncUpdateContext, PartialResult, partial_result_mut};
use super::func_count::BaseCount;

#[derive(Debug)]
struct PartialResult4CountDistinctInt {
    val_set: Int64SetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct4Int.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct4Int {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct4Int {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct4Int{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct4Int {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct4Int {
    // go: countOriginalWithDistinct4Int.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_int64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountDistinctInt { val_set })
    }

    // go: countOriginalWithDistinct4Int.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountDistinctInt>(pr);
        p.val_set = new_int64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct4Int.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctInt>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct4Int.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctInt>(pr);

        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_int(sctx, st, row)? else {
                continue;
            };
            if p.val_set.exist(input) {
                continue;
            }
            p.val_set.insert(input);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4CountDistinctReal {
    val_set: Float64SetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct4Real.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct4Real {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct4Real {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct4Real{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct4Real {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct4Real {
    // go: countOriginalWithDistinct4Real.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_float64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountDistinctReal { val_set })
    }

    // go: countOriginalWithDistinct4Real.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountDistinctReal>(pr);
        p.val_set = new_float64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct4Real.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctReal>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct4Real.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctReal>(pr);

        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_real(sctx, st, row)? else {
                continue;
            };
            if p.val_set.exist(input) {
                continue;
            }
            p.val_set.insert(input);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4CountDistinctDecimal {
    val_set: StringSetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct4Decimal.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct4Decimal {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct4Decimal {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct4Decimal{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct4Decimal {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct4Decimal {
    // go: countOriginalWithDistinct4Decimal.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountDistinctDecimal { val_set })
    }

    // go: countOriginalWithDistinct4Decimal.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountDistinctDecimal>(pr);
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct4Decimal.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctDecimal>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct4Decimal.UpdatePartialResult L2 (memDelta dropped, A1 [M-ERRVAL])
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctDecimal>(pr);

        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_decimal(sctx, st, row)? else {
                continue;
            };
            let (hash, err) = input.to_hash_key();
            err?;
            if p.val_set.exist(&hash) {
                continue;
            }
            p.val_set.insert(&hash);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4CountDistinctDuration {
    val_set: Int64SetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct4Duration.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct4Duration {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct4Duration {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct4Duration{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct4Duration {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct4Duration {
    // go: countOriginalWithDistinct4Duration.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_int64_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountDistinctDuration { val_set })
    }

    // go: countOriginalWithDistinct4Duration.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountDistinctDuration>(pr);
        p.val_set = new_int64_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct4Duration.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctDuration>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct4Duration.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctDuration>(pr);

        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_duration(sctx, st, row)? else {
                continue;
            };
            if p.val_set.exist(input.duration) {
                continue;
            }
            p.val_set.insert(input.duration);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4CountDistinctString {
    val_set: StringSetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct4String.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct4String {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct4String {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct4String{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct4String {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct4String {
    // go: countOriginalWithDistinct4String.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountDistinctString { val_set })
    }

    // go: countOriginalWithDistinct4String.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountDistinctString>(pr);
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct4String.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctString>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct4String.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountDistinctString>(pr);
        let collator = get_collator(self.base.base.args[0].get_type(sctx).get_collate());

        for &row in rows_in_group {
            let Some(input) = self.base.base.args[0].eval_string(sctx, st, row)? else {
                continue;
            };
            let key = collator.key(&input);
            if p.val_set.exist(&key) {
                continue;
            }
            p.val_set.insert(&key);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

#[derive(Debug)]
struct PartialResult4CountWithDistinct {
    val_set: StringSetWithMemoryUsage<ColAlloc>,
}

/// countOriginalWithDistinct.
#[derive(Clone, Debug)]
pub struct CountOriginalWithDistinct {
    pub base: BaseCount,
    /// The allocator of the DISTINCT set (A1, spec §5.3).
    alloc: ColAlloc,
}

impl CountOriginalWithDistinct {
    /// The function with the set in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal countOriginalWithDistinct{baseCount{base}}
    pub fn new(base: BaseCount) -> Self {
        CountOriginalWithDistinct {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

impl AggFunc for CountOriginalWithDistinct {
    // go: countOriginalWithDistinct.AllocPartialResult L2 (memDelta dropped, A1; the set's allocator, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        let (val_set, _) = new_string_set_with_memory_usage(self.alloc.clone());
        Box::new(PartialResult4CountWithDistinct { val_set })
    }

    // go: countOriginalWithDistinct.ResetPartialResult L2 (the set's allocator, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4CountWithDistinct>(pr);
        p.val_set = new_string_set_with_memory_usage(self.alloc.clone()).0;
    }

    // go: countOriginalWithDistinct.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountWithDistinct>(pr);
        chk.append_int64(self.base.base.col(), p.val_set.count() as i64);
        Ok(())
    }

    // go: countOriginalWithDistinct.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4CountWithDistinct>(pr);

        let mut encoded_bytes = Vec::new();
        let mut collators = Vec::with_capacity(self.base.base.args.len());
        for arg in &self.base.base.args {
            collators.push(get_collator(arg.get_type(sctx).get_collate()));
        }
        // 16 bytes covers every fixed-width value encoded into buf (int64, float64, Time, Duration).
        let mut buf = [0u8; 16];

        for &row in rows_in_group {
            let mut has_null = false;
            encoded_bytes.clear();

            for (arg, &collator) in self.base.base.args.iter().zip(collators.iter()) {
                let (next_bytes, is_null) =
                    eval_and_encode(sctx, st, arg, collator, row, &mut buf, encoded_bytes)?;
                encoded_bytes = next_bytes;
                if is_null {
                    has_null = true;
                    break;
                }
            }
            if has_null || p.val_set.exist(&encoded_bytes) {
                continue;
            }
            p.val_set.insert(&encoded_bytes);
        }

        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the DISTINCT set (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

/// evalAndEncode eval one row with an expression and encode value to bytes.
// go: evalAndEncode L2 (`st: &EvalState` for expression evaluation, N4 §3.4)
pub(crate) fn eval_and_encode(
    sctx: &dyn EvalContext,
    st: &EvalState,
    arg: &ExprRef,
    collator: &dyn Collator,
    row: Row<'_>,
    buf: &mut [u8],
    mut encoded_bytes: Vec<u8>,
) -> Result<(Vec<u8>, bool), Error> {
    let tp = arg.get_type(sctx).eval_type();
    match tp {
        ETInt => {
            let Some(val) = arg.eval_int(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_int64(encoded_bytes, buf, val);
        }
        ETReal => {
            let Some(val) = arg.eval_real(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_float64(encoded_bytes, buf, val);
        }
        ETDecimal => {
            let Some(val) = arg.eval_decimal(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_decimal(encoded_bytes, &val)?;
        }
        ETTimestamp | ETDatetime => {
            let Some(val) = arg.eval_time(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_time(encoded_bytes, buf, val);
        }
        ETDuration => {
            let Some(val) = arg.eval_duration(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_duration(encoded_bytes, buf, val);
        }
        ETJson => {
            let Some(val) = arg.eval_json(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = val.hash_value(encoded_bytes);
        }
        ETVectorFloat32 => {
            let Some(val) = arg.eval_vector_float32(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = val.serialize_to(encoded_bytes);
        }
        ETString => {
            let Some(val) = arg.eval_string(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encode_compact_bytes(&mut encoded_bytes, &collator.key(&val));
        }
        _ => {
            return Err(errorf(
                "unsupported column type for encode %d",
                &[Arg::from(tp.0 as i64)],
            ));
        }
    }
    Ok((encoded_bytes, false))
}

// go: appendInt64 L1
fn append_int64(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: i64) -> Vec<u8> {
    buf[..8].copy_from_slice(&val.to_ne_bytes());
    encoded_bytes.extend_from_slice(&buf[..8]);
    encoded_bytes
}

// go: appendFloat64 L2 (R23 #390: fold -0.0 to +0.0 so multi-arg matches single-arg Float64Set)
fn append_float64(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: f64) -> Vec<u8> {
    // Ruling R23 (#390, constitution art. 9.3): -0.0 == +0.0 in SQL and single-arg Float64Set,
    // so canonicalize -0.0 to +0.0 before writing the 8-byte key.
    let val = if val == 0.0 { 0.0 } else { val };
    buf[..8].copy_from_slice(&val.to_bits().to_ne_bytes());
    encoded_bytes.extend_from_slice(&buf[..8]);
    encoded_bytes
}

// go: appendDecimal L1 [M-ERRVAL]
fn append_decimal(mut encoded_bytes: Vec<u8>, val: &MyDecimal) -> Result<Vec<u8>, Error> {
    let (hash, err) = val.to_hash_key();
    encoded_bytes.extend_from_slice(&hash);
    err?;
    Ok(encoded_bytes)
}

/// WriteTime writes `t` into `buf`.
// go: WriteTime L1
pub fn write_time(buf: &mut [u8], t: Time) {
    buf[..2].copy_from_slice(&(t.year() as u16).to_be_bytes());
    buf[2] = t.month() as u8;
    buf[3] = t.day() as u8;
    buf[4] = t.hour() as u8;
    buf[5] = t.minute() as u8;
    buf[6] = t.second() as u8;
    buf[8..12].copy_from_slice(&(t.microsecond() as u32).to_be_bytes());
    buf[12] = t.tp();
    buf[13] = t.fsp() as u8;

    buf[7] = 0;
    buf[14] = 0;
    buf[15] = 0;
}

// go: appendTime L2 (R23 #390: zero Type/Fsp bytes so equal times of different Type/Fsp match)
fn append_time(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: Time) -> Vec<u8> {
    write_time(buf, val);
    // Ruling R23 (#390, constitution art. 9.3): Time comparison ignores Type and Fsp, while
    // expressions such as TIMESTAMP(str) and COALESCE(date, datetime) can yield equal times
    // with differing Fsp or Type in one argument column. Clear bytes 12..14 to key on value only.
    buf[12] = 0;
    buf[13] = 0;
    encoded_bytes.extend_from_slice(&buf[..16]);
    encoded_bytes
}

// go: appendDuration L2 (R23 #390: encode Duration value only, omitting Fsp to match single-arg)
fn append_duration(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: Duration) -> Vec<u8> {
    // Ruling R23 (#390, constitution art. 9.3): Duration comparison and single-arg
    // CountOriginalWithDistinct4Duration key on Duration only, ignoring Fsp.
    buf[..8].copy_from_slice(&val.duration.to_ne_bytes());
    encoded_bytes.extend_from_slice(&buf[..8]);
    encoded_bytes
}
