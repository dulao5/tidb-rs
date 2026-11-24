//! go-file: pkg/expression/aggregation/descriptor.go
//!
//! `AggFuncDesc`: the aggregate function descriptor the planner builds and the executors read
//! (D08.11 spec §2.1 part A). Plan nodes hold it by value (`Vec<AggFuncDesc>`, decision point
//! 4, spec §4.3): every field is an `Arc` or a small value, so a clone is cheap; where Go calls
//! `Clone()` the Rust caller calls [`AggFuncDesc::clone_desc`].
//!
//! `AggFuncDesc.Hash64` / `Equals` implement cascades' `base.HashEquals` and are not ported
//! (EXC.B, spec §2.4); `AggFuncDesc.GetAggFunc` builds the legacy evaluators, which only the
//! mock store uses (tracker block X, spec §2.3, decision point 1).

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors as goerrors;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{NotNullFlag, TypeBit, TypeLonglong, TypeString};
use tidb_types::datum::{Datum, GoValue, new_datum};
use tidb_types::field_type::new_field_type;

use super::aggregation::AggFunctionMode;
use super::base_func::{BaseFuncDesc, new_base_func_desc};
use super::window_func::WindowFuncDesc;
use crate::ast_funcs as ast;
use crate::byitem::ByItems;
use crate::column::Column;
use crate::context::{BuildContext, EvalContext, ParamValues};
use crate::expression::{Expr, ExprRef, evaluate_expr_with_null};
use crate::schema::Schema;
use crate::size;

// go-exception: EXC.B AggFuncDesc.Hash64 (cascades base.HashEquals; D08.11 spec §2.4)
// go-exception: EXC.B AggFuncDesc.Equals (cascades base.HashEquals; D08.11 spec §2.4)

/// AggFuncDesc describes an aggregation function signature, only used in planner.
#[derive(Clone, Debug)]
pub struct AggFuncDesc {
    /// The embedded `baseFuncDesc` (M-SELF): `desc.name`, `desc.args`, `desc.ret_tp` read
    /// through `Deref`.
    pub base: BaseFuncDesc,
    /// Mode represents the execution mode of the aggregation function.
    pub mode: AggFunctionMode,
    /// HasDistinct represents whether the aggregation function contains distinct attribute.
    pub has_distinct: bool,
    /// OrderByItems represents the order by clause used in GROUP_CONCAT
    pub order_by_items: Vec<ByItems>,
    /// GroupingID is used for distinguishing with not-set 0, starting from 1.
    pub grouping_id: isize,
}

impl Deref for AggFuncDesc {
    type Target = BaseFuncDesc;

    // glue: M-SELF (Go struct embedding of baseFuncDesc)
    fn deref(&self) -> &BaseFuncDesc {
        &self.base
    }
}

impl DerefMut for AggFuncDesc {
    // glue: M-SELF (Go struct embedding of baseFuncDesc)
    fn deref_mut(&mut self) -> &mut BaseFuncDesc {
        &mut self.base
    }
}

impl AggFuncDesc {
    /// `&AggFuncDesc{baseFuncDesc: b, HasDistinct: hasDistinct}`: the other fields are Go's
    /// zero values.
    // glue: Go struct literal of AggFuncDesc
    pub fn from_base(base: BaseFuncDesc, has_distinct: bool) -> AggFuncDesc {
        AggFuncDesc {
            base,
            mode: AggFunctionMode::CompleteMode,
            has_distinct,
            order_by_items: Vec::new(),
            grouping_id: 0,
        }
    }
}

/// NewAggFuncDesc creates an aggregation function signature descriptor.
/// this func cannot be called twice as the TypeInfer has changed the type of args in the first time.
// go: NewAggFuncDesc L1 [M-ERR]
pub fn new_agg_func_desc(
    ctx: &dyn BuildContext,
    name: &str,
    args: Vec<ExprRef>,
    has_distinct: bool,
) -> Result<AggFuncDesc, Error> {
    let (b, err) = new_base_func_desc(ctx, name, args);
    err?;
    Ok(AggFuncDesc::from_base(b, has_distinct))
}

/// NewAggFuncDescForWindowFunc creates an aggregation function from window functions, where
/// baseFuncDesc may be ready.
// go: NewAggFuncDescForWindowFunc L1 [M-ERR]
pub fn new_agg_func_desc_for_window_func(
    ctx: &dyn BuildContext,
    desc: &WindowFuncDesc,
    has_distinct: bool,
) -> Result<AggFuncDesc, Error> {
    if desc.ret_tp_is_nil() {
        // safety check
        return new_agg_func_desc(ctx, &desc.name, desc.args.clone(), has_distinct);
    }
    Ok(AggFuncDesc::from_base(
        BaseFuncDesc::from_parts(
            desc.name.clone(),
            desc.args.clone(),
            Some(Arc::clone(&desc.ret_tp)),
        ),
        has_distinct,
    ))
}

