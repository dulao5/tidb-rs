//! go-file: pkg/expression/collation.go
//!
//! Block D08.2 owns this file: the `collationInfo` embedded in every node, the
//! `Coercibility`/`Repertoire` types, and the collation derivation of built-in functions
//! (`deriveCollation`, `CheckAndDeriveCollationFromExprs`, `inferCollation`).
//!
//! `collationInfo.Hash64`/`Equals` implement cascades' `base.HashEquals` and are not ported
//! (EXC.B, like `Column.Hash64`).
//!
//! `coer`/`coerInit` are atomics in Go because `Coercibility()` initialises them lazily on
//! nodes that are shared between plans; they are atomics here too, so `&self` suffices.

#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use tidb_base::gofmt::Arg;
use tidb_base::{Error, mysql};
use tidb_chunk::Row;
use tidb_parser::gocharset::{
    self, CharsetASCII, CharsetBin, CharsetGBK, CharsetUTF8, CharsetUTF8MB4, CollationASCII,
    CollationBin, CollationGBKBin, CollationLatin1, CollationUTF8, CollationUTF8MB4,
    CollationUTF8MB40900Bin,
};
use tidb_parser::types::{ETInt, ETJson, ETString, EvalType};
use tidb_types::collate;
use tidb_types::etc::{FieldType, is_binary_str, is_type_bit};

use crate::ast_funcs as ast;
use crate::builtin_registry::get_display_name;
use crate::column::Column;
use crate::constant::Constant;
use crate::context::{BuildContext, EvalContext};
use crate::expression::{EvalState, Expr, ExprRef};
use crate::scalar_function::ScalarFunction;

/// ExprCollation is a struct that store the collation related information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExprCollation {
    /// Coer.
    pub coer: Coercibility,
    /// Repe.
    pub repe: Repertoire,
    /// Charset.
    pub charset: String,
    /// Collation.
    pub collation: String,
}

/// Coercibility values are used to check whether the collation of one item can be coerced
/// to the collation of other. See https://dev.mysql.com/doc/refman/8.0/en/charset-collation-coercibility.html
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coercibility(pub i32);

/// CoercibilityExplicit is derived from an explicit COLLATE clause.
pub const CoercibilityExplicit: Coercibility = Coercibility(0);
/// CoercibilityNone is derived from the concatenation of two strings with different collations.
pub const CoercibilityNone: Coercibility = Coercibility(1);
/// CoercibilityImplicit is derived from a column or a stored routine parameter or local variable or cast() function.
pub const CoercibilityImplicit: Coercibility = Coercibility(2);
/// CoercibilitySysconst is derived from a "system constant" (the string returned by functions such as USER() or VERSION()).
pub const CoercibilitySysconst: Coercibility = Coercibility(3);
/// CoercibilityCoercible is derived from a literal.
pub const CoercibilityCoercible: Coercibility = Coercibility(4);
/// CoercibilityNumeric is derived from a numeric or temporal value.
pub const CoercibilityNumeric: Coercibility = Coercibility(5);
/// CoercibilityIgnorable is derived from NULL or an expression that is derived from NULL.
pub const CoercibilityIgnorable: Coercibility = Coercibility(6);

/// Repertoire is the set of characters a string can contain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Repertoire(pub i32);

/// ASCII is pure ASCII U+0000..U+007F.
pub const ASCII: Repertoire = Repertoire(0x01);
/// EXTENDED is extended characters: U+0080..U+FFFF
pub const EXTENDED: Repertoire = Repertoire(0x01 << 1);
/// UNICODE is ASCII | EXTENDED
pub const UNICODE: Repertoire = Repertoire(0x01 | (0x01 << 1));

/// The `collationInfo` embedded in expression nodes.
#[derive(Debug, Default)]
pub struct CollationInfo {
    coer: AtomicI32,
    coer_init: AtomicBool,
    pub(crate) repertoire: Repertoire,

    pub(crate) charset: String,
    pub(crate) collation: String,

    is_explicit_charset: bool,
}

impl Clone for CollationInfo {
    // glue: Go copies the struct by value (the atomics are copied with it)
    fn clone(&self) -> Self {
        CollationInfo {
            coer: AtomicI32::new(self.coer.load(Ordering::Relaxed)),
            coer_init: AtomicBool::new(self.coer_init.load(Ordering::Relaxed)),
            repertoire: self.repertoire,
            charset: self.charset.clone(),
            collation: self.collation.clone(),
            is_explicit_charset: self.is_explicit_charset,
        }
    }
}

