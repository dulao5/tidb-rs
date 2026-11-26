//! go-file: pkg/executor/aggfuncs/func_count_distinct.go
//!
//! `COUNT(DISTINCT)` (part B1 of D12.4: the six `countOriginalWithDistinct*` implementations
//! and the encoding helpers `evalAndEncode`, `appendInt64`, `appendFloat64`, `appendDecimal`,
//! `WriteTime`, `appendTime`, `appendDuration`) and `APPROX_COUNT_DISTINCT` (part B2 of D12.4:
//! `intHash64`, `partialResult4ApproxCountDistinct`, `baseApproxCountDistinct`, and the four
//! mode implementations).
//!
//! The DISTINCT sets and the `APPROX_COUNT_DISTINCT` hash table buffer allocate through
//! [`AggFunc::set_alloc`] (A1, spec §5.3); the manual `MemUsage` method and `memDelta`
//! returns are omitted as A1 exceptions (spec §3.6, §9 item 1).
//!
//! Note on encoding isolation (issue #393 / #394): `COUNT(DISTINCT)` uses [`eval_and_encode`]
//! with ruling R23 (#390) canonicalization (`±0.0`, `Duration.Fsp`, `Time.Type`/`Time.Fsp`),
//! whereas `APPROX_COUNT_DISTINCT` uses a separate Go-raw encoder [`eval_and_encode_go_raw`]
//! so serialized `Partial1` hashes and TiFlash / Go TiDB cluster interoperability stay
//! bit-identical to Go v8.5.8 (Constitution art. 1.2, art. 9.3).

// Go identifiers for EvalType constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

// go-exception: A1 partialResult4ApproxCountDistinct.MemUsage (only computes Go's manual
// memDelta for APPROX_COUNT_DISTINCT; Rust tracks the buffer directly in ColAlloc, spec §3.6)

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_base::errors::{self as goerrors, errorf};
use tidb_base::farm;
use tidb_base::gofmt::Arg;
use tidb_base::gostd::conv::{f64_to_u8, f64_to_u64};
use tidb_base::gostd::math::{log, log2};
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_codec::bytes::encode_compact_bytes;
use tidb_codec::number::{decode_uvarint, encode_uvarint};
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

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};
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

