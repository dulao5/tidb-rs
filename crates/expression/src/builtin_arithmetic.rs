//! go-file: pkg/expression/builtin_arithmetic.go
//! go-file: pkg/expression/builtin_arithmetic_vec.go
//!
//! `builtin_arithmetic.go` (D08.5), first batch: `+ - * / DIV %` over INT, REAL and DECIMAL
//! (the six function classes and their 20 signatures) and `+ - *` over VECTOR (3 signatures).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::errors::RedactLogDisable;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{
    self, MaxDecimalScale, MaxDecimalWidth, MaxRealWidth, TypeBit, has_unsigned_flag,
};
use tidb_parser::types::UnspecifiedLength;
use tidb_parser::types::{ETDecimal, ETInt, ETReal, ETVectorFloat32, EvalType};
use tidb_types::datum::KindBinaryLiteral;
use tidb_types::errors::{ErrOverflow, ErrTruncated};
use tidb_types::etc::{FieldType, is_binary_str, is_type_temporal};
use tidb_types::fsp::UnspecifiedFsp;
use tidb_types::helper::{decimal_length2_precision, precision2_length_no_truncation};
use tidb_types::mydecimal::{
    MODE_HALF_UP, MyDecimal, decimal_add, decimal_div, decimal_mod, decimal_mul, decimal_sub,
};
use tidb_types::overflow::{div_int_with_uint, div_int64, div_uint_with_int};
use tidb_types::vector::VectorFloat32;

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::builtin_cast::{decimal_value_arg, type_after_real_wrap};
use crate::builtin_registry::FunctionClass;
use crate::context::{BuildContext, EvalContext, err_ctx, sql_mode, type_ctx};
use crate::errors::{errTruncatedWrongValue, handle_division_by_zero_error};
use crate::eval::{BuiltinFunc, SigDesc, SigView};
use crate::expression::{Expr, ExprRef};
use tidb_expression_macros::eval_fn;

// ---------------------------------------------------------------- type helpers

/// isConstantBinaryLiteral return true if expr is constant binary literal
// go: isConstantBinaryLiteral L1
pub fn is_constant_binary_literal(ctx: &dyn EvalContext, expr: &Expr) -> bool {
    if is_binary_str(&expr.get_type(ctx))
        && let Expr::Constant(v) = expr
        && v.value.kind() == KindBinaryLiteral
    {
        return true;
    }
    false
}

/// numericContextResultType returns types.EvalType for numeric function's parameters.
/// the returned types.EvalType should be one of: types.ETInt, types.ETDecimal, types.ETReal
// go: numericContextResultType L1
pub fn numeric_context_result_type(ctx: &dyn EvalContext, expr: &Expr) -> EvalType {
    let ft = expr.get_type(ctx);
    if is_type_temporal(ft.get_type()) {
        if ft.get_decimal() > 0 {
            return ETDecimal;
        }
        return ETInt;
    }
    // to solve https://github.com/pingcap/tidb/issues/27698
    // if expression is constant binary literal, like `0x1234`, `0b00011`, cast to integer
    // for other binary str column related expression, like varbinary, cast to float/double.
    if is_constant_binary_literal(ctx, expr) || ft.get_type() == TypeBit {
        return ETInt;
    }
    let mut eval_tp4_ft = ETReal;
    if !ft.hybrid() {
        eval_tp4_ft = ft.eval_type();
        if eval_tp4_ft != ETDecimal && eval_tp4_ft != ETInt {
            eval_tp4_ft = ETReal;
        }
    }
    eval_tp4_ft
}

/// setFlenDecimal4RealOrDecimal is called to set proper `flen` and `decimal` of return
/// type according to the two input parameter's types.
// go: setFlenDecimal4RealOrDecimal L1
pub fn set_flen_decimal4_real_or_decimal(
    ctx: &dyn EvalContext,
    ret_tp: &mut FieldType,
    arg0: &Expr,
    arg1: &Expr,
    is_real: bool,
    is_multiply: bool,
) {
    let (a, b) = (arg0.get_type(ctx), arg1.get_type(ctx));
    if a.get_decimal() != UnspecifiedLength && b.get_decimal() != UnspecifiedLength {
        ret_tp.set_decimal_under_limit(a.get_decimal() + b.get_decimal());
        if !is_multiply {
            ret_tp.set_decimal_under_limit(a.get_decimal().max(b.get_decimal()));
        }
        if !is_real && ret_tp.get_decimal() > MaxDecimalScale {
            ret_tp.set_decimal(MaxDecimalScale);
        }
        if a.get_flen() == UnspecifiedLength || b.get_flen() == UnspecifiedLength {
            ret_tp.set_flen(UnspecifiedLength);
            return;
        }
        if is_multiply {
            let digits_int = a.get_flen() - a.get_decimal() + b.get_flen() - b.get_decimal();
            ret_tp.set_flen_under_limit(digits_int + ret_tp.get_decimal());
        } else {
            let digits_int = (a.get_flen() - a.get_decimal()).max(b.get_flen() - b.get_decimal());
            ret_tp.set_flen_under_limit(digits_int + ret_tp.get_decimal() + 1);
        }
        if is_real {
            ret_tp.set_flen(ret_tp.get_flen().min(MaxRealWidth));
            return;
        }
        ret_tp.set_flen_under_limit(ret_tp.get_flen().min(MaxDecimalWidth));
        return;
    }
    if is_real {
        ret_tp.set_flen(UnspecifiedLength);
        ret_tp.set_decimal(UnspecifiedLength);
    } else {
        ret_tp.set_flen(MaxDecimalWidth);
        ret_tp.set_decimal(MaxDecimalScale);
    }
}

