//! go-file: pkg/expression/builtin_convert_charset.go
//!
//! The internal functions `to_binary` and `from_binary` and `HandleBinaryLiteral`, which
//! wraps a string argument in one of them when a function (CAST among them) converts between
//! a binary string and a character set (block D08.8; moved from D08.6 because CAST to STRING
//! needs it, spec `specs/D08.8-cast` §1).

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassExpression;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::TypeNull;
use tidb_parser::gocharset::encoding::{OpDecode, OpEncode, find_encoding};
use tidb_parser::gocharset::{
    CharsetASCII, CharsetBin, CharsetLatin1, CharsetUTF8, CharsetUTF8MB4, CollationBin,
};
use tidb_parser::types::{ETString, EvalType};
use tidb_types::etc::FieldType;

use crate::ast_funcs as ast;
use crate::builtin::{BaseFunctionClass, new_base_builtin_func_with_tp};
use crate::collation::ExprCollation;
use crate::constant_fold::fold_constant;
use crate::context::{BuildContext, EvalContext, sql_mode, type_ctx};
use crate::eval::{BuiltinFunc, SigDesc, SigView};
use crate::expression::{Expr, ExprRef};
use crate::scalar_function::ScalarFunction;
use tidb_expression_macros::eval_fn;

/// errCannotConvertString returns when the string can not convert to other charset.
pub static errCannotConvertString: LazyLock<Error> =
    LazyLock::new(|| ClassExpression.new_std(errno::ErrCannotConvertString));

/// InternalFuncToBinary accepts a string and returns another string encoded in a given
/// charset.
pub const InternalFuncToBinary: &str = "to_binary";

/// InternalFuncFromBinary accepts a string and returns another string decode in a given
/// charset.
pub const InternalFuncFromBinary: &str = "from_binary";

// tipb.ScalarFuncSig (go-tipb expression.pb.go).
const ToBinary: i32 = 7071;
const FromBinary: i32 = 7072;

/// `builtinInternalToBinarySig`.
pub static builtinInternalToBinarySig: SigDesc = SigDesc {
    name: "builtinInternalToBinarySig",
    pb_code: ToBinary,
    vectorized: true,
    drivers: INTERNAL_TO_BINARY,
    args_fallback: crate::eval::ArgsFallback::None,
};

/// `builtinInternalFromBinarySig`.
pub static builtinInternalFromBinarySig: SigDesc = SigDesc {
    name: "builtinInternalFromBinarySig",
    pb_code: FromBinary,
    vectorized: true,
    drivers: INTERNAL_FROM_BINARY,
    args_fallback: crate::eval::ArgsFallback::None,
};

/// `fmt.Errorf("unexpected argTp: %d", argTp)`.
// glue: the default branch of the to_binary/from_binary function classes
fn unexpected_arg_tp(arg_tp: EvalType) -> Error {
    Error::plain(format!("unexpected argTp: {}", arg_tp.0))
}

/// `tidbToBinaryFunctionClass`.
#[derive(Clone, Debug)]
pub struct TidbToBinaryFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
}

impl TidbToBinaryFunctionClass {
    // go: tidbToBinaryFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let arg_tp = args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        if arg_tp != ETString {
            return Err(unexpected_arg_tp(arg_tp));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETString, &[ETString])?;
        let mut tp = bf.args[0].get_type(&*ctx.get_eval_ctx()).into_owned();
        tp.set_type(tidb_base::mysql::TypeVarString);
        tp.set_charset(CharsetBin);
        tp.set_collate(CollationBin);
        bf.tp = tp;
        Ok(bf.with_sig(&builtinInternalToBinarySig))
    }
}

// go: builtinInternalToBinarySig.evalString L1
// go: builtinInternalToBinarySig.vecEvalString L2 (the vector driver of this body, M-EVAL)
// go: builtinInternalToBinarySig.vectorized L2 (SigDesc.vectorized)
// go: builtinInternalToBinarySig.Clone L2 (BuiltinFunc::clone_from_base)
#[eval_fn(String)]
pub fn internal_to_binary(
    _ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let enc = find_encoding(f.arg_type(0).get_charset());
    let (ret, err) = enc.transform(val, OpEncode);
    err?;
    out.extend_from_slice(&ret);
    Ok(Some(()))
}

/// `tidbFromBinaryFunctionClass`.
#[derive(Clone, Debug)]
pub struct TidbFromBinaryFunctionClass {
    /// The embedded `baseFunctionClass`.
    pub base: BaseFunctionClass,
    /// `tp`.
    pub tp: FieldType,
    /// `cannotConvertStringAsWarning`.
    pub cannot_convert_string_as_warning: bool,
}

