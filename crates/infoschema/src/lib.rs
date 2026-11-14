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
//! - [`builder`], [`builder_misc`], [`bundle_builder`]: full load and schema diffs;
//! - [`infoschema`]: `infoSchemaMisc` and helpers; [`sieve`]: the table cache;
//! - [`context`]: `pkg/infoschema/context`; [`error`]: the package's errors.
//!
//! Block D06.4 part 1 (specs/D06.4-table): the read path of the table abstraction.
//! - [`table`]: `pkg/table`, the `Table` / `Index` interfaces, columns and default values;
//! - [`tables`]: `pkg/table/tables`, `TableCommon`, `TableFromMeta`, raw row decoding;
//! - [`util`]: the `pkg/util` leaf files they need first (`generatedexpr`, `misc`).
//!
//! Stand-ins until their blocks land: [`autoid`] (`pkg/meta/autoid`, D06.2 part 2),
//! [`placement`] (`pkg/ddl/placement`, X).
//!
//! tidb-rs is read-only: the writers (`Mutator.Create*`, `TxStructure.HSet`, ...) run in
//! Go TiDB and are not ported (X, tracker rules).

pub mod autoid;
pub mod builder;
pub mod builder_misc;
pub mod bundle_builder;
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
pub mod tables;
pub mod util;

pub use builder::{Builder, new_builder};
pub use infoschema_v2::{Data, InfoSchemaV2, new_data};

impl tidb_sessionctx::expropt::MetaOnlyInfoSchema for InfoSchemaV2 {
    // glue: stand-in: interface method MetaOnlyInfoSchema.SchemaMetaVersion (infoSchemaMisc.SchemaMetaVersion)
    fn schema_meta_version(&self) -> i64 {
        self.base().schema_meta_version()
    }
}

/// The error of a temporary boundary stub (constitution §4.3): MySQL 1235 with the RFC code
/// text `blocked-by:<block>`, the same error as `tidb_base::blocked::blocked` of VS1-02,
/// which replaces this copy once it lands.
// glue: boundary stub error (constitution §4.3, VS1 plan §4.3)
pub(crate) fn blocked(block: &str, what: &str) -> tidb_base::Error {
    use tidb_base::errors::{IntoErrCode, mysql_error_code, normalize, rfc_code_text};
    let msg = format!("This version of TiDB doesn't yet support '{what} (blocked-by:{block})'");
    normalize(
        msg,
        [
            rfc_code_text(format!("blocked-by:{block}")),
            mysql_error_code(tidb_base::mysql::ErrNotSupportedYet.into_err_code()),
        ],
    )
}
