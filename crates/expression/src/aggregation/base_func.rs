//! go-file: pkg/expression/aggregation/base_func.go
//!
//! The base of the aggregate and window function descriptors: name, arguments and return
//! type, the return type inference per function name, the casts wrapped around the arguments
//! and the default value on empty input (D08.11 spec §2.1 part A, §4.1, §4.2).
//!
//! Carriers (spec §4.1): `Args` are shared expressions (M-EXPR); `RetTp` is an
//! `Arc<FieldType>` changed through `Arc::make_mut` (M-FT, N13 §8). Go's `RetTp` can be nil;
//! the Rust field always holds a type, and [`BaseFuncDesc::ret_tp_is_nil`] says when Go's
//! would be nil (before the type inference, and after `typeInfer4LeadLag` drops the error of
//! `InferType4ControlFuncs`, spec §8 item 4, decision point 8).
//!
//! `baseFuncDesc.Hash64` / `Equals` implement cascades' `base.HashEquals` and are not
//! ported (EXC.B, spec §2.4).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::errors::{self as goerrors, RedactLogDisable};
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{
    self, MaxBlobWidth, MaxDecimalScale, MaxDecimalWidth, MaxFieldCharLength, MaxRealWidth,
    NotFixedDec, NotNullFlag, TypeDate, TypeDatetime, TypeDouble, TypeDuration, TypeEnum,
    TypeFloat, TypeInt24, TypeJSON, TypeLong, TypeLonglong, TypeNewDate, TypeNewDecimal, TypeNull,
    TypeSet, TypeShort, TypeString, TypeTimestamp, TypeTiny, TypeVarString, TypeYear, UnsignedFlag,
};
use tidb_chunk::Row;
use tidb_parser::gocharset::get_default_charset_and_collate;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, UnspecifiedLength,
};
use tidb_types::datum::{Datum, new_int_datum, new_uint_datum};
use tidb_types::etc::FieldType;
use tidb_types::field_type::{new_field_type, set_bin_chs_cln_flag};
use tidb_types::field_type_builder::new_field_type_builder;

use crate::ast_funcs as ast;
use crate::builtin_cast::{
    build_cast_function, wrap_with_cast_as_decimal, wrap_with_cast_as_duration,
    wrap_with_cast_as_int, wrap_with_cast_as_json, wrap_with_cast_as_real,
    wrap_with_cast_as_string, wrap_with_cast_as_time, wrap_with_cast_as_vector_float32,
};
use crate::builtin_control::infer_type4_control_funcs;
use crate::context::{BuildContext, EvalContext, ParamValues};
use crate::expression::{ConstNone, EvalState, Expr, ExprRef};
use crate::size;

// go-exception: EXC.B baseFuncDesc.Hash64 (cascades base.HashEquals; D08.11 spec §2.4)
// go-exception: EXC.B baseFuncDesc.Equals (cascades base.HashEquals; D08.11 spec §2.4)

/// baseFuncDesc describes an function signature, only used in planner.
#[derive(Clone, Debug)]
pub struct BaseFuncDesc {
    /// Name represents the function name.
    pub name: String,
    /// Args represents the arguments of the function.
    pub args: Vec<ExprRef>,
    /// RetTp represents the return type of the function.
    pub ret_tp: Arc<FieldType>,
    /// Go's `RetTp == nil` (spec §4.1): set until the type inference assigns a type, and
    /// when `typeInfer4LeadLag` keeps the nil type of a failed `InferType4ControlFuncs`.
    ret_tp_nil: bool,
}

impl BaseFuncDesc {
    /// A descriptor with the given fields (Go's struct literal
    /// `baseFuncDesc{Name: name, Args: args, RetTp: retTp}`; `None` is a nil `RetTp`).
    // glue: Go struct literal of baseFuncDesc (RetTp nil carried by the marker, spec §4.1)
    pub fn from_parts(name: String, args: Vec<ExprRef>, ret_tp: Option<Arc<FieldType>>) -> Self {
        match ret_tp {
            Some(ret_tp) => BaseFuncDesc {
                name,
                args,
                ret_tp,
                ret_tp_nil: false,
            },
            None => BaseFuncDesc {
                name,
                args,
                ret_tp: Arc::new(FieldType::default()),
                ret_tp_nil: true,
            },
        }
    }

