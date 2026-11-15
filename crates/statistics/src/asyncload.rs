//! go-file: pkg/statistics/asyncload/async_load.go
//!
//! `AsyncLoadHistogramNeededItems` (the global set of column / index statistics the stats
//! handle should load in the background) belongs to the stats loading of D11.2. tidb-rs has
//! no stats handle yet (VS1 plan §3: the new stack only sees pseudo statistics), so there is
//! nothing to load: the request is dropped. It never changes an estimate of the statement
//! that makes it (Go loads asynchronously, for later statements).

use tidb_model::TableItemID;

/// `asyncload.AsyncLoadHistogramNeededItems.Insert(item, true)`.
// glue: stub of neededStatsMap.Insert, blocked-by:D11.2 (stats handle and its async loader; no statistics to load before it)
pub(crate) fn insert_needed_item(item: TableItemID) {
    let _ = item;
}