impl TidbFromBinaryFunctionClass {
    // go: tidbFromBinaryFunctionClass.getFunction L1
    pub fn get_function(
        &self,
        ctx: &dyn BuildContext,
        args: Vec<ExprRef>,
    ) -> Result<BuiltinFunc, Error> {
        self.base.verify_args(&args)?;
        let arg_tp = args[0].get_type(&*ctx.get_eval_ctx()).eval_type();
        if arg_tp != ETString {
            return Err(unexpected_arg_tp(arg_tp));
        }
        let mut bf =
            new_base_builtin_func_with_tp(ctx, self.base.func_name, args, ETString, &[ETString])?;
        bf.tp = self.tp.clone();
        bf.extra = crate::eval::SigExtra::FromBinary {
            cannot_convert_string_as_warning: self.cannot_convert_string_as_warning,
        };
        Ok(bf.with_sig(&builtinInternalFromBinarySig))
    }
}

/// When the conversion fails and `cannotConvertStringAsWarning` is set outside strict mode,
/// Go's row version returns what `Transform` produced and its vector version the input
/// bytes: each driver follows its own Go version (D08.1 spec §3.4 (c); spec §4).
// go: builtinInternalFromBinarySig.evalString L1
// go: builtinInternalFromBinarySig.vecEvalString L2 (the vector driver of this body; a non-strict conversion warning keeps the input bytes, as in Go)
// go: builtinInternalFromBinarySig.vectorized L2 (SigDesc.vectorized)
// go: builtinInternalFromBinarySig.Clone L2 (BuiltinFunc::clone_from_base copies SigExtra::FromBinary, as Go)
#[eval_fn(String)]
pub fn internal_from_binary(
    ctx: &dyn EvalContext,
    f: &impl SigView,
    val: &[u8],
    out: &mut Vec<u8>,
) -> Result<Option<()>, Error> {
    let enc = find_encoding(f.tp.get_charset());
    let (ret, err) = enc.transform(val, OpDecode);
    if err.is_err() {
        let str_hex = format_invalid_chars(val);
        let err = errCannotConvertString.gen_with_stack_by_args(vec![
            Arg::Str(str_hex.into_bytes().into()),
            Arg::from(CharsetBin),
            Arg::from(f.tp.get_charset().to_string()),
        ]);

        if f.cannot_convert_string_as_warning() {
            let tc = type_ctx(ctx);
            tc.append_warning(err);
            if sql_mode(ctx).has_strict_mode() {
                return Ok(None);
            }
            if f.vector() {
                out.extend_from_slice(val);
            } else {
                out.extend_from_slice(&ret);
            }
            return Ok(Some(()));
        }

        return Err(err);
    }
    out.extend_from_slice(&ret);
    Ok(Some(()))
}

/// BuildToBinaryFunction builds to_binary function.
// go: BuildToBinaryFunction L1
pub fn build_to_binary_function(ctx: &dyn BuildContext, expr: ExprRef) -> ExprRef {
    let fc = TidbToBinaryFunctionClass {
        base: BaseFunctionClass {
            func_name: InternalFuncToBinary,
            min_args: 1,
            max_args: 1,
        },
    };
    let mut f = match fc.get_function(ctx, vec![ExprRef::clone(&expr)]) {
        Ok(f) => f,
        Err(_) => return expr,
    };
    let ret_type = f.get_ret_tp().clone();
    let res =
        Expr::ScalarFunction(ScalarFunction::new(InternalFuncToBinary, ret_type, f)).into_ref();
    fold_constant(ctx, res)
}

/// BuildFromBinaryFunction builds from_binary function.
// go: BuildFromBinaryFunction L1
pub fn build_from_binary_function(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    tp: FieldType,
    cannot_convert_string_as_warning: bool,
) -> ExprRef {
    let fc = TidbFromBinaryFunctionClass {
        base: BaseFunctionClass {
            func_name: InternalFuncFromBinary,
            min_args: 1,
            max_args: 1,
        },
        tp: tp.clone(),
        cannot_convert_string_as_warning,
    };
    let f = match fc.get_function(ctx, vec![ExprRef::clone(&expr)]) {
        Ok(f) => f,
        Err(_) => return expr,
    };
    let res = Expr::ScalarFunction(ScalarFunction::new(InternalFuncFromBinary, tp, f)).into_ref();
    fold_constant(ctx, res)
}

/// `funcProp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FuncProp {
    /// funcPropNone (the zero value: a function not in the map).
    None,
    /// The arguments of these functions are wrapped with to_binary(). For compatibility
    /// reason, legacy charsets arguments are not wrapped. Legacy charsets: utf8mb4, utf8,
    /// latin1, ascii, binary.
    BinAware,
    /// The arguments of these functions are wrapped with to_binary() or from_binary()
    /// according to the evaluated result charset and the argument charset. For binary
    /// argument && string result, wrap it with from_binary(). For string argument && binary
    /// result, wrap it with to_binary().
    Auto,
}

