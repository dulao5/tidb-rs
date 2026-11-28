//! The values of Go's `context.Context` that `pkg/kv` reads (mapping rule M-GOCTX).
//!
//! Go threads a `context.Context` through every KV call; `pkg/kv` reads the request
//! source (`util.RequestSourceKey`, set by `util.WithInternalSourceType`) and carries an
//! optional [`KillCancel`] handle so async RPC and backoff futures are dropped immediately
//! when killed (N3 3.1 item 6, §3.3). [`Context`] is cheap to clone, as Go's derived
//! contexts are cheap to create.

use tidb_runtime::KillCancel;

/// RequestSource contains the source label of the request, used for tracking resource
/// consuming.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestSource {
    pub request_source_internal: bool,
    pub request_source_type: String,
    /// ExplicitRequestSourceType is a type that is set from the session variable and may
    /// specified by the client or users. It is a complement to the RequestSourceType and
    /// provides additional information about how a request was initiated.
    pub explicit_request_source_type: String,
}

/// The values of a Go `context.Context` that the KV layer reads.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// `ctx.Value(util.RequestSourceKey)`.
    request_source: Option<RequestSource>,
    /// Session-level cancellation wakeup bound to `SqlKiller.signal`.
    kill_cancel: Option<KillCancel>,
}

impl Context {
    /// `context.Background()`.
    // glue: Go context.Background (M-GOCTX)
    pub fn background() -> Context {
        Context::default()
    }

    /// `context.WithValue(ctx, util.RequestSourceKey, source)`.
    // glue: Go context.WithValue for RequestSourceKey (M-GOCTX)
    pub fn with_request_source(&self, source: RequestSource) -> Context {
        Context {
            request_source: Some(source),
            kill_cancel: self.kill_cancel.clone(),
        }
    }

    /// Attaches a session-level [`KillCancel`] handle (Go `context.WithCancel`, M-GOCTX).
    // glue: N3 §3.3 session cancellation propagation into tidb-kv
    pub fn with_kill_cancel(&self, cancel: KillCancel) -> Context {
        Context {
            request_source: self.request_source.clone(),
            kill_cancel: Some(cancel),
        }
    }

    /// `ctx.Value(util.RequestSourceKey)`.
    // glue: Go ctx.Value(RequestSourceKey) (M-GOCTX)
    pub fn request_source(&self) -> Option<&RequestSource> {
        self.request_source.as_ref()
    }

    /// Session-level [`KillCancel`] handle if attached.
    // glue: N3 §3.3 session cancellation propagation
    pub fn kill_cancel(&self) -> Option<&KillCancel> {
        self.kill_cancel.as_ref()
    }
}

/// WithInternalSourceType create context with internal source.
// go-dep: client-go util.WithInternalSourceType L1
pub fn with_internal_source_type(ctx: &Context, source: &str) -> Context {
    ctx.with_request_source(RequestSource {
        request_source_internal: true,
        request_source_type: source.to_owned(),
        ..Default::default()
    })
}
