//! go-file: pkg/expression/builtin_control.go
//!
//! The type inference of the control functions (`pkg/expression/builtin_control.go`, block
//! D08.9): `InferType4ControlFuncs` and its helpers. Ported ahead of the control functions
//! because COALESCE (D08.4 batch 2) uses it; IF, IFNULL, NULLIF, CASE WHEN, LEAD and LAG
//! come with D08.9.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::mysql::{
    self, BinaryFlag, DefaultCharset, DefaultCollationName, MaxRealWidth, NotNullFlag,
};
use tidb_parser::ast::funcs as ast;
use tidb_parser::types::{ETDecimal, ETInt, ETString, EvalType, UnspecifiedLength};
use tidb_types::etc::{FieldType, is_binary_str, is_non_binary_str, is_string};
use tidb_types::field_type::{
    agg_field_type, aggregate_eval_type, set_bin_chs_cln_flag, set_type_flag,
    try_to_fix_flen_of_datetime,
};

use crate::collation::check_and_derive_collation_from_exprs;
use crate::context::BuildContext;
use crate::expression::ExprRef;

/// The larger of two display widths; -1 (unknown, e.g. an expression) gives `MaxRealWidth`.
// go: maxlen L1
pub(crate) fn maxlen(lhs_flen: isize, rhs_flen: isize) -> isize {
    // -1 indicates that the length is unknown, such as the case for expressions.
    if lhs_flen < 0 || rhs_flen < 0 {
        return MaxRealWidth;
    }
    lhs_flen.max(rhs_flen)
}

// go: setFlenFromArgs L1
fn set_flen_from_args(
    eval_type: EvalType,
    result_field_type: &mut FieldType,
    arg_tps: &[&FieldType],
) {
    if eval_type == ETDecimal || eval_type == ETInt {
        let mut max_arg_flen = 0;
        for tp in arg_tps {
            let mut flag_len = 0;
            if !mysql::has_unsigned_flag(tp.get_flag()) {
                flag_len = 1;
            }
            let mut flen = tp.get_flen() - flag_len;
            if tp.get_decimal() != UnspecifiedLength {
                flen -= tp.get_decimal();
            }
            max_arg_flen = maxlen(max_arg_flen, flen);
        }
        // For a decimal field, the `length` and `flen` are not the same.
        // `length` only holds the binary data, while `flen` represents the number of digits required to display the field, including the negative sign.
        // In the current implementation of TiDB, `flen` and `length` are treated as the same, so the `length` of a decimal may be inconsistent with that of MySQL.
        let result_flen = max_arg_flen + result_field_type.get_decimal() + 1; // account for -1 len fields
        result_field_type.set_flen_under_limit(result_flen);
    } else if eval_type == ETString {
        let mut max_len = 0;
        for tp in arg_tps {
            match tp.get_type() {
                mysql::TypeTiny => max_len = maxlen(4, max_len),
                mysql::TypeShort => max_len = maxlen(6, max_len),
                mysql::TypeInt24 => max_len = maxlen(9, max_len),
                mysql::TypeLong => max_len = maxlen(11, max_len),
                mysql::TypeLonglong => max_len = maxlen(20, max_len),
                _ => {
                    let arg_flen = tp.get_flen();
                    if arg_flen == UnspecifiedLength {
                        result_field_type.set_flen(UnspecifiedLength);
                        return;
                    }
                    max_len = maxlen(arg_flen, max_len);
                }
            }
        }
        result_field_type.set_flen(max_len);
    } else {
        let mut max_len = 0;
        for tp in arg_tps {
            max_len = maxlen(tp.get_flen(), max_len);
        }
        result_field_type.set_flen(max_len);
    }
}

