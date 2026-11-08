//! Port of Go TiDB v8.5.8 `pkg/util/chunk` (block D01.7): `Chunk`, `Column`, `Row`, the
//! chunk codec, `List`, the row iterators and comparators.
//!
//! The design is fixed by N5a (`specs/exceptions/N5a-chunk/spec.md`): Go's column format
//! (L1), buffers that are owned or zero-copy slices of a response buffer, 1-byte aligned
//! typed views, `Arc<Column>` for column sharing, and A1's memory accounting with no reuse
//! pool. The block spec is `specs/D01.7-chunk/`. N0 rank 3.

#![deny(unsafe_code)]

pub mod alloc;
pub mod chunk;
pub mod chunk_util;
pub mod codec;
pub mod column;
pub mod compare;
mod goslice;
pub mod iterator;
pub mod list;
pub mod row;

pub use chunk::Chunk;
pub use column::Column;
pub use goslice::ColAlloc;
pub use list::List;
pub use list::RowPtr;
pub use row::Row;

/// The shared handle of a chunk column (N5a 3.2: `MakeRef` / `SwapColumns` share a column).
/// `triomphe::Arc` has no weak count, so `make_mut`'s uniqueness check is one load instead of
/// std's compare-and-swap (D01.7 plan §3); the alias keeps the choice in one place.
pub type ColArc<T> = triomphe::Arc<T>;