/// convertActionMap collects from https://dev.mysql.com/doc/refman/8.0/en/string-functions.html.
static convertActionMap: &[(FuncProp, &[&str])] = &[
    (
        FuncProp::None,
        &[
            /* args != strings */
            ast::Bin,
            ast::CharFunc,
            ast::DateFormat,
            ast::Oct,
            ast::Space,
            /* only 1 string arg, no implicit conversion */
            ast::CharLength,
            ast::CharacterLength,
            ast::FromBase64,
            ast::Lcase,
            ast::Left,
            ast::LoadFile,
            ast::Lower,
            ast::LTrim,
            ast::Mid,
            ast::Ord,
            ast::Quote,
            ast::Repeat,
            ast::Reverse,
            ast::Right,
            ast::RTrim,
            ast::Soundex,
            ast::Substr,
            ast::Substring,
            ast::Ucase,
            ast::Unhex,
            ast::Upper,
            ast::WeightString,
        ],
    ),
    (
        FuncProp::BinAware,
        &[
            /* result is binary-aware */
            ast::ASCII,
            ast::BitLength,
            ast::Hex,
            ast::Length,
            ast::OctetLength,
            ast::ToBase64,
            /* encrypt functions */
            ast::AesDecrypt,
            ast::Decode,
            ast::Encode,
            ast::PasswordFunc,
            ast::MD5,
            ast::SHA,
            ast::SHA1,
            ast::SHA2,
            ast::SM3,
            ast::Compress,
            ast::AesEncrypt,
        ],
    ),
    (
        FuncProp::Auto,
        &[
            /* string functions */ ast::Concat,
            ast::ConcatWS,
            ast::ExportSet,
            ast::Field,
            ast::FindInSet,
            ast::InsertFunc,
            ast::Instr,
            ast::Lpad,
            ast::Locate,
            ast::Lpad,
            ast::MakeSet,
            ast::Position,
            ast::Replace,
            ast::Rpad,
            ast::SubstringIndex,
            ast::Trim,
            ast::Elt,
            /* operators */
            ast::GE,
            ast::LE,
            ast::GT,
            ast::LT,
            ast::EQ,
            ast::NE,
            ast::NullEQ,
            ast::If,
            ast::Ifnull,
            ast::In,
            ast::Case,
            ast::Cast,
            /* string comparing */
            ast::Like,
            ast::Ilike,
            ast::Strcmp,
            /* regex */
            ast::Regexp,
            ast::RegexpLike,
            ast::RegexpInStr,
            ast::RegexpSubstr,
            ast::RegexpReplace,
            /* math */
            ast::CRC32,
        ],
    ),
];

/// `convertFuncsMap`, filled by `init` from `convertActionMap` (no name is in two lists, so
/// Go's map iteration order does not matter).
// go: init L1
static convertFuncsMap: LazyLock<HashMap<&'static str, FuncProp>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for (k, fns) in convertActionMap {
        for f in *fns {
            m.insert(*f, *k);
        }
    }
    m
});

/// HandleBinaryLiteral wraps `expr` with to_binary or from_binary sig.
// go: HandleBinaryLiteral L1
pub fn handle_binary_literal(
    ctx: &dyn BuildContext,
    expr: ExprRef,
    ec: &ExprCollation,
    func_name: &str,
    explicit_cast: bool,
) -> ExprRef {
    let ectx = ctx.get_eval_ctx();
    let arg_tp = expr.get_type(&*ectx).into_owned();
    let (arg_chs, dst_chs) = (arg_tp.get_charset(), ec.charset.as_str());
    match convertFuncsMap
        .get(func_name)
        .copied()
        .unwrap_or(FuncProp::None)
    {
        FuncProp::None => return expr,
        FuncProp::BinAware => {
            if is_legacy_charset(arg_chs) {
                return expr;
            }
            drop(ectx);
            return build_to_binary_function(ctx, expr);
        }
        FuncProp::Auto => {
            if arg_chs != CharsetBin && dst_chs == CharsetBin {
                if is_legacy_charset(arg_chs) {
                    return expr;
                }
                drop(ectx);
                return build_to_binary_function(ctx, expr);
            } else if arg_chs == CharsetBin
                && dst_chs != CharsetBin
                && arg_tp.get_type() != TypeNull
            {
                let mut ft = arg_tp.clone();
                ft.set_charset(&ec.charset);
                ft.set_collate(&ec.collation);
                drop(ectx);
                return build_from_binary_function(ctx, expr, ft, explicit_cast);
            }
        }
    }
    expr
}

// go: isLegacyCharset L1
fn is_legacy_charset(chs: &str) -> bool {
    matches!(
        chs,
        CharsetUTF8 | CharsetUTF8MB4 | CharsetASCII | CharsetLatin1 | CharsetBin
    )
}

// go: formatInvalidChars L1
fn format_invalid_chars(src: &[u8]) -> String {
    let mut sb = String::new();
    const maxBytesToShow: usize = 5;
    for (i, &b) in src.iter().enumerate() {
        if i > maxBytesToShow {
            sb.push_str("...");
            break;
        }
        if b > 0x7f {
            sb.push_str(&format!("\\x{b:X}"));
        } else {
            sb.push(b as char);
        }
    }
    sb
}
