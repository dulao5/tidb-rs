//! go-file: pkg/executor/aggfuncs/func_json_objectagg.go
//!
//! `JSON_OBJECTAGG` (part B1 of D12.4) and the shared [`get_real_json_value`] conversion used
//! by both `JSON_ARRAYAGG` and `JSON_OBJECTAGG`.
//!
//! The growing `entries` map and its keys allocate through [`AggFunc::set_alloc`] (A1,
//! spec §5.3). Go's manual `getValMemDelta` is omitted under exception A1 (spec §3.6, §9 item 1).
//! The spill serialization methods (`SerializePartialResult`, `DeserializePartialResult`,
//! `deserializeForSpill`) belong to part S.

// go-exception: A1 getValMemDelta (only computes Go's manual memDelta for JSON_*AGG; Rust
// accounts the growing partial result containers through ColAlloc / TrackedAlloc, spec §3.6)

// Go identifiers for Datum kind constants in patterns (M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;
use std::collections::BTreeMap;

use allocator_api2::boxed::Box as AllocBox;
use allocator_api2::vec::Vec as AllocVec;
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::mysql;
use tidb_chunk::ColAlloc;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::row::Row;
use tidb_expression::expression::EvalState;
use tidb_kv::hack::{GoLayout, MemAwareMap, new_mem_aware_map_in};
use tidb_parser::gocharset as charset;
use tidb_parser::types::FieldType;
use tidb_types::convert::to_string as types_to_string;
use tidb_types::datum::{Datum, GoValue, KindBinaryLiteral, KindBytes, KindMysqlBit, KindString};
use tidb_types::json::{
    ErrInvalidJSONCharset, ErrJSONDocumentNULLKey, ErrUnsupportedSecondArgumentType, JSONValue,
    Opaque, create_binary_json_with_check,
};

use super::aggfuncs::{
    AggFunc, AggFuncUpdateContext, BaseAggFunc, PartialResult, partial_result_mut,
};