impl CollationInfo {
    // go: collationInfo.HasCoercibility L1
    pub fn has_coercibility(&self) -> bool {
        self.coer_init.load(Ordering::SeqCst)
    }

    // go: collationInfo.Coercibility L1
    pub fn coercibility(&self) -> Coercibility {
        Coercibility(self.coer.load(Ordering::SeqCst))
    }

    // go: collationInfo.SetCoercibility L1
    pub fn set_coercibility(&self, val: Coercibility) {
        self.coer.store(val.0, Ordering::SeqCst);
        self.coer_init.store(true, Ordering::SeqCst);
    }

    // go: collationInfo.Repertoire L1
    pub fn repertoire(&self) -> Repertoire {
        self.repertoire
    }

    // go: collationInfo.SetRepertoire L1
    pub fn set_repertoire(&mut self, r: Repertoire) {
        self.repertoire = r;
    }

    // go: collationInfo.SetCharsetAndCollation L1
    pub fn set_charset_and_collation(&mut self, chs: &str, coll: &str) {
        self.charset = chs.to_string();
        self.collation = coll.to_string();
    }

    // go: collationInfo.CharsetAndCollation L1
    pub fn charset_and_collation(&self) -> (&str, &str) {
        (&self.charset, &self.collation)
    }

    // go: collationInfo.IsExplicitCharset L1
    pub fn is_explicit_charset(&self) -> bool {
        self.is_explicit_charset
    }

    // go: collationInfo.SetExplicitCharset L1
    pub fn set_explicit_charset(&mut self, explicit: bool) {
        self.is_explicit_charset = explicit;
    }

    /// Byte size of the two strings (used by `MemoryUsage`).
    // glue: len(col.charset)+len(col.collation) in Column.MemoryUsage
    pub(crate) fn strings_len(&self) -> i64 {
        (self.charset.len() + self.collation.len()) as i64
    }
}

/// deriveCoercibilityForScalarFunc: never called in Go.
// go: deriveCoercibilityForScalarFunc L1
pub(crate) fn derive_coercibility_for_scalar_func(_sf: &ScalarFunction) -> Coercibility {
    panic!("this function should never be called")
}

// go: deriveCoercibilityForConstant L1
pub(crate) fn derive_coercibility_for_constant(c: &Constant) -> Coercibility {
    if c.value.is_null() {
        return CoercibilityIgnorable;
    } else if c.ret_type.eval_type() != ETString {
        return CoercibilityNumeric;
    }
    CoercibilityCoercible
}

// go: deriveCoercibilityForColumn L1
pub(crate) fn derive_coercibility_for_column(c: &Column) -> Coercibility {
    let rt: &FieldType = &c.ret_type;
    // For specified type null, it should return CoercibilityIgnorable, which means it got the lowest priority in DeriveCollationFromExprs.
    if rt.get_type() == mysql::TypeNull {
        return CoercibilityIgnorable;
    } else if is_type_bit(rt) {
        return CoercibilityImplicit;
    }

    let et = rt.eval_type();
    if et == ETJson {
    } else if et == ETString {
        return CoercibilityImplicit;
    } else {
        return CoercibilityNumeric;
    }
    CoercibilityImplicit
}

/// The default when no case of `deriveCollation` applies.
// glue: tail of deriveCollation
fn derive_collation_default(ctx: &dyn BuildContext, ret_type: EvalType) -> ExprCollation {
    let mut ec = ExprCollation {
        coer: CoercibilityNumeric,
        repe: ASCII,
        charset: CharsetBin.to_string(),
        collation: CollationBin.to_string(),
    };
    if ret_type == ETString {
        (ec.charset, ec.collation) = ctx.get_charset_info();
        ec.coer = CoercibilityCoercible;
        if ec.charset != CharsetASCII {
            ec.repe = UNICODE;
        }
    }
    ec
}

