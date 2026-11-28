//! Read-only transactions of the prototype session: `BEGIN` / `START TRANSACTION`,
//! `autocommit = 0`, `COMMIT` / `ROLLBACK` (glue; D05.4 spec §6, #547).
//!
//! The prototype holds the transaction (spec §6.3: the router keeps explicit transactions and
//! `autocommit = 0` off the new stack until its metadata-lock entries reach the cluster). This
//! module is the transaction's state and the read-timestamp rules of Go v8.5.8's transaction
//! context providers; the session runs the reads ([`crate::Session`]):
//!
//! - `BEGIN` takes the start ts at once (`baseTxnContextProvider.OnInitialize` with
//!   `EnterNewTxnWithBeginStmt` activates the transaction, `pkg/sessiontxn/isolation/base.go`);
//!   with `autocommit = 0` the transaction starts at the first statement that needs a read ts
//!   (`EnterNewTxnBeforeStmt`, activated by `GetStmtReadTS`; `base.go:303-305` sets `InTxn`
//!   then).
//! - Optimistic, pessimistic repeatable-read and serializable transactions read the start ts;
//!   pessimistic read-committed ones take a new ts for every statement, except the statement
//!   that activates a lazy transaction, which reads the start ts itself
//!   (`readcommitted.go:103`, `:142-158`).
//!
//! The provider's choice is Go's `txnManager.newProviderWithRequest`: the mode of `BEGIN
//! PESSIMISTIC/OPTIMISTIC`, else the session's `tidb_txn_mode` (a lazy transaction:
//! `session.PrepareTxnCtx`, pessimistic only for `tidb_txn_mode = pessimistic`); a pessimistic
//! transaction by `IsolationLevelForNewTxn`.

use std::sync::Arc;
use std::time::Instant;

use tidb_meta::InfoSchema;
pub(crate) use tidb_metrics::TxnEnd;

/// Go's `ast.Pessimistic` (the value `tidb_txn_mode` and `BEGIN PESSIMISTIC` give).
const PESSIMISTIC: &str = "PESSIMISTIC";
/// Go's `ast.ReadCommitted`.
const READ_COMMITTED: &str = "READ-COMMITTED";

/// Where the statements of a transaction read (the provider's `getStmtReadTSFunc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadTsRule {
    /// The transaction's start ts: optimistic (`optimistic.go:46`), pessimistic repeatable
    /// read (`repeatable_read.go:66`) and serializable (`serializable.go:50`).
    StartTs,
    /// A new ts for every statement: pessimistic read committed (`getStmtTS`).
    PerStatement,
}

/// The read ts of a statement in a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StmtTs {
    /// Take a TSO: it is the start ts of the transaction this statement activates (a lazy
    /// transaction), and the statement reads at it.
    Activate,
    /// Read at the transaction's start ts.
    Start(u64),
    /// Take a new TSO and read at it (read committed).
    NewTso,
}

/// Go's provider choice (`txnManager.newProviderWithRequest`): pessimistic and read committed
/// is the only combination with a ts per statement. `mode` is the transaction mode (`BEGIN
/// PESSIMISTIC/OPTIMISTIC`, else `tidb_txn_mode`; empty or `OPTIMISTIC` is optimistic),
/// `isolation` the session's `IsolationLevelForNewTxn()`.
// glue: the read-ts part of txnManager.newProviderWithRequest (D05.4 spec §6.1)
pub(crate) fn read_ts_rule(mode: &str, isolation: &str) -> ReadTsRule {
    if mode == PESSIMISTIC && isolation == READ_COMMITTED {
        ReadTsRule::PerStatement
    } else {
        ReadTsRule::StartTs
    }
}

/// The mode of a lazy transaction (`session.PrepareTxnCtx`, `session.go:4196-4202`):
/// pessimistic only when `tidb_txn_mode` is; otherwise optimistic (Go passes `ast.Optimistic`
/// explicitly, so an empty `tidb_txn_mode` is optimistic too).
// glue: the txnMode choice of session.PrepareTxnCtx (D05.4 spec §6.1)
pub(crate) fn lazy_txn_mode(tidb_txn_mode: &str) -> &'static str {
    if tidb_txn_mode == PESSIMISTIC {
        PESSIMISTIC
    } else {
        "OPTIMISTIC"
    }
}

