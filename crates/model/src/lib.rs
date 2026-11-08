//! Port of Go TiDB v8.5.8 `pkg/meta/model` (block D06.1): the schema structs that Go TiDB
//! stores in TiKV as JSON, their `encoding/json` form, and their methods.
//!
//! The structs, constants and JSON encoders/decoders are generated from Go reflection by the
//! tracker's `govec/cmd/rsgen -model` (`*_gen.rs`); the Go methods are ported by hand in the
//! modules named after the Go files. See `specs/D06.1-model/`.

#![forbid(unsafe_code)]

mod any;
mod bdr;
mod column;
mod consts_gen;
mod db;
mod gocopy;
mod gotime;
mod index;
mod job;
mod placement;
mod reorg;
mod resource_group;
mod table;
mod types_etc;
mod types_gen;

pub use any::AnyValue;
pub use bdr::*;
pub use column::{
    DefaultValueRef, find_column_info, find_column_info_by_id, gen_removing_obj_name,
    gen_unique_changing_column_name, new_extra_handle_col_info, new_extra_phys_tbl_id_col_info,
};
pub use consts_gen::*;
pub use db::*;
pub use index::*;
pub use job::*;
pub use resource_group::*;
pub use table::*;
pub use types_etc::*;
pub use types_gen::*;
