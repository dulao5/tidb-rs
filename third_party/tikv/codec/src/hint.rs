// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Branch prediction hints that work on stable Rust.
//!
//! `core::intrinsics::{likely, unlikely}` are nightly-only. Calling a `#[cold]`
//! function on the unexpected path gives LLVM the same hint.

#[inline(always)]
#[cold]
fn cold() {}

#[inline(always)]
pub(crate) fn likely(b: bool) -> bool {
    if !b {
        cold();
    }
    b
}

#[inline(always)]
pub(crate) fn unlikely(b: bool) -> bool {
    if b {
        cold();
    }
    b
}
