//! go-file: pkg/meta/meta_autoid.go
//!
//! The auto ID counters of a table, stored in the database's hash. Only the reads are
//! ported; `Put`, `Inc`, `Del`, `CopyTo` (and `AutoIDAccessors.Put` / `Del`) are X.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_model::TableInfoVersion5;

use super::Mutator;

/// `model.AutoIDGroup` (job_args.go): the auto IDs of a table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoIDGroup {
    /// RowID.
    pub row_id: i64,
    /// IncrementID.
    pub increment_id: i64,
    /// RandomID.
    pub random_id: i64,
}

/// AutoIDAccessor represents the entry to retrieve/mutate auto IDs.
#[derive(Clone, Copy)]
pub struct AutoIDAccessor<'a> {
    m: &'a Mutator,
    database_id: i64,
    table_id: i64,
    id_encode_fn: fn(i64) -> Vec<u8>,
}

impl AutoIDAccessor<'_> {
    /// Get implements the interface AutoIDAccessor. Go-shaped (the parsed value is returned
    /// with ParseInt's error).
    // go: autoIDAccessor.Get L1
    pub async fn get(&self) -> (i64, Result<(), Error>) {
        let m = self.m;
        m.txn
            .h_get_int64(
                &Mutator::db_key(self.database_id),
                &(self.id_encode_fn)(self.table_id),
            )
            .await
    }
}

/// AutoIDAccessors represents all the auto IDs of a table.
///
/// Deviation (spec §4, L2): Go's accessors share one `autoIDAccessor` value and each
/// picker (`RowID()`, `IncrementID()`, ...) changes its `idEncodeFn` and returns a pointer
/// to it, so an accessor obtained earlier switches to the latest picked key. Rust returns
/// an independent accessor per pick; Go callers use each accessor right after picking it.
#[derive(Clone, Copy)]
pub struct AutoIDAccessors<'a> {
    access: AutoIDAccessor<'a>,
}

/// sepAutoIncVer is the table version that separates the auto increment ID from the row ID.
const sepAutoIncVer: u16 = TableInfoVersion5;

impl<'a> AutoIDAccessors<'a> {
    /// Get implements the interface AutoIDAccessors. Go-shaped: the IDs read before an error
    /// are returned with it.
    // go: autoIDAccessors.Get L1
    pub async fn get(&self) -> (AutoIDGroup, Result<(), Error>) {
        let mut auto_ids = AutoIDGroup::default();
        let (v, err) = self.row_id().get().await;
        auto_ids.row_id = v;
        if err.is_err() {
            return (auto_ids, err);
        }
        let (v, err) = self.increment_id(sepAutoIncVer).get().await;
        auto_ids.increment_id = v;
        if err.is_err() {
            return (auto_ids, err);
        }
        let (v, err) = self.random_id().get().await;
        auto_ids.random_id = v;
        (auto_ids, err)
    }

    /// RowID is used to get the _tidb_rowid meta key-value accessor.
    // go: autoIDAccessors.RowID L1
    pub fn row_id(&self) -> AutoIDAccessor<'a> {
        AutoIDAccessor {
            id_encode_fn: Mutator::auto_table_id_key,
            ..self.access
        }
    }

    /// IncrementID is used to get the auto_increment ID meta key-value accessor.
    // go: autoIDAccessors.IncrementID L1
    pub fn increment_id(&self, table_version: u16) -> AutoIDAccessor<'a> {
        // _tidb_rowid and auto_increment ID in old version TiDB share the same meta key-value.
        let id_encode_fn = if table_version < sepAutoIncVer {
            Mutator::auto_table_id_key
        } else {
            Mutator::auto_increment_id_key
        };
        AutoIDAccessor {
            id_encode_fn,
            ..self.access
        }
    }

    /// RandomID is used to get the auto_random ID meta key-value accessor.
    // go: autoIDAccessors.RandomID L1
    pub fn random_id(&self) -> AutoIDAccessor<'a> {
        AutoIDAccessor {
            id_encode_fn: Mutator::auto_random_table_id_key,
            ..self.access
        }
    }

    /// SequenceValue is used to get the sequence value key-value accessor.
    // go: autoIDAccessors.SequenceValue L1
    pub fn sequence_value(&self) -> AutoIDAccessor<'a> {
        AutoIDAccessor {
            id_encode_fn: Mutator::sequence_key,
            ..self.access
        }
    }

    /// SequenceCycle is used to get the sequence cycle key-value accessor.
    // go: autoIDAccessors.SequenceCycle L1
    pub fn sequence_cycle(&self) -> AutoIDAccessor<'a> {
        AutoIDAccessor {
            id_encode_fn: Mutator::sequence_cycle_key,
            ..self.access
        }
    }
}

/// NewAutoIDAccessors creates a new AutoIDAccessors.
// go: NewAutoIDAccessors L1
pub fn new_auto_id_accessors(m: &Mutator, database_id: i64, table_id: i64) -> AutoIDAccessors<'_> {
    AutoIDAccessors {
        access: AutoIDAccessor {
            m,
            database_id,
            table_id,
            // Go's zero idEncodeFn is never called: every Get goes through a picker.
            id_encode_fn: Mutator::auto_table_id_key,
        },
    }
}