/// Result of [`get_real_json_value`]: either a value accepted by the JSON type switch in
/// `JSON_ARRAYAGG` / `JSON_OBJECTAGG`, or the raw [`GoValue`] to format into
/// `ErrUnsupportedSecondArgumentType`.
#[derive(Debug)]
pub(crate) enum RealJsonValue {
    Supported(JSONValue<'static>),
    Unsupported(GoValue),
}

/// Wrapper around [`JSONValue`] carrying Go's `any` (interface) layout for [`MemAwareMap`].
#[derive(Clone, Debug)]
struct JsonEntry(JSONValue<'static>);

impl GoLayout for JsonEntry {
    const GO_SIZE: u64 = 16;
    const GO_ALIGN: u64 = 8;
}

/// jsonObjectAgg.
#[derive(Clone, Debug)]
pub struct JsonObjectAgg {
    pub base: BaseAggFunc,
    /// The allocator of the partial result's map and keys (A1, spec §5.3).
    alloc: ColAlloc,
}

impl JsonObjectAgg {
    /// The function with `entries` in the global allocator until [`AggFunc::set_alloc`].
    // glue: Go struct literal jsonObjectAgg{base}
    pub fn new(base: BaseAggFunc) -> Self {
        JsonObjectAgg {
            base,
            alloc: ColAlloc::Global,
        }
    }
}

#[derive(Debug)]
struct PartialResult4JsonObjectAgg {
    entries: MemAwareMap<AllocBox<[u8], ColAlloc>, JsonEntry, ColAlloc>,
}

/// Copies `key` into `alloc` as a boxed byte slice (Go: `strings.Clone(key)`).
// glue: Go strings.Clone into the map's allocator (A1 3.3)
fn box_key_in(key: &[u8], alloc: ColAlloc) -> AllocBox<[u8], ColAlloc> {
    let mut buf = AllocVec::with_capacity_in(key.len(), alloc);
    buf.extend_from_slice(key);
    buf.into_boxed_slice()
}

impl AggFunc for JsonObjectAgg {
    // go: jsonObjectAgg.AllocPartialResult L2 (memDelta dropped, A1; map in ColAlloc, spec §5.3)
    fn alloc_partial_result(&self) -> PartialResult {
        Box::new(PartialResult4JsonObjectAgg {
            entries: new_mem_aware_map_in(0, self.alloc.clone()),
        })
    }

    // go: jsonObjectAgg.ResetPartialResult L2 (map in ColAlloc, spec §5.3)
    fn reset_partial_result(&self, pr: &mut PartialResult) {
        let p = partial_result_mut::<PartialResult4JsonObjectAgg>(pr);
        p.entries = new_mem_aware_map_in(0, self.alloc.clone());
    }

    // go: jsonObjectAgg.AppendFinalResult2Chunk L1
    fn append_final_result2_chunk(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        pr: &mut PartialResult,
        chk: &mut Chunk,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4JsonObjectAgg>(pr);
        if p.entries.m.is_empty() {
            chk.append_null(self.base.col());
            return Ok(());
        }

        let mut obj = BTreeMap::new();
        for (k, v) in &p.entries.m {
            obj.insert(k.to_vec(), v.0.clone());
        }
        let bj = create_binary_json_with_check(&JSONValue::Object(obj))?;
        chk.append_json(self.base.col(), &bj);
        Ok(())
    }

    // go: jsonObjectAgg.UpdatePartialResult L2 (memDelta dropped, A1)
    fn update_partial_result(
        &self,
        sctx: &dyn AggFuncUpdateContext,
        st: &EvalState,
        rows_in_group: &[Row<'_>],
        pr: &mut PartialResult,
    ) -> Result<(), Error> {
        let p = partial_result_mut::<PartialResult4JsonObjectAgg>(pr);
        for &row in rows_in_group {
            let Some(key) = self.base.args[0].eval_string(sctx, st, row)? else {
                return Err(ErrJSONDocumentNULLKey.clone());
            };

            let key_tp = self.base.args[0].get_type(sctx);
            if key_tp.get_charset() == charset::CharsetBin {
                return Err(ErrInvalidJSONCharset
                    .gen_with_stack_by_args(vec![Arg::from(key_tp.get_charset())]));
            }

            let key_boxed = box_key_in(&key, self.alloc.clone());
            let value = self.base.args[1].eval(sctx, st, row)?;
            let real_val = get_real_json_value(&value, &self.base.args[1].get_type(sctx))?;

            match real_val {
                RealJsonValue::Supported(val) => {
                    p.entries.set_ext(key_boxed, JsonEntry(val));
                }
                RealJsonValue::Unsupported(val) => {
                    return Err(
                        ErrUnsupportedSecondArgumentType.gen_with_stack_by_args(vec![val.arg()])
                    );
                }
            }
        }
        Ok(())
    }

    // go: jsonObjectAgg.MergePartialResult L2 (memDelta dropped, A1)
    fn merge_partial_result(
        &self,
        _sctx: &dyn AggFuncUpdateContext,
        src: &mut PartialResult,
        dst: &mut PartialResult,
    ) -> Result<(), Error> {
        let p1 = partial_result_mut::<PartialResult4JsonObjectAgg>(src);
        let p2 = partial_result_mut::<PartialResult4JsonObjectAgg>(dst);
        // When the result of this function is normalized, values having duplicate keys are
        // discarded, and only the last value encountered is used with that key in the returned
        // object.
        for (k, v) in &p1.entries.m {
            p2.entries.set(box_key_in(k, self.alloc.clone()), v.clone());
        }
        Ok(())
    }

    // glue: A1 3.3 TrackedAlloc of the partial result map and keys (spec §5.3)
    fn set_alloc(&mut self, alloc: ColAlloc) {
        self.alloc = alloc;
    }
}

// go: getRealJSONValue L1
pub(crate) fn get_real_json_value(value: &Datum, ft: &FieldType) -> Result<RealJsonValue, Error> {
    match value.kind() {
        KindBinaryLiteral | KindMysqlBit | KindBytes => {
            let buf = value.get_bytes().to_vec();
            return Ok(RealJsonValue::Supported(JSONValue::Opaque(Opaque {
                type_code: ft.get_type(),
                buf: Cow::Owned(buf),
            })));
        }
        KindString if ft.get_charset() == charset::CharsetBin => {
            let buf = value.get_bytes();
            let result_buf = if ft.get_type() == mysql::TypeString {
                // the tailing zero should also be in the opaque json
                let flen = usize::try_from(ft.get_flen())
                    .expect("runtime error: makeslice: len out of range");
                let mut out = vec![0u8; flen];
                let n = buf.len().min(out.len());
                out[..n].copy_from_slice(&buf[..n]);
                out
            } else {
                buf.to_vec()
            };
            return Ok(RealJsonValue::Supported(JSONValue::Opaque(Opaque {
                type_code: ft.get_type(),
                buf: Cow::Owned(result_buf),
            })));
        }
        _ => {}
    }

    let real_val = value.get_value();
    // appendBinary does not support some type such as uint8, types.time, so convert is needed here
    match real_val {
        GoValue::Nil => Ok(RealJsonValue::Supported(JSONValue::Nil)),
        GoValue::Bool(v) => Ok(RealJsonValue::Supported(JSONValue::Bool(v))),
        GoValue::Int64(v) => Ok(RealJsonValue::Supported(JSONValue::Int64(v))),
        GoValue::Uint64(v) => Ok(RealJsonValue::Supported(JSONValue::Uint64(v))),
        GoValue::Float32(v) => Ok(RealJsonValue::Supported(JSONValue::Float64(f64::from(v)))),
        GoValue::Float64(v) => Ok(RealJsonValue::Supported(JSONValue::Float64(v))),
        GoValue::Decimal(d) => {
            let (float64_val, err) = d.to_float64();
            err?;
            Ok(RealJsonValue::Supported(JSONValue::Float64(float64_val)))
        }
        GoValue::Bytes(b) => {
            let str_val = types_to_string(&GoValue::Bytes(b))?;
            Ok(RealJsonValue::Supported(JSONValue::String(str_val)))
        }
        GoValue::String(s) => Ok(RealJsonValue::Supported(JSONValue::String(s))),
        GoValue::BinaryJSON(bj) => Ok(RealJsonValue::Supported(JSONValue::BinaryJSON(bj))),
        GoValue::Time(t) => Ok(RealJsonValue::Supported(JSONValue::Time(t))),
        GoValue::Duration(d) => Ok(RealJsonValue::Supported(JSONValue::Duration(d))),
        other => Ok(RealJsonValue::Unsupported(other)),
    }
}