/// The collation of a built-in function's result: which arguments take part depends on the
/// function. `args[i]` panics where Go indexes out of range.
// go: deriveCollation L1
pub fn derive_collation(
    ctx: &dyn BuildContext,
    func_name: &str,
    args: &[ExprRef],
    ret_type: EvalType,
    arg_tps: &[EvalType],
) -> Result<ExprCollation, Error> {
    let d = |a: &[&ExprRef], et: EvalType| -> Result<ExprCollation, Error> {
        let v: Vec<ExprRef> = a.iter().map(|e| ExprRef::clone(e)).collect();
        check_and_derive_collation_from_exprs(ctx, func_name, et, &v)
    };
    match func_name {
        ast::Concat
        | ast::ConcatWS
        | ast::Lower
        | ast::Lcase
        | ast::Reverse
        | ast::Upper
        | ast::Ucase
        | ast::Quote
        | ast::Coalesce
        | ast::Greatest
        | ast::Least => {
            return check_and_derive_collation_from_exprs(ctx, func_name, ret_type, args);
        }
        ast::Left
        | ast::Right
        | ast::Repeat
        | ast::Trim
        | ast::LTrim
        | ast::RTrim
        | ast::Substr
        | ast::SubstringIndex
        | ast::Replace
        | ast::Substring
        | ast::Mid
        | ast::Translate => return d(&[&args[0]], ret_type),
        ast::InsertFunc => return d(&[&args[0], &args[3]], ret_type),
        ast::Lpad | ast::Rpad => return d(&[&args[0], &args[2]], ret_type),
        ast::Elt | ast::ExportSet | ast::MakeSet => {
            return check_and_derive_collation_from_exprs(ctx, func_name, ret_type, &args[1..]);
        }
        ast::FindInSet | ast::Regexp => {
            return check_and_derive_collation_from_exprs(ctx, func_name, ETInt, args);
        }
        ast::Field => {
            if arg_tps[0] == ETString {
                return check_and_derive_collation_from_exprs(ctx, func_name, ret_type, args);
            }
        }
        ast::RegexpReplace => return d(&[&args[0], &args[1], &args[2]], ret_type),
        ast::Locate
        | ast::Instr
        | ast::Position
        | ast::RegexpLike
        | ast::RegexpSubstr
        | ast::RegexpInStr => return d(&[&args[0], &args[1]], ret_type),
        ast::GE | ast::LE | ast::GT | ast::LT | ast::EQ | ast::NE | ast::NullEQ | ast::Strcmp => {
            // if compare type is string, we should determine which collation should be used.
            if arg_tps[0] == ETString {
                let mut ec = check_and_derive_collation_from_exprs(ctx, func_name, ETInt, args)?;
                ec.coer = CoercibilityNumeric;
                ec.repe = ASCII;
                return Ok(ec);
            }
        }
        ast::If => return d(&[&args[1], &args[2]], ret_type),
        ast::Ifnull => return d(&[&args[0], &args[1]], ret_type),
        ast::Like | ast::Ilike => {
            let mut ec = d(&[&args[0], &args[1]], ETInt)?;
            ec.coer = CoercibilityNumeric;
            ec.repe = ASCII;
            return Ok(ec);
        }
        ast::In => {
            if args[0].get_type(&*ctx.get_eval_ctx()).eval_type() == ETString {
                return check_and_derive_collation_from_exprs(ctx, func_name, ETInt, args);
            }
        }
        ast::DateFormat | ast::TimeFormat => {
            let (charset_info, collation) = ctx.get_charset_info();
            return Ok(ExprCollation {
                coer: args[1].coercibility(),
                repe: args[1].repertoire(),
                charset: charset_info,
                collation,
            });
        }
        ast::Cast => {
            // We assume all the cast are implicit.
            let ectx = ctx.get_eval_ctx();
            let tp = args[0].get_type(&*ectx);
            let mut ec = ExprCollation {
                coer: args[0].coercibility(),
                repe: args[0].repertoire(),
                charset: tp.get_charset().to_string(),
                collation: tp.get_collate().to_string(),
            };
            // Cast to string type should use @@character_set_connection and @@collation_connection.
            if ret_type == ETString {
                (ec.charset, ec.collation) = ctx.get_charset_info();
            }
            return Ok(ec);
        }
        ast::Case => {
            // FIXME: case function aggregate collation is not correct (Go's comment).
            // Arguments with odd index are the `then expression`.
            if arg_tps[1] == ETString {
                let mut field_args = Vec::new();
                let mut i = 1;
                while i < args.len() {
                    field_args.push(ExprRef::clone(&args[i]));
                    i += 2;
                }
                if args.len() % 2 == 1 {
                    field_args.push(ExprRef::clone(&args[args.len() - 1]));
                }
                return check_and_derive_collation_from_exprs(
                    ctx,
                    func_name,
                    ret_type,
                    &field_args,
                );
            }
        }
        ast::Database
        | ast::User
        | ast::CurrentUser
        | ast::Version
        | ast::CurrentRole
        | ast::TiDBVersion
        | ast::CurrentResourceGroup => {
            let (chs, coll) = gocharset::get_default_charset_and_collate();
            return Ok(ExprCollation {
                coer: CoercibilitySysconst,
                repe: UNICODE,
                charset: chs.to_string(),
                collation: coll.to_string(),
            });
        }
        ast::Format
        | ast::Space
        | ast::ToBase64
        | ast::UUID
        | ast::Hex
        | ast::MD5
        | ast::SHA
        | ast::SHA2
        | ast::SM3 => {
            // should return ASCII repertoire, MySQL's doc says it depends on character_set_connection, but it not true from its source code.
            let (charset, collation) = ctx.get_charset_info();
            return Ok(ExprCollation {
                coer: CoercibilityCoercible,
                repe: ASCII,
                charset,
                collation,
            });
        }
        ast::JSONPretty | ast::JSONQuote => {
            // JSON function always return utf8mb4 and utf8mb4_bin.
            return Ok(ExprCollation {
                coer: CoercibilityCoercible,
                repe: UNICODE,
                charset: CharsetUTF8MB4.to_string(),
                collation: CollationUTF8MB4.to_string(),
            });
        }
        _ => {}
    }
    Ok(derive_collation_default(ctx, ret_type))
}