// go: setDecimalFromArgs L1
fn set_decimal_from_args(
    eval_type: EvalType,
    result_field_type: &mut FieldType,
    arg_tps: &[&FieldType],
) {
    if eval_type == ETInt {
        result_field_type.set_decimal(0);
    } else {
        let mut max_decimal = 0;
        for tp in arg_tps {
            if tp.get_decimal() == UnspecifiedLength {
                result_field_type.set_decimal(UnspecifiedLength);
                return;
            }
            max_decimal = max_decimal.max(tp.get_decimal());
        }
        result_field_type.set_decimal_under_limit(max_decimal);
    }
}

/// NonBinaryStr means the arg is a string but not binary string
// go: hasNonBinaryStr L1
fn has_non_binary_str(args: &[&FieldType]) -> bool {
    args.iter().any(|a| is_non_binary_str(a))
}

// go: hasBinaryStr L1
fn has_binary_str(args: &[&FieldType]) -> bool {
    args.iter().any(|a| is_binary_str(a))
}

// go: addCollateAndCharsetAndFlagFromArgs L1
fn add_collate_and_charset_and_flag_from_args(
    ctx: &dyn BuildContext,
    func_name: &str,
    eval_type: EvalType,
    result_field_type: &mut FieldType,
    args: &[ExprRef],
) -> Result<(), Error> {
    let ectx = ctx.get_eval_ctx();
    match func_name {
        ast::If | ast::Ifnull | ast::WindowFuncLead | ast::WindowFuncLag => {
            if args.len() != 2 {
                panic!("unexpected length of args for if/ifnull/lead/lag");
            }
            let lhs = args[0].get_type(&*ectx).into_owned();
            let rhs = args[1].get_type(&*ectx).into_owned();
            if is_non_binary_str(&lhs) && !is_binary_str(&rhs) {
                let ec = check_and_derive_collation_from_exprs(ctx, func_name, eval_type, args)?;
                result_field_type.set_collate(&ec.collation);
                result_field_type.set_charset(&ec.charset);
                result_field_type.set_flag(0);
                if mysql::has_binary_flag(lhs.get_flag()) || !is_non_binary_str(&rhs) {
                    result_field_type.add_flag(BinaryFlag);
                }
            } else if is_non_binary_str(&rhs) && !is_binary_str(&lhs) {
                let ec = check_and_derive_collation_from_exprs(ctx, func_name, eval_type, args)?;
                result_field_type.set_collate(&ec.collation);
                result_field_type.set_charset(&ec.charset);
                result_field_type.set_flag(0);
                if mysql::has_binary_flag(rhs.get_flag()) || !is_non_binary_str(&lhs) {
                    result_field_type.add_flag(BinaryFlag);
                }
            } else if is_binary_str(&lhs) || is_binary_str(&rhs) || !eval_type.is_string_kind() {
                set_bin_chs_cln_flag(result_field_type);
            } else {
                result_field_type.set_charset(DefaultCharset);
                result_field_type.set_collate(DefaultCollationName);
                result_field_type.set_flag(0);
            }
        }
        ast::Case => {
            if args.is_empty() {
                panic!("unexpected length 0 of args for casewhen");
            }
            let ec = check_and_derive_collation_from_exprs(ctx, func_name, eval_type, args)?;
            result_field_type.set_collate(&ec.collation);
            result_field_type.set_charset(&ec.charset);
            for a in args {
                let tp = a.get_type(&*ectx);
                if mysql::has_binary_flag(tp.get_flag()) || !is_non_binary_str(&tp) {
                    result_field_type.add_flag(BinaryFlag);
                    break;
                }
            }
        }
        ast::Coalesce => {
            // TODO ast.Case and ast.Coalesce should be merged into the same branch
            let owned: Vec<FieldType> = args
                .iter()
                .map(|a| a.get_type(&*ectx).into_owned())
                .collect();
            let arg_types: Vec<&FieldType> = owned.iter().collect();

            let non_binary_str_exist = has_non_binary_str(&arg_types);
            let binary_str_exist = has_binary_str(&arg_types);
            if !binary_str_exist && non_binary_str_exist {
                let ec = check_and_derive_collation_from_exprs(ctx, func_name, eval_type, args)?;
                result_field_type.set_collate(&ec.collation);
                result_field_type.set_charset(&ec.charset);
                result_field_type.set_flag(0);

                // hasNonStringType means that there is a type that is not string
                let has_non_string_type = arg_types.iter().any(|t| !is_string(t.get_type()));
                if has_non_string_type {
                    result_field_type.add_flag(BinaryFlag);
                }
            } else if binary_str_exist || !eval_type.is_string_kind() {
                set_bin_chs_cln_flag(result_field_type);
            } else {
                result_field_type.set_charset(DefaultCharset);
                result_field_type.set_collate(DefaultCollationName);
                result_field_type.set_flag(0);
            }
        }
        _ => panic!("unexpected function: {func_name}"),
    }
    Ok(())
}

