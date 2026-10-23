// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

// libtest benchmarks need a nightly compiler; enable them with the `nightly`
// feature.
#![cfg_attr(all(test, feature = "nightly"), feature(test))]

#[cfg(all(test, feature = "nightly"))]
extern crate test;
#[allow(unused_extern_crates)]
extern crate tikv_alloc;

pub mod buffer;
pub mod byte;
mod convert;
mod error;
mod hint;
pub mod number;

pub mod prelude {
    pub use super::{
        buffer::{BufferReader, BufferWriter},
        byte::{
            CompactByteDecoder, CompactByteEncoder, MemComparableByteDecoder,
            MemComparableByteEncoder,
        },
        number::{NumberDecoder, NumberEncoder},
    };
}

pub use self::error::{Error, ErrorInner, Result};
