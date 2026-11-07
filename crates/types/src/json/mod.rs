//! The JSON type (block D01.5, exception A4: `specs/exceptions/A4-json/spec.md`): the Go files
//! `pkg/types/json_*.go`, one Rust file each.

mod json_binary;
mod json_binary_functions;
mod json_constants;
mod json_path_expr;

pub use json_binary::{
    BinaryJSON, JSONValue, Opaque, create_binary_json, create_binary_json_with_check, json_valid,
    parse_binary_json_from_string,
};
pub use json_binary_functions::{
    compare_binary_json, contains_binary_json, merge_binary_json, merge_patch_binary_json,
    overlaps_binary_json, peek_bytes_as_json, unquote_string,
};
pub use json_constants::*;
pub use json_path_expr::{JSONPathExpression, parse_json_path_expr};