/// InferType4ControlFuncs infer result type for builtin IF, IFNULL, NULLIF, CASEWHEN, COALESCE, LEAD and LAG.
// go: InferType4ControlFuncs L1
pub fn infer_type4_control_funcs(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: &[ExprRef],
) -> Result<FieldType, Error> {
    let args_num = args.len();
    if args_num == 0 {
        panic!("unexpected length 0 of args");
    }
    let ectx = ctx.get_eval_ctx();
    let types: Vec<FieldType> = args
        .iter()
        .map(|a| a.get_type(&*ectx).into_owned())
        .collect();
    drop(ectx);
    let mut null_fields: Vec<&FieldType> = Vec::with_capacity(args_num);
    let mut not_null_fields: Vec<&FieldType> = Vec::with_capacity(args_num);
    for t in &types {
        if t.get_type() == mysql::TypeNull {
            null_fields.push(t);
        } else {
            not_null_fields.push(t);
        }
    }
    let mut result_field_type;
    if null_fields.len() == args_num {
        // all field is TypeNull
        result_field_type = null_fields[0].clone();
        // If any of arg is NULL, result type need unset NotNullFlag.
        let mut temp_flag = result_field_type.get_flag();
        set_type_flag(&mut temp_flag, NotNullFlag, false);
        result_field_type.set_flag(temp_flag);

        result_field_type.set_type(mysql::TypeNull);
        result_field_type.set_flen(0);
        result_field_type.set_decimal(0);
        set_bin_chs_cln_flag(&mut result_field_type);
    } else {
        if not_null_fields.len() == 1 {
            result_field_type = not_null_fields[0].clone();
        } else {
            result_field_type = agg_field_type(&not_null_fields);
            let mut temp_flag = 0;
            let eval_type = aggregate_eval_type(&not_null_fields, &mut temp_flag);
            result_field_type.set_flag(temp_flag);
            set_decimal_from_args(eval_type, &mut result_field_type, &not_null_fields);
            add_collate_and_charset_and_flag_from_args(
                ctx,
                func_name,
                eval_type,
                &mut result_field_type,
                args,
            )?;
            set_flen_from_args(eval_type, &mut result_field_type, &not_null_fields);
        }

        // If any of arg is NULL, result type need unset NotNullFlag.
        if !null_fields.is_empty() {
            let mut temp_flag = result_field_type.get_flag();
            set_type_flag(&mut temp_flag, NotNullFlag, false);
            result_field_type.set_flag(temp_flag);
        }

        let result_eval_type = result_field_type.eval_type();
        // fix decimal for int and string.
        if result_eval_type == ETInt {
            result_field_type.set_decimal(0);
        } else if result_eval_type == ETString {
            result_field_type.set_decimal(UnspecifiedLength);
        }
        // fix type for enum and set
        if result_field_type.get_type() == mysql::TypeEnum
            || result_field_type.get_type() == mysql::TypeSet
        {
            match result_eval_type {
                ETInt => result_field_type.set_type(mysql::TypeLonglong),
                ETString => result_field_type.set_type(mysql::TypeVarchar),
                _ => {}
            }
        }
        // fix flen for datetime
        try_to_fix_flen_of_datetime(&mut result_field_type);
    }
    Ok(result_field_type)
}