/// Go v8.5.8 raw `evalAndEncode` for `APPROX_COUNT_DISTINCT` (without R23 #390 canonicalization).
///
/// Kept separate from [`eval_and_encode`] so `APPROX_COUNT_DISTINCT` preserves bit-identical
/// `farm::hash64` hashes and `Partial1` serialized bytes with Go v8.5.8 and TiFlash
/// (issue #393, upstream-pitfall #394, Constitution art. 1.2 & art. 9.3).
// glue: Go v8.5.8 raw evalAndEncode for APPROX_COUNT_DISTINCT (isolated from R23 #390; see #394)
pub(crate) fn eval_and_encode_go_raw(
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
            encoded_bytes = append_float64_go_raw(encoded_bytes, buf, val);
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
            encoded_bytes = append_time_go_raw(encoded_bytes, buf, val);
        }
        ETDuration => {
            let Some(val) = arg.eval_duration(sctx, st, row)? else {
                return Ok((encoded_bytes, true));
            };
            encoded_bytes = append_duration_go_raw(encoded_bytes, buf, val);
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

// glue: Go v8.5.8 raw appendFloat64 (preserves -0.0 sign bit, isolated from R23 #390)
fn append_float64_go_raw(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: f64) -> Vec<u8> {
    buf[..8].copy_from_slice(&val.to_bits().to_ne_bytes());
    encoded_bytes.extend_from_slice(&buf[..8]);
    encoded_bytes
}

// glue: Go v8.5.8 raw appendTime (preserves Type and Fsp bytes, isolated from R23 #390)
fn append_time_go_raw(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: Time) -> Vec<u8> {
    write_time(buf, val);
    encoded_bytes.extend_from_slice(&buf[..16]);
    encoded_bytes
}

// glue: Go v8.5.8 raw appendDuration (16-byte Go struct layout: int64 Duration + int Fsp, isolated from R23 #390)
fn append_duration_go_raw(mut encoded_bytes: Vec<u8>, buf: &mut [u8], val: Duration) -> Vec<u8> {
    buf[..8].copy_from_slice(&val.duration.to_ne_bytes());
    buf[8..16].copy_from_slice(&(val.fsp as i64).to_ne_bytes());
    encoded_bytes.extend_from_slice(&buf[..16]);
    encoded_bytes
}

// go: intHash64 L1 [M-INT]
pub(crate) fn int_hash64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// The maximum degree of buffer size before the values are discarded.
const UNIQUES_HASH_MAX_SIZE_DEGREE: u8 = 17;
/// The maximum number of elements before the values are discarded.
const UNIQUES_HASH_MAX_SIZE: u32 = 1u32 << (UNIQUES_HASH_MAX_SIZE_DEGREE - 1);
/// Initial buffer size degree.
const UNIQUES_HASH_SET_INITIAL_SIZE_DEGREE: u8 = 4;
/// The number of least significant bits used for thinning. The remaining high-order bits are
/// used to determine the position in the hash table.
const UNIQUES_HASH_BITS_FOR_SKIP: u8 = 32 - UNIQUES_HASH_MAX_SIZE_DEGREE;

type ApproxCountDistinctHashValue = u32;

/// `partialResult4ApproxCountDistinct` uses the BJKST algorithm to compute an approximate
/// count distinct.
#[derive(Debug)]
pub struct PartialResult4ApproxCountDistinct {
    /// Number of elements.
    size: u32,
    /// The size of the table as a power of 2.
    size_degree: u8,
    /// Skip elements not divisible by `2 ^ skip_degree`.
    skip_degree: u8,
    /// The hash table contains an element with a hash value of 0.
    has_zero: bool,
    /// Open-addressed hash table buffer tracked through `alloc` (A1, spec §5.3).
    buf: AllocVec<ApproxCountDistinctHashValue, ColAlloc>,
    alloc: ColAlloc,
}

/// NewPartialResult4ApproxCountDistinct builds a partial result for agg function
/// ApproxCountDistinct.
// go: NewPartialResult4ApproxCountDistinct L1
pub fn new_partial_result_4_approx_count_distinct() -> PartialResult4ApproxCountDistinct {
    new_partial_result_4_approx_count_distinct_in(ColAlloc::Global)
}

// glue: NewPartialResult4ApproxCountDistinct in a ColAlloc (A1 3.3, spec §5.3)
fn new_partial_result_4_approx_count_distinct_in(
    alloc: ColAlloc,
) -> PartialResult4ApproxCountDistinct {
    let mut p = PartialResult4ApproxCountDistinct {
        size: 0,
        size_degree: 0,
        skip_degree: 0,
        has_zero: false,
        buf: AllocVec::new_in(alloc.clone()),
        alloc,
    };
    p.reset();
    p
}

impl PartialResult4ApproxCountDistinct {
    /// InsertHash64 inserts a 64-bit hash value by truncating to `u32`.
    // go: partialResult4ApproxCountDistinct.InsertHash64 L1
    pub fn insert_hash64(&mut self, x: u64) {
        // no need to rehash, just cast into uint32
        self.insert_hash(x as ApproxCountDistinctHashValue);
    }

    // go: partialResult4ApproxCountDistinct.alloc L2 (allocates buf in ColAlloc, spec §5.3)
    fn alloc(&mut self, new_size_degree: u8) {
        self.size = 0;
        self.skip_degree = 0;
        self.has_zero = false;
        let mut buf = AllocVec::new_in(self.alloc.clone());
        buf.resize((1u32 << new_size_degree) as usize, 0);
        self.buf = buf;
        self.size_degree = new_size_degree;
    }

    // go: partialResult4ApproxCountDistinct.reset L1
    fn reset(&mut self) {
        self.alloc(UNIQUES_HASH_SET_INITIAL_SIZE_DEGREE);
    }

    // go: partialResult4ApproxCountDistinct.bufSize L1
    #[inline]
    fn buf_size(&self) -> u32 {
        1u32 << self.size_degree
    }

    // go: partialResult4ApproxCountDistinct.mask L1
    #[inline]
    fn mask(&self) -> u32 {
        self.buf_size() - 1
    }

    // go: partialResult4ApproxCountDistinct.place L1
    #[inline]
    fn place(&self, x: ApproxCountDistinctHashValue) -> u32 {
        (x >> UNIQUES_HASH_BITS_FOR_SKIP) & self.mask()
    }

    /// Increase the size of the buffer 2 times or up to `new_size_degree`.
    // go: partialResult4ApproxCountDistinct.resize L2 (allocates new buf in ColAlloc, spec §5.3)
    fn resize(&mut self, mut new_size_degree: u8) {
        let old_size = self.buf_size();
        if new_size_degree == 0 {
            new_size_degree = self.size_degree + 1;
        }
        let mut new_buf = AllocVec::new_in(self.alloc.clone());
        new_buf.resize((1u32 << new_size_degree) as usize, 0);
        let old_buf = std::mem::replace(&mut self.buf, new_buf);
        self.size_degree = new_size_degree;

        // Move some items to new locations.
        for i in 0..old_size as usize {
            let x = old_buf[i];
            if x != 0 {
                self.reinsert_impl(x);
            }
        }
    }

    // go: partialResult4ApproxCountDistinct.readAndMerge L1 [M-F2I]
    pub(crate) fn read_and_merge(&mut self, mut rb: &[u8]) -> Result<(), Error> {
        let rhs_skip_degree = rb[0];
        rb = &rb[1..];

        if rhs_skip_degree > self.skip_degree {
            self.skip_degree = rhs_skip_degree;
            self.rehash();
        }

        let (mut rb, rhs_size) = decode_uvarint(rb)?;

        if rhs_size > u64::from(UNIQUES_HASH_MAX_SIZE) {
            return Err(goerrors::new(
                "Cannot read partialResult4ApproxCountDistinct: too large size degree",
            ));
        }

        if self.buf_size() < rhs_size as u32 {
            let new_size_degree = UNIQUES_HASH_SET_INITIAL_SIZE_DEGREE
                .max(f64_to_u8(log2((rhs_size - 1) as f64)).wrapping_add(2));
            self.resize(new_size_degree);
        }

        for _ in 0..(rhs_size as u32) {
            let x = ApproxCountDistinctHashValue::from_ne_bytes(rb[..4].try_into().unwrap());
            rb = &rb[4..];
            self.insert_hash(x);
        }

        Ok(())
    }

    /// Correct system errors due to collisions during hashing in uint32.
    // go: partialResult4ApproxCountDistinct.fixedSize L1 [M-INT, M-F2I]
    pub(crate) fn fixed_size(&self) -> u64 {
        if self.skip_degree == 0 {
            return u64::from(self.size);
        }

        let shift = 1u64.checked_shl(u32::from(self.skip_degree)).unwrap_or(0);
        let mut res = u64::from(self.size).wrapping_mul(shift);

        // Pseudo-random remainder.
        res = res.wrapping_add(int_hash64(u64::from(self.size)) & shift.wrapping_sub(1));

        // When different elements randomly scattered across 2^32 buckets, filled buckets with average of `res` obtained.
        let p32 = 1u64 << 32;
        let fixed_res =
            (p32 as f64 * (log(p32 as f64) - log(p32.wrapping_sub(res) as f64))).round();
        f64_to_u64(fixed_res)
    }

    // go: partialResult4ApproxCountDistinct.insertHash L1
    fn insert_hash(&mut self, hash_value: ApproxCountDistinctHashValue) {
        if !self.good(hash_value) {
            return;
        }

        self.insert_impl(hash_value);
        self.shrink_if_need();
    }

    /// The value is divided by `2 ^ skip_degree`.
    // go: partialResult4ApproxCountDistinct.good L1 [M-INT]
    #[inline]
    fn good(&self, hash: ApproxCountDistinctHashValue) -> bool {
        let shifted = hash
            .checked_shr(u32::from(self.skip_degree))
            .unwrap_or(0)
            .checked_shl(u32::from(self.skip_degree))
            .unwrap_or(0);
        hash == shifted
    }

    /// Insert a value.
    // go: partialResult4ApproxCountDistinct.insertImpl L1
    fn insert_impl(&mut self, x: ApproxCountDistinctHashValue) {
        if x == 0 {
            if !self.has_zero {
                self.size += 1;
            }
            self.has_zero = true;
            return;
        }

        let mut place_value = self.place(x);
        while self.buf[place_value as usize] != 0 && self.buf[place_value as usize] != x {
            place_value += 1;
            place_value &= self.mask();
        }

        if self.buf[place_value as usize] == x {
            return;
        }

        self.buf[place_value as usize] = x;
        self.size += 1;
    }

    /// If the hash table is full enough, then do resize.
    /// If there are too many items, then throw half the pieces until they are small enough.
    // go: partialResult4ApproxCountDistinct.shrinkIfNeed L1
    fn shrink_if_need(&mut self) {
        if self.size > self.max_fill() {
            if self.size > UNIQUES_HASH_MAX_SIZE {
                while self.size > UNIQUES_HASH_MAX_SIZE {
                    self.skip_degree += 1;
                    self.rehash();
                }
            } else {
                self.resize(0);
            }
        }
    }

    // go: partialResult4ApproxCountDistinct.maxFill L1
    #[inline]
    fn max_fill(&self) -> u32 {
        1u32 << (self.size_degree - 1)
    }

    /// Delete all values whose hashes do not divide by `2 ^ skip_degree`.
    // go: partialResult4ApproxCountDistinct.rehash L1
    fn rehash(&mut self) {
        let n = self.buf_size() as usize;
        for i in 0..n {
            if self.buf[i] != 0 && !self.good(self.buf[i]) {
                self.buf[i] = 0;
                self.size -= 1;
            }
        }

        for i in 0..n {
            if self.buf[i] != 0 && i as u32 != self.place(self.buf[i]) {
                let x = self.buf[i];
                self.buf[i] = 0;
                self.reinsert_impl(x);
            }
        }
    }

    /// Insert a value into the new buffer that was in the old buffer.
    /// Used when increasing the size of the buffer, as well as when reading from a file.
    // go: partialResult4ApproxCountDistinct.reinsertImpl L1
    fn reinsert_impl(&mut self, x: ApproxCountDistinctHashValue) {
        let mut place_value = self.place(x);
        while self.buf[place_value as usize] != 0 {
            place_value += 1;
            place_value &= self.mask();
        }

        self.buf[place_value as usize] = x;
    }

    // go: partialResult4ApproxCountDistinct.merge L1
    fn merge(&mut self, tar: &PartialResult4ApproxCountDistinct) {
        if tar.skip_degree > self.skip_degree {
            self.skip_degree = tar.skip_degree;
            self.rehash();
        }

        if !self.has_zero && tar.has_zero {
            self.has_zero = true;
            self.size += 1;
            self.shrink_if_need();
        }

        for i in 0..tar.buf_size() as usize {
            if tar.buf[i] != 0 && self.good(tar.buf[i]) {
                self.insert_impl(tar.buf[i]);
                self.shrink_if_need();
            }
        }
    }

    /// Serialize encodes the partial result into its compact wire format.
    // go: partialResult4ApproxCountDistinct.Serialize L1
    pub fn serialize(&self) -> Vec<u8> {
        let mut res = Vec::with_capacity(1 + 10 + (self.size as usize) * 4);

        res.push(self.skip_degree);
        encode_uvarint(&mut res, u64::from(self.size));

        if self.has_zero {
            res.extend_from_slice(&0u32.to_le_bytes());
        }

        for i in 0..self.buf_size() as usize {
            if self.buf[i] != 0 {
                res.extend_from_slice(&self.buf[i].to_le_bytes());
            }
        }
        res
    }
}

/// baseApproxCountDistinct.
#[derive(Clone, Debug)]
pub struct BaseApproxCountDistinct {
    pub base: BaseAggFunc,
    alloc: ColAlloc,
}

impl BaseApproxCountDistinct {
    // glue: Go struct literal baseApproxCountDistinct{baseAggFunc{...}}
    pub fn new(base: BaseAggFunc) -> Self {
        BaseApproxCountDistinct {
            base,
            alloc: ColAlloc::Global,
        }
    }

    // go: baseApproxCountDistinct.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk_impl(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4ApproxCountDistinct>(pr);
        chk.append_int64(self.base.col(), p.fixed_size() as i64);
        Ok(())
    }

    // go: baseApproxCountDistinct.AllocPartialResult L2 (memDelta dropped, A1; buffer in ColAlloc, spec §5.3)
    fn alloc_partial_result_impl(&self) -> PartialResult {
        Box::new(new_partial_result_4_approx_count_distinct_in(
            self.alloc.clone(),
        ))
    }

    // go: baseApproxCountDistinct.ResetPartialResult L1
    fn reset_partial_result_impl(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4ApproxCountDistinct>(pr);
        p.reset();
    }

    // go: baseApproxCountDistinct.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result_impl(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4ApproxCountDistinct>(src);
        let p2 = partial_result_mut::<PartialResult4ApproxCountDistinct>(dst);
        p2.merge(p1);
        Ok(())
    }
}

