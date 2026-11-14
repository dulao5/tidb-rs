//! `#[eval_fn]`: one function body, two drivers (N4 §3.2, mapping rule M-EVAL).
//!
//! The body is the port of Go's row version (`evalXxx`). The attribute keeps the body and
//! generates, next to it:
//!
//! - `<name>__row(f, ctx, st, row)`: the row driver. Arguments are evaluated when the body
//!   needs them: a strict body gets them left to right, returning NULL at the first NULL,
//!   which is the fixed prologue of Go's row versions; a `lazy` body gets an [`Args`]
//!   accessor and evaluates arguments itself.
//! - `<name>__vec(f, ctx, n, cols, result)`: the vector driver. The arguments are already
//!   evaluated column by column (RPN order); the driver merges their null bitmaps into the
//!   result (fixed-length results) and calls the body for every row that is not NULL.
//! - `<NAME>: SigDrivers`, the pair as a constant for the signature's `SigDesc`.
//!
//! ```ignore
//! #[eval_fn(Int)]
//! fn plus_int(ctx: &dyn EvalContext, f: &impl SigView, a: i64, b: i64) -> Result<Option<i64>, Error> { .. }
//!
//! #[eval_fn(String, lazy)]
//! fn concat(ctx: &dyn EvalContext, f: &impl SigView, args: &mut impl Args, out: &mut Vec<u8>)
//!     -> Result<Option<()>, Error> { .. }
//! ```
//!
//! Result types: `Int` → `Option<i64>`, `Real` → `Option<f64>`, `Decimal` →
//! `Option<MyDecimal>`, `Time` → `Option<Time>`, `Duration` → `Option<Duration>` (the
//! column keeps the nanoseconds; the declared fsp of the result applies when it is read
//! back), `String` → `Option<()>` with the bytes appended to `out` (the vector driver reuses
//! one buffer for the whole chunk).
//!
//! `novec`: the signature has no vector version of its own (Go defines no `vecEvalXxx`
//! for it): the vector driver returns the `baseBuiltinFunc.vecEvalXxx` error. Whether the
//! executor may take the vector path at all is `SigDesc::vectorized`, a separate property
//! (D08.1 spec §3.5).
//! Strict argument types: `i64` (EvalInt), `f64` (EvalReal), `&MyDecimal` (EvalDecimal),
//! `&[u8]` (EvalString), `Time` (EvalTime), `Duration` (EvalDuration; the vector driver reads
//! it with the argument's declared fsp, as Go's vector versions do), `&BinaryJSON` (EvalJSON),
//! `&VectorFloat32` (EvalVectorFloat32).
//!
//! [`Args`]: ../tidb_expression/eval/trait.Args.html

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, Pat, Type};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ret {
    Int,
    Real,
    Decimal,
    String,
    Time,
    Duration,
    Json,
    Vector,
}

impl Ret {
    /// The `EvalType` constant of the result.
    fn eval_type(self) -> TokenStream2 {
        match self {
            Ret::Int => quote!(::tidb_expression::eval::rt::ETInt),
            Ret::Real => quote!(::tidb_expression::eval::rt::ETReal),
            Ret::Decimal => quote!(::tidb_expression::eval::rt::ETDecimal),
            Ret::String => quote!(::tidb_expression::eval::rt::ETString),
            Ret::Time => quote!(::tidb_expression::eval::rt::ETDatetime),
            Ret::Duration => quote!(::tidb_expression::eval::rt::ETDuration),
            Ret::Json => quote!(::tidb_expression::eval::rt::ETJson),
            Ret::Vector => quote!(::tidb_expression::eval::rt::ETVectorFloat32),
        }
    }
}

