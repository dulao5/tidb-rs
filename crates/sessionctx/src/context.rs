//! go-file: pkg/sessionctx/context.go
//!
//! Block D00.1 holds two functions of this file: `basicCtxType.String` and
//! `ValidateSnapshotReadTS`. The `sessionctx.Context` interface itself is the session (N2 §3.1:
//! `Session` owning `SessionVars`, `Arc<StmtCtx>` and the shared parts) and comes with D05.3.
//! `ValidateSnapshotReadTS` needs `kv.Storage.GetOracle` (the oracle client, D07.1) and is
//! exempted until then.

// Constants keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

/// The keys of the session context values (Go `basicCtxType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BasicCtxType(pub i64);

/// QueryString is the key for original query string.
pub const QueryString: BasicCtxType = BasicCtxType(1);
/// Initing is the key for indicating if the server is running bootstrap or upgrade job.
pub const Initing: BasicCtxType = BasicCtxType(2);
/// LastExecuteDDL is the key for whether the session execute a ddl command last time.
pub const LastExecuteDDL: BasicCtxType = BasicCtxType(3);

impl BasicCtxType {
    /// String implements fmt.Stringer.
    // go: basicCtxType.String L1
    pub fn string(self) -> &'static str {
        match self {
            QueryString => "query_string",
            Initing => "initing",
            LastExecuteDDL => "last_execute_ddl",
            _ => "unknown",
        }
    }
}

impl std::fmt::Display for BasicCtxType {
    // glue: fmt.Stringer, used by `%s` / `%v`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.string())
    }
}