/// CheckAndDeriveCollationFromExprs derives collation information from these expressions,
/// return error if derives collation error.
// go: CheckAndDeriveCollationFromExprs L1
pub fn check_and_derive_collation_from_exprs(
    ctx: &dyn BuildContext,
    func_name: &str,
    eval_type: EvalType,
    args: &[ExprRef],
) -> Result<ExprCollation, Error> {
    let ectx = ctx.get_eval_ctx();
    let Some(mut ec) = infer_collation(&*ectx, args) else {
        return Err(illegal_mix_collation_err(&*ectx, func_name, args));
    };

    if eval_type != ETString && ec.coer == CoercibilityNone {
        return Err(illegal_mix_collation_err(&*ectx, func_name, args));
    }

    if eval_type == ETString && ec.coer == CoercibilityNumeric {
        (ec.charset, ec.collation) = ctx.get_charset_info();
        ec.coer = CoercibilityCoercible;
        ec.repe = ASCII;
    }

    if !safe_convert(ctx, &ec, args) {
        return Err(illegal_mix_collation_err(&*ectx, func_name, args));
    }

    Ok(fix_string_type_for_max_length(&*ectx, func_name, args, ec))
}

/// fixStringTypeForMaxLength changes the collation of a string function to the binary one
/// when an argument is JSON (pingcap/tidb#52833); `args` are the arguments
/// `deriveCollation` passed, not all arguments of the function.
// go: fixStringTypeForMaxLength L1
fn fix_string_type_for_max_length(
    ctx: &dyn EvalContext,
    func_name: &str,
    args: &[ExprRef],
    mut ec: ExprCollation,
) -> ExprCollation {
    let is_json = |e: &ExprRef| e.get_type(ctx).eval_type() == ETJson;
    let mut should_change_to_bin = false;

    match func_name {
        ast::Reverse
        | ast::Lower
        | ast::Upper
        | ast::SubstringIndex
        | ast::Trim
        | ast::Quote
        | ast::InsertFunc
        | ast::Substr
        | ast::Repeat
        | ast::Replace => {
            should_change_to_bin = is_json(&args[0]);
        }
        ast::Concat | ast::ConcatWS | ast::Elt | ast::MakeSet => {
            should_change_to_bin = args.iter().any(is_json);
        }
        ast::ExportSet => {
            if args.len() >= 2 {
                should_change_to_bin = is_json(&args[0]) || is_json(&args[1]);
            }
            if args.len() >= 3 {
                should_change_to_bin = should_change_to_bin || is_json(&args[2]);
            }
        }
        _ => {}
    }

    if should_change_to_bin {
        ec.collation = collate::convert_and_get_bin_collation(&ec.collation).to_string();
    }
    ec
}