/// The mode of a `BEGIN`: its own (`BEGIN PESSIMISTIC/OPTIMISTIC`), else the session's
/// `tidb_txn_mode` (`newProviderWithRequest`: `if txnMode == "" { txnMode = sessVars.TxnMode }`).
// glue: the txnMode default of txnManager.newProviderWithRequest (D05.4 spec §6.1)
pub(crate) fn begin_txn_mode<'a>(begin_mode: &'a str, tidb_txn_mode: &'a str) -> &'a str {
    if begin_mode.is_empty() {
        tidb_txn_mode
    } else {
        begin_mode
    }
}

/// A transaction of the prototype session, from its activation (the start ts was taken: Go's
/// `txn.Valid()` and `InTxn`) to its end.
#[derive(Debug)]
pub(crate) struct PocTxn {
    rule: ReadTsRule,
    /// `TxnCtx.IsPessimistic`: the `txn_mode` label of the transaction metrics.
    pessimistic: bool,
    /// `TxnCtx.StartTS`.
    start_ts: u64,
    /// `TxnCtx.CreateTime`: the transaction metrics measure from it.
    created: Instant,
    /// The InfoSchema visible at `start_ts`, once a statement read at the start ts found it
    /// (D05.4 spec §6.4): the later statements that read the start ts plan with it.
    start_schema: Option<Arc<InfoSchema>>,
}

impl PocTxn {
    /// A transaction of `mode` at `isolation` activated at `start_ts`; `created` is when its
    /// context was made (`BEGIN`, or the start of the statement that activated it).
    // glue: the prototype's transaction state (D05.4 spec §6.4)
    pub(crate) fn new(mode: &str, isolation: &str, start_ts: u64, created: Instant) -> PocTxn {
        PocTxn {
            rule: read_ts_rule(mode, isolation),
            pessimistic: mode == PESSIMISTIC,
            start_ts,
            created,
            start_schema: None,
        }
    }

    /// The start ts (`TxnCtx.StartTS`).
    // glue: the prototype's transaction state (D05.4 spec §6.4)
    pub(crate) fn start_ts(&self) -> u64 {
        self.start_ts
    }

    /// The read-ts rule.
    #[cfg(test)]
    // glue: the prototype's transaction state (D05.4 spec §6.4)
    pub(crate) fn rule(&self) -> ReadTsRule {
        self.rule
    }

    /// The read ts of the next statement of this (active) transaction: the start ts, or a new
    /// TSO for read committed (after `BEGIN` already the first statement takes one:
    /// `OnStmtStart` → `prepareStmt(!isTxnPrepared)` with the transaction prepared).
    // glue: the read-ts function of the providers (getStmtReadTSFunc), D05.4 spec §6.1
    pub(crate) fn stmt_ts(&self) -> StmtTs {
        match self.rule {
            ReadTsRule::StartTs => StmtTs::Start(self.start_ts),
            ReadTsRule::PerStatement => StmtTs::NewTso,
        }
    }

    /// The InfoSchema to plan a statement that reads at `ts` with, if known.
    // glue: the InfoSchema of the transaction's start ts (D05.4 spec §6.4)
    pub(crate) fn schema_at(&self, ts: u64) -> Option<Arc<InfoSchema>> {
        (ts == self.start_ts)
            .then(|| self.start_schema.clone())
            .flatten()
    }

    /// A statement read at `ts` with `schema`, the one visible there.
    // glue: the InfoSchema of the transaction's start ts (D05.4 spec §6.4)
    pub(crate) fn read_with(&mut self, ts: u64, schema: &Arc<InfoSchema>) {
        if ts == self.start_ts && self.start_schema.is_none() {
            self.start_schema = Some(Arc::clone(schema));
        }
    }

    /// The transaction ended. Records Go's `tidb_session_transaction_duration_seconds`
    /// (`recordOnTransactionExecution` at a commit, `session.go:839-841`; the rollback
    /// executor's, `simple.go:801-815`; `recordAbortTxnDuration` when the `SET` that turned
    /// `autocommit` on failed later, `tidb.go:327-330`): the transaction is valid, it has a
    /// start ts.
    // glue: the transaction metrics at commit / rollback (D05.4 spec §6.1)
    pub(crate) fn end(self, end: TxnEnd) {
        tidb_metrics::transaction_ended(self.created.elapsed(), self.pessimistic, end);
    }
}