/// approxCountDistinctOriginal.
#[derive(Clone, Debug)]
pub struct ApproxCountDistinctOriginal {
    pub base: BaseApproxCountDistinct,
}

impl ApproxCountDistinctOriginal {
    // glue: Go struct literal approxCountDistinctOriginal{base}
    pub fn new(base: BaseApproxCountDistinct) -> Self {
        ApproxCountDistinctOriginal { base }
    }

    // go: approxCountDistinctOriginal.UpdatePartialResult L2 (memDelta dropped, A1; uses eval_and_encode_go_raw isolated from R23 #390, see #393/#394)
    fn update_partial_result_impl(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4ApproxCountDistinct>(pr);
        let mut encoded_bytes = Vec::new();
        // 16 bytes covers every fixed-width value encoded into buf (int64, float64, Time, Duration).
        let mut buf = [0u8; 16];
        let mut collators = Vec::with_capacity(self.base.base.args.len());
        for arg in &self.base.base.args {
            collators.push(get_collator(arg.get_type(sctx).get_collate()));
        }

        for &row in rows_in_group {
            let mut has_null = false;
            encoded_bytes.clear();

            for (arg, &collator) in self.base.base.args.iter().zip(collators.iter()) {
                let (next_bytes, is_null) =
                    eval_and_encode_go_raw(sctx, st, arg, collator, row, &mut buf, encoded_bytes)?;
                encoded_bytes = next_bytes;
                if is_null {
                    has_null = true;
                    break;
                }
            }
            if has_null {
                continue;
            }
            let x = farm::hash64(&encoded_bytes);
            p.insert_hash64(x);
        }

        Ok(())
    }
}