// go: arithmeticDivideFunctionClass.setType4DivDecimal L1
fn set_type4_div_decimal(
    ret_tp: &mut FieldType,
    a: &FieldType,
    b: &FieldType,
    div_prec_increment: isize,
) {
    let (mut deca, mut decb) = (a.get_decimal(), b.get_decimal());
    if deca == UnspecifiedFsp {
        deca = 0;
    }
    if decb == UnspecifiedFsp {
        decb = 0;
    }
    ret_tp.set_decimal_under_limit(deca + div_prec_increment);
    if a.get_flen() == UnspecifiedLength {
        ret_tp.set_flen(MaxDecimalWidth);
        return;
    }
    let a_prec = decimal_length2_precision(
        a.get_flen(),
        a.get_decimal(),
        has_unsigned_flag(a.get_flag()),
    );
    ret_tp.set_flen_under_limit(a_prec + decb + div_prec_increment);
    let l = precision2_length_no_truncation(
        ret_tp.get_flen(),
        ret_tp.get_decimal(),
        has_unsigned_flag(ret_tp.get_flag()),
    );
    ret_tp.set_flen_under_limit(l);
}

// go: arithmeticDivideFunctionClass.setType4DivReal L1
fn set_type4_div_real(ret_tp: &mut FieldType) {
    ret_tp.set_decimal(UnspecifiedLength);
    ret_tp.set_flen(MaxRealWidth);
}

// go: arithmeticModFunctionClass.setType4ModRealOrDecimal L1
fn set_type4_mod_real_or_decimal(
    ret_tp: &mut FieldType,
    a: &FieldType,
    b: &FieldType,
    is_decimal: bool,
) {
    if a.get_decimal() == UnspecifiedLength || b.get_decimal() == UnspecifiedLength {
        ret_tp.set_decimal(UnspecifiedLength);
    } else {
        ret_tp.set_decimal_under_limit(a.get_decimal().max(b.get_decimal()));
    }

    if a.get_flen() == UnspecifiedLength || b.get_flen() == UnspecifiedLength {
        ret_tp.set_flen(UnspecifiedLength);
    } else {
        ret_tp.set_flen(a.get_flen().max(b.get_flen()));
        if is_decimal {
            let l = ret_tp.get_flen();
            ret_tp.set_flen_under_limit(l);
            return;
        }
        ret_tp.set_flen(ret_tp.get_flen().min(MaxRealWidth));
    }
}

// go: testIfSumOverflowsUll L1
#[inline]
fn test_if_sum_overflows_ull(a: u64, b: u64) -> bool {
    u64::MAX - a < b
}

// ---------------------------------------------------------------- function classes

/// The six arithmetic function classes (their `getFunction` differ by `kind`).
#[derive(Debug)]
pub struct ArithmeticFunctionClass {
    /// The embedded baseFunctionClass.
    pub base: BaseFunctionClass,
    kind: ArithKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArithKind {
    Plus,
    Minus,
    Multiply,
    Divide,
    IntDivide,
    Mod,
}

macro_rules! arith_class {
    ($name:ident, $fname:expr, $kind:ident) => {
        #[doc = concat!("The arithmetic function class of `", stringify!($fname), "`.")]
        pub static $name: ArithmeticFunctionClass = ArithmeticFunctionClass {
            base: BaseFunctionClass {
                func_name: $fname,
                min_args: 2,
                max_args: 2,
            },
            kind: ArithKind::$kind,
        };
    };
}

arith_class!(PLUS_CLASS, ast::Plus, Plus);
arith_class!(MINUS_CLASS, ast::Minus, Minus);
arith_class!(MULTIPLY_CLASS, ast::Mul, Multiply);
arith_class!(DIVIDE_CLASS, ast::Div, Divide);
arith_class!(INT_DIVIDE_CLASS, ast::IntDiv, IntDivide);
arith_class!(MOD_CLASS, ast::Mod, Mod);

impl ArithmeticFunctionClass {
    // glue: `args[0].GetType(..).EvalType().IsVectorKind() || args[1]...` of the Plus/Minus/Multiply getFunction
    fn args_vector_kind(&self, ctx: &dyn BuildContext, args: &[ExprRef]) -> bool {
        let ev = ctx.get_eval_ctx();
        args[0].get_type(&*ev).eval_type().is_vector_kind()
            || args[1].get_type(&*ev).eval_type().is_vector_kind()
    }