/// The read ts of a statement with `txn` the session's transaction (`None`: none is active,
/// so the statement activates a lazy one, `autocommit = 0`).
// glue: GetStmtReadTS with or without an active transaction (D05.4 spec §6.1)
pub(crate) fn stmt_ts(txn: Option<&PocTxn>) -> StmtTs {
    txn.map_or(StmtTs::Activate, PocTxn::stmt_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's provider choice for each transaction mode and isolation level
    /// (`newProviderWithRequest`): only pessimistic read committed reads a ts per statement.
    #[test]
    fn provider_choice_follows_go() {
        for (mode, isolation, want) in [
            ("PESSIMISTIC", "READ-COMMITTED", ReadTsRule::PerStatement),
            ("PESSIMISTIC", "REPEATABLE-READ", ReadTsRule::StartTs),
            ("PESSIMISTIC", "SERIALIZABLE", ReadTsRule::StartTs),
            ("PESSIMISTIC", "READ-UNCOMMITTED", ReadTsRule::StartTs),
            ("OPTIMISTIC", "READ-COMMITTED", ReadTsRule::StartTs),
            ("OPTIMISTIC", "REPEATABLE-READ", ReadTsRule::StartTs),
            ("", "READ-COMMITTED", ReadTsRule::StartTs),
        ] {
            assert_eq!(read_ts_rule(mode, isolation), want, "{mode} {isolation}");
        }
    }

    /// `session.PrepareTxnCtx`: a lazy transaction is pessimistic only for
    /// `tidb_txn_mode = pessimistic`; `BEGIN`'s own mode wins over the variable.
    #[test]
    fn transaction_modes() {
        assert_eq!(lazy_txn_mode("PESSIMISTIC"), "PESSIMISTIC");
        assert_eq!(lazy_txn_mode("OPTIMISTIC"), "OPTIMISTIC");
        assert_eq!(lazy_txn_mode(""), "OPTIMISTIC");
        assert_eq!(begin_txn_mode("", "PESSIMISTIC"), "PESSIMISTIC");
        assert_eq!(begin_txn_mode("", ""), "");
        assert_eq!(begin_txn_mode("OPTIMISTIC", "PESSIMISTIC"), "OPTIMISTIC");
        assert_eq!(begin_txn_mode("PESSIMISTIC", "OPTIMISTIC"), "PESSIMISTIC");
    }

    /// The statements of one transaction: repeatable read reads the start ts every time;
    /// read committed takes a new TSO for every statement after the activation (the
    /// activating statement of a lazy transaction reads the start ts it took). No transaction:
    /// the statement activates one.
    #[test]
    fn statement_read_ts() {
        assert_eq!(stmt_ts(None), StmtTs::Activate);
        let now = Instant::now();
        let rr = PocTxn::new("PESSIMISTIC", "REPEATABLE-READ", 100, now);
        assert_eq!(rr.rule(), ReadTsRule::StartTs);
        for _ in 0..3 {
            assert_eq!(stmt_ts(Some(&rr)), StmtTs::Start(100));
        }
        let rc = PocTxn::new("PESSIMISTIC", "READ-COMMITTED", 100, now);
        for _ in 0..3 {
            assert_eq!(stmt_ts(Some(&rc)), StmtTs::NewTso);
        }
        let optimistic_rc = PocTxn::new("OPTIMISTIC", "READ-COMMITTED", 100, now);
        assert_eq!(stmt_ts(Some(&optimistic_rc)), StmtTs::Start(100));
    }

    /// The InfoSchema found at the start ts is kept for the later statements that read the
    /// start ts; a statement at another ts (read committed) neither uses nor replaces it.
    #[test]
    fn start_schema_is_cached_for_the_start_ts() {
        let mut t = PocTxn::new("PESSIMISTIC", "READ-COMMITTED", 100, Instant::now());
        assert!(t.schema_at(100).is_none());
        let s7 = Arc::new(InfoSchema::new(7, vec![]));
        let s8 = Arc::new(InfoSchema::new(8, vec![]));
        t.read_with(200, &s8);
        assert!(t.schema_at(100).is_none());
        assert!(t.schema_at(200).is_none());
        t.read_with(100, &s7);
        assert_eq!(t.schema_at(100).map(|s| s.version()), Some(7));
        t.read_with(100, &s8);
        assert_eq!(t.schema_at(100).map(|s| s.version()), Some(7));
    }
}
