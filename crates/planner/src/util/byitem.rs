//! go-file: pkg/planner/util/byitem.go
//!
//! `ByItems.Hash64` / `ByItems.Equals` implement cascades' `base.HashEquals`
//! (`pkg/planner/cascades/base` is EXC.B, not ported; as `Column.Hash64` in D08.1 spec §4
//! item 2). The expression is an `ExprRef` (`M-EXPR`): `Clone` shares it.

use std::sync::Arc;

use tidb_base::errors::RedactLogDisable;
use tidb_expression::{EvalContext, ExprRef, ParamValues};

/// `size.SizeOfBool`.
const SIZE_OF_BOOL: i64 = 1;

// go-exception: EXC.B ByItems.Hash64 (cascades base.HashEquals; D08.1 spec §4 item 2)
// go-exception: EXC.B ByItems.Equals (cascades base.HashEquals; D08.1 spec §4 item 2)

/// ByItems wraps a "by" item.
#[derive(Clone, Debug)]
pub struct ByItems {
    /// Expr.
    pub expr: ExprRef,
    /// Desc.
    pub desc: bool,
}

impl ByItems {
    /// StringWithCtx implements expression.StringerWithCtx interface.
    // go: ByItems.StringWithCtx L1
    pub fn string_with_ctx(&self, ctx: &dyn ParamValues, redact: &str) -> Vec<u8> {
        let mut s = self.expr.string_with_ctx(ctx, redact);
        if self.desc {
            s.extend_from_slice(b" true");
        }
        s
    }

    /// Clone makes a copy of ByItems.
    // go: ByItems.Clone L1 [M-EXPR]
    pub fn clone_item(&self) -> ByItems {
        ByItems {
            expr: Arc::clone(&self.expr),
            desc: self.desc,
        }
    }

    /// Equal checks whether two ByItems are equal.
    // go: ByItems.Equal L1
    pub fn equal(&self, ctx: &dyn EvalContext, other: &ByItems) -> bool {
        self.expr.equal(ctx, &other.expr) && self.desc == other.desc
    }

    /// MemoryUsage return the memory usage of ByItems.
    // go: ByItems.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        SIZE_OF_BOOL + self.expr.memory_usage()
    }
}

/// StringifyByItemsWithCtx is used to print ByItems slice.
// go: StringifyByItemsWithCtx L1
pub fn stringify_by_items_with_ctx(ctx: &dyn EvalContext, by_items: &[ByItems]) -> Vec<u8> {
    let mut sb = b"[".to_vec();
    for (i, item) in by_items.iter().enumerate() {
        sb.extend(item.string_with_ctx(ctx, RedactLogDisable));
        if i != by_items.len() - 1 {
            sb.push(b' ');
        }
    }
    sb.push(b']');
    sb
}
