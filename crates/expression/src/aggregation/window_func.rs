//! go-file: pkg/expression/aggregation/window_func.go
//!
//! Window function descriptors (D08.11 spec §2.1 part C): `WindowFuncDesc`,
//! `NewWindowFuncDesc`, `UseDefaultFrame`, `NeedFrame`, `WindowFuncDesc.Clone`.
//! The two functions that convert to tipb and check TiFlash push-down live in
//! `tidb_expr_pushdown::aggregation::window_func` (spec §4.8, N0 rule 6).

#![allow(non_upper_case_globals)]

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::{NotNullFlag, TypeEnum, TypeSet};
use tidb_parser::ast::{BoundType, FrameBound, FrameClause, FrameExtent, FrameType};

use super::base_func::{BaseFuncDesc, new_base_func_desc};
use crate::ast_funcs as ast;
use crate::context::BuildContext;
use crate::expression::{Expr, ExprRef};
use crate::util::get_uint64_from_constant;

/// WindowFuncDesc describes a window function signature, only used in planner.
#[derive(Clone, Debug)]
pub struct WindowFuncDesc {
    /// The embedded `baseFuncDesc` (M-SELF).
    pub base: BaseFuncDesc,
}

impl Deref for WindowFuncDesc {
    type Target = BaseFuncDesc;

    // glue: M-SELF (Go struct embedding of baseFuncDesc)
    fn deref(&self) -> &BaseFuncDesc {
        &self.base
    }
}

impl DerefMut for WindowFuncDesc {
    // glue: M-SELF (Go struct embedding of baseFuncDesc)
    fn deref_mut(&mut self) -> &mut BaseFuncDesc {
        &mut self.base
    }
}

/// NewWindowFuncDesc creates a window function signature descriptor.
///
/// Returns `Ok(None)` where Go returns `(nil, nil)` for invalid window function arguments
/// (the caller turns `None` into `ErrWrongArguments`, spec §7 item 4).
// go: NewWindowFuncDesc L1 [M-ERR, M-FT, M-PANIC]
pub fn new_window_func_desc(
    ctx: &dyn BuildContext,
    name: &str,
    args: Vec<ExprRef>,
    skip_check_args: bool,
) -> Result<Option<WindowFuncDesc>, Error> {
    // if we are in the prepare statement, skip the params check since it's not been initialized.
    if !skip_check_args {
        match name.to_lowercase().as_str() {
            ast::WindowFuncNthValue => {
                let (val, is_null, ok) = get_uint64_from_constant(&*ctx.get_eval_ctx(), &args[1]);
                // nth_value does not allow `0`, but allows `null`.
                if !ok || (val == 0 && !is_null) {
                    return Ok(None);
                }
            }
            ast::WindowFuncNtile => {
                let (val, is_null, ok) = get_uint64_from_constant(&*ctx.get_eval_ctx(), &args[0]);
                // ntile does not allow `0`, but allows `null`.
                if !ok || (val == 0 && !is_null) {
                    return Ok(None);
                }
            }
            ast::WindowFuncLead | ast::WindowFuncLag if args.len() >= 2 => {
                let (_, is_null, ok) = get_uint64_from_constant(&*ctx.get_eval_ctx(), &args[1]);
                if !ok || is_null {
                    return Ok(None);
                }
            }
            _ => {}
        }
    }

    let (mut base, err) = new_base_func_desc(ctx, name, args);

    // Some window functions' return column type must be nullable or not nullable.
    // Go switches on the raw (un-lowercased) name (#400).
    match name {
        ast::WindowFuncRowNumber
        | ast::WindowFuncRank
        | ast::WindowFuncDenseRank
        | ast::WindowFuncCumeDist
        | ast::WindowFuncPercentRank
        | ast::AggFuncCount
        | ast::AggFuncApproxCountDistinct => {
            if base.ret_tp_is_nil() {
                panic!("runtime error: invalid memory address or nil pointer dereference");
            }
            Arc::make_mut(&mut base.ret_tp).set_flag(NotNullFlag);
        }
        ast::AggFuncBitAnd | ast::AggFuncBitOr | ast::AggFuncBitXor => {
            if base.ret_tp_is_nil() {
                panic!("runtime error: invalid memory address or nil pointer dereference");
            }
            // Divergence ruling R25 (#398): Go calls SetFlag(NotNullFlag), wiping UnsignedFlag
            // from typeInfer4BitFuncs and turning >= 2^63 results negative; AddFlag keeps
            // UnsignedFlag (and BinaryFlag) matching non-window bit_* and MySQL.
            Arc::make_mut(&mut base.ret_tp).add_flag(NotNullFlag);
        }
        ast::WindowFuncLead | ast::WindowFuncLag => {
            let both_not_null = {
                let ectx = ctx.get_eval_ctx();
                base.args.len() == 3
                    && (base.args[0].get_type(&*ectx).get_flag() & NotNullFlag) != 0
                    && (base.args[2].get_type(&*ectx).get_flag() & NotNullFlag) != 0
            };
            if base.ret_tp_is_nil() {
                // Go's `base.RetTp.SetFlag` / `DelFlag` dereferences nil when typeInfer4LeadLag
                // dropped InferType4ControlFuncs's error (#401, spec §9 decision 8).
                panic!("runtime error: invalid memory address or nil pointer dereference");
            }
            if both_not_null {
                // Divergence ruling R25 (#398): Go calls SetFlag(NotNullFlag), wiping UnsignedFlag
                // from InferType4ControlFuncs and turning >= 2^63 results negative; AddFlag keeps
                // UnsignedFlag and other inferred flags.
                Arc::make_mut(&mut base.ret_tp).add_flag(NotNullFlag);
            } else {
                Arc::make_mut(&mut base.ret_tp).del_flag(NotNullFlag);
            }
        }
        _ => {
            if base.ret_tp_is_nil() {
                panic!("runtime error: invalid memory address or nil pointer dereference");
            }
            let aliases_arg0 = matches!(
                base.name.as_str(),
                ast::AggFuncFirstRow
                    | ast::WindowFuncFirstValue
                    | ast::WindowFuncLastValue
                    | ast::WindowFuncNthValue
            ) && !base.args.is_empty()
                && (base.name == ast::AggFuncFirstRow
                    || !matches!(
                        base.args[0].get_type(&*ctx.get_eval_ctx()).get_type(),
                        TypeEnum | TypeSet
                    ));
            Arc::make_mut(&mut base.ret_tp).del_flag(NotNullFlag);
            // Go aliasing (M-FT): window_func.go:76, DelFlag(NotNullFlag) mutates the *FieldType
            // aliased from args[0] by typeInfer4MaxMin (base_func.go:314), clearing NotNullFlag
            // on args[0] (and the source column in the query output schema; confirmed V1, #402).
            if aliases_arg0 {
                let shared = Arc::clone(&base.ret_tp);
                match Arc::make_mut(&mut base.args[0]) {
                    Expr::Column(c) => c.ret_type = shared,
                    Expr::CorrelatedColumn(c) => c.column.ret_type = shared,
                    Expr::ScalarFunction(f) => f.function.tp = shared,
                    Expr::Constant(c) => c.ret_type = (*shared).clone(),
                }
            }
        }
    }

    err?;
    Ok(Some(WindowFuncDesc { base }))
}

