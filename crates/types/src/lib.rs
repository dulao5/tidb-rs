//! Port of Go TiDB v8.5.8 `pkg/types` (so far blocks D01.2 `MyDecimal`, D01.3 time and
//! Duration, with `types.Context`, D01.6 `VectorFloat32`, and D01.9 `util/collate`).
//!
//! The Go code is the specification: every function maps to one Go function (see the
//! `// go:` annotations and `specs/D01.2-mydecimal/`).

#![forbid(unsafe_code)]

pub mod binary_literal;
pub mod collate;
pub mod compare;
pub mod context;
pub mod convert;
pub mod core_time;
pub mod datum;
pub mod datum_eval;
pub mod enum_type;
pub mod errors;
pub mod etc;
pub mod field_name;
pub mod field_type;
pub mod field_type_builder;
pub mod fsp;
pub mod gostd;
pub mod helper;
pub mod json;
mod jsoniter;
pub mod mydecimal;
#[cfg(test)]
mod mydecimal_test;
pub mod overflow;
pub mod set_type;
pub mod stringutil;
pub mod time;
mod truncate;
mod util_parser;
pub mod vector;
mod vector_functions;

pub use errors::Error;
pub use mydecimal::{
    MODE_CEILING, MODE_HALF_UP, MODE_TRUNCATE, MY_DECIMAL_STRUCT_SIZE, MyDecimal, RoundMode,
    decimal_add, decimal_bin_size, decimal_div, decimal_mod, decimal_mul, decimal_neg,
    decimal_peak, decimal_sub, new_dec_from_float_for_test, new_dec_from_int,
    new_dec_from_string_for_test, new_dec_from_uint, new_max_or_min_dec,
};
