//! The statistics crate (N0 rank 10): blocks D11.1 and D11.2 (`pkg/statistics`).
//!
//! D11.1 part 1 (VS1-29, specs/D11.1-statistics): the structures the planner reads —
//! [`table::Table`], [`table::HistColl`], [`column::Column`], [`index::Index`],
//! [`histogram::Histogram`] — their read methods and [`table::pseudo_table`]. Building
//! histograms (ANALYZE, delegated to Go TiDB), the Count-Min / FM sketches, sampling, merging
//! global statistics and the storage encodings are D11.1 part 2; loading statistics and the
//! stats cache are D11.2.
//!
//! Statistics are long-lived shared metadata (N13 `M-SHARE`, spec §3): the stats cache will
//! hand out `Arc<Table>` snapshots, read-only; an update builds a new value and replaces the
//! cache entry. None of the types here has interior mutability (`check_model_mutability.py`
//! covers this crate).
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`, so a bare `.clone()`
// is always a value clone (Go `Clone`).
#![warn(clippy::clone_on_ref_ptr)]
// Go package-level names (Version0, PseudoRowCount, AllEvicted, ...), mapping rule M-GONAME.
#![allow(non_upper_case_globals)]

pub mod asyncload;
pub mod cmsketch;
pub mod column;
pub mod fmsketch;
pub mod histogram;
pub mod index;
pub mod scalar;
pub mod table;

pub use column::{Column, column_stats_is_invalid};
pub use histogram::{Histogram, Version0, Version1, Version2};
pub use index::{Index, index_stats_is_invalid};
pub use table::{HistColl, Table, pseudo_table};

/// Static assertions: the shared statistics can be handed to other threads (the stats cache
/// publishes `Arc<Table>` snapshots).
// glue: N13 (Send + Sync of the shared statistics)
const _: () = {
    // glue: N13 static assertion
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Table>();
    assert_send_sync::<HistColl>();
    assert_send_sync::<Column>();
    assert_send_sync::<Index>();
};
