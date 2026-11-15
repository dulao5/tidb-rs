//! go-file: pkg/planner/core/logical_plan_builder.go
//!
//! So far the LIMIT helpers the point get fast path calls (VS1-11): `extractLimitCountOffset`
//! and `getUintFromNode`. The logical plan builder itself is VS1-21's (D09.1).

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::dbterror::plannererrors::ErrWrongArguments;
use tidb_parser::ast::{self, Expr};
use tidb_sessionctx::exprctx::BuildContext;
use tidb_types::convert::str_to_uint;

/// getUintFromNode gets uint64 value from ast.Node.
/// For ordinary statement, node should be uint64 constant value.
/// For prepared statement, node is string. We should convert it to uint64.
///
/// Returns `(uVal, isNull, isExpectedType)`. A parameter marker (`LIMIT ?`) reads the value
/// `EXECUTE` bound to it; the Rust AST keeps no value on the marker (the plan cache, D13.1,
/// VS1-15), so that case is a boundary stub.
// go: getUintFromNode L2 (a parameter marker is blocked-by:D13.1)
pub fn get_uint_from_node(
    ctx: &dyn BuildContext,
    n: &Expr,
    must_int64_or_uint64: bool,
) -> Result<(u64, bool, bool), Error> {
    let _ = must_int64_or_uint64;
    let val = match n {
        // val = v.GetValue()
        Expr::Value(v) => &v.datum,
        Expr::ParamMarker(_) => {
            // glue: blocked-by:D13.1 (the value of a parameter marker in EXECUTE)
            return Err(blocked(
                "D13.1",
                "a parameter marker in LIMIT (the value bound by EXECUTE)",
            ));
        }
        _ => return Ok((0, false, false)),
    };
    match val {
        ast::Datum::Uint(v) => return Ok((*v, false, true)),
        ast::Datum::Int(v) => {
            if *v >= 0 {
                return Ok((*v as u64, false, true));
            }
        }
        // TRUE / FALSE are int64 datums in Go (Datum.SetValue of a bool).
        ast::Datum::Bool(b) => return Ok((u64::from(*b), false, true)),
        ast::Datum::Str(v) => {
            let (u_val, err) = str_to_uint(&ctx.get_eval_ctx().type_ctx(), v.as_bytes(), false);
            if err.is_err() {
                return Ok((0, false, false));
            }
            return Ok((u_val, false, true));
        }
        _ => {}
    }
    Ok((0, false, false))
}

// go: extractLimitCountOffset L1
pub fn extract_limit_count_offset(
    ctx: &dyn BuildContext,
    limit: &ast::Limit,
) -> Result<(u64, u64), Error> {
    let mut offset = 0;
    // limit.Count != nil: always set by the parser.
    let (c, _, is_expected_type) = get_uint_from_node(ctx, &limit.count, true)?;
    if !is_expected_type {
        return Err(ErrWrongArguments.gen_with_stack_by_args(tidb_base::go_args!["LIMIT"]));
    }
    let count = c;
    if let Some(o) = &limit.offset {
        let (o, _, is_expected_type) = get_uint_from_node(ctx, o, true)?;
        if !is_expected_type {
            return Err(ErrWrongArguments.gen_with_stack_by_args(tidb_base::go_args!["LIMIT"]));
        }
        offset = o;
    }
    Ok((count, offset))
}
