//! `pkg/sessiontxn/staleread` (block D05.4): stale reads are part 2 (`AS OF TIMESTAMP`,
//! `tidb_read_staleness`, `tx_read_ts`, external ts — VS1 plan §1.2 routes them to the
//! PoC). Part 1 has `IsStmtStaleness`, which the isolation providers call, and the
//! skeleton of the staleness provider.

mod provider;
mod util;

pub use provider::{StalenessTxnContextProvider, new_staleness_txn_context_provider};
pub use util::is_stmt_staleness;
