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
//! `Option<MyDecimal>`, `String` → `Option<()>` with the bytes appended to `out` (the vector
//! driver reuses one buffer for the whole chunk).
//! Strict argument types: `i64` (EvalInt), `f64` (EvalReal), `&MyDecimal` (EvalDecimal),
//! `&[u8]` (EvalString).
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
}

#[derive(Default)]
struct Attr {
    ret: Option<Ret>,
    lazy: bool,
    /// `eager`: the row driver evaluates every argument before looking at NULL (Go row
    /// versions such as `CompareInt` that evaluate both sides first).
    eager: bool,
    checked: Option<Checked>,
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
            "lazy" => a.lazy = true,
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
                return Err(
                    meta.error("eval_fn: options are Int, Real, Decimal, String, lazy, eager, checked(..)")
                );
            }
        }
        Ok(())
    });
    syn::parse::Parser::parse(parser, attr)?;
    if a.ret.is_none() {
        return Err(syn::Error::new(
            Span::call_site(),
            "eval_fn: the result type (Int, Real, Decimal or String) is required",
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
}

fn arg_kind(ty: &Type) -> Option<ArgKind> {
    let s = quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
        "i64" => Some(ArgKind::Int),
        "f64" => Some(ArgKind::Real),
        "&[u8]" => Some(ArgKind::Bytes),
        _ if s.starts_with('&') && s.ends_with("MyDecimal") => Some(ArgKind::Decimal),
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
                    "eval_fn: strict argument types are i64, f64, &MyDecimal and &[u8]",
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
        Ret::Decimal => (quote!(::std::option::Option<#rt::MyDecimal>), quote!(Decimal)),
        Ret::String => (
            quote!(::std::option::Option<::std::vec::Vec<u8>>),
            quote!(String),
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
            call_args.push(if matches!(k, ArgKind::Bytes | ArgKind::Decimal) {
                quote!(&#v)
            } else {
                quote!(#v)
            });
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
    let vec_body = if attr.lazy {
        match ret {
            Ret::String => quote! {
                #rt::reset_string(result, n);
                let mut out = ::std::vec::Vec::new();
                for i in 0..n {
                    let mut args = #rt::VecArgs::new(f, cols, i);
                    out.clear();
                    match #name(ctx, &s, &mut args, &mut out)? {
                        ::std::option::Option::Some(()) => result.append_bytes(&out),
                        ::std::option::Option::None => result.append_null(),
                    }
                }
                ::std::result::Result::Ok(())
            },
            Ret::Int | Ret::Real | Ret::Decimal => {
                let out = match ret {
                    Ret::Int => quote!(#rt::IntOut),
                    Ret::Real => quote!(#rt::RealOut),
                    _ => quote!(#rt::DecimalOut),
                };
                quote! {
                    let mut res = #out::new(result, n, &[]);
                    for i in 0..n {
                        let mut args = #rt::VecArgs::new(f, cols, i);
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
            };
            views.push(quote!(let #v = #view::new(cols[#i], n);));
            gets.push(if *k == ArgKind::Decimal {
                quote!(&#v.get(i))
            } else {
                quote!(#v.get(i))
            });
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
            Ret::Int | Ret::Real | Ret::Decimal => {
                let out = match ret {
                    Ret::Int => quote!(#rt::IntOut),
                    Ret::Real => quote!(#rt::RealOut),
                    _ => quote!(#rt::DecimalOut),
                };
                quote! {
                    #(#views)*
                    let mut res = #out::new(result, n, cols);
                    res.run(n, #[inline(always)] |i| #name(ctx, &s, #(#gets),*))
                }
            }
        }
    };

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
            let types = #krate::eval::ArgTypes::new(f, ctx);
            let s = #krate::eval::VecSig::new(f, &types);
            #vec_body
        }

        /// The row and vector drivers of this signature (`#[eval_fn]`).
        #vis const #const_name: #krate::eval::SigDrivers =
            #krate::eval::SigDrivers::#drivers_variant { row: #row_name, vec: #vec_name };
    })
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
///   row's value is written and the error flags of the rows that are not NULL are ORed. Only
///   if one is set, a second scan finds the first such row and returns its error — the row
///   and the error Go's vector version stops at.
///
/// Only for bodies without per-row side effects (warnings, state): those keep the one-pass
/// form.
fn expand_checked(attr: Attr, mut func: ItemFn) -> syn::Result<TokenStream2> {
    let ret = attr.ret.expect("checked in parse_attr");
    let ck = attr.checked.as_ref().expect("checked");
    if attr.lazy || matches!(ret, Ret::String | Ret::Decimal) {
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
            #krate::eval::SigDrivers::#drivers_variant { row: #row_name, vec: #vec_name };
    })
}
