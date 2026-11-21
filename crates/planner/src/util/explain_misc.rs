//! go-file: pkg/planner/util/explain_misc.go
//!
//! The EXPLAIN text of ORDER BY / PARTITION BY items (leaf util file, constitution §4.4).
//! `Expression.ExplainInfo` is ported only for columns (`pkg/expression/explain.go`, the rest
//! is block D10.9): an item with another expression returns `blocked-by:D10.9`.

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_expression::{EvalContext, Expr};

use crate::property::SortItem;
use crate::util::ByItems;

/// The EXPLAIN text of an expression (`Expression.ExplainInfo`): columns only.
// glue: dispatch of Expression.ExplainInfo (ScalarFunction / Constant / CorrelatedColumn ExplainInfo are block D10.9)
fn expr_explain_info(ctx: &dyn EvalContext, e: &Expr) -> Result<String, Error> {
    match e {
        Expr::Column(c) => Ok(String::from_utf8_lossy(&c.explain_info(ctx)).into_owned()),
        _ => Err(blocked(
            "D10.9",
            "the EXPLAIN text of a non-column expression (Expression.ExplainInfo)",
        )),
    }
}

/// ExplainByItems generates explain information for ByItems.
// go: ExplainByItems L2 (Expression.ExplainInfo of a non-column item is blocked-by:D10.9)
pub fn explain_by_items(
    ctx: &dyn EvalContext,
    buffer: &mut String,
    by_items: &[ByItems],
) -> Result<(), Error> {
    for (i, item) in by_items.iter().enumerate() {
        if item.desc {
            buffer.push_str(&format!("{}:desc", expr_explain_info(ctx, &item.expr)?));
        } else {
            buffer.push_str(&expr_explain_info(ctx, &item.expr)?);
        }

        if i + 1 < by_items.len() {
            buffer.push_str(", ");
        }
    }
    Ok(())
}

/// ExplainPartitionBy produce text for p.PartitionBy. Common for window functions and TopN.
// go: ExplainPartitionBy L1
pub fn explain_partition_by(
    ctx: &dyn EvalContext,
    buffer: &mut String,
    partition_by: &[SortItem],
    normalized: bool,
) {
    if !partition_by.is_empty() {
        buffer.push_str("partition by ");
        for (i, item) in partition_by.iter().enumerate() {
            buffer.push_str(&String::from_utf8_lossy(
                &item.col.column_explain_info(ctx, normalized),
            ));
            if i + 1 < partition_by.len() {
                buffer.push_str(", ");
            }
        }
    }
}
