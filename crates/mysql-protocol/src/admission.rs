//! A command's statement-admission token together with Go's `tidb_server_tokens` gauge
//! (`server.getToken` / `releaseToken`): both are released by `Drop`, so a connection task
//! aborted in the middle of a command (shutdown, a future KILL CONNECTION) does not leak the
//! gauge.

use std::time::Instant;

/// Held for the duration of one command (Go: from `getToken` in `dispatch` to the deferred
/// `releaseToken`).
pub struct HeldToken {
    _token: Option<tidb_runtime::admission::Token>,
}

impl HeldToken {
    /// Go `server.getToken`: wait for a token from `admission` (`None` admits at once), then
    /// `tokens` + 1 and the wait in `get_token_duration`.
    pub async fn acquire(admission: Option<&tidb_runtime::Admission>) -> HeldToken {
        let start = Instant::now();
        let token = match admission {
            Some(a) => Some(a.get().await),
            None => None,
        };
        tidb_metrics::token_acquired(start.elapsed());
        HeldToken { _token: token }
    }
}

impl Drop for HeldToken {
    /// Go `server.releaseToken`: the token goes back (its own `Drop`) and `tokens` - 1.
    fn drop(&mut self) {
        tidb_metrics::token_released();
    }
}