    /// Whether Go's `RetTp` is nil here (spec §4.1, §8 item 4).
    // glue: Go's nil *FieldType (spec §4.1)
    pub fn ret_tp_is_nil(&self) -> bool {
        self.ret_tp_nil
    }

    /// `a.RetTp = tp`.
    // glue: assignment of a non-nil RetTp (clears the nil marker)
    fn set_ret_tp(&mut self, tp: Arc<FieldType>) {
        self.ret_tp = tp;
        self.ret_tp_nil = false;
    }

    /// The return type for an in-place change (`a.RetTp.SetXxx`), copied on write (M-FT).
    // glue: Arc::make_mut of the shared RetTp (M-FT, N13 §8)
    fn ret_tp_mut(&mut self) -> &mut FieldType {
        Arc::make_mut(&mut self.ret_tp)
    }
}

/// `a.Args[i].GetType(ctx)` as a shared pointer: Go's `a.RetTp = a.Args[0].GetType(ctx)`
/// makes the descriptor share the argument's `*FieldType` (spec §5). A column and a scalar
/// function share their `Arc`; a constant's type is copied (it is written only by copy on
/// write anyway, so the result is the same).
// glue: the argument's *FieldType as an Arc (M-FT)
fn arg_type_arc(ctx: &dyn EvalContext, e: &Expr) -> Arc<FieldType> {
    match e {
        Expr::Column(c) => Arc::clone(&c.ret_type),
        Expr::CorrelatedColumn(c) => Arc::clone(&c.column.ret_type),
        Expr::ScalarFunction(f) => Arc::clone(&f.function.tp),
        Expr::Constant(_) => Arc::new(e.get_type(ctx).into_owned()),
    }
}

/// The shared `retTp` of WrapCastForAggArgs's time branch after WrapWithCastAsTime wrapped
/// `arg` into `res` (Go aliasing, M-FT, base_func.go:456): the CAST's type is that `tp`
/// itself. A CAST folded to a constant has a copy of it (constant_fold.go:234), with NOT NULL
/// set from the value; `tp` keeps NOT NULL only when it had it and the argument is not
/// nullable (BuildCastFunctionWithCheck's `tp.DelFlag(NotNullFlag)`).
// glue: Go aliasing of the *FieldType WrapWithCastAsTime changes (M-FT)
fn shared_time_tp_after(
    ctx: &dyn EvalContext,
    prev: &FieldType,
    arg: &Expr,
    res: &Expr,
) -> FieldType {
    match res {
        Expr::Constant(c) => {
            let mut tp = c.ret_type.clone();
            let arg_not_null = mysql::has_not_null_flag(arg.get_type(ctx).get_flag());
            if mysql::has_not_null_flag(prev.get_flag()) && arg_not_null {
                tp.add_flag(NotNullFlag);
            } else {
                tp.del_flag(NotNullFlag);
            }
            tp
        }
        _ => res.get_type(ctx).into_owned(),
    }
}

// go: newBaseFuncDesc L2 (Go returns the descriptor and the error together; the Rust tuple keeps both, as NewWindowFuncDesc reads RetTp before the error)
pub(crate) fn new_base_func_desc(
    ctx: &dyn BuildContext,
    name: &str,
    args: Vec<ExprRef>,
) -> (BaseFuncDesc, Result<(), Error>) {
    let mut b = BaseFuncDesc::from_parts(name.to_lowercase(), args, None);
    let err = b.type_infer(ctx);
    (b, err)
}

impl BaseFuncDesc {
    // go: baseFuncDesc.equal L1
    pub(crate) fn equal(&self, ctx: &dyn EvalContext, other: &BaseFuncDesc) -> bool {
        if self.name != other.name || self.args.len() != other.args.len() {
            return false;
        }
        for i in 0..self.args.len() {
            if !self.args[i].equal(ctx, &other.args[i]) {
                return false;
            }
        }
        true
    }

