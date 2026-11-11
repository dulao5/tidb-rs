//! The InfoSchema crate (N0 rank 9): blocks D06.2, D06.3, D06.4 and D06.6.
//!
//! Ported so far, block D06.2 part 1 (specs/D06.2-meta): the read side of TiDB's meta
//! layout.
//! - [`structure`]: `pkg/structure`, the string / hash / list layout of meta keys over a
//!   `kv.Retriever`;
//! - [`meta`]: `pkg/meta`, `meta.Reader` (databases, tables, policies, resource groups,
//!   schema version and diffs, DDL jobs, the meta autoid accessors);
//! - [`metadef`]: `pkg/meta/metadef`, the system database names.
//!
//! tidb-rs is read-only: the writers (`Mutator.Create*`, `TxStructure.HSet`, ...) run in
//! Go TiDB and are not ported (X, tracker rules).

pub mod meta;
pub mod metadef;
pub mod structure;
