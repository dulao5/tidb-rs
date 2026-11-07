//! Port of Go TiDB v8.5.8 `pkg/parser/mysql`: MySQL protocol and type constants, error
//! codes, messages and SQLSTATEs, charsets and collations, privileges (block D00.2).
//!
//! Constants keep their Go names (`mysql::TypeLonglong`, `mysql::ErrDupEntry`) so that ported
//! code reads like the Go it comes from; the `*_gen.rs` files, `errcode.rs` and `state.rs` are
//! generated from the Go source (tracker `govec/cmd/rsgen`) and checked against Go by the
//! govec domain "base".

#![allow(non_upper_case_globals)]

mod charset;
mod charset_gen;
#[path = "const.rs"]
mod const_;
mod const_gen;
mod errcode;
mod errname;
mod errname_gen;
mod error;
mod locale_format;
mod privs;
mod privs_gen;
mod state;
#[path = "type.rs"]
mod type_;
mod type_gen;
mod util;
mod util_gen;

pub use charset::*;
pub use charset_gen::*;
pub use const_::*;
pub use const_gen::*;
pub use errcode::*;
pub use errname::*;
pub use errname_gen::*;
pub use error::*;
pub use locale_format::*;
pub use privs::*;
pub use privs_gen::*;
pub use state::*;
pub use type_::*;
pub use type_gen::*;
pub use util::*;
pub use util_gen::*;
