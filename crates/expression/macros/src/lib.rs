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
//! Result types: `Int` → `Option<i64>`, `Real` → `Option<f64>`, `String` → `Option<()>` with
//! the bytes appended to `out` (the vector driver reuses one buffer for the whole chunk).
//! Strict argument types: `i64` (EvalInt), `f64` (EvalReal), `&[u8]` (EvalString).
//!
//! [`Args`]: ../tidb_expression/eval/trait.Args.html

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{FnArg, ItemFn, Pat, Token, Type};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ret {
    Int,
    Real,
    String,
}

struct Attr {
    ret: Ret,
    lazy: bool,
}

impl Parse for Attr {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let items = Punctuated::<Ident, Token![,]>::parse_terminated(input)?;
        let mut ret = None;
        let mut lazy = false;
        for it in items {
            match it.to_string().as_str() {
                "Int" => ret = Some(Ret::Int),
                "Real" => ret = Some(Ret::Real),
                "String" => ret = Some(Ret::String),
                "lazy" => lazy = true,
                other => {
                    return Err(syn::Error::new(
                        it.span(),
                        format!("eval_fn: unknown option `{other}` (Int, Real, String, lazy)"),
                    ));
                }
            }
        }
        let Some(ret) = ret else {
            return Err(syn::Error::new(
                Span::call_site(),
                "eval_fn: the result type (Int, Real or String) is required",
            ));
        };
        Ok(Attr { ret, lazy })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    Int,
    Real,
    Bytes,
}

fn arg_kind(ty: &Type) -> Option<ArgKind> {
    let s = quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
        "i64" => Some(ArgKind::Int),
        "f64" => Some(ArgKind::Real),
        "&[u8]" => Some(ArgKind::Bytes),
        _ => None,
    }
}

/// See the crate documentation.
#[proc_macro_attribute]
pub fn eval_fn(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = syn::parse_macro_input!(attr as Attr);
    let func = syn::parse_macro_input!(item as ItemFn);
    match expand(attr, func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(attr: Attr, mut func: ItemFn) -> syn::Result<TokenStream2> {
    let name = func.sig.ident.clone();
    let vis = func.vis.clone();
    let row_name = format_ident!("{}__row", name);
    let vec_name = format_ident!("{}__vec", name);
    let const_name = Ident::new(&name.to_string().to_uppercase(), name.span());
    let krate = quote!(::tidb_expression);
    let rt = quote!(#krate::eval::rt);

    // Parameters: ctx, f, then the arguments (strict) or `args` (lazy), then `out` (String).
    let params: Vec<&FnArg> = func.sig.inputs.iter().collect();
    let fixed = 2 + usize::from(attr.lazy) + usize::from(attr.ret == Ret::String);
    if params.len() < fixed || (attr.lazy && params.len() != fixed) {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "eval_fn: expected (ctx, f, args.. | args: &mut impl Args, [out: &mut Vec<u8>])",
        ));
    }
    let mut kinds = Vec::new();
    if !attr.lazy {
        let end = params.len() - usize::from(attr.ret == Ret::String);
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
                    "eval_fn: strict argument types are i64, f64 and &[u8]",
                ));
            };
            kinds.push(k);
        }
    }
    func.attrs.push(syn::parse_quote!(#[inline(always)]));
    func.attrs
        .push(syn::parse_quote!(#[allow(clippy::too_many_arguments)]));

    let (row_ret, drivers_variant) = match attr.ret {
        Ret::Int => (quote!(::std::option::Option<i64>), quote!(Int)),
        Ret::Real => (quote!(::std::option::Option<f64>), quote!(Real)),
        Ret::String => (
            quote!(::std::option::Option<::std::vec::Vec<u8>>),
            quote!(String),
        ),
    };
    let err = quote!(#krate::eval::rt::Error);

    // ---- row driver
    let row_body = if attr.lazy {
        let call = if attr.ret == Ret::String {
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
        let mut stmts = Vec::new();
        let mut call_args = Vec::new();
        for (i, k) in kinds.iter().enumerate() {
            let v = format_ident!("a{}", i);
            let m = match k {
                ArgKind::Int => quote!(eval_int),
                ArgKind::Real => quote!(eval_real),
                ArgKind::Bytes => quote!(eval_string),
            };
            stmts.push(quote! {
                let ::std::option::Option::Some(#v) = f.args[#i].#m(ctx, st, row)? else {
                    return ::std::result::Result::Ok(::std::option::Option::None);
                };
            });
            call_args.push(if *k == ArgKind::Bytes {
                quote!(&#v)
            } else {
                quote!(#v)
            });
        }
        let call = if attr.ret == Ret::String {
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
        match attr.ret {
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
            Ret::Int | Ret::Real => {
                let out = if attr.ret == Ret::Int {
                    quote!(#rt::IntOut)
                } else {
                    quote!(#rt::RealOut)
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
                ArgKind::Bytes => quote!(#rt::BytesView),
            };
            views.push(quote!(let #v = #view::new(cols[#i], n);));
            gets.push(quote!(#v.get(i)));
            null_checks.push(quote!(cols[#i].is_null(i)));
        }
        match attr.ret {
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
            Ret::Int | Ret::Real => {
                let out = if attr.ret == Ret::Int {
                    quote!(#rt::IntOut)
                } else {
                    quote!(#rt::RealOut)
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