    /// Go copies the `FieldType` value and clones every argument; the Rust copy shares both
    /// (M-FT, M-EXPR): later changes go through copy on write, so the two descriptors stay
    /// independent (spec §7 item 3).
    // go: baseFuncDesc.clone L1 [M-FT, M-EXPR, M-NAME]
    pub(crate) fn clone_base(&self) -> BaseFuncDesc {
        BaseFuncDesc {
            name: self.name.clone(),
            args: self.args.iter().map(Arc::clone).collect(),
            ret_tp: Arc::clone(&self.ret_tp),
            ret_tp_nil: self.ret_tp_nil,
        }
    }

    /// StringWithCtx returns the string within given context.
    // go: baseFuncDesc.StringWithCtx L1 [M-BYTES]
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        let mut buffer = self.name.as_bytes().to_vec();
        buffer.push(b'(');
        for (i, arg) in self.args.iter().enumerate() {
            buffer.extend(arg.string_with_ctx(ctx, redact));
            if i + 1 != self.args.len() {
                buffer.extend_from_slice(b", ");
            }
        }
        buffer.push(b')');
        buffer
    }

    /// TypeInfer infers the arguments and return types of an function.
    ///
    /// Returns the error of building a CAST around an argument too (D08.8's `BuildCastFunction`
    /// returns it in Rust; spec §4.2, §7 item 1).
    // go: baseFuncDesc.TypeInfer L1 [M-ERR]
    pub fn type_infer(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        match self.name.as_str() {
            ast::AggFuncCount => self.type_infer4_count(),
            ast::AggFuncApproxCountDistinct => self.type_infer4_approx_count_distinct(),
            ast::AggFuncApproxPercentile => {
                return self.type_infer4_approx_percentile(&*ctx.get_eval_ctx());
            }
            ast::AggFuncSum => self.type_infer4_sum(&*ctx.get_eval_ctx()),
            ast::AggFuncAvg => self.type_infer4_avg(&*ctx.get_eval_ctx()),
            ast::AggFuncGroupConcat => self.type_infer4_group_concat(ctx)?,
            ast::AggFuncMax
            | ast::AggFuncMin
            | ast::AggFuncFirstRow
            | ast::WindowFuncFirstValue
            | ast::WindowFuncLastValue
            | ast::WindowFuncNthValue => self.type_infer4_max_min(ctx)?,
            ast::AggFuncBitAnd | ast::AggFuncBitOr | ast::AggFuncBitXor => {
                self.type_infer4_bit_funcs(ctx)?
            }
            ast::WindowFuncRowNumber | ast::WindowFuncRank | ast::WindowFuncDenseRank => {
                self.type_infer4_number_funcs()
            }
            ast::WindowFuncCumeDist => self.type_infer4_cume_dist(),
            ast::WindowFuncNtile => self.type_infer4_ntile(),
            ast::WindowFuncPercentRank => self.type_infer4_percent_rank(),
            ast::WindowFuncLead | ast::WindowFuncLag => self.type_infer4_lead_lag(ctx)?,
            ast::AggFuncVarPop
            | ast::AggFuncStddevPop
            | ast::AggFuncVarSamp
            | ast::AggFuncStddevSamp => self.type_infer4_pop_or_samp(),
            ast::AggFuncJsonArrayagg => self.type_infer4_json_array_agg(),
            ast::AggFuncJsonObjectAgg => return self.type_infer4_json_object_agg(ctx),
            _ => {
                return Err(goerrors::errorf(
                    "unsupported agg function: %s",
                    &[Arg::from(&self.name)],
                ));
            }
        }
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4Count L1
    fn type_infer4_count(&mut self) {
        let mut tp = new_field_type(TypeLonglong);
        tp.set_flen(21);
        tp.set_decimal(0);
        // count never returns null
        tp.add_flag(NotNullFlag);
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
    }

    // go: baseFuncDesc.typeInfer4ApproxCountDistinct L1
    fn type_infer4_approx_count_distinct(&mut self) {
        self.type_infer4_count();
    }

    // go: baseFuncDesc.typeInfer4ApproxPercentile L1 [M-ERR]
    fn type_infer4_approx_percentile(&mut self, ctx: &dyn EvalContext) -> Result<(), Error> {
        if self.args.len() != 2 {
            return Err(goerrors::new("APPROX_PERCENTILE should take 2 arguments"));
        }

        if self.args[1].const_level() == ConstNone {
            return Err(goerrors::new(
                "APPROX_PERCENTILE should take a constant expression as percentage argument",
            ));
        }
        let st = EvalState::default();
        let (percent, is_null) = match self.args[1].eval_int(ctx, &st, Row::default()) {
            Ok(Some(v)) => (v, false),
            Ok(None) => (0, true),
            Err(_) => {
                // fmt.Errorf: a plain error.
                let mut msg = b"APPROX_PERCENTILE: Invalid argument ".to_vec();
                msg.extend(self.args[1].string_with_ctx(ctx, RedactLogDisable));
                return Err(Error::plain(msg));
            }
        };
        if percent <= 0 || percent > 100 || is_null {
            if is_null {
                return Err(goerrors::new(
                    "APPROX_PERCENTILE: Percentage value cannot be NULL",
                ));
            }
            return Err(Error::plain(format!(
                "Percentage value {percent} is out of range [1, 100]"
            )));
        }

        let arg_tp = self.args[0].get_type(ctx).into_owned();
        match arg_tp.get_type() {
            TypeTiny | TypeShort | TypeInt24 | TypeLong | TypeLonglong => {
                self.set_ret_tp(Arc::new(new_field_type(TypeLonglong)));
            }
            TypeDouble | TypeFloat => {
                self.set_ret_tp(Arc::new(new_field_type(TypeDouble)));
            }
            TypeNewDecimal => {
                let mut tp = new_field_type(TypeNewDecimal);
                tp.set_flen(MaxDecimalWidth);
                tp.set_decimal(arg_tp.get_decimal());
                if tp.get_decimal() < 0 || tp.get_decimal() > MaxDecimalScale {
                    tp.set_decimal(MaxDecimalScale);
                }
                self.set_ret_tp(Arc::new(tp));
            }
            TypeDate | TypeDatetime | TypeNewDate | TypeTimestamp => {
                self.set_ret_tp(Arc::new(arg_tp));
            }
            _ => {
                let mut tp = arg_tp;
                tp.del_flag(NotNullFlag);
                self.set_ret_tp(Arc::new(tp));
            }
        }
        Ok(())
    }

    /// typeInfer4Sum should return a "decimal", otherwise it returns a "double".
    /// Because child returns integer or decimal type.
    // go: baseFuncDesc.typeInfer4Sum L1
    fn type_infer4_sum(&mut self, ctx: &dyn EvalContext) {
        let arg_tp = self.args[0].get_type(ctx).into_owned();
        let mut tp;
        match arg_tp.get_type() {
            TypeTiny | TypeShort | TypeInt24 | TypeLong | TypeLonglong | TypeYear => {
                tp = new_field_type(TypeNewDecimal);
                tp.set_flen_under_limit(arg_tp.get_flen() + 21);
                tp.set_decimal(0);
                if arg_tp.get_flen() < 0 {
                    tp.set_flen(MaxDecimalWidth);
                }
            }
            TypeNewDecimal => {
                tp = new_field_type(TypeNewDecimal);
                tp.update_flen_and_decimal_under_limit(&arg_tp, 0, 22);
            }
            TypeDouble | TypeFloat => {
                tp = new_field_type(TypeDouble);
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(arg_tp.get_decimal());
            }
            _ => {
                tp = new_field_type(TypeDouble);
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(UnspecifiedLength);
            }
        }
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
    }

    /// TypeInfer4AvgSum infers the type of sum from avg, which should extend the precision of
    /// decimal compatible with mysql.
    // go: baseFuncDesc.TypeInfer4AvgSum L1 [M-FT]
    pub fn type_infer4_avg_sum(&mut self, avg_ret_type: &FieldType) {
        if avg_ret_type.get_type() == TypeNewDecimal {
            let flen = MaxDecimalWidth.min(self.ret_tp.get_flen() + 22);
            self.ret_tp_mut().set_flen(flen);
        }
    }

    /// TypeInfer4FinalCount infers the type of sum agg which is rewritten from final count
    /// agg run on MPP mode.
    // go: baseFuncDesc.TypeInfer4FinalCount L1
    pub fn type_infer4_final_count(&mut self, final_count_ret_type: &FieldType) {
        self.set_ret_tp(Arc::new(final_count_ret_type.clone()));
    }

    /// typeInfer4Avg should returns a "decimal", otherwise it returns a "double".
    /// Because child returns integer or decimal type.
    // go: baseFuncDesc.typeInfer4Avg L1 [M-INT]
    fn type_infer4_avg(&mut self, ctx: &dyn EvalContext) {
        let div_prec_incre = ctx.get_div_precision_increment() as isize;
        let arg_tp = self.args[0].get_type(ctx).into_owned();
        let mut tp;
        match arg_tp.get_type() {
            TypeTiny | TypeShort | TypeInt24 | TypeLong | TypeLonglong => {
                tp = new_field_type(TypeNewDecimal);
                tp.set_decimal_under_limit(div_prec_incre);
                let (flen, _) = mysql::get_default_field_length_and_decimal(arg_tp.get_type());
                tp.set_flen_under_limit(flen + div_prec_incre);
            }
            TypeYear | TypeNewDecimal => {
                tp = new_field_type(TypeNewDecimal);
                tp.update_flen_and_decimal_under_limit(&arg_tp, div_prec_incre, div_prec_incre);
            }
            TypeDouble | TypeFloat => {
                tp = new_field_type(TypeDouble);
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(arg_tp.get_decimal());
            }
            TypeDate | TypeDuration | TypeDatetime | TypeTimestamp => {
                tp = new_field_type(TypeDouble);
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(4);
            }
            _ => {
                tp = new_field_type(TypeDouble);
                tp.set_flen(MaxRealWidth);
                tp.set_decimal(UnspecifiedLength);
            }
        }
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
    }

    // go: baseFuncDesc.typeInfer4GroupConcat L1 [M-ERR]
    fn type_infer4_group_concat(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        let mut tp = new_field_type(TypeVarString);
        let (charset, collate) = get_default_charset_and_collate();
        tp.set_charset(charset);
        tp.set_collate(collate);

        tp.set_flen(MaxBlobWidth);
        tp.set_decimal(0);
        self.set_ret_tp(Arc::new(tp));
        // TODO: a.Args[i] = expression.WrapWithCastAsString(ctx, a.Args[i])
        let mut i = 0;
        while i + 1 < self.args.len() {
            let arg_tp = self.args[i].get_type(&*ctx.get_eval_ctx()).into_owned();
            if arg_tp.get_type() == TypeNewDecimal {
                self.args[i] = build_cast_function(ctx, Arc::clone(&self.args[i]), arg_tp)?;
            }
            i += 1;
        }
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4MaxMin L1 [M-FT, M-IFACE, M-ERR]
    fn type_infer4_max_min(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        let arg_is_scala_func = matches!(&*self.args[0], Expr::ScalarFunction(_));
        if arg_is_scala_func && self.args[0].get_type(&*ctx.get_eval_ctx()).get_type() == TypeFloat
        {
            // For scalar function, the result of "float32" is set to the "float64"
            // field in the "Datum". If we do not wrap a cast-as-double function on a.Args[0],
            // error would happen when extracting the evaluation of a.Args[0] to a ProjectionExec.
            let mut tp = new_field_type(TypeDouble);
            tp.set_flen(MaxRealWidth);
            tp.set_decimal(UnspecifiedLength);
            set_bin_chs_cln_flag(&mut tp);
            self.args[0] = build_cast_function(ctx, Arc::clone(&self.args[0]), tp)?;
        }
        let ectx = ctx.get_eval_ctx();
        self.set_ret_tp(arg_type_arc(&*ectx, &self.args[0]));
        if self.name == ast::AggFuncMax
            || self.name == ast::AggFuncMin
            || self.name == ast::WindowFuncLead
            || self.name == ast::WindowFuncLag
        {
            let mut tp = self.args[0].get_type(&*ectx).into_owned();
            tp.del_flag(NotNullFlag);
            self.set_ret_tp(Arc::new(tp));
        }
        // issue #13027, #13961
        if (self.ret_tp.get_type() == TypeEnum || self.ret_tp.get_type() == TypeSet)
            && (self.name != ast::AggFuncFirstRow
                && self.name != ast::AggFuncMax
                && self.name != ast::AggFuncMin)
        {
            self.set_ret_tp(Arc::new(
                new_field_type_builder()
                    .set_type(TypeString)
                    .set_flen(MaxFieldCharLength)
                    .build_p(),
            ));
        }
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4BitFuncs L1 [M-ERR]
    fn type_infer4_bit_funcs(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        let mut tp = new_field_type(TypeLonglong);
        tp.set_flen(21);
        set_bin_chs_cln_flag(&mut tp);
        tp.add_flag(UnsignedFlag | NotNullFlag);
        self.set_ret_tp(Arc::new(tp));
        self.args[0] = wrap_with_cast_as_int(ctx, Arc::clone(&self.args[0]))?;
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4JsonArrayAgg L1
    fn type_infer4_json_array_agg(&mut self) {
        let mut tp = new_field_type(TypeJSON);
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
    }

    // go: baseFuncDesc.typeInfer4JsonObjectAgg L1 [M-ERR]
    fn type_infer4_json_object_agg(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        let mut tp = new_field_type(TypeJSON);
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
        self.args[0] = wrap_with_cast_as_string(ctx, Arc::clone(&self.args[0]))?;
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4NumberFuncs L1
    fn type_infer4_number_funcs(&mut self) {
        let mut tp = new_field_type(TypeLonglong);
        tp.set_flen(21);
        set_bin_chs_cln_flag(&mut tp);
        self.set_ret_tp(Arc::new(tp));
    }

    // go: baseFuncDesc.typeInfer4CumeDist L1
    fn type_infer4_cume_dist(&mut self) {
        let mut tp = new_field_type(TypeDouble);
        tp.set_flen(MaxRealWidth);
        tp.set_decimal(NotFixedDec);
        self.set_ret_tp(Arc::new(tp));
    }

    // go: baseFuncDesc.typeInfer4Ntile L1
    fn type_infer4_ntile(&mut self) {
        let mut tp = new_field_type(TypeLonglong);
        tp.set_flen(21);
        set_bin_chs_cln_flag(&mut tp);
        tp.add_flag(UnsignedFlag);
        self.set_ret_tp(Arc::new(tp));
    }

    /// Go sets the flag, not the length, to `MaxRealWidth` (spec §8 item 1); ported as is.
    // go: baseFuncDesc.typeInfer4PercentRank L1
    fn type_infer4_percent_rank(&mut self) {
        let mut tp = new_field_type(TypeDouble);
        tp.set_flag(MaxRealWidth as usize);
        tp.set_decimal(NotFixedDec);
        self.set_ret_tp(Arc::new(tp));
    }

    /// With three arguments Go drops the error of `InferType4ControlFuncs` and keeps its nil
    /// type (spec §8 item 4): the nil marker is set (decision point 8).
    // go: baseFuncDesc.typeInfer4LeadLag L1 [M-ERR]
    fn type_infer4_lead_lag(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        if self.args.len() < 3 {
            self.type_infer4_max_min(ctx)?;
        } else {
            // Merge the type of first and third argument.
            // FIXME: select lead(b collate utf8mb4_unicode_ci, 1, 'lead' collate utf8mb4_general_ci) over() as a from t; should report error.
            let args = [Arc::clone(&self.args[0]), Arc::clone(&self.args[2])];
            match infer_type4_control_funcs(ctx, &self.name, &args) {
                Ok(tp) => self.set_ret_tp(Arc::new(tp)),
                Err(_) => {
                    self.ret_tp = Arc::new(FieldType::default());
                    self.ret_tp_nil = true;
                }
            }
        }
        Ok(())
    }

    // go: baseFuncDesc.typeInfer4PopOrSamp L1
    fn type_infer4_pop_or_samp(&mut self) {
        // var_pop/std/var_samp/stddev_samp's return value type is double
        let mut tp = new_field_type(TypeDouble);
        tp.set_flen(MaxRealWidth);
        tp.set_decimal(UnspecifiedLength);
        self.set_ret_tp(Arc::new(tp));
    }

    /// GetDefaultValue gets the default value when the function's input is null.
    /// According to MySQL, default values of the function are listed as follows:
    /// e.g.
    /// Table t which is empty:
    /// ```text
    /// +-------+---------+---------+
    /// | Table | Field   | Type    |
    /// +-------+---------+---------+
    /// | t     | a       | int(11) |
    /// +-------+---------+---------+
    /// ```
    ///
    /// Query: `select avg(a), sum(a), count(a), bit_xor(a), bit_or(a), bit_and(a), max(a), min(a), group_concat(a), approx_count_distinct(a), approx_percentile(a, 50) from test.t;`
    /// ```text
    /// +--------+--------+----------+------------+-----------+----------------------+--------+--------+-----------------+--------------------------+--------------------------+
    /// | avg(a) | sum(a) | count(a) | bit_xor(a) | bit_or(a) | bit_and(a)           | max(a) | min(a) | group_concat(a) | approx_count_distinct(a) | approx_percentile(a, 50) |
    /// +--------+--------+----------+------------+-----------+----------------------+--------+--------+-----------------+--------------------------+--------------------------+
    /// |   NULL |   NULL |        0 |          0 |         0 | 18446744073709551615 |   NULL |   NULL | NULL            |                        0 |                     NULL |
    /// +--------+--------+----------+------------+-----------+----------------------+--------+--------+-----------------+--------------------------+--------------------------+
    /// ```
    // go: baseFuncDesc.GetDefaultValue L1
    pub fn get_default_value(&self) -> Datum {
        let mut v = Datum::default();
        match self.name.as_str() {
            ast::AggFuncCount | ast::AggFuncBitOr | ast::AggFuncBitXor => {
                v = new_int_datum(0);
            }
            ast::AggFuncApproxCountDistinct => {
                if self.ret_tp.get_type() != TypeString {
                    v = new_int_datum(0);
                }
            }
            ast::AggFuncFirstRow
            | ast::AggFuncAvg
            | ast::AggFuncSum
            | ast::AggFuncMax
            | ast::AggFuncMin
            | ast::AggFuncGroupConcat
            | ast::AggFuncApproxPercentile => {
                v = Datum::default();
            }
            ast::AggFuncBitAnd => {
                v = new_uint_datum(u64::MAX);
            }
            _ => {}
        }
        v
    }
}

/// We do not need to wrap cast upon these functions,
/// since the EvalXXX method called by the arg is determined by the corresponding arg type.
/// (Go's package-level map; only looked up, M-MAPORDER.)
const noNeedCastAggFuncs: &[&str] = &[
    ast::AggFuncCount,
    ast::AggFuncApproxCountDistinct,
    ast::AggFuncApproxPercentile,
    ast::AggFuncMax,
    ast::AggFuncMin,
    ast::AggFuncFirstRow,
    ast::WindowFuncNtile,
    ast::AggFuncJsonArrayagg,
    ast::AggFuncJsonObjectAgg,
];

impl BaseFuncDesc {
    /// WrapCastForAggArgs wraps the args of an aggregate function with a cast function.
    ///
    /// Returns the error of building a CAST (spec §4.2, §7 item 1).
    // go: baseFuncDesc.WrapCastForAggArgs L1 [M-ERR, M-PANIC, M-FT]
    pub fn wrap_cast_for_agg_args(&mut self, ctx: &dyn BuildContext) -> Result<(), Error> {
        if self.args.is_empty() {
            return Ok(());
        }
        if noNeedCastAggFuncs.contains(&self.name.as_str()) {
            return Ok(());
        }
        if self.ret_tp_nil {
            // `retTp.EvalType()` on Go's nil RetTp.
            panic!("runtime error: invalid memory address or nil pointer dereference");
        }
        type CastFunc = Box<dyn Fn(&dyn BuildContext, ExprRef) -> Result<ExprRef, Error>>;
        let et = self.ret_tp.eval_type();
        // Go aliasing (M-FT): base_func.go:456 passes `retTp`, which is `a.RetTp` itself, to
        // WrapWithCastAsTime, which changes it in place (decimal, flen, flags) for every
        // argument it wraps; the CAST it builds keeps that same *FieldType as its RetType
        // (builtin_cast.go BuildCastFunctionWithCheck, `RetType: tp`). So each wrap starts from
        // the type the previous one left, and the descriptor and every CAST built here end
        // with the last one (spec §5). `time_tp` is that shared type.
        let mut time_tp: Option<FieldType> = None;
        let cast_func: CastFunc = if et == ETInt {
            Box::new(wrap_with_cast_as_int)
        } else if et == ETReal {
            Box::new(wrap_with_cast_as_real)
        } else if et == ETString {
            Box::new(wrap_with_cast_as_string)
        } else if et == ETDecimal {
            Box::new(wrap_with_cast_as_decimal)
        } else if et == ETDatetime || et == ETTimestamp {
            time_tp = Some((*self.ret_tp).clone());
            Box::new(|_: &dyn BuildContext, _: ExprRef| unreachable!("the time cast uses time_tp"))
        } else if et == ETDuration {
            Box::new(wrap_with_cast_as_duration)
        } else if et == ETJson {
            Box::new(wrap_with_cast_as_json)
        } else if et == ETVectorFloat32 {
            Box::new(wrap_with_cast_as_vector_float32)
        } else {
            // Go formats the EvalType with its String method; FieldType.EvalType never
            // returns another value, so this is unreachable.
            panic!("unsupported type {} during evaluation", et.0);
        };
        let mut time_wrapped = vec![];
        for i in 0..self.args.len() {
            // Do not cast the second args of these functions, as they are simply non-negative numbers.
            if i == 1
                && (self.name == ast::WindowFuncLead
                    || self.name == ast::WindowFuncLag
                    || self.name == ast::WindowFuncNthValue)
            {
                continue;
            }
            if self.args[i].get_type(&*ctx.get_eval_ctx()).get_type() == TypeNull {
                continue;
            }
            if let Some(tp) = time_tp.as_mut() {
                let arg = Arc::clone(&self.args[i]);
                let res = wrap_with_cast_as_time(ctx, Arc::clone(&arg), tp.clone())?;
                if !Arc::ptr_eq(&res, &arg) {
                    *tp = shared_time_tp_after(&*ctx.get_eval_ctx(), tp, &arg, &res);
                    time_wrapped.push(i);
                }
                self.args[i] = res;
                continue;
            }
            self.args[i] = cast_func(ctx, Arc::clone(&self.args[i]))?;
        }
        if let Some(tp) = time_tp
            && !time_wrapped.is_empty()
        {
            for i in time_wrapped {
                if let Expr::ScalarFunction(f) = Arc::make_mut(&mut self.args[i]) {
                    *f.ret_type_mut() = tp.clone();
                }
            }
            self.set_ret_tp(Arc::new(tp));
        }
        Ok(())
    }

    /// MemoryUsage return the memory usage of baseFuncDesc
    // go: baseFuncDesc.MemoryUsage L1 [M-INT]
    pub fn memory_usage(&self) -> i64 {
        let mut sum = size::SizeOfString + self.name.len() as i64;
        if !self.ret_tp_nil {
            sum += self.ret_tp.memory_usage();
        }
        for expr in &self.args {
            sum += expr.memory_usage();
        }
        sum
    }
}
