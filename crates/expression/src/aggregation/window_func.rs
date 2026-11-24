//! go-file: pkg/expression/aggregation/window_func.go
//!
//! Part A of D08.11 needs only the type: `NewAggFuncDescForWindowFunc` (descriptor.go) takes
//! a `*WindowFuncDesc`. Its functions (`NewWindowFuncDesc`, `UseDefaultFrame`, `NeedFrame`,
//! `Clone`) come with part C (D08.11 spec §2.1); the two that convert to tipb live in
//! tidb-expr-pushdown (spec §4.8).

use std::ops::{Deref, DerefMut};

use super::base_func::BaseFuncDesc;

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