    // go: arithmeticPlusFunctionClass.getFunction L1
    fn plus(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        if self.args_vector_kind(ctx, &args) {
            let bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETVectorFloat32,
                &[ETVectorFloat32, ETVectorFloat32],
            )?;
            return Ok(bf.with_sig(&builtinArithmeticPlusVectorFloat32Sig));
        }
        let ev = ctx.get_eval_ctx();
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        if lhs_eval_tp == ETReal || rhs_eval_tp == ETReal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETReal,
                &[ETReal, ETReal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, true, false);
            return Ok(bf.with_sig(&builtinArithmeticPlusRealSig));
        } else if lhs_eval_tp == ETDecimal || rhs_eval_tp == ETDecimal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETDecimal,
                &[ETDecimal, ETDecimal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, false, false);
            return Ok(bf.with_sig(&builtinArithmeticPlusDecimalSig));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        // Go reads args[i] after newBaseBuiltinFuncWithTp replaced them with the wrapped ones.
        if has_unsigned_flag(bf.args[0].get_type(&*ev).get_flag())
            || has_unsigned_flag(bf.args[1].get_type(&*ev).get_flag())
        {
            bf.tp.add_flag(mysql::UnsignedFlag);
        }
        Ok(bf.with_sig(&builtinArithmeticPlusIntSig))
    }

    // go: arithmeticMinusFunctionClass.getFunction L1
    fn minus(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        if self.args_vector_kind(ctx, &args) {
            let bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETVectorFloat32,
                &[ETVectorFloat32, ETVectorFloat32],
            )?;
            return Ok(bf.with_sig(&builtinArithmeticMinusVectorFloat32Sig));
        }
        let ev = ctx.get_eval_ctx();
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        if lhs_eval_tp == ETReal || rhs_eval_tp == ETReal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETReal,
                &[ETReal, ETReal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, true, false);
            return Ok(bf.with_sig(&builtinArithmeticMinusRealSig));
        } else if lhs_eval_tp == ETDecimal || rhs_eval_tp == ETDecimal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETDecimal,
                &[ETDecimal, ETDecimal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, false, false);
            return Ok(bf.with_sig(&builtinArithmeticMinusDecimalSig));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        // Go reads args[i] after newBaseBuiltinFuncWithTp replaced them with the wrapped ones.
        if (has_unsigned_flag(bf.args[0].get_type(&*ev).get_flag())
            || has_unsigned_flag(bf.args[1].get_type(&*ev).get_flag()))
            && !ev.sql_mode().has_no_unsigned_subtraction_mode()
        {
            bf.tp.add_flag(mysql::UnsignedFlag);
        }
        Ok(bf.with_sig(&builtinArithmeticMinusIntSig))
    }

    // go: arithmeticMultiplyFunctionClass.getFunction L1
    fn multiply(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        if self.args_vector_kind(ctx, &args) {
            let bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETVectorFloat32,
                &[ETVectorFloat32, ETVectorFloat32],
            )?;
            return Ok(bf.with_sig(&builtinArithmeticMultiplyVectorFloat32Sig));
        }
        let ev = ctx.get_eval_ctx();
        let (lhs_flag, rhs_flag) = (
            args[0].get_type(&*ev).get_flag(),
            args[1].get_type(&*ev).get_flag(),
        );
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        if lhs_eval_tp == ETReal || rhs_eval_tp == ETReal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETReal,
                &[ETReal, ETReal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, true, true);
            return Ok(bf.with_sig(&builtinArithmeticMultiplyRealSig));
        } else if lhs_eval_tp == ETDecimal || rhs_eval_tp == ETDecimal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETDecimal,
                &[ETDecimal, ETDecimal],
            )?;
            let (a0, a1) = (bf.args[0].clone(), bf.args[1].clone());
            set_flen_decimal4_real_or_decimal(&*ev, &mut bf.tp, &a0, &a1, false, true);
            return Ok(bf.with_sig(&builtinArithmeticMultiplyDecimalSig));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        if has_unsigned_flag(lhs_flag) || has_unsigned_flag(rhs_flag) {
            bf.tp.add_flag(mysql::UnsignedFlag);
            return Ok(bf.with_sig(&builtinArithmeticMultiplyIntUnsignedSig));
        }
        Ok(bf.with_sig(&builtinArithmeticMultiplyIntSig))
    }

    // go: arithmeticDivideFunctionClass.getFunction L1
    fn divide(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        let ev = ctx.get_eval_ctx();
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        if lhs_eval_tp == ETReal || rhs_eval_tp == ETReal {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETReal,
                &[ETReal, ETReal],
            )?;
            set_type4_div_real(&mut bf.tp);
            return Ok(bf.with_sig(&builtinArithmeticDivideRealSig));
        }
        let mut bf = new_base_builtin_func_with_tp(
            ctx,
            self.base.func_name,
            args,
            ETDecimal,
            &[ETDecimal, ETDecimal],
        )?;
        let (lhs_tp, rhs_tp) = (
            bf.args[0].get_type(&*ev).into_owned(),
            bf.args[1].get_type(&*ev).into_owned(),
        );
        set_type4_div_decimal(
            &mut bf.tp,
            &lhs_tp,
            &rhs_tp,
            ev.get_div_precision_increment() as isize,
        );
        Ok(bf.with_sig(&builtinArithmeticDivideDecimalSig))
    }

    // go: arithmeticIntDivideFunctionClass.getFunction L1
    fn int_divide(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        let ev = ctx.get_eval_ctx();
        let (lhs_flag, rhs_flag) = (
            args[0].get_type(&*ev).get_flag(),
            args[1].get_type(&*ev).get_flag(),
        );
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        let unsigned = has_unsigned_flag(lhs_flag) || has_unsigned_flag(rhs_flag);
        if lhs_eval_tp == ETInt && rhs_eval_tp == ETInt {
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETInt,
                &[ETInt, ETInt],
            )?;
            if unsigned {
                bf.tp.add_flag(mysql::UnsignedFlag);
            }
            return Ok(bf.with_sig(&builtinArithmeticIntDivideIntSig));
        }
        let mut bf = new_base_builtin_func_with_tp(
            ctx,
            self.base.func_name,
            args,
            ETInt,
            &[ETDecimal, ETDecimal],
        )?;
        if unsigned {
            bf.tp.add_flag(mysql::UnsignedFlag);
        }
        Ok(bf.with_sig(&builtinArithmeticIntDivideDecimalSig))
    }

    // go: arithmeticModFunctionClass.getFunction L1
    fn modulo(&self, ctx: &dyn BuildContext, args: Vec<ExprRef>) -> Result<BuiltinFunc, Error> {
        let ev = ctx.get_eval_ctx();
        let (lhs_eval_tp, rhs_eval_tp) = (
            numeric_context_result_type(&*ev, &args[0]),
            numeric_context_result_type(&*ev, &args[1]),
        );
        let lhs_unsigned = has_unsigned_flag(args[0].get_type(&*ev).get_flag());
        if lhs_eval_tp == ETReal || rhs_eval_tp == ETReal {
            // Go reads lhsTp/rhsTp (pointers taken before newBaseBuiltinFuncWithTp) after it.
            let (lhs_tp, rhs_tp) = (
                type_after_real_wrap(&*ev, &args[0]),
                type_after_real_wrap(&*ev, &args[1]),
            );
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETReal,
                &[ETReal, ETReal],
            )?;
            set_type4_mod_real_or_decimal(&mut bf.tp, &lhs_tp, &rhs_tp, false);
            if lhs_unsigned {
                bf.tp.add_flag(mysql::UnsignedFlag);
            }
            return Ok(bf.with_sig(&builtinArithmeticModRealSig));
        } else if lhs_eval_tp == ETDecimal || rhs_eval_tp == ETDecimal {
            let (lhs_tp, rhs_tp) = (
                args[0].get_type(&*ev).into_owned(),
                args[1].get_type(&*ev).into_owned(),
            );
            let mut bf = new_base_builtin_func_with_tp(
                ctx,
                self.base.func_name,
                args,
                ETDecimal,
                &[ETDecimal, ETDecimal],
            )?;
            set_type4_mod_real_or_decimal(&mut bf.tp, &lhs_tp, &rhs_tp, true);
            if lhs_unsigned {
                bf.tp.add_flag(mysql::UnsignedFlag);
            }
            return Ok(bf.with_sig(&builtinArithmeticModDecimalSig));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETInt, &[ETInt, ETInt])?;
        if lhs_unsigned {
            bf.tp.add_flag(mysql::UnsignedFlag);
        }
        // Go reads args[i] after newBaseBuiltinFuncWithTp replaced them with the wrapped ones.
        let is_lhs_unsigned = has_unsigned_flag(bf.args[0].get_type(&*ev).get_flag());
        let is_rhs_unsigned = has_unsigned_flag(bf.args[1].get_type(&*ev).get_flag());
        Ok(match (is_lhs_unsigned, is_rhs_unsigned) {
            (true, true) => bf.with_sig(&builtinArithmeticModIntUnsignedUnsignedSig),
            (true, false) => bf.with_sig(&builtinArithmeticModIntUnsignedSignedSig),
            (false, true) => bf.with_sig(&builtinArithmeticModIntSignedUnsignedSig),
            (false, false) => bf.with_sig(&builtinArithmeticModIntSignedSignedSig),
        })
    }
}