impl AggFunc for ApproxCountDistinctOriginal {
    // glue: M-SELF delegation to embedded baseApproxCountDistinct.AllocPartialResult
    fn alloc_partial_result(&self) -> PartialResult {
        self.base.alloc_partial_result_impl()
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.ResetPartialResult
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.base.reset_partial_result_impl(pr);
    }

    // glue: forwards to approxCountDistinctOriginal.UpdatePartialResult
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.update_partial_result_impl(sctx, st, rows_in_group, pr)
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.MergePartialResult
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        self.base.merge_partial_result_impl(sctx, src, dst)
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.AppendFinalResult2Chunk
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.base.append_final_result2_chunk_impl(sctx, pr, chk)
    }

    // glue: A1 3.3 TrackedAlloc of the BJKST hash buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.base.alloc = alloc;
    }
}

/// approxCountDistinctPartial1.
#[derive(Clone, Debug)]
pub struct ApproxCountDistinctPartial1 {
    pub orig: ApproxCountDistinctOriginal,
}

impl ApproxCountDistinctPartial1 {
    // glue: Go struct literal approxCountDistinctPartial1{approxCountDistinctOriginal{base}}
    pub fn new(orig: ApproxCountDistinctOriginal) -> Self {
        ApproxCountDistinctPartial1 { orig }
    }