// go: safeConvert L1
fn safe_convert(ctx: &dyn BuildContext, ec: &ExprCollation, args: &[ExprRef]) -> bool {
    let enc = gocharset::encoding::find_encoding_take_utf8_as_noop(&ec.charset);
    let ectx = ctx.get_eval_ctx();
    for arg in args {
        let tp = arg.get_type(&*ectx);
        if tp.get_charset() == ec.charset {
            continue;
        }

        // If value has ASCII repertoire, or it is binary string, just skip it.
        if arg.repertoire() == ASCII || is_binary_str(&tp) {
            continue;
        }

        if let Expr::Constant(c) = &**arg {
            match c.eval_string(&*ectx, &EvalState::new(), Row::default()) {
                Err(_) => return false,
                Ok(None) => continue,
                Ok(Some(s)) => {
                    if !enc.is_valid(&s) {
                        return false;
                    }
                }
            }
        } else if tp.get_collate() != CharsetBin
            && ec.charset != CharsetBin
            && !is_unicode_collation(&ec.charset)
        {
            return false;
        }
    }

    true
}

/// The charset and collation an argument takes part in the aggregation with: JSON is
/// utf8mb4_bin, BIT is binary.
// glue: repeated prologue of inferCollation's loop
fn infer_arg_charset(ctx: &dyn EvalContext, e: &Expr) -> (String, String) {
    let tp = e.get_type(ctx);
    if tp.eval_type() == ETJson {
        (CharsetUTF8MB4.to_string(), CollationUTF8MB4.to_string())
    } else if is_type_bit(&tp) {
        (CharsetBin.to_string(), CollationBin.to_string())
    } else {
        (tp.get_charset().to_string(), tp.get_collate().to_string())
    }
}

/// inferCollation infers collation, charset, coercibility and check the legitimacy.
// go: inferCollation L1
#[allow(clippy::if_same_then_else)] // Go's branches, in Go's order
pub(crate) fn infer_collation(ctx: &dyn EvalContext, exprs: &[ExprRef]) -> Option<ExprCollation> {
    if exprs.is_empty() {
        // TODO: see if any function with no arguments could run here.
        let (dst_charset, dst_collation) = gocharset::get_default_charset_and_collate();
        return Some(ExprCollation {
            coer: CoercibilityIgnorable,
            repe: UNICODE,
            charset: dst_charset.to_string(),
            collation: dst_collation.to_string(),
        });
    }

    let mut repertoire = exprs[0].repertoire();
    let mut coercibility = exprs[0].coercibility();
    let (mut dst_charset, mut dst_collation) = infer_arg_charset(ctx, &exprs[0]);
    let mut unknown_cs = false;

    // Aggregate arguments one by one, agg(a, b, c) := agg(agg(a, b), c).
    for arg in &exprs[1..] {
        let (arg_charset, arg_collation) = infer_arg_charset(ctx, arg);
        let (arg_coer, arg_repe) = (arg.coercibility(), arg.repertoire());
        // If one of the arguments is binary charset, we allow it can be used with other charsets.
        // If they have the same coercibility, let the binary charset one to be the winner because binary has more precedence.
        if dst_collation == CollationBin || arg_collation == CollationBin {
            if coercibility > arg_coer
                || (coercibility == arg_coer && arg_collation == CollationBin)
            {
                (coercibility, dst_charset, dst_collation) = (arg_coer, arg_charset, arg_collation);
            }
            repertoire = Repertoire(repertoire.0 | arg_repe.0);
            continue;
        }

        // If charset is different, only if conversion without data loss is allowed (see Go).
        if dst_charset != arg_charset {
            if coercibility < arg_coer {
                if arg_repe == ASCII
                    || arg_coer >= CoercibilitySysconst
                    || is_unicode_collation(&dst_charset)
                {
                    repertoire = Repertoire(repertoire.0 | arg_repe.0);
                    continue;
                }
            } else if coercibility == arg_coer {
                if (is_unicode_collation(&dst_charset) && !is_unicode_collation(&arg_charset))
                    || (dst_charset == CharsetUTF8MB4 && arg_charset == CharsetUTF8)
                {
                    repertoire = Repertoire(repertoire.0 | arg_repe.0);
                    continue;
                } else if (is_unicode_collation(&arg_charset)
                    && !is_unicode_collation(&dst_charset))
                    || (arg_charset == CharsetUTF8MB4 && dst_charset == CharsetUTF8)
                {
                    (coercibility, dst_charset, dst_collation) =
                        (arg_coer, arg_charset, arg_collation);
                    repertoire = Repertoire(repertoire.0 | arg_repe.0);
                    continue;
                } else if repertoire == ASCII && arg_repe != ASCII {
                    (coercibility, dst_charset, dst_collation) =
                        (arg_coer, arg_charset, arg_collation);
                    repertoire = Repertoire(repertoire.0 | arg_repe.0);
                    continue;
                } else if repertoire != ASCII && arg_repe == ASCII {
                    repertoire = Repertoire(repertoire.0 | arg_repe.0);
                    continue;
                }
            } else if repertoire == ASCII
                || coercibility >= CoercibilitySysconst
                || is_unicode_collation(&arg_charset)
            {
                (coercibility, dst_charset, dst_collation) = (arg_coer, arg_charset, arg_collation);
                repertoire = Repertoire(repertoire.0 | arg_repe.0);
                continue;
            }

            // Cannot apply conversion.
            repertoire = Repertoire(repertoire.0 | arg_repe.0);
            (coercibility, dst_charset, dst_collation) = (
                CoercibilityNone,
                CharsetBin.to_string(),
                CollationBin.to_string(),
            );
            unknown_cs = true;
        } else {
            // If charset is the same, use lower coercibility, if coercibility is the same and none of them are _bin,
            // derive to CoercibilityNone and _bin collation.
            if coercibility == arg_coer {
                if dst_collation == arg_collation {
                } else if coercibility == CoercibilityExplicit {
                    return None;
                } else if is_bin_collation(&dst_collation) {
                } else if is_bin_collation(&arg_collation) {
                    (coercibility, dst_charset, dst_collation) =
                        (arg_coer, arg_charset, arg_collation);
                } else {
                    let bin = get_bin_collation(&arg_charset).to_string();
                    (coercibility, dst_charset, dst_collation) =
                        (CoercibilityNone, arg_charset, bin);
                }
            } else if coercibility > arg_coer {
                (coercibility, dst_charset, dst_collation) = (arg_coer, arg_charset, arg_collation);
            }
            repertoire = Repertoire(repertoire.0 | arg_repe.0);
        }
    }

    if unknown_cs && coercibility != CoercibilityExplicit {
        return None;
    }

    Some(ExprCollation {
        coer: coercibility,
        repe: repertoire,
        charset: dst_charset,
        collation: dst_collation,
    })
}