/// noFrameWindowFuncs is the functions that operate on the entire partition,
/// they should not have frame specifications.
/// (Go's package-level map; only looked up, M-MAPORDER.)
const noFrameWindowFuncs: &[&str] = &[
    ast::WindowFuncCumeDist,
    ast::WindowFuncDenseRank,
    ast::WindowFuncLag,
    ast::WindowFuncLead,
    ast::WindowFuncNtile,
    ast::WindowFuncPercentRank,
    ast::WindowFuncRank,
    ast::WindowFuncRowNumber,
];

/// UseDefaultFrame indicates if the window function has a provided frame that will override
/// user's designation.
// go: UseDefaultFrame L1 [M-MAPORDER]
pub fn use_default_frame(name: &str) -> (bool, FrameClause) {
    if name.to_lowercase() == ast::WindowFuncRowNumber {
        return (
            true,
            FrameClause {
                tp: FrameType::Rows,
                extent: FrameExtent {
                    start: FrameBound {
                        tp: BoundType::CurrentRow,
                        ..FrameBound::default()
                    },
                    end: FrameBound {
                        tp: BoundType::CurrentRow,
                        ..FrameBound::default()
                    },
                },
            },
        );
    }
    (false, FrameClause::default())
}

/// NeedFrame checks if the function need frame specification.
// go: NeedFrame L1 [M-MAPORDER]
pub fn need_frame(name: &str) -> bool {
    !noFrameWindowFuncs.contains(&name.to_lowercase().as_str())
}

impl WindowFuncDesc {
    /// Clone makes a copy of SortItem.
    // go: WindowFuncDesc.Clone L1 [M-FT, M-EXPR, M-NAME]
    pub fn clone_desc(&self) -> WindowFuncDesc {
        WindowFuncDesc {
            base: self.base.clone_base(),
        }
    }
}