#[derive(Default)]
struct Attr {
    ret: Option<Ret>,
    lazy: bool,
    /// `eager`: the row driver evaluates every argument before looking at NULL (Go row
    /// versions such as `CompareInt` that evaluate both sides first).
    eager: bool,
    checked: Option<Checked>,
    /// `args(Int, String, ..)`: the evaluation types the vector driver reads the arguments of
    /// a `lazy` body as, where Go's vector version reads an argument as another type than its
    /// own without a CAST (D08.1 spec §4.1). The last type repeats for the extra arguments of a
    /// variadic signature. Without it a `lazy` body reads each argument as its own type.
    args: Option<Vec<ArgKind>>,
    /// `novec`: the signature has no vector version of its own (Go defines no `vecEvalXxx`
    /// for it, whatever its `vectorized()` says): the vector driver returns the
    /// `baseBuiltinFunc.vecEvalXxx` error (D08.1 spec §3.5).
    novec: bool,
}

/// `checked(nflags = N, flags = path, err = path)`: see [`expand_checked`].
struct Checked {
    nflags: u32,
    flags: syn::Path,
    err: syn::Path,
}

fn parse_attr(attr: TokenStream) -> syn::Result<Attr> {
    let mut a = Attr::default();
    let parser = syn::meta::parser(|meta| {
        let id = meta
            .path
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        match id.as_str() {
            "Int" => a.ret = Some(Ret::Int),
            "Real" => a.ret = Some(Ret::Real),
            "Decimal" => a.ret = Some(Ret::Decimal),
            "String" => a.ret = Some(Ret::String),
            "Time" => a.ret = Some(Ret::Time),
            "Duration" => a.ret = Some(Ret::Duration),
            "Json" => a.ret = Some(Ret::Json),
            "VectorFloat32" => a.ret = Some(Ret::Vector),
            "lazy" => a.lazy = true,
            "novec" => a.novec = true,
            "args" => {
                let mut v = Vec::new();
                meta.parse_nested_meta(|m| {
                    let k = match m.path.get_ident().map(|i| i.to_string()).as_deref() {
                        Some("Int") => ArgKind::Int,
                        Some("Real") => ArgKind::Real,
                        Some("Decimal") => ArgKind::Decimal,
                        Some("String") => ArgKind::Bytes,
                        Some("Time") => ArgKind::Time,
                        Some("Duration") => ArgKind::Duration,
                        Some("Json") => ArgKind::Json,
                        Some("VectorFloat32") => ArgKind::Vector,
                        _ => {
                            return Err(m.error(
                                "eval_fn: args(Int | Real | Decimal | String | Time | Duration | Json | VectorFloat32, ..)",
                            ));
                        }
                    };
                    v.push(k);
                    Ok(())
                })?;
                if v.is_empty() {
                    return Err(meta.error("eval_fn: args(..) needs at least one type"));
                }
                a.args = Some(v);
            }
            "eager" => a.eager = true,
            "checked" => {
                let (mut nflags, mut flags, mut err) = (0u32, None, None);
                meta.parse_nested_meta(|m| {
                    if m.path.is_ident("nflags") {
                        let lit: syn::LitInt = m.value()?.parse()?;
                        nflags = lit.base10_parse()?;
                    } else if m.path.is_ident("flags") {
                        flags = Some(m.value()?.parse::<syn::Path>()?);
                    } else if m.path.is_ident("err") {
                        err = Some(m.value()?.parse::<syn::Path>()?);
                    } else {
                        return Err(m.error("eval_fn: checked(nflags = N, flags = f, err = f)"));
                    }
                    Ok(())
                })?;
                let (Some(flags), Some(err)) = (flags, err) else {
                    return Err(meta.error("eval_fn: checked needs flags = .. and err = .."));
                };
                a.checked = Some(Checked { nflags, flags, err });
            }
            _ => {
                return Err(meta.error(
                    "eval_fn: options are Int, Real, Decimal, String, Time, Duration, Json, VectorFloat32, lazy, eager, novec, args(..), checked(..)",
                ));
            }
        }
        Ok(())
    });
    syn::parse::Parser::parse(parser, attr)?;
    if a.novec && a.checked.is_some() {
        return Err(syn::Error::new(
            Span::call_site(),
            "eval_fn: novec and checked do not go together",
        ));
    }
    if a.args.is_some() && !a.lazy {
        return Err(syn::Error::new(
            Span::call_site(),
            "eval_fn: args(..) is for lazy bodies (a strict body's parameters give the types)",
        ));
    }
    if a.ret.is_none() {
        return Err(syn::Error::new(
            Span::call_site(),
            "eval_fn: the result type (Int, Real, Decimal, String, Time, Duration, Json or VectorFloat32) is required",
        ));
    }
    Ok(a)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    Int,
    Real,
    Decimal,
    Bytes,
    Time,
    Duration,
    Json,
    Vector,
}

