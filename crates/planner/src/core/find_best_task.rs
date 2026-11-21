//! go-file: pkg/planner/core/find_best_task.go
//!
//! So far the two pure helpers the plan enumeration of the slice's operators uses
//! (`exhaust_physical_plans.go`, `task.go`). `findBestTask` and the access paths are block
//! D10.2 (the FindBestTask task of the first vertical slice).

use std::sync::Arc;

use tidb_expression::Expr;

use crate::property::{PhysicalProperty, SortItem};
use crate::util::ByItems;

/// GetPropByOrderByItems will check if this sort property can be pushed or not. In order to simplify the problem, we only
/// consider the case that all expression are columns.
///
/// Go's `(nil, false)` is `None`.
// go: GetPropByOrderByItems L1
pub fn get_prop_by_order_by_items(items: &[ByItems]) -> Option<PhysicalProperty> {
    let mut prop_items = Vec::with_capacity(items.len());
    for item in items {
        let Expr::Column(col) = &*item.expr else {
            return None;
        };
        prop_items.push(SortItem {
            col: Arc::new(col.clone()),
            desc: item.desc,
        });
    }
    Some(PhysicalProperty {
        sort_items: prop_items,
        ..Default::default()
    })
}

/// GetPropByOrderByItemsContainScalarFunc will check if this sort property can be pushed or not. In order to simplify the
/// problem, we only consider the case that all expression are columns or some special scalar functions.
///
/// Go's `(nil, false, false)` is `None`; otherwise the property and `onlyColumn`.
// go: GetPropByOrderByItemsContainScalarFunc L1
pub fn get_prop_by_order_by_items_contain_scalar_func(
    items: &[ByItems],
) -> Option<(PhysicalProperty, bool)> {
    let mut prop_items = Vec::with_capacity(items.len());
    let mut only_column = true;
    for item in items {
        match &*item.expr {
            Expr::Column(expr) => prop_items.push(SortItem {
                col: Arc::new(expr.clone()),
                desc: item.desc,
            }),
            Expr::ScalarFunction(expr) => {
                let (col, desc) = expr.get_single_column(item.desc);
                let col = col?;
                prop_items.push(SortItem {
                    col: Arc::new(col.clone()),
                    desc,
                });
                only_column = false;
            }
            _ => return None,
        }
    }
    Some((
        PhysicalProperty {
            sort_items: prop_items,
            ..Default::default()
        },
        only_column,
    ))
}