impl FunctionClass for ArithmeticFunctionClass {
    // glue: dispatch to the getFunction of the six arithmetic classes (annotated above)
    fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        match self.kind {
            ArithKind::Plus => self.plus(ctx, args),
            ArithKind::Minus => self.minus(ctx, args),
            ArithKind::Multiply => self.multiply(ctx, args),
            ArithKind::Divide => self.divide(ctx, args),
            ArithKind::IntDivide => self.int_divide(ctx, args),
            ArithKind::Mod => self.modulo(ctx, args),
        }
    }
}

// ---------------------------------------------------------------- helpers of the bodies

// glue: mysql.HasUnsignedFlag(b.args[i].GetType(ctx).GetFlag())
#[inline]
fn unsigned(f: &impl SigView, i: usize) -> bool {
    has_unsigned_flag(f.arg_type(i).get_flag())
}

/// The unsigned flags of the two arguments: bit 0 = lhs, bit 1 = rhs.
// glue: the flag combination of the checked int bodies (D08.1 spec §3.4)
#[inline]
fn unsigned_flags(f: &impl SigView) -> u8 {
    u8::from(unsigned(f, 0)) | (u8::from(unsigned(f, 1)) << 1)
}

// glue: no flags (a checked body compiled once)
#[inline]
fn no_flags(_f: &impl SigView) -> u8 {
    0
}

/// `types.ErrOverflow.GenWithStackByArgs(tp, fmt.Sprintf("(%s <op> %s)", args...))`.
// glue: the overflow error of the arithmetic signatures
fn overflow_err(ctx: &dyn EvalContext, f: &BuiltinFunc, tp: &'static str, op: &str) -> Error {
    ErrOverflow.gen_with_stack_by_args(vec![
        Arg::from(tp),
        Arg::from(format!(
            "({} {op} {})",
            String::from_utf8_lossy(&f.args[0].string_with_ctx(ctx, RedactLogDisable)),
            String::from_utf8_lossy(&f.args[1].string_with_ctx(ctx, RedactLogDisable))
        )),
    ])
}

/// A `*types.MyDecimal` as a `fmt` argument (its String method).
// glue: Go passes a *types.MyDecimal to GenWithStackByArgs
fn decimal_ptr_arg(d: &MyDecimal) -> Arg {
    Arg::Stringer(d.string().into_bytes(), "*types.MyDecimal")
}

// ---------------------------------------------------------------- PLUS

// glue: the error builder of the checked body (plus_int): Go's overflow error for one row
#[cold]
fn plus_int_err(ctx: &dyn EvalContext, f: &impl SigView, _a: i64, _b: i64) -> Error {
    let tp = if unsigned(f, 0) || unsigned(f, 1) {
        "BIGINT UNSIGNED"
    } else {
        "BIGINT"
    };
    overflow_err(ctx, f, tp, "+")
}

/// `builtinArithmeticPlusIntSig.evalInt`: the overflow checks of the four unsigned-flag
/// combinations. The `checked` form (D08.1 spec §3.4): no per-row side effects, NULL rows
/// skipped before the error, and the row and vector versions report the same error.
// go: builtinArithmeticPlusIntSig.evalInt L1
// go: builtinArithmeticPlusIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticPlusIntSig.vecEvalInt L2 (the checked vector driver: one loop per unsigned-flag combination)
// go: builtinArithmeticPlusIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinArithmeticPlusIntSig.plusUU L2 (FL = 3 of the checked driver)
// go: builtinArithmeticPlusIntSig.plusUS L2 (FL = 1 of the checked driver)
// go: builtinArithmeticPlusIntSig.plusSU L2 (FL = 2 of the checked driver)
// go: builtinArithmeticPlusIntSig.plusSS L2 (FL = 0 of the checked driver)
#[eval_fn(Int, checked(nflags = 2, flags = unsigned_flags, err = plus_int_err))]
pub fn plus_int<const FL: u8>(a: i64, b: i64) -> (i64, bool) {
    let (is_lhs_unsigned, is_rhs_unsigned) = (FL & 1 != 0, FL & 2 != 0);
    let overflow = match (is_lhs_unsigned, is_rhs_unsigned) {
        (true, true) => (a as u64) > u64::MAX - (b as u64),
        (true, false) => {
            (b < 0 && (b.wrapping_neg() as u64) > (a as u64))
                || (b > 0 && (a as u64) > u64::MAX - (b as u64))
        }
        (false, true) => {
            (a < 0 && (a.wrapping_neg() as u64) > (b as u64))
                || (a > 0 && (b as u64) > u64::MAX - (a as u64))
        }
        (false, false) => (a > 0 && b > i64::MAX - a) || (a < 0 && b < i64::MIN - a),
    };
    (a.wrapping_add(b), overflow)
}