    // go: approxCountDistinctPartial1.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk_impl(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4ApproxCountDistinct>(pr);
        chk.append_bytes(self.orig.base.base.col(), &p.serialize());
        Ok(())
    }
}

impl AggFunc for ApproxCountDistinctPartial1 {
    // glue: M-SELF delegation to embedded baseApproxCountDistinct.AllocPartialResult
    fn alloc_partial_result(&self) -> PartialResult {
        self.orig.base.alloc_partial_result_impl()
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.ResetPartialResult
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.orig.base.reset_partial_result_impl(pr);
    }

    // glue: M-SELF delegation to embedded approxCountDistinctOriginal.UpdatePartialResult
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.orig
            .update_partial_result_impl(sctx, st, rows_in_group, pr)
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.MergePartialResult
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        self.orig.base.merge_partial_result_impl(sctx, src, dst)
    }

    // glue: forwards to approxCountDistinctPartial1.AppendFinalResult2Chunk
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.append_final_result2_chunk_impl(sctx, pr, chk)
    }

    // glue: A1 3.3 TrackedAlloc of the BJKST hash buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.orig.base.alloc = alloc;
    }
}

/// approxCountDistinctPartial2.
#[derive(Clone, Debug)]
pub struct ApproxCountDistinctPartial2 {
    pub partial1: ApproxCountDistinctPartial1,
}

