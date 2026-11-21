//! go-file: pkg/planner/core/resolve_indices.go
//!
//! So far only `refine4NeighbourProj`, which `InjectExtraProjection` (the optimizer driver,
//! block D09.13) calls; the `ResolveIndices` methods are block D10.9.

use std::collections::BTreeMap;
use std::sync::Arc;

use tidb_base::disjointset::new_int_set;
use tidb_expression::Expr;

use crate::base::PhysicalPlan;
use crate::core::physical_plans::PhysicalProjection;

/// refine4NeighbourProj: the column indices of `p`'s expressions point into `child_proj`'s
/// output; output columns that repeat one input column are merged to the first of them.
///
/// Go ranges over a map; the groups of different input columns are disjoint, so the order
/// does not matter (a `BTreeMap` here). `p`'s expressions are written through `make_mut`
/// (they are fresh copies when `InjectExtraProjection` calls this).
// go: refine4NeighbourProj L1 [M-EXPR]
pub fn refine4_neighbour_proj(p: &mut PhysicalProjection, child_proj: &PhysicalProjection) {
    let mut input_idx2_output_idxes: BTreeMap<isize, Vec<usize>> = BTreeMap::new();
    for (i, expr) in child_proj.exprs.iter().enumerate() {
        let Expr::Column(col) = &**expr else {
            continue;
        };
        input_idx2_output_idxes
            .entry(col.index)
            .or_default()
            .push(i);
    }
    let mut child_schema_union_set = new_int_set(child_proj.schema().len());
    for output_idxes in input_idx2_output_idxes.values() {
        if output_idxes.len() <= 1 {
            continue;
        }
        for i in 1..output_idxes.len() {
            child_schema_union_set.union(output_idxes[0], output_idxes[i]);
        }
    }

    for expr in p.exprs.iter_mut() {
        if !matches!(&**expr, Expr::Column(_)) {
            continue;
        }
        let Expr::Column(col) = Arc::make_mut(expr) else {
            unreachable!()
        };
        col.index = child_schema_union_set.find_root(col.index as usize) as isize;
    }
}