/// `builtinArithmeticPlusDecimalSig.evalDecimal`.
// go: builtinArithmeticPlusDecimalSig.evalDecimal L1
// go: builtinArithmeticPlusDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticPlusDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticPlusDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn plus_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut c = MyDecimal::default();
    if let Err(e) = decimal_add(a, b, &mut c) {
        if ErrOverflow.equal(&e) {
            return Err(overflow_err(ctx, f, "DECIMAL", "+"));
        }
        return Err(e);
    }
    Ok(Some(c))
}

// glue: the error builder of the checked body (plus_real): Go's overflow error for one row
#[cold]
fn plus_real_err(ctx: &dyn EvalContext, f: &impl SigView, _a: f64, _b: f64) -> Error {
    overflow_err(ctx, f, "DOUBLE", "+")
}

/// `builtinArithmeticPlusRealSig.evalReal`: both arguments evaluated before NULL (`eager`).
// go: builtinArithmeticPlusRealSig.evalReal L1
// go: builtinArithmeticPlusRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticPlusRealSig.vecEvalReal L2 (the checked vector driver of this body)
// go: builtinArithmeticPlusRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, eager, checked(nflags = 0, flags = no_flags, err = plus_real_err))]
pub fn plus_real<const FL: u8>(a: f64, b: f64) -> (f64, bool) {
    let r = a + b;
    (r, !r.is_finite())
}

// ---------------------------------------------------------------- MINUS

// glue: the error builder of the checked body (minus_real): Go's overflow error for one row
#[cold]
fn minus_real_err(ctx: &dyn EvalContext, f: &impl SigView, _a: f64, _b: f64) -> Error {
    overflow_err(ctx, f, "DOUBLE", "-")
}

/// `builtinArithmeticMinusRealSig.evalReal`.
// go: builtinArithmeticMinusRealSig.evalReal L1
// go: builtinArithmeticMinusRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMinusRealSig.vecEvalReal L2 (the checked vector driver of this body)
// go: builtinArithmeticMinusRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, checked(nflags = 0, flags = no_flags, err = minus_real_err))]
pub fn minus_real<const FL: u8>(a: f64, b: f64) -> (f64, bool) {
    let r = a - b;
    (r, !r.is_finite())
}

/// `builtinArithmeticMinusDecimalSig.evalDecimal`.
// go: builtinArithmeticMinusDecimalSig.evalDecimal L1
// go: builtinArithmeticMinusDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMinusDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticMinusDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn minus_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut c = MyDecimal::default();
    if let Err(e) = decimal_sub(a, b, &mut c) {
        if ErrOverflow.equal(&e) {
            return Err(overflow_err(ctx, f, "DECIMAL", "-"));
        }
        return Err(e);
    }
    Ok(Some(c))
}

/// returns true when overflowed
// go: builtinArithmeticMinusIntSig.overflowCheck L1
fn minus_overflow_check(
    is_lhs_unsigned: bool,
    is_rhs_unsigned: bool,
    signed: bool,
    a: i64,
    b: i64,
) -> bool {
    let res = a.wrapping_sub(b);
    let (ua, ub) = (a as u64, b as u64);
    let mut res_unsigned = false;
    if is_lhs_unsigned {
        if is_rhs_unsigned {
            if ua < ub {
                if res >= 0 {
                    return true;
                }
            } else {
                res_unsigned = true;
            }
        } else if b >= 0 {
            if ua > ub {
                res_unsigned = true;
            }
        } else {
            if test_if_sum_overflows_ull(ua, b.wrapping_neg() as u64) {
                return true;
            }
            res_unsigned = true;
        }
    } else if is_rhs_unsigned {
        if (a.wrapping_sub(i64::MIN) as u64) < ub {
            return true;
        }
    } else if a > 0 && b < 0 {
        res_unsigned = true;
    } else if a < 0 && b > 0 && res >= 0 {
        return true;
    }

    if (!signed && !res_unsigned && res < 0)
        || (signed && res_unsigned && (res as u64) > i64::MAX as u64)
    {
        return true;
    }

    false
}

/// `builtinArithmeticMinusIntSig.evalInt`. Not `checked`: the error type depends on the SQL
/// mode (`NO_UNSIGNED_SUBTRACTION`), which a checked flag function cannot read.
// go: builtinArithmeticMinusIntSig.evalInt L1
// go: builtinArithmeticMinusIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMinusIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticMinusIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn minus_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: i64,
    b: i64,
) -> Result<Option<i64>, Error> {
    let force_to_signed = sql_mode(ctx).has_no_unsigned_subtraction_mode();
    let is_lhs_unsigned = unsigned(f, 0);
    let is_rhs_unsigned = unsigned(f, 1);

    let signed = force_to_signed || (!is_lhs_unsigned && !is_rhs_unsigned);
    let err_type = if signed { "BIGINT" } else { "BIGINT UNSIGNED" };
    if minus_overflow_check(is_lhs_unsigned, is_rhs_unsigned, signed, a, b) {
        return Err(overflow_err(ctx, f, err_type, "-"));
    }
    Ok(Some(a.wrapping_sub(b)))
}

// ---------------------------------------------------------------- MULTIPLY

// glue: the error builder of the checked body (multiply_real): Go's overflow error for one row
#[cold]
fn multiply_real_err(ctx: &dyn EvalContext, f: &impl SigView, _a: f64, _b: f64) -> Error {
    overflow_err(ctx, f, "DOUBLE", "*")
}

/// `builtinArithmeticMultiplyRealSig.evalReal`: only an infinite product is an error (NaN
/// is not).
// go: builtinArithmeticMultiplyRealSig.evalReal L1
// go: builtinArithmeticMultiplyRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMultiplyRealSig.vecEvalReal L2 (the checked vector driver of this body)
// go: builtinArithmeticMultiplyRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, checked(nflags = 0, flags = no_flags, err = multiply_real_err))]
pub fn multiply_real<const FL: u8>(a: f64, b: f64) -> (f64, bool) {
    let r = a * b;
    (r, r.is_infinite())
}

