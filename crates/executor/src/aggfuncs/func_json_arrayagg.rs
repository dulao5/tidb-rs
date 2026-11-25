//! go-file: pkg/executor/aggfuncs/func_json_arrayagg.go
//!
//! `JSON_ARRAYAGG` (part B1 of D12.4).
//!
//! The growing `entries` slice allocates through [`AggFunc::set_alloc`] (A1, spec §5.3); the
//! manual `memDelta` Go adds up via `getValMemDelta` is dropped (spec §3.6, §9 item 1). The
//! spill serialization methods (`SerializePartialResult`, `DeserializePartialResult`,
//! `deserializeForSpill`) belong to part S.

use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_types::json::{
    ErrUnsupportedSecondArgumentType, JSONValue, create_binary_json_with_check,
};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};
use super::func_json_objectagg::{RealJsonValue, get_real_json_value};

/// jsonArrayagg.
#[derive(Clone, Debug)]
pub struct JsonArrayagg {
    pub base: BaseAggFunc,
    /// The allocator of the partial result's `entries` buffer (A1, spec §5.3).
    alloc: ColAlloc,
}

impl JsonArrayagg {
    /// The function with `entries` in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal jsonArrayagg{base}
    pub fn new(base: BaseAggFunc) -> Self {
        JsonArrayagg {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

#[derive(Debug)]
struct PartialResult4JsonArrayagg {
    entries: AllocVec<JSONValue<'static>, ColAlloc>,
}

impl AggFunc for JsonArrayagg {
    // go: jsonArrayagg.AllocPartialResult L2 (memDelta dropped, A1; entries in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4JsonArrayagg {
            entries: AllocVec::new_in(self.alloc.clone()),
        })
    }

    // go: jsonArrayagg.ResetPartialResult L1
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4JsonArrayagg>(pr);
        p.entries.clear();
    }

    // go: jsonArrayagg.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4JsonArrayagg>(pr);
        if p.entries.is_empty() {
            chk.append_null(self.base.col());
            return Ok(());
        }

        let entries: Vec<JSONValue<'static>> = p.entries.iter().cloned().collect();
        let json = create_binary_json_with_check(&JSONValue::Array(entries))?;
        chk.append_json(self.base.col(), &json);
        Ok(())
    }

    // go: jsonArrayagg.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        ctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4JsonArrayagg>(pr);
        for &row in rows_in_group {
            let item = self.base.args[0].eval(ctx, st, row)?;
            let real_item = get_real_json_value(&item, &self.base.args[0].get_type(ctx))?;
            match real_item {
                RealJsonValue::Supported(val) => p.entries.push(val),
                RealJsonValue::Unsupported(val) => {
                    return Err(
                        ErrUnsupportedSecondArgumentType.gen_with_stack_by_args(vec![val.arg()])
                    );
                }
            }
        }
        Ok(())
    }

    // go: jsonArrayagg.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4JsonArrayagg>(src);
        let p2 = partial_result_mut::<PartialResult4JsonArrayagg>(dst);
        p2.entries.extend(p1.entries.iter().cloned());
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the partial result entries (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}