impl ArgKind {
    /// Whether the body takes the argument by reference.
    fn by_ref(self) -> bool {
        matches!(
            self,
            ArgKind::Bytes | ArgKind::Decimal | ArgKind::Json | ArgKind::Vector
        )
    }
}

/// The evaluation types the vector driver reads its arguments as (`SigDrivers::args`).
fn arg_types(kinds: &[ArgKind]) -> TokenStream2 {
    let ts: Vec<TokenStream2> = kinds
        .iter()
        .map(|k| match k {
            ArgKind::Int => quote!(::tidb_expression::eval::rt::ETInt),
            ArgKind::Real => quote!(::tidb_expression::eval::rt::ETReal),
            ArgKind::Decimal => quote!(::tidb_expression::eval::rt::ETDecimal),
            ArgKind::Bytes => quote!(::tidb_expression::eval::rt::ETString),
            ArgKind::Time => quote!(::tidb_expression::eval::rt::ETDatetime),
            ArgKind::Duration => quote!(::tidb_expression::eval::rt::ETDuration),
            ArgKind::Json => quote!(::tidb_expression::eval::rt::ETJson),
            ArgKind::Vector => quote!(::tidb_expression::eval::rt::ETVectorFloat32),
        })
        .collect();
    quote!(&[#(#ts),*])
}

fn arg_kind(ty: &Type) -> Option<ArgKind> {
    let s = quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
        "i64" => Some(ArgKind::Int),
        "f64" => Some(ArgKind::Real),
        "&[u8]" => Some(ArgKind::Bytes),
        _ if s.starts_with('&') && s.ends_with("MyDecimal") => Some(ArgKind::Decimal),
        _ if s == "Time" || s.ends_with("::Time") => Some(ArgKind::Time),
        _ if s == "Duration" || s.ends_with("::Duration") => Some(ArgKind::Duration),
        _ if s.starts_with('&') && s.contains("BinaryJSON") => Some(ArgKind::Json),
        _ if s.starts_with('&') && s.contains("VectorFloat32") => Some(ArgKind::Vector),
        _ => None,
    }
}

/// See the crate documentation.
#[proc_macro_attribute]
pub fn eval_fn(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = match parse_attr(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    let func = syn::parse_macro_input!(item as ItemFn);
    let r = if attr.checked.is_some() {
        expand_checked(attr, func)
    } else {
        expand(attr, func)
    };
    match r {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(attr: Attr, mut func: ItemFn) -> syn::Result<TokenStream2> {
    let ret = attr.ret.expect("checked in parse_attr");
    let name = func.sig.ident.clone();
    let vis = func.vis.clone();
    let row_name = format_ident!("{}__row", name);
    let vec_name = format_ident!("{}__vec", name);
    let const_name = Ident::new(&name.to_string().to_uppercase(), name.span());
    let krate = quote!(::tidb_expression);
    let rt = quote!(#krate::eval::rt);

    // Parameters: ctx, f, then the arguments (strict) or `args` (lazy), then `out` (String).
    let params: Vec<&FnArg> = func.sig.inputs.iter().collect();
    let fixed = 2 + usize::from(attr.lazy) + usize::from(ret == Ret::String);
    if params.len() < fixed || (attr.lazy && params.len() != fixed) {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "eval_fn: expected (ctx, f, args.. | args: &mut impl Args, [out: &mut Vec<u8>])",
        ));
    }
    let mut kinds = Vec::new();
    if !attr.lazy {
        let end = params.len() - usize::from(ret == Ret::String);
        for p in &params[2..end] {
            let FnArg::Typed(pt) = p else {
                return Err(syn::Error::new_spanned(p, "eval_fn: unexpected receiver"));
            };
            if !matches!(&*pt.pat, Pat::Ident(_)) {
                return Err(syn::Error::new_spanned(&pt.pat, "eval_fn: expected a name"));
            }
            let Some(k) = arg_kind(&pt.ty) else {
                return Err(syn::Error::new_spanned(
                    &pt.ty,
                    "eval_fn: strict argument types are i64, f64, &MyDecimal, &[u8], Time, Duration, &BinaryJSON and &VectorFloat32",
                ));
            };
            kinds.push(k);
        }
    }
    func.attrs.push(syn::parse_quote!(#[inline(always)]));
    func.attrs
        .push(syn::parse_quote!(#[allow(clippy::too_many_arguments)]));

    let (row_ret, drivers_variant) = match ret {
        Ret::Int => (quote!(::std::option::Option<i64>), quote!(Int)),
        Ret::Real => (quote!(::std::option::Option<f64>), quote!(Real)),
        Ret::Decimal => (
            quote!(::std::option::Option<#rt::MyDecimal>),
            quote!(Decimal),
        ),
        Ret::String => (
            quote!(::std::option::Option<::std::vec::Vec<u8>>),
            quote!(String),
        ),
        Ret::Time => (quote!(::std::option::Option<#rt::Time>), quote!(Time)),
        Ret::Duration => (
            quote!(::std::option::Option<#rt::Duration>),
            quote!(Duration),
        ),
        Ret::Json => (
            quote!(::std::option::Option<#rt::BinaryJSON<'static>>),
            quote!(Json),
        ),
        Ret::Vector => (
            quote!(::std::option::Option<#rt::VectorFloat32<'static>>),
            quote!(VectorFloat32),
        ),
    };
    let err = quote!(#krate::eval::rt::Error);

    // ---- row driver
    let row_body = if attr.lazy {
        let call = if ret == Ret::String {
            quote! {
                let mut out = ::std::vec::Vec::new();
                ::std::result::Result::Ok(#name(ctx, &s, &mut args, &mut out)?.map(|()| out))
            }
        } else {
            quote!(#name(ctx, &s, &mut args))
        };
        quote! {
            let s = #krate::eval::RowSig::new(f, ctx);
            let mut args = #rt::RowArgs::new(f, st, row);
            #call
        }
    } else {
        let stmts = row_prologue(&kinds, attr.eager);
        let mut call_args = Vec::new();
        for (i, k) in kinds.iter().enumerate() {
            let v = format_ident!("a{}", i);
            call_args.push(if k.by_ref() { quote!(&#v) } else { quote!(#v) });
        }
        let call = if ret == Ret::String {
            quote! {
                let mut out = ::std::vec::Vec::new();
                ::std::result::Result::Ok(#name(ctx, &s, #(#call_args,)* &mut out)?.map(|()| out))
            }
        } else {
            quote!(#name(ctx, &s, #(#call_args),*))
        };
        quote! { #(#stmts)* let s = #krate::eval::RowSig::new(f, ctx); #call }
    };

    // ---- vector driver
    // A lazy body with VECTOR arguments (`args(VectorFloat32, ..)`) reads them through views
    // taken once per chunk (Go's vector versions read `buf.GetVectorFloat32(i)` row by row
    // from the evaluated columns).
    let vector_args: Vec<usize> = attr
        .args
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == ArgKind::Vector)
        .map(|(i, _)| i)
        .collect();
    let (vviews, vec_args_new) = if attr.lazy && !vector_args.is_empty() {
        let nviews = attr.args.as_ref().map_or(0, |a| a.len());
        let elems = (0..nviews).map(|k| {
            if vector_args.contains(&k) {
                quote!(cols.get(#k).map(|c| #rt::VectorView::new(c, n)))
            } else {
                quote!(::std::option::Option::None)
            }
        });
        (
            quote!(let vviews: [::std::option::Option<#rt::VectorView<'_>>; #nviews] = [#(#elems),*];),
            quote!(#rt::VecArgs::with_vectors(f, cols, &vviews, i)),
        )
    } else {
        (quote!(), quote!(#rt::VecArgs::new(f, cols, i)))
    };
    let vec_body = if attr.lazy {
        match ret {
            Ret::String => quote! {
                #rt::reset_string(result, n);
                let mut out = ::std::vec::Vec::new();
                #vviews
                for i in 0..n {
                    let mut args = #vec_args_new;
                    out.clear();
                    match #name(ctx, &s, &mut args, &mut out)? {
                        ::std::option::Option::Some(()) => result.append_bytes(&out),
                        ::std::option::Option::None => result.append_null(),
                    }
                }
                ::std::result::Result::Ok(())
            },
            Ret::Json | Ret::Vector => {
                let (reserve, append) = appended_out(ret);
                quote! {
                    result.#reserve(n);
                        #vviews
                    for i in 0..n {
                        let mut args = #vec_args_new;
                        match #name(ctx, &s, &mut args)? {
                            ::std::option::Option::Some(v) => result.#append(&v),
                            ::std::option::Option::None => result.append_null(),
                        }
                    }
                    ::std::result::Result::Ok(())
                }
            }
            Ret::Int | Ret::Real | Ret::Decimal | Ret::Time | Ret::Duration => {
                let out = fixed_out(ret);
                quote! {
                    let mut res = #out::new(result, n, &[]);
                        #vviews
                    for i in 0..n {
                        let mut args = #vec_args_new;
                        match #name(ctx, &s, &mut args)? {
                            ::std::option::Option::Some(v) => res.set(i, v),
                            ::std::option::Option::None => res.set_null(i),
                        }
                    }
                    ::std::result::Result::Ok(())
                }
            }
        }
    } else {
        let mut views = Vec::new();
        let mut gets = Vec::new();
        let mut null_checks = Vec::new();
        for (i, k) in kinds.iter().enumerate() {
            let v = format_ident!("v{}", i);
            let view = match k {
                ArgKind::Int => quote!(#rt::IntView),
                ArgKind::Real => quote!(#rt::RealView),
                ArgKind::Decimal => quote!(#rt::DecimalView),
                ArgKind::Bytes => quote!(#rt::BytesView),
                ArgKind::Time => quote!(#rt::TimeView),
                ArgKind::Duration => quote!(#rt::DurationView),
                ArgKind::Json => quote!(#rt::JsonView),
                ArgKind::Vector => quote!(#rt::VectorView),
            };
            views.push(if *k == ArgKind::Duration {
                quote!(let #v = #view::new(cols[#i], n, f, ctx, #i);)
            } else {
                quote!(let #v = #view::new(cols[#i], n);)
            });
            gets.push(
                if matches!(k, ArgKind::Decimal | ArgKind::Json | ArgKind::Vector) {
                    quote!(&#v.get(i))
                } else {
                    quote!(#v.get(i))
                },
            );
            null_checks.push(quote!(cols[#i].is_null(i)));
        }
        match ret {
            Ret::String => quote! {
                #(#views)*
                #rt::reset_string(result, n);
                let mut out = ::std::vec::Vec::new();
                for i in 0..n {
                    if false #(|| #null_checks)* {
                        result.append_null();
                        continue;
                    }
                    out.clear();
                    match #name(ctx, &s, #(#gets,)* &mut out)? {
                        ::std::option::Option::Some(()) => result.append_bytes(&out),
                        ::std::option::Option::None => result.append_null(),
                    }
                }
                ::std::result::Result::Ok(())
            },
            Ret::Json | Ret::Vector => {
                let (reserve, append) = appended_out(ret);
                quote! {
                    #(#views)*
                    result.#reserve(n);
                    for i in 0..n {
                        if false #(|| #null_checks)* {
                            result.append_null();
                            continue;
                        }
                        match #name(ctx, &s, #(#gets),*)? {
                            ::std::option::Option::Some(v) => result.#append(&v),
                            ::std::option::Option::None => result.append_null(),
                        }
                    }
                    ::std::result::Result::Ok(())
                }
            }
            Ret::Int | Ret::Real | Ret::Decimal | Ret::Time | Ret::Duration => {
                let out = fixed_out(ret);
                quote! {
                    #(#views)*
                    let mut res = #out::new(result, n, cols);
                    res.run(n, #[inline(always)] |i| #name(ctx, &s, #(#gets),*))
                }
            }
        }
    };

    let vec_body = if attr.novec {
        let et = ret.eval_type();
        quote!(::std::result::Result::Err(#krate::eval::base_vec_eval_error(#et)))
    } else {
        quote! {
            let types = #krate::eval::ArgTypes::new(f, ctx);
            let s = #krate::eval::VecSig::new(f, &types);
            #vec_body
        }
    };
    let args_ts = arg_types(attr.args.as_deref().unwrap_or(&kinds));
    Ok(quote! {
        #func

        #[allow(non_snake_case, unused_variables)]
        #vis fn #row_name(
            f: &#krate::eval::BuiltinFunc,
            ctx: &dyn #krate::EvalContext,
            st: &#krate::EvalState,
            row: #rt::Row<'_>,
        ) -> ::std::result::Result<#row_ret, #err> {
            #row_body
        }

        #[allow(non_snake_case, unused_variables, unused_mut)]
        #vis fn #vec_name(
            f: &#krate::eval::BuiltinFunc,
            ctx: &dyn #krate::EvalContext,
            n: usize,
            cols: &[&#rt::Column],
            result: &mut #rt::Column,
        ) -> ::std::result::Result<(), #err> {
            #vec_body
        }

        /// The row and vector drivers of this signature (`#[eval_fn]`).
        #vis const #const_name: #krate::eval::SigDrivers =
            #krate::eval::SigDrivers::#drivers_variant { row: #row_name, vec: #vec_name, args: #args_ts };
    })
}

/// The writer of a fixed-length result column.
fn fixed_out(ret: Ret) -> TokenStream2 {
    let rt = quote!(::tidb_expression::eval::rt);
    match ret {
        Ret::Int => quote!(#rt::IntOut),
        Ret::Real => quote!(#rt::RealOut),
        Ret::Decimal => quote!(#rt::DecimalOut),
        Ret::Time => quote!(#rt::TimeOut),
        Ret::Duration => quote!(#rt::DurationOut),
        Ret::String | Ret::Json | Ret::Vector => {
            unreachable!("variable-length results are appended")
        }
    }
}

/// The `Reserve*` / `Append*` methods of a JSON or vector result column (Go's
/// `result.ReserveJSON(n)` then `AppendJSON` / `AppendNull` per row).
fn appended_out(ret: Ret) -> (TokenStream2, TokenStream2) {
    match ret {
        Ret::Json => (quote!(reserve_json), quote!(append_json)),
        Ret::Vector => (
            quote!(reserve_vector_float32),
            quote!(append_vector_float32),
        ),
        _ => unreachable!("only JSON and vector results use appended_out"),
    }
}

/// The row driver's argument evaluation for strict bodies: left to right with a NULL
/// return at the first NULL (Go's usual prologue), or, `eager`, every argument first.
fn row_prologue(kinds: &[ArgKind], eager: bool) -> Vec<TokenStream2> {
    let mut stmts = Vec::new();
    let method = |k: &ArgKind| match k {
        ArgKind::Int => quote!(eval_int),
        ArgKind::Real => quote!(eval_real),
        ArgKind::Decimal => quote!(eval_decimal),
        ArgKind::Bytes => quote!(eval_string),
        ArgKind::Time => quote!(eval_time),
        ArgKind::Duration => quote!(eval_duration),
        ArgKind::Json => quote!(eval_json),
        ArgKind::Vector => quote!(eval_vector_float32),
    };
    if eager {
        let mut names = Vec::new();
        for (i, k) in kinds.iter().enumerate() {
            let v = format_ident!("a{}", i);
            let m = method(k);
            stmts.push(quote!(let #v = f.args[#i].#m(ctx, st, row)?;));
            names.push(v);
        }
        stmts.push(quote! {
            let (#(::std::option::Option::Some(#names),)*) = (#(#names,)*) else {
                return ::std::result::Result::Ok(::std::option::Option::None);
            };
        });
    } else {
        for (i, k) in kinds.iter().enumerate() {
            let v = format_ident!("a{}", i);
            let m = method(k);
            stmts.push(quote! {
                let ::std::option::Option::Some(#v) = f.args[#i].#m(ctx, st, row)? else {
                    return ::std::result::Result::Ok(::std::option::Option::None);
                };
            });
        }
    }
    stmts
}

/// `checked(nflags = N, flags = flags_fn, err = err_fn)`: the body is
/// `fn name<const FL: u8>(a: T, ..) -> (R, bool)` — the value and whether the row is an error
/// row — where `FL` is the signature's flag combination (`flags_fn(f) -> u8`, `N` bits, e.g.
/// the unsigned flags of the arguments) as a constant, and `err_fn(ctx, f, a, ..) -> Error`
/// is the cold function that builds Go's error for one row.
///
/// - Row driver: the strict (or `eager`) prologue, then the body for this row's flags; an
///   error row returns `err_fn(..)`.
/// - Vector driver: the flags are read once per chunk and select the copy of the loop
///   compiled for them (one driver per flag combination). The loop has no early exit: every
///   row's value is written and the error flags of all rows, NULL ones included, are ORed
///   (not masked with the null bitmap, #165). Only if one is set, a second scan finds the
///   first flagged row that is not NULL and returns its error — the row and the error Go's
///   vector version stops at; if every flagged row is NULL there is no error.
///
/// Only for bodies without per-row side effects (warnings, state): those keep the one-pass
/// form.
fn expand_checked(attr: Attr, mut func: ItemFn) -> syn::Result<TokenStream2> {
    let ret = attr.ret.expect("checked in parse_attr");
    let ck = attr.checked.as_ref().expect("checked");
    if attr.lazy || !matches!(ret, Ret::Int | Ret::Real) {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "eval_fn: checked is for strict Int/Real bodies",
        ));
    }
    let name = func.sig.ident.clone();
    let vis = func.vis.clone();
    let row_name = format_ident!("{}__row", name);
    let vec_name = format_ident!("{}__vec", name);
    let const_name = Ident::new(&name.to_string().to_uppercase(), name.span());
    let krate = quote!(::tidb_expression);
    let rt = quote!(#krate::eval::rt);
    let err_ty = quote!(#krate::eval::rt::Error);
    let mut kinds = Vec::new();
    for p in &func.sig.inputs {
        let FnArg::Typed(pt) = p else {
            return Err(syn::Error::new_spanned(p, "eval_fn: unexpected receiver"));
        };
        match arg_kind(&pt.ty) {
            Some(k @ (ArgKind::Int | ArgKind::Real)) => kinds.push(k),
            _ => {
                return Err(syn::Error::new_spanned(
                    &pt.ty,
                    "eval_fn: checked argument types are i64 and f64",
                ));
            }
        }
    }
    func.attrs.push(syn::parse_quote!(#[inline(always)]));
    let flags = &ck.flags;
    let errf = &ck.err;
    let fls: Vec<u8> = (0..(1u32 << ck.nflags) as u8).collect();
    let (row_t, out_t, drivers_variant) = match ret {
        Ret::Int => (quote!(i64), quote!(#rt::IntOut), quote!(Int)),
        _ => (quote!(f64), quote!(#rt::RealOut), quote!(Real)),
    };
    let stmts = row_prologue(&kinds, attr.eager);
    let row_args: Vec<_> = (0..kinds.len()).map(|i| format_ident!("a{}", i)).collect();
    let views: Vec<_> = kinds
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let v = format_ident!("v{}", i);
            let view = if *k == ArgKind::Int {
                quote!(#rt::IntView)
            } else {
                quote!(#rt::RealView)
            };
            quote!(let #v = #view::new(cols[#i], n);)
        })
        .collect();
    let vnames: Vec<_> = (0..kinds.len()).map(|i| format_ident!("v{}", i)).collect();
    let vtypes: Vec<_> = kinds
        .iter()
        .map(|k| {
            if *k == ArgKind::Int {
                quote!(#rt::IntView<'_>)
            } else {
                quote!(#rt::RealView<'_>)
            }
        })
        .collect();
    let row_arms: Vec<_> = fls
        .iter()
        .map(|fl| quote!(#fl => #name::<#fl>(#(#row_args),*),))
        .collect();
    let vec_arms: Vec<_> = fls
        .iter()
        .map(|fl| quote!(#fl => run::<#fl>(&mut res, n, #(&#vnames),*),))
        .collect();
    let args_ts = arg_types(attr.args.as_deref().unwrap_or(&kinds));
    Ok(quote! {
        #func

        #[allow(non_snake_case, unused_variables)]
        #vis fn #row_name(
            f: &#krate::eval::BuiltinFunc,
            ctx: &dyn #krate::EvalContext,
            st: &#krate::EvalState,
            row: #rt::Row<'_>,
        ) -> ::std::result::Result<::std::option::Option<#row_t>, #err_ty> {
            #(#stmts)*
            let s = #krate::eval::RowSig::new(f, ctx);
            let (v, bad) = match #flags(&s) {
                #(#row_arms)*
                _ => ::std::unreachable!(),
            };
            if bad {
                return ::std::result::Result::Err(#errf(ctx, &s, #(#row_args),*));
            }
            ::std::result::Result::Ok(::std::option::Option::Some(v))
        }

        #[allow(non_snake_case, unused_variables)]
        #vis fn #vec_name(
            f: &#krate::eval::BuiltinFunc,
            ctx: &dyn #krate::EvalContext,
            n: usize,
            cols: &[&#rt::Column],
            result: &mut #rt::Column,
        ) -> ::std::result::Result<(), #err_ty> {
            #[inline(always)]
            fn run<const FL: u8>(
                res: &mut #out_t<'_>,
                n: usize,
                #(#vnames: &#vtypes),*
            ) -> ::std::option::Option<usize> {
                if !res.run_checked(n, |i| #name::<FL>(#(#vnames.get(i)),*)) {
                    return ::std::option::Option::None;
                }
                (0..n).find(|&i| !res.is_null(i) && #name::<FL>(#(#vnames.get(i)),*).1)
            }
            let types = #krate::eval::ArgTypes::new(f, ctx);
            let s = #krate::eval::VecSig::new(f, &types);
            #(#views)*
            let mut res = #out_t::new(result, n, cols);
            let first = match #flags(&s) {
                #(#vec_arms)*
                _ => ::std::unreachable!(),
            };
            match first {
                ::std::option::Option::None => ::std::result::Result::Ok(()),
                ::std::option::Option::Some(i) => {
                    ::std::result::Result::Err(#errf(ctx, &s, #(#vnames.get(i)),*))
                }
            }
        }

        /// The row and vector drivers of this signature (`#[eval_fn]`).
        #vis const #const_name: #krate::eval::SigDrivers =
            #krate::eval::SigDrivers::#drivers_variant { row: #row_name, vec: #vec_name, args: #args_ts };
    })
}