impl AggFuncDesc {
    /// StringWithCtx returns the string representation within given ctx.
    // go: AggFuncDesc.StringWithCtx L1 [M-BYTES]
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        let mut buffer = self.name.as_bytes().to_vec();
        buffer.push(b'(');
        if self.has_distinct {
            buffer.extend_from_slice(b"distinct ");
        }
        for (i, arg) in self.args.iter().enumerate() {
            buffer.extend(arg.string_with_ctx(ctx, redact));
            if i + 1 != self.args.len() {
                buffer.extend_from_slice(b", ");
            }
        }
        if !self.order_by_items.is_empty() {
            buffer.extend_from_slice(b" order by ");
        }
        for (i, arg) in self.order_by_items.iter().enumerate() {
            buffer.extend(arg.string_with_ctx(ctx, redact));
            if i + 1 != self.order_by_items.len() {
                buffer.extend_from_slice(b", ");
            }
        }
        buffer.push(b')');
        buffer
    }

    /// Equal checks whether two aggregation function signatures are equal.
    // go: AggFuncDesc.Equal L1
    pub fn equal(&self, ctx: &dyn EvalContext, other: &AggFuncDesc) -> bool {
        if self.has_distinct != other.has_distinct {
            return false;
        }
        if self.order_by_items.len() != other.order_by_items.len() {
            return false;
        }
        for i in 0..self.order_by_items.len() {
            if !self.order_by_items[i].equal(ctx, &other.order_by_items[i]) {
                return false;
            }
        }
        self.base.equal(ctx, &other.base)
    }

    /// Clone copies an aggregation function signature totally.
    ///
    /// The copy shares the arguments, the return type and the ORDER BY expressions (M-EXPR,
    /// M-FT; spec §7 item 3): later changes go through copy on write.
    // go: AggFuncDesc.Clone L1 [M-FT, M-EXPR, M-NAME]
    pub fn clone_desc(&self) -> AggFuncDesc {
        let mut clone = AggFuncDesc {
            base: self.base.clone_base(),
            mode: self.mode,
            has_distinct: self.has_distinct,
            order_by_items: Vec::with_capacity(self.order_by_items.len()),
            grouping_id: self.grouping_id,
        };
        for by_item in &self.order_by_items {
            clone.order_by_items.push(by_item.clone_item());
        }
        clone
    }

    /// Split splits `a` into two aggregate descriptors for partial phase and
    /// final phase individually.
    /// This function is only used when executing aggregate function parallelly.
    /// ordinal indicates the column ordinal of the intermediate result.
    // go: AggFuncDesc.Split L1 [M-FT, M-INT]
    pub fn split(&self, ordinal: &[isize]) -> (AggFuncDesc, AggFuncDesc) {
        let mut partial_agg_desc = self.clone_desc();
        if self.mode == AggFunctionMode::CompleteMode {
            partial_agg_desc.mode = AggFunctionMode::Partial1Mode;
        } else if self.mode == AggFunctionMode::FinalMode {
            partial_agg_desc.mode = AggFunctionMode::Partial2Mode;
        }
        let mut final_agg_desc = AggFuncDesc {
            base: BaseFuncDesc::from_parts(
                self.name.clone(),
                Vec::new(),
                // finalAggDesc.RetTp = a.RetTp (the same pointer, spec §5: V0)
                (!self.ret_tp_is_nil()).then(|| Arc::clone(&self.ret_tp)),
            ),
            mode: AggFunctionMode::FinalMode, // We only support FinalMode now in final phase.
            has_distinct: self.has_distinct,
            order_by_items: Vec::new(),
            grouping_id: 0,
        };
        let column = |index: isize, ret_type: Arc<_>| -> ExprRef {
            Expr::Column(Column {
                index,
                ret_type,
                ..Column::default()
            })
            .into_ref()
        };
        match self.name.as_str() {
            ast::AggFuncAvg => {
                let args = vec![
                    column(ordinal[0], Arc::new(new_field_type(TypeLonglong))),
                    column(ordinal[1], Arc::clone(&self.ret_tp)),
                ];
                final_agg_desc.args = args;
            }
            ast::AggFuncApproxCountDistinct => {
                let args = vec![column(ordinal[0], Arc::new(new_field_type(TypeString)))];
                final_agg_desc.args = args;
            }
            _ => {
                let args = vec![column(ordinal[0], Arc::clone(&self.ret_tp))];
                final_agg_desc.args = args;
                if final_agg_desc.name == ast::AggFuncGroupConcat
                    || final_agg_desc.name == ast::AggFuncApproxPercentile
                {
                    let separator = Arc::clone(&self.args[self.args.len() - 1]);
                    final_agg_desc.args.push(separator); // separator
                }
            }
        }
        (partial_agg_desc, final_agg_desc)
    }

    /// EvalNullValueInOuterJoin gets the null value when the aggregation is upon an outer join,
    /// and the aggregation function's input is null.
    /// If there is no matching row for the inner table of an outer join,
    /// an aggregation function only involves constant and/or columns belongs to the inner table
    /// will be set to the null value.
    /// The input stands for the schema of Aggregation's child. If the function can't produce a
    /// null value, the second return value will be false.
    /// e.g.
    /// Table t with only one row:
    /// ```text
    /// +-------+---------+---------+
    /// | Table | Field   | Type    |
    /// +-------+---------+---------+
    /// | t     | a       | int(11) |
    /// +-------+---------+---------+
    /// +------+
    /// | a    |
    /// +------+
    /// |    1 |
    /// +------+
    /// ```
    ///
    /// Table s which is empty:
    /// ```text
    /// +-------+---------+---------+
    /// | Table | Field   | Type    |
    /// +-------+---------+---------+
    /// | s     | a       | int(11) |
    /// +-------+---------+---------+
    /// ```
    ///
    /// Query: `select t.a as `t.a`,  count(95), sum(95), avg(95), bit_or(95), bit_and(95), bit_or(95), max(95), min(95), s.a as `s.a`, avg(95) from t left join s on t.a = s.a;`
    /// ```text
    /// +------+-----------+---------+---------+------------+-------------+------------+---------+---------+------+----------+
    /// | t.a  | count(95) | sum(95) | avg(95) | bit_or(95) | bit_and(95) | bit_or(95) | max(95) | min(95) | s.a  | avg(s.a) |
    /// +------+-----------+---------+---------+------------+-------------+------------+---------+---------+------+----------+
    /// |    1 |         1 |      95 | 95.0000 |         95 |          95 |         95 |      95 |      95 | NULL |     NULL |
    /// +------+-----------+---------+---------+------------+-------------+------------+---------+---------+------+----------+
    /// ```
    ///
    /// Go panics on a function it does not handle; the callers filter the names first
    /// (spec §4.6), so the panic is kept (M-PANIC).
    // go: AggFuncDesc.EvalNullValueInOuterJoin L1 [M-ERR, M-PANIC]
    pub fn eval_null_value_in_outer_join(
        &self,
        ctx: &dyn BuildContext,
        schema: &Schema,
    ) -> Result<(Datum, bool), Error> {
        match self.name.as_str() {
            ast::AggFuncCount => self.eval_null_value_in_outer_join4_count(ctx, schema),
            ast::AggFuncSum | ast::AggFuncMax | ast::AggFuncMin | ast::AggFuncFirstRow => {
                self.eval_null_value_in_outer_join4_sum(ctx, schema)
            }
            ast::AggFuncAvg | ast::AggFuncGroupConcat => Ok((Datum::default(), false)),
            ast::AggFuncBitAnd => self.eval_null_value_in_outer_join4_bit_and(ctx, schema),
            ast::AggFuncBitOr | ast::AggFuncBitXor => {
                self.eval_null_value_in_outer_join4_bit_or(ctx, schema)
            }
            _ => panic!("unsupported agg function"),
        }
    }

    // go: AggFuncDesc.evalNullValueInOuterJoin4Count L1 [M-IFACE, M-ERR]
    fn eval_null_value_in_outer_join4_count(
        &self,
        ctx: &dyn BuildContext,
        schema: &Schema,
    ) -> Result<(Datum, bool), Error> {
        for arg in &self.args {
            let result = evaluate_expr_with_null(ctx, schema, arg)?;
            match &*result {
                Expr::Constant(con) if !con.value.is_null() => {}
                Expr::Constant(_) => return Ok((Datum::default(), true)),
                _ => return Ok((Datum::default(), false)),
            }
        }
        Ok((new_datum(GoValue::Int(1)), true))
    }

    // go: AggFuncDesc.evalNullValueInOuterJoin4Sum L1 [M-IFACE, M-ERR]
    fn eval_null_value_in_outer_join4_sum(
        &self,
        ctx: &dyn BuildContext,
        schema: &Schema,
    ) -> Result<(Datum, bool), Error> {
        let result = evaluate_expr_with_null(ctx, schema, &self.args[0])?;
        match &*result {
            Expr::Constant(con) if !con.value.is_null() => Ok((con.value.clone(), true)),
            Expr::Constant(_) => Ok((Datum::default(), true)),
            _ => Ok((Datum::default(), false)),
        }
    }

    // go: AggFuncDesc.evalNullValueInOuterJoin4BitAnd L1 [M-IFACE, M-ERR]
    fn eval_null_value_in_outer_join4_bit_and(
        &self,
        ctx: &dyn BuildContext,
        schema: &Schema,
    ) -> Result<(Datum, bool), Error> {
        let result = evaluate_expr_with_null(ctx, schema, &self.args[0])?;
        match &*result {
            Expr::Constant(con) if !con.value.is_null() => Ok((con.value.clone(), true)),
            _ => Ok((new_datum(GoValue::Uint64(u64::MAX)), true)),
        }
    }

    // go: AggFuncDesc.evalNullValueInOuterJoin4BitOr L1 [M-IFACE, M-ERR]
    fn eval_null_value_in_outer_join4_bit_or(
        &self,
        ctx: &dyn BuildContext,
        schema: &Schema,
    ) -> Result<(Datum, bool), Error> {
        let result = evaluate_expr_with_null(ctx, schema, &self.args[0])?;
        match &*result {
            Expr::Constant(con) if !con.value.is_null() => Ok((con.value.clone(), true)),
            _ => Ok((new_datum(GoValue::Int(0)), true)),
        }
    }

    /// UpdateNotNullFlag4RetType checks if we should remove the NotNull flag for the return
    /// type of the agg.
    // go: AggFuncDesc.UpdateNotNullFlag4RetType L1 [M-FT, M-ERR]
    pub fn update_not_null_flag4_ret_type(
        &mut self,
        has_group_by: bool,
        all_aggs_first_row: bool,
    ) -> Result<(), Error> {
        let mut remove_not_null = false;
        match self.name.as_str() {
            ast::AggFuncCount
            | ast::AggFuncApproxCountDistinct
            | ast::AggFuncApproxPercentile
            | ast::AggFuncBitAnd
            | ast::AggFuncBitOr
            | ast::AggFuncBitXor
            | ast::WindowFuncFirstValue
            | ast::WindowFuncLastValue
            | ast::WindowFuncNthValue
            | ast::WindowFuncRowNumber
            | ast::WindowFuncRank
            | ast::WindowFuncDenseRank
            | ast::WindowFuncCumeDist
            | ast::WindowFuncNtile
            | ast::WindowFuncPercentRank
            | ast::WindowFuncLead
            | ast::WindowFuncLag
            | ast::AggFuncJsonObjectAgg
            | ast::AggFuncJsonArrayagg
            | ast::AggFuncVarSamp
            | ast::AggFuncVarPop
            | ast::AggFuncStddevPop
            | ast::AggFuncStddevSamp => {
                remove_not_null = false;
            }
            ast::AggFuncSum | ast::AggFuncAvg | ast::AggFuncGroupConcat => {
                if !has_group_by {
                    remove_not_null = true;
                }
            }
            // `select max(a) from empty_tbl` returns `null`, while `select max(a) from empty_tbl group by b` returns empty.
            ast::AggFuncMax | ast::AggFuncMin => {
                if !has_group_by && self.ret_tp.get_type() != TypeBit {
                    remove_not_null = true;
                }
            }
            // `select distinct a from empty_tbl` returns empty
            // `select a from empty_tbl group by b` returns empty
            // `select a, max(a) from empty_tbl` returns `(null, null)`
            // `select a, max(a) from empty_tbl group by b` returns empty
            // `select a, count(a) from empty_tbl` returns `(null, 0)`
            // `select a, count(a) from empty_tbl group by b` returns empty
            ast::AggFuncFirstRow => {
                if !all_aggs_first_row && !has_group_by {
                    remove_not_null = true;
                }
            }
            _ => {
                return Err(goerrors::errorf(
                    "unsupported agg function: %s",
                    &[Arg::from(&self.name)],
                ));
            }
        }
        if remove_not_null {
            // a.RetTp = a.RetTp.Clone(); a.RetTp.DelFlag(NotNullFlag): copy on write.
            Arc::make_mut(&mut self.base.ret_tp).del_flag(NotNullFlag);
        }
        Ok(())
    }

    /// MemoryUsage the memory usage of AggFuncDesc
    // go: AggFuncDesc.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = self.base.memory_usage() + size::SizeOfInt + size::SizeOfBool;
        for item in &self.order_by_items {
            sum += item.memory_usage();
        }
        sum
    }
}