impl ApproxCountDistinctPartial2 {
    // glue: Go struct literal approxCountDistinctPartial2{approxCountDistinctPartial1{...}}
    pub fn new(partial1: ApproxCountDistinctPartial1) -> Self {
        ApproxCountDistinctPartial2 { partial1 }
    }

    // go: approxCountDistinctPartial2.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result_impl(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4ApproxCountDistinct>(pr);
        for &row in rows_in_group {
            let Some(input) = self.partial1.orig.base.base.args[0].eval_string(sctx, st, row)?
            else {
                continue;
            };
            p.read_and_merge(&input)?;
        }
        Ok(())
    }
}

impl AggFunc for ApproxCountDistinctPartial2 {
    // glue: M-SELF delegation to embedded baseApproxCountDistinct.AllocPartialResult
    fn alloc_partial_result(&self) -> PartialResult {
        self.partial1.orig.base.alloc_partial_result_impl()
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.ResetPartialResult
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.partial1.orig.base.reset_partial_result_impl(pr);
    }

    // glue: forwards to approxCountDistinctPartial2.UpdatePartialResult
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.update_partial_result_impl(sctx, st, rows_in_group, pr)
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.MergePartialResult
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        self.partial1
            .orig
            .base
            .merge_partial_result_impl(sctx, src, dst)
    }

    // glue: M-SELF delegation to embedded approxCountDistinctPartial1.AppendFinalResult2Chunk
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.partial1.append_final_result2_chunk_impl(sctx, pr, chk)
    }

    // glue: A1 3.3 TrackedAlloc of the BJKST hash buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.partial1.orig.base.alloc = alloc;
    }
}

/// approxCountDistinctFinal.
#[derive(Clone, Debug)]
pub struct ApproxCountDistinctFinal {
    pub partial2: ApproxCountDistinctPartial2,
}

impl ApproxCountDistinctFinal {
    // glue: Go struct literal approxCountDistinctFinal{approxCountDistinctPartial2{...}}
    pub fn new(partial2: ApproxCountDistinctPartial2) -> Self {
        ApproxCountDistinctFinal { partial2 }
    }
}

impl AggFunc for ApproxCountDistinctFinal {
    // glue: M-SELF delegation to embedded baseApproxCountDistinct.AllocPartialResult
    fn alloc_partial_result(&self) -> PartialResult {
        self.partial2.partial1.orig.base.alloc_partial_result_impl()
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.ResetPartialResult
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        self.partial2
            .partial1
            .orig
            .base
            .reset_partial_result_impl(pr);
    }

    // glue: M-SELF delegation to embedded approxCountDistinctPartial2.UpdatePartialResult
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        self.partial2
            .update_partial_result_impl(sctx, st, rows_in_group, pr)
    }

    // glue: M-SELF delegation to embedded baseApproxCountDistinct.MergePartialResult
    fn merge_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        self.partial2
            .partial1
            .orig
            .base
            .merge_partial_result_impl(sctx, src, dst)
    }

    // go: approxCountDistinctFinal.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        self.partial2
            .partial1
            .orig
            .base
            .append_final_result2_chunk_impl(sctx, pr, chk)
    }

    // glue: A1 3.3 TrackedAlloc of the BJKST hash buffer (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.partial2.partial1.orig.base.alloc = alloc;
    }
}
