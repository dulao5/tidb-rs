//! The InfoSchema crate (N0 rank 9): blocks D06.2, D06.3, D06.4 and D06.6.
//!
//! Block D06.2 part 1 (specs/D06.2-meta): the read side of TiDB's meta layout.
//! - [`structure`]: `pkg/structure`, the string / hash / list layout of meta keys over a
//!   `kv.Retriever`;
//! - [`meta`]: `pkg/meta`, `meta.Reader` (databases, tables, policies, resource groups,
//!   schema version and diffs, DDL jobs, the meta autoid accessors);
//! - [`metadef`]: `pkg/meta/metadef`, the system database names.
//!
//! Block D06.3 part 1 (specs/D06.3-infoschema): `pkg/infoschema`, infoschema v2 only (the
//! owner's decision; v1 is not ported), its caching layout by exception A3.
//! - [`infoschema_v2`]: `Data` (the multi-version index shared by all versions) and
//!   `InfoSchemaV2` (one version's view);
//! - [`builder`]: the table construction and charset conversions shared with the lazy
//!   table loading (the full load and the schema diffs come in the next PR of part 1);
//! - [`infoschema`]: `infoSchemaMisc` and helpers; [`sieve`]: the table cache;
//! - [`context`]: `pkg/infoschema/context`; [`error`]: the package's errors.
//!
//! Stand-ins until their blocks land: [`table`] (`pkg/table`, D06.4), [`autoid`]
//! (`pkg/meta/autoid`, D06.2 part 2), [`placement`] (`pkg/ddl/placement`, X).
//!
//! tidb-rs is read-only: the writers (`Mutator.Create*`, `TxStructure.HSet`, ...) run in
//! Go TiDB and are not ported (X, tracker rules).

pub mod autoid;
pub mod builder;
pub mod context;
pub mod error;
pub mod infoschema;
pub mod infoschema_v2;
pub mod meta;
pub mod metadef;
pub mod placement;
pub mod sieve;
pub mod structure;
pub mod table;

pub use infoschema_v2::{Data, InfoSchemaV2, new_data};

impl tidb_sessionctx::expropt::MetaOnlyInfoSchema for InfoSchemaV2 {
    // glue: stand-in: interface method MetaOnlyInfoSchema.SchemaMetaVersion (infoSchemaMisc.SchemaMetaVersion)
    fn schema_meta_version(&self) -> i64 {
        self.base().schema_meta_version()
    }
}