// go: isUnicodeCollation L1
fn is_unicode_collation(ch: &str) -> bool {
    ch == CharsetUTF8 || ch == CharsetUTF8MB4
}

/// isBinCollation checks whether the collation has _bin semantics for coercibility
/// derivation (not the same as `collate.IsBinCollation`, see Go's comment).
// go: isBinCollation L1
fn is_bin_collation(collate: &str) -> bool {
    collate == CollationASCII
        || collate == CollationLatin1
        || collate == CollationUTF8
        || collate == CollationUTF8MB4
        || collate == CollationGBKBin
        || collate == CollationUTF8MB40900Bin
}

/// getBinCollation get binary collation by charset. Go logs "unexpected charset" for other
/// charsets (unreachable) and returns utf8mb4_bin.
// go: getBinCollation L2 (the unreachable error log is not written)
fn get_bin_collation(cs: &str) -> &'static str {
    match cs {
        CharsetUTF8 => CollationUTF8,
        CharsetUTF8MB4 => CollationUTF8MB4,
        CharsetGBK => CollationGBKBin,
        // it must return something, never reachable
        _ => CollationUTF8MB4,
    }
}

static coerString: [&str; 7] = [
    "EXPLICIT",
    "NONE",
    "IMPLICIT",
    "SYSCONST",
    "COERCIBLE",
    "NUMERIC",
    "IGNORABLE",
];

// go: illegalMixCollationErr L1
fn illegal_mix_collation_err(ctx: &dyn EvalContext, func_name: &str, args: &[ExprRef]) -> Error {
    let func_name = get_display_name(func_name);
    let coll = |e: &ExprRef| Arg::from(e.get_type(ctx).get_collate().to_string());
    let coer = |e: &ExprRef| Arg::from(coerString[e.coercibility().0 as usize].to_string());

    match args.len() {
        2 => collate::ErrIllegalMix2Collation.fast_gen_by_args(vec![
            coll(&args[0]),
            coer(&args[0]),
            coll(&args[1]),
            coer(&args[1]),
            Arg::from(func_name.to_string()),
        ]),
        3 => collate::ErrIllegalMix3Collation.fast_gen_by_args(vec![
            coll(&args[0]),
            coer(&args[0]),
            coll(&args[1]),
            coer(&args[1]),
            coll(&args[2]),
            coer(&args[2]),
            Arg::from(func_name.to_string()),
        ]),
        _ => {
            collate::ErrIllegalMixCollation.fast_gen_by_args(vec![Arg::from(func_name.to_string())])
        }
    }
}
