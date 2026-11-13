//! go-file: pkg/expression/aggregation/descriptor.go (boundary stub)
//!
//! blocked-by:D08.11 — `AggFuncDesc` and its type inference. The stub keeps the fields
//! column pruning reads (`Name`, `Args`, `OrderByItems`, `RetTp`); `NewAggFuncDesc` only
//! covers `count` and `firstrow` over constant arguments, which is all the rule creates.

use tidb_base::Error;
use tidb_base::mysql::{BinaryFlag, NotNullFlag, TypeLonglong};
use tidb_expression::ExprRef;
use tidb_parser::ast::funcs as ast;
use tidb_sessionctx::exprctx::BuildContext;
use tidb_types::etc::FieldType;
use tidb_types::field_type::new_field_type;

use crate::util::ByItems;

/// AggFuncDesc describes an aggregation function signature, only used in planner.
#[derive(Clone, Debug)]
pub struct AggFuncDesc {
    /// Name represents the aggregation function name.
    pub name: String,
    /// Args represents the arguments of the aggregation function.
    pub args: Vec<ExprRef>,
    /// RetTp represents the return type of the aggregation function.
    pub ret_tp: FieldType,
    /// HasDistinct represents whether the aggregation function contains distinct attribute.
    pub has_distinct: bool,
    /// OrderByItems represents the order by clause used in GROUP_CONCAT.
    pub order_by_items: Vec<ByItems>,
}

/// NewAggFuncDesc creates an aggregation function signature descriptor.
// glue: stub, blocked-by:D08.11 (count / firstrow only)
pub fn new_agg_func_desc(
    ctx: &dyn BuildContext,
    name: &str,
    args: Vec<ExprRef>,
    has_distinct: bool,
) -> Result<AggFuncDesc, Error> {
    let ret_tp = match name {
        ast::AggFuncCount => {
            // typeInfer4Count
            let mut tp = new_field_type(TypeLonglong);
            tp.set_flen(21);
            tp.set_decimal(0);
            tp.add_flag(BinaryFlag | NotNullFlag);
            tp
        }
        ast::AggFuncFirstRow => args[0].get_type(&*ctx.get_eval_ctx()).into_owned(),
        _ => {
            return Err(tidb_base::errors::new(format!(
                "blocked-by:D08.11 aggregate function {name}"
            )));
        }
    };
    Ok(AggFuncDesc {
        name: name.to_owned(),
        args,
        ret_tp,
        has_distinct,
        order_by_items: Vec::new(),
    })
}