/// `builtinArithmeticMultiplyDecimalSig.evalDecimal`: `ErrTruncated` is ignored.
// go: builtinArithmeticMultiplyDecimalSig.evalDecimal L1
// go: builtinArithmeticMultiplyDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMultiplyDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticMultiplyDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn multiply_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut c = MyDecimal::default();
    if let Err(e) = decimal_mul(a, b, &mut c)
        && !ErrTruncated.equal(&e)
    {
        if ErrOverflow.equal(&e) {
            return Err(overflow_err(ctx, f, "DECIMAL", "*"));
        }
        return Err(e);
    }
    Ok(Some(c))
}

// glue: the error builder of the checked body (multiply_int_unsigned): Go's overflow error for one row
#[cold]
fn multiply_int_unsigned_err(ctx: &dyn EvalContext, f: &impl SigView, _a: i64, _b: i64) -> Error {
    overflow_err(ctx, f, "BIGINT UNSIGNED", "*")
}

/// `builtinArithmeticMultiplyIntUnsignedSig.evalInt`.
// go: builtinArithmeticMultiplyIntUnsignedSig.evalInt L1
// go: builtinArithmeticMultiplyIntUnsignedSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMultiplyIntUnsignedSig.vecEvalInt L2 (the checked vector driver of this body)
// go: builtinArithmeticMultiplyIntUnsignedSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, checked(nflags = 0, flags = no_flags, err = multiply_int_unsigned_err))]
pub fn multiply_int_unsigned<const FL: u8>(a: i64, b: i64) -> (i64, bool) {
    let (unsigned_a, unsigned_b) = (a as u64, b as u64);
    let result = unsigned_a.wrapping_mul(unsigned_b);
    (
        result as i64,
        unsigned_a != 0 && result / unsigned_a != unsigned_b,
    )
}

// glue: the error builder of the checked body (multiply_int): Go's overflow error for one row
#[cold]
fn multiply_int_err(ctx: &dyn EvalContext, f: &impl SigView, _a: i64, _b: i64) -> Error {
    overflow_err(ctx, f, "BIGINT", "*")
}

/// `builtinArithmeticMultiplyIntSig.evalInt`.
// go: builtinArithmeticMultiplyIntSig.evalInt L1
// go: builtinArithmeticMultiplyIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticMultiplyIntSig.vecEvalInt L2 (the checked vector driver of this body)
// go: builtinArithmeticMultiplyIntSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int, checked(nflags = 0, flags = no_flags, err = multiply_int_err))]
pub fn multiply_int<const FL: u8>(a: i64, b: i64) -> (i64, bool) {
    let result = a.wrapping_mul(b);
    // Go's integer division does not panic on MinInt64 / -1 (the quotient is MinInt64):
    // wrapping_div; the second test catches that row.
    let overflow = (a != 0 && result.wrapping_div(a) != b) || (result == i64::MIN && a == -1);
    (result, overflow)
}

// ---------------------------------------------------------------- DIVIDE

/// `builtinArithmeticDivideRealSig.evalReal`.
// go: builtinArithmeticDivideRealSig.evalReal L1
// go: builtinArithmeticDivideRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticDivideRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticDivideRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real)]
pub fn divide_real(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: f64,
    b: f64,
) -> Result<Option<f64>, Error> {
    if b == 0.0 {
        handle_division_by_zero_error(ctx)?;
        return Ok(None);
    }
    let result = a / b;
    if result.is_infinite() {
        return Err(overflow_err(ctx, f, "DOUBLE", "/"));
    }
    Ok(Some(result))
}

/// `builtinArithmeticDivideDecimalSig.evalDecimal`. On `ErrTruncated` the row version calls
/// `tc.HandleTruncate` with the `*MyDecimal`, the vector version `ec.HandleError` with the
/// `types.MyDecimal` value (printed field by field): each driver follows its own Go version
/// (D08.1 spec §3.4 (c), spec §4).
// go: builtinArithmeticDivideDecimalSig.evalDecimal L1
// go: builtinArithmeticDivideDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticDivideDecimalSig.vecEvalDecimal L2 (the vector driver of this body; ErrTruncated via ec.HandleError with the decimal value, as in Go)
// go: builtinArithmeticDivideDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn divide_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut c = MyDecimal::default();
    match decimal_div(a, b, &mut c, ctx.get_div_precision_increment() as isize) {
        Err(e) if tidb_types::errors::ErrDivByZero.equal(&e) => {
            handle_division_by_zero_error(ctx)?;
            Ok(None)
        }
        Err(e) if ErrTruncated.equal(&e) => {
            if f.vector() {
                err_ctx(ctx).handle_error(
                    errTruncatedWrongValue
                        .gen_with_stack_by_args(vec![Arg::from("DECIMAL"), decimal_value_arg(&c)]),
                )?;
            } else {
                type_ctx(ctx).handle_truncate(Err(errTruncatedWrongValue
                    .gen_with_stack_by_args(vec![Arg::from("DECIMAL"), decimal_ptr_arg(&c)])))?;
            }
            Ok(Some(c))
        }
        Ok(()) => {
            let (_, frac) = c.precision_and_frac();
            if frac < f.tp.get_decimal() {
                c.round_in_place(f.tp.get_decimal(), MODE_HALF_UP)?;
            }
            Ok(Some(c))
        }
        Err(e) if ErrOverflow.equal(&e) => Err(overflow_err(ctx, f, "DECIMAL", "/")),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- INT DIVIDE

/// `builtinArithmeticIntDivideIntSig.evalInt`.
// go: builtinArithmeticIntDivideIntSig.evalInt L1
// go: builtinArithmeticIntDivideIntSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticIntDivideIntSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticIntDivideIntSig.vectorized L2 (SigDesc.vectorized)
// go: builtinArithmeticIntDivideIntSig.divideUU L2 (the body for two unsigned arguments)
// go: builtinArithmeticIntDivideIntSig.divideUS L2 (the body for unsigned / signed)
// go: builtinArithmeticIntDivideIntSig.divideSU L2 (the body for signed / unsigned)
// go: builtinArithmeticIntDivideIntSig.divideSS L2 (the body for two signed arguments)
#[eval_fn(Int)]
pub fn int_divide_int(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: i64,
    b: i64,
) -> Result<Option<i64>, Error> {
    if b == 0 {
        handle_division_by_zero_error(ctx)?;
        return Ok(None);
    }
    let ret = match (unsigned(f, 0), unsigned(f, 1)) {
        (true, true) => ((a as u64) / (b as u64)) as i64,
        (true, false) => div_uint_with_int(a as u64, b)? as i64,
        (false, true) => div_int_with_uint(a, b as u64)? as i64,
        (false, false) => div_int64(a, b)?,
    };
    Ok(Some(ret))
}

/// `builtinArithmeticIntDivideDecimalSig.evalInt`. The overflow error text: the row version
/// prints the arguments (`StringWithCtx`), the vector version the two decimal values
/// (D08.1 spec §3.4 (c), spec §4).
// go: builtinArithmeticIntDivideDecimalSig.evalInt L1
// go: builtinArithmeticIntDivideDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticIntDivideDecimalSig.vecEvalInt L2 (the vector driver of this body; its overflow error prints the decimal values, as in Go)
// go: builtinArithmeticIntDivideDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Int)]
pub fn int_divide_decimal(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<i64>, Error> {
    let ec = err_ctx(ctx);
    let mut c = MyDecimal::default();
    match decimal_div(a, b, &mut c, ctx.get_div_precision_increment() as isize) {
        Err(e) if tidb_types::errors::ErrDivByZero.equal(&e) => {
            handle_division_by_zero_error(ctx)?;
            return Ok(None);
        }
        Err(e) if ErrTruncated.equal(&e) || ErrOverflow.equal(&e) => {
            ec.handle_error(
                errTruncatedWrongValue
                    .gen_with_stack_by_args(vec![Arg::from("DECIMAL"), decimal_ptr_arg(&c)]),
            )?;
        }
        Err(e) => return Err(e),
        Ok(()) => {}
    }

    let overflow = |tp: &'static str| -> Error {
        if f.vector() {
            ErrOverflow.gen_with_stack_by_args(vec![
                Arg::from(tp),
                Arg::from(format!("({} DIV {})", a.string(), b.string())),
            ])
        } else {
            overflow_err(ctx, f, tp, "DIV")
        }
    };

    let ret = if unsigned(f, 0) || unsigned(f, 1) {
        let (val, err) = c.to_uint();
        // err returned by ToUint may be ErrTruncated or ErrOverflow, only handle ErrOverflow, ignore ErrTruncated.
        if let Err(e) = &err
            && ErrOverflow.equal(e)
        {
            let (v, err) = c.to_int();
            // when the final result is at (-1, 0], it should be return 0 instead of the error
            if v == 0 && err.as_ref().is_err_and(|e| ErrTruncated.equal(e)) {
                return Ok(Some(0));
            }
            return Err(overflow("BIGINT UNSIGNED"));
        }
        val as i64
    } else {
        let (v, err) = c.to_int();
        // err returned by ToInt may be ErrTruncated or ErrOverflow, only handle ErrOverflow, ignore ErrTruncated.
        if let Err(e) = &err
            && ErrOverflow.equal(e)
        {
            return Err(overflow("BIGINT"));
        }
        v
    };
    Ok(Some(ret))
}

// ---------------------------------------------------------------- MOD

/// `builtinArithmeticModRealSig.evalReal`: both arguments first (`eager`).
// go: builtinArithmeticModRealSig.evalReal L1
// go: builtinArithmeticModRealSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModRealSig.vecEvalReal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModRealSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Real, eager)]
pub fn mod_real(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    a: f64,
    b: f64,
) -> Result<Option<f64>, Error> {
    if b == 0.0 {
        handle_division_by_zero_error(ctx)?;
        return Ok(None);
    }
    Ok(Some(tidb_base::gostd::math::r#mod(a, b)))
}

/// `builtinArithmeticModDecimalSig.evalDecimal`.
// go: builtinArithmeticModDecimalSig.evalDecimal L1
// go: builtinArithmeticModDecimalSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModDecimalSig.vecEvalDecimal L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModDecimalSig.vectorized L2 (SigDesc.vectorized)
#[eval_fn(Decimal)]
pub fn mod_decimal(
    ctx: &dyn EvalContext,
    _f: &impl SigView,
    a: &MyDecimal,
    b: &MyDecimal,
) -> Result<Option<MyDecimal>, Error> {
    let mut c = MyDecimal::default();
    match decimal_mod(a, b, &mut c) {
        Err(e) if tidb_types::errors::ErrDivByZero.equal(&e) => {
            handle_division_by_zero_error(ctx)?;
            Ok(None)
        }
        Err(e) => Err(e),
        Ok(()) => Ok(Some(c)),
    }
}

/// The four signed/unsigned MOD signatures: both arguments first (`eager`), then NULL, then
/// division by zero.
macro_rules! mod_int {
    ($fname:ident, |$a:ident, $b:ident| $e:expr) => {
        #[eval_fn(Int, eager)]
        pub fn $fname(
            ctx: &dyn EvalContext,
            _f: &impl SigView,
            $a: i64,
            $b: i64,
        ) -> Result<Option<i64>, Error> {
            if $b == 0 {
                handle_division_by_zero_error(ctx)?;
                return Ok(None);
            }
            Ok(Some($e))
        }
    };
}

// go: builtinArithmeticModIntUnsignedUnsignedSig.evalInt L1
// go: builtinArithmeticModIntUnsignedUnsignedSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModIntUnsignedUnsignedSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModIntUnsignedUnsignedSig.vectorized L2 (SigDesc.vectorized)
mod_int!(mod_int_unsigned_unsigned, |a, b| ((a as u64) % (b as u64))
    as i64);
// go: builtinArithmeticModIntUnsignedSignedSig.evalInt L1
// go: builtinArithmeticModIntUnsignedSignedSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModIntUnsignedSignedSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModIntUnsignedSignedSig.vectorized L2 (SigDesc.vectorized)
mod_int!(mod_int_unsigned_signed, |a, b| if b < 0 {
    ((a as u64) % (b.wrapping_neg() as u64)) as i64
} else {
    ((a as u64) % (b as u64)) as i64
});
// go: builtinArithmeticModIntSignedUnsignedSig.evalInt L1
// go: builtinArithmeticModIntSignedUnsignedSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModIntSignedUnsignedSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModIntSignedUnsignedSig.vectorized L2 (SigDesc.vectorized)
mod_int!(mod_int_signed_unsigned, |a, b| if a < 0 {
    (((a.wrapping_neg() as u64) % (b as u64)) as i64).wrapping_neg()
} else {
    ((a as u64) % (b as u64)) as i64
});
// go: builtinArithmeticModIntSignedSignedSig.evalInt L1
// go: builtinArithmeticModIntSignedSignedSig.Clone L2 (BuiltinFunc::clone_from_base)
// go: builtinArithmeticModIntSignedSignedSig.vecEvalInt L2 (the vector driver of this body, M-EVAL)
// go: builtinArithmeticModIntSignedSignedSig.vectorized L2 (SigDesc.vectorized)
// Go: MinInt64 % -1 is 0 (no panic, the spec defines x % -1 == 0); wrapping_rem matches.
mod_int!(mod_int_signed_signed, |a, b| a.wrapping_rem(b));

// ---------------------------------------------------------------- VECTOR

/// The VECTOR `+ - *`: both arguments first (`eager`); Go defines no vector version
/// (`novec`, `vectorized()` false).
macro_rules! vector_op {
    ($fname:ident, $m:ident) => {
        #[eval_fn(VectorFloat32, eager, novec)]
        pub fn $fname(
            _ctx: &dyn EvalContext,
            _f: &impl SigView,
            a: &VectorFloat32<'_>,
            b: &VectorFloat32<'_>,
        ) -> Result<Option<VectorFloat32<'static>>, Error> {
            Ok(Some(a.$m(b)?))
        }
    };
}

// go: builtinArithmeticPlusVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinArithmeticPlusVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
vector_op!(plus_vector_float32, add);
// go: builtinArithmeticMinusVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinArithmeticMinusVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
vector_op!(minus_vector_float32, sub);
// go: builtinArithmeticMultiplyVectorFloat32Sig.evalVectorFloat32 L1
// go: builtinArithmeticMultiplyVectorFloat32Sig.Clone L2 (BuiltinFunc::clone_from_base)
vector_op!(multiply_vector_float32, mul);

// ---------------------------------------------------------------- signature descriptors

macro_rules! arith_sig {
    ($name:ident, $pb:expr, $drivers:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: $pb,
            vectorized: true,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::None,
        };
    };
}

// tipb.ScalarFuncSig codes (go-tipb expression.pb.go).
arith_sig!(builtinArithmeticPlusRealSig, 200, PLUS_REAL);
arith_sig!(builtinArithmeticPlusDecimalSig, 201, PLUS_DECIMAL);
arith_sig!(builtinArithmeticPlusIntSig, 203, PLUS_INT);
arith_sig!(builtinArithmeticMinusRealSig, 204, MINUS_REAL);
arith_sig!(builtinArithmeticMinusDecimalSig, 205, MINUS_DECIMAL);
arith_sig!(builtinArithmeticMinusIntSig, 207, MINUS_INT);
arith_sig!(builtinArithmeticMultiplyRealSig, 208, MULTIPLY_REAL);
arith_sig!(builtinArithmeticMultiplyDecimalSig, 209, MULTIPLY_DECIMAL);
arith_sig!(builtinArithmeticMultiplyIntSig, 210, MULTIPLY_INT);
arith_sig!(builtinArithmeticDivideRealSig, 211, DIVIDE_REAL);
arith_sig!(builtinArithmeticDivideDecimalSig, 212, DIVIDE_DECIMAL);
arith_sig!(builtinArithmeticIntDivideIntSig, 213, INT_DIVIDE_INT);
arith_sig!(
    builtinArithmeticIntDivideDecimalSig,
    214,
    INT_DIVIDE_DECIMAL
);
arith_sig!(builtinArithmeticModRealSig, 215, MOD_REAL);
arith_sig!(builtinArithmeticModDecimalSig, 216, MOD_DECIMAL);
arith_sig!(
    builtinArithmeticMultiplyIntUnsignedSig,
    218,
    MULTIPLY_INT_UNSIGNED
);
arith_sig!(
    builtinArithmeticModIntUnsignedUnsignedSig,
    223,
    MOD_INT_UNSIGNED_UNSIGNED
);
arith_sig!(
    builtinArithmeticModIntUnsignedSignedSig,
    224,
    MOD_INT_UNSIGNED_SIGNED
);
arith_sig!(
    builtinArithmeticModIntSignedUnsignedSig,
    225,
    MOD_INT_SIGNED_UNSIGNED
);
arith_sig!(
    builtinArithmeticModIntSignedSignedSig,
    226,
    MOD_INT_SIGNED_SIGNED
);

// Go: no setPbCode for the VECTOR signatures (pb code 0), vectorized() false.
macro_rules! vector_sig {
    ($name:ident, $drivers:ident) => {
        #[doc = concat!("`", stringify!($name), "`.")]
        pub static $name: SigDesc = SigDesc {
            name: stringify!($name),
            pb_code: 0,
            vectorized: false,
            drivers: $drivers,
            args_fallback: crate::eval::ArgsFallback::None,
        };
    };
}
vector_sig!(builtinArithmeticPlusVectorFloat32Sig, PLUS_VECTOR_FLOAT32);
vector_sig!(builtinArithmeticMinusVectorFloat32Sig, MINUS_VECTOR_FLOAT32);
vector_sig!(
    builtinArithmeticMultiplyVectorFloat32Sig,
    MULTIPLY_VECTOR_FLOAT32
);
